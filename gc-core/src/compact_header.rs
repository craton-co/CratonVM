// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Compact 64-bit object headers per JEP 519 / Project Lilliput.
//!
//! Would reduce the per-object header from 16 bytes ([`ObjectHeader`], 32 until
//! the 2026-08-06/07 shrink) to 8 bytes,
//! saving 8 bytes per live object. Identity hash codes are stored in a
//! separate side table so the common case (no hash requested) pays nothing.
//!
//! ## Bit layout (64 bits)
//!
//! ```text
//! 63       32 31    25 24 23 22 21 20      4 3      0
//! +---------+--------+-----+--+--+----------+--------+
//! | NKlass  | GC age | Lck |H |A | varied   | flags  |
//! +---------+--------+-----+--+--+----------+--------+
//!   32 bits   7 bits  2 b  1  1    17 bits    4 bits
//! ```
//!
//! * **NKlass (63-32):** Compressed class pointer (`NarrowKlass`).
//! * **GC age (31-25):** Survivor age, max 127.
//! * **Lock (24-23):** `00` unlocked, `01` thin-lock, `10` inflated, `11` GC-forwarded.
//! * **H (22):** Has identity hash code in the side table.
//! * **A (21):** 1 = array, 0 = object.
//! * **Varied (20-4):** Reserved / array-length low bits / forwarding bits.
//! * **Flags (3-0):** GC flags or array element type.
//!
//! When the lock state is `11` (forwarded), bits 31-25 and 22-0 encode a
//! forwarding pointer shifted right by 3 (object-aligned). That inline field
//! is 30 bits wide; its top bit (bit 29 of the shifted value) is reserved as
//! an *overflow tag*. When clear, the remaining 29 bits hold `addr >> 3`
//! directly, covering addresses up to 4 GB inline. When set, the remaining 29
//! bits hold a *token* into the [`ForwardingOverflowTable`] side table, which
//! stores the full (up to 64-bit) target address. This keeps the common case
//! branch-free while never silently truncating a high (>4 GB, incl. >8 GB)
//! forwarding target.
//!
//! ## Status: NOT a reachable header mode (gc-common w1-g, 2026-09-23)
//!
//! No `VmHeap` backend lays an object out with this header. Every allocator
//! writes `cratonvm_types::ObjectHeader`, which is **16** bytes since
//! 2026-08-07 (class id, shape, and a mark word that already absorbed the
//! forwarding pointer, identity hash and GC quartet). Nothing outside this
//! file's tests constructs a `CompactHeader`, `CompactAllocator`,
//! `HashCodeTable` or `NarrowKlassTable`. (`VmHeap::get_compact_header`, the one runtime reader,
//! which decoded `class_id`/`shape` as a compact header, was deleted by
//! gc-common w2-e; `VmConfig::use_compact_headers` only echoes
//! `-XX:+UseCompactObjectHeaders` back into the reported JVM flags.) Treat
//! this module as a design prototype: in particular the process-global
//! [`clear_forwarding_overflow_table`] obligation below is unmet by
//! construction, because no collector ever forwards through a
//! `CompactHeader`.
//!
//! **Feature-gated since gc-common w4-e (2026-09-23).** The whole module (and
//! its crate-root re-exports) compiles only under
//! `cfg(any(test, doc, feature = "lilliput-prototype"))`: its own unit tests
//! and rustdoc still see it, a default build cannot name it, and the seven
//! `vm/src/vm/tests.rs` `s54_*` tests that exercised it through the crate root
//! were deleted (this file's tests cover the same ground). The identity hash of
//! a real object lives in the 16-byte header's mark word and moves with the
//! object; [`HashCodeTable::update_after_gc`] is NOT why hashes are GC-stable.
//! Record: `common-g-unreachable-header-and-oop-modes` under
//! `docs/feature-designs/generative-gc-architecture.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

// ---------------------------------------------------------------------------
// ForwardingOverflowTable — side table for forwarding targets that do not fit
// the inline 30-bit (post-tag: 29-bit) field of a compact header.
// ---------------------------------------------------------------------------

/// Global side table holding full forwarding-pointer targets that cannot be
/// encoded inline in a [`CompactHeader`].
///
/// A compact header only has 30 bits for an object-aligned forwarding pointer.
/// Reserving the top bit as an overflow tag leaves 29 bits, so addresses up to
/// 4 GB encode inline. On a real 64-bit VM, heap arenas can be mmap'd far above
/// that (well past 8 GB). For such targets the encoder stores the *full*
/// `usize` address here, keyed by a process-unique 29-bit token, and writes the
/// tag plus token inline. The decoder detects the tag and recovers the exact
/// address from this table — no truncation, ever.
///
/// The table mirrors the [`HashCodeTable`] side-table pattern used for identity
/// hashes (`RwLock<FxHashMap<..>>`). It is keyed by the inline token (which is
/// itself carried inside the 64-bit header value), so any `Copy` of the header
/// decodes to the same address.
struct ForwardingOverflowTable {
    /// token -> full forwarding target address.
    table: RwLock<FxHashMap<u64, usize>>,
    /// Monotonic source of fresh tokens. Starts at 1; token 0 is never handed
    /// out (a zeroed inline field must never resolve to a real overflow entry).
    next_token: AtomicU64,
}

impl ForwardingOverflowTable {
    pub fn new() -> Self {
        Self {
            table: RwLock::new(FxHashMap::default()),
            next_token: AtomicU64::new(1),
        }
    }

    /// Intern a full address, returning a fresh token in `1..=MAX_TOKEN`.
    ///
    /// Panics if the 29-bit token space is exhausted. Panicking is strictly
    /// better than wrapping a token and aliasing two unrelated objects to the
    /// same heap address.
    ///
    /// gci i44/ch: the bound is CUMULATIVE, not "simultaneously live" as this
    /// said. [`Self::clear`] empties the table but deliberately does not rewind
    /// `next_token` (a header still carrying a cleared token must resolve to
    /// `0`, not to an unrelated newer target), so the process can intern about
    /// 5.4e8 overflow forwardings in its whole life however often the table is
    /// cleared. A collector that adopts this header must either bound that
    /// total or rewind the counter at a point where it can prove no header
    /// carries a token.
    fn intern(&self, addr: usize) -> u64 {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        assert!(
            token <= CompactHeader::FORWARD_MAX_TOKEN,
            "forwarding overflow token space exhausted ({} live overflow forwardings)",
            CompactHeader::FORWARD_MAX_TOKEN,
        );
        self.table.write().insert(token, addr);
        token
    }

    /// Resolve a previously interned token to its full address.
    ///
    /// Returns `0` for an unknown token; callers only consult this for headers
    /// already tagged as overflow, so a miss indicates the table was cleared
    /// out from under a live forwarding (a logic error elsewhere).
    fn resolve(&self, token: u64) -> usize {
        self.table.read().get(&token).copied().unwrap_or(0)
    }

    /// Discard all interned forwardings. Safe to call once a GC cycle's
    /// forwarding pointers are no longer needed (objects have been relocated
    /// and references fixed up).
    fn clear(&self) {
        self.table.write().clear();
    }

    /// Number of interned overflow forwardings currently retained.
    pub fn len(&self) -> usize {
        self.table.read().len()
    }
}

/// Process-wide overflow table for compact-header forwarding pointers.
///
/// Lazily initialized via [`OnceLock`] (mirrors the `OnceLock` pattern used
/// elsewhere in this crate) so no `const` constructor is required.
static FORWARDING_OVERFLOW: std::sync::OnceLock<ForwardingOverflowTable> =
    std::sync::OnceLock::new();

#[inline]
fn forwarding_overflow() -> &'static ForwardingOverflowTable {
    FORWARDING_OVERFLOW.get_or_init(ForwardingOverflowTable::new)
}

/// Clear the global forwarding-overflow side table.
///
/// The GC should call this after a moving collection has fully relocated
/// objects and rewritten all references, so interned high-address forwardings
/// do not accumulate across cycles.
///
/// **NOT CALLED YET — this is an obligation, not a description.** As of
/// 2026-08-07 the only caller in the tree is this module's own
/// `forwarding_overflow_table_clears` test, because `CompactHeader` itself is
/// not yet adopted on any collector path (`migrate_to_compact`'s callers are
/// all tests too). Whoever adopts it inherits this: without a per-cycle clear
/// the table grows monotonically for the lifetime of the process, and
/// [`ForwardingOverflowTable::intern`] `assert!`s once the 29-bit token space is
/// exhausted — a hard panic rather than a leak you can outrun. The right place
/// is wherever the collector finishes rewriting references, next to whatever
/// already drops the pointer map.
///
/// Note the ordering constraint that makes this sharper than "free some
/// memory": clearing while any live header still carries an overflow token
/// makes that header's [`CompactHeader::forwarding_ptr`] answer `0`, silently,
/// because [`ForwardingOverflowTable::resolve`] returns `0` for an unknown
/// token. Too early is worse than too late.
pub fn clear_forwarding_overflow_table() {
    forwarding_overflow().clear();
}

/// Number of high-address forwarding pointers currently held in the global
/// overflow side table (primarily for diagnostics/tests).
pub fn forwarding_overflow_table_len() -> usize {
    forwarding_overflow().len()
}

// ---------------------------------------------------------------------------
// LockState
// ---------------------------------------------------------------------------

/// Two-bit lock state stored in the compact header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum LockState {
    /// No lock held.
    Unlocked = 0b00,
    /// Thin (biased/stack) lock.
    ThinLocked = 0b01,
    /// Inflated heavyweight monitor.
    Inflated = 0b10,
    /// GC forwarding pointer installed; bits 31-25 and 22-0 hold the split
    /// 30-bit inline forwarding field (`addr >> 3` or an overflow token).
    Forwarded = 0b11,
}

impl LockState {
    fn from_bits(bits: u8) -> Self {
        match bits & 0b11 {
            0b00 => Self::Unlocked,
            0b01 => Self::ThinLocked,
            0b10 => Self::Inflated,
            0b11 => Self::Forwarded,
            _ => unreachable!(),
        }
    }
}

// ---------------------------------------------------------------------------
// CompactHeader
// ---------------------------------------------------------------------------

/// 64-bit compact object header (Project Lilliput / JEP 519).
///
/// See module-level docs for the bit layout.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub struct CompactHeader(u64);

impl CompactHeader {
    // -- Bit-field geometry -------------------------------------------------

    pub const SIZE: usize = 8;
    pub const MAX_GC_AGE: u8 = 127; // 7 bits

    pub const KLASS_SHIFT: u32 = 32;
    pub const AGE_SHIFT: u32 = 25;
    pub const AGE_MASK: u64 = 0x7F << 25;
    pub const LOCK_SHIFT: u32 = 23;
    pub const LOCK_MASK: u64 = 0x3 << 23;
    pub const HASH_BIT: u64 = 1 << 22;
    pub const ARRAY_BIT: u64 = 1 << 21;
    pub const ELEM_TYPE_MASK: u64 = 0xF;
    /// Bits used for the forwarding pointer: bits 31-25 and 22-0 (excludes
    /// the lock-state bits 24-23 which are set to `11` as the forwarded tag).
    /// 30 bits total (the inline forwarding field).
    pub const FORWARD_MASK: u64 = 0xFE7F_FFFF; // bits 31-25 | bits 22-0

    /// Number of bits in the inline forwarding field (split across the header).
    pub const FORWARD_FIELD_BITS: u32 = 30;
    /// The top bit of the inline forwarding field is reserved as an *overflow
    /// tag*: when set, the remaining 29 bits hold a side-table token rather than
    /// `addr >> 3` directly.
    pub const FORWARD_OVERFLOW_TAG: u64 = 1 << (Self::FORWARD_FIELD_BITS - 1); // bit 29
    /// Payload mask for the inline forwarding field below the overflow tag
    /// (29 bits). Holds either the inline `addr >> 3` or an overflow token.
    pub const FORWARD_PAYLOAD_MASK: u64 = Self::FORWARD_OVERFLOW_TAG - 1; // bits 28-0
    /// Largest object-aligned address that fits inline (without the overflow
    /// side table). 29 payload bits, shifted left by 3 for 8-byte alignment:
    /// `(2^29 - 1) << 3` ≈ 4 GB. Targets at or below this encode inline; higher
    /// targets (incl. anything above 8 GB) go through the overflow side table.
    pub const FORWARD_INLINE_MAX_ADDR: usize = (Self::FORWARD_PAYLOAD_MASK as usize) << 3;
    /// Largest token the overflow side table can mint (29-bit token space).
    pub const FORWARD_MAX_TOKEN: u64 = Self::FORWARD_PAYLOAD_MASK;

    // -- Construction -------------------------------------------------------

    /// Create a header for a regular (non-array) object.
    pub fn new_object(narrow_klass: u32, gc_age: u8) -> Self {
        let age = (gc_age.min(Self::MAX_GC_AGE) as u64) << Self::AGE_SHIFT;
        let klass = (narrow_klass as u64) << Self::KLASS_SHIFT;
        Self(klass | age)
    }

    /// Create a header for an array object.
    ///
    /// `element_type` occupies the lowest 4 bits (values 0-15).
    pub fn new_array(narrow_klass: u32, element_type: u8, gc_age: u8) -> Self {
        let age = (gc_age.min(Self::MAX_GC_AGE) as u64) << Self::AGE_SHIFT;
        let klass = (narrow_klass as u64) << Self::KLASS_SHIFT;
        let elem = (element_type & 0xF) as u64;
        Self(klass | age | Self::ARRAY_BIT | elem)
    }

    // -- Compressed class pointer (NarrowKlass) -----------------------------

    /// Upper 32 bits: compressed class pointer.
    #[inline]
    pub fn narrow_klass(&self) -> u32 {
        (self.0 >> Self::KLASS_SHIFT) as u32
    }

    /// Replace the compressed class pointer.
    #[inline]
    pub fn set_narrow_klass(&mut self, klass: u32) {
        // Clear upper 32 bits, then set.
        self.0 = (self.0 & 0x0000_0000_FFFF_FFFF) | ((klass as u64) << Self::KLASS_SHIFT);
    }

    // -- GC age -------------------------------------------------------------

    /// Bits 31-25: GC survivor age (0..=127). Returns `0` when the header is
    /// forwarded (the forwarding field overwrites the lower 32 bits).
    #[inline]
    pub fn gc_age(&self) -> u8 {
        if self.is_forwarded() {
            return 0;
        }
        ((self.0 & Self::AGE_MASK) >> Self::AGE_SHIFT) as u8
    }

    /// Set the GC survivor age (clamped to [`MAX_GC_AGE`]).
    #[inline]
    pub fn set_gc_age(&mut self, age: u8) {
        let clamped = age.min(Self::MAX_GC_AGE) as u64;
        self.0 = (self.0 & !Self::AGE_MASK) | (clamped << Self::AGE_SHIFT);
    }

    /// Increment the GC age by one, capping at [`MAX_GC_AGE`]. Returns the new age.
    #[inline]
    pub fn increment_age(&mut self) -> u8 {
        let new_age = (self.gc_age() + 1).min(Self::MAX_GC_AGE);
        self.set_gc_age(new_age);
        new_age
    }

    // -- Lock state ---------------------------------------------------------

    /// Bits 24-23: lock state.
    #[inline]
    pub fn lock_state(&self) -> LockState {
        let bits = ((self.0 & Self::LOCK_MASK) >> Self::LOCK_SHIFT) as u8;
        LockState::from_bits(bits)
    }

    /// Set the lock state.
    #[inline]
    pub fn set_lock_state(&mut self, state: LockState) {
        self.0 = (self.0 & !Self::LOCK_MASK) | ((state as u64) << Self::LOCK_SHIFT);
    }

    // -- Identity hash code flag --------------------------------------------

    /// Bit 22: whether this object has a hash code in the side table. Returns
    /// `false` when the header is forwarded (the forwarding field overwrites
    /// the lower 32 bits).
    #[inline]
    pub fn has_hash_code(&self) -> bool {
        !self.is_forwarded() && (self.0 & Self::HASH_BIT != 0)
    }

    /// Set the identity-hash-code-present bit.
    #[inline]
    pub fn set_has_hash_code(&mut self) {
        self.0 |= Self::HASH_BIT;
    }

    // -- Array flag ---------------------------------------------------------

    /// Bit 21: 1 if this object is an array. Returns `false` when the header
    /// is forwarded (the forwarding field overwrites the lower 32 bits).
    #[inline]
    pub fn is_array(&self) -> bool {
        !self.is_forwarded() && (self.0 & Self::ARRAY_BIT != 0)
    }

    /// Element type for arrays (lower 4 bits). Meaningless for non-arrays;
    /// returns `0` when the header is forwarded.
    #[inline]
    pub fn element_type(&self) -> u8 {
        if self.is_forwarded() {
            return 0;
        }
        (self.0 & Self::ELEM_TYPE_MASK) as u8
    }

    // -- GC forwarding ------------------------------------------------------

    /// Returns `true` when the header encodes a forwarding pointer
    /// (lock state == `Forwarded`).
    #[inline]
    pub fn is_forwarded(&self) -> bool {
        self.lock_state() == LockState::Forwarded
    }

    /// Write a 30-bit value into the split inline forwarding field, set the
    /// `Forwarded` lock tag, and preserve the upper 32 bits (klass).
    ///
    /// The field is split around the lock-state bits (24-23): the low 23 bits
    /// of `field` land in header bits 22-0, the high 7 bits in header bits
    /// 31-25. `field` must fit in 30 bits.
    #[inline]
    fn write_forward_field(&mut self, field: u64) {
        debug_assert!(
            field >> Self::FORWARD_FIELD_BITS == 0,
            "forwarding field exceeds 30 bits"
        );
        let low = field & 0x7F_FFFF; // 23 bits -> header bits 22-0
        let high = (field >> 23) & 0x7F; // 7 bits  -> header bits 31-25
        let lock_bits = (LockState::Forwarded as u64) << Self::LOCK_SHIFT;
        let upper = self.0 & 0xFFFF_FFFF_0000_0000;
        self.0 = upper | (high << 25) | lock_bits | low;
    }

    /// Read the 30-bit value out of the split inline forwarding field.
    #[inline]
    fn read_forward_field(&self) -> u64 {
        let low = self.0 & 0x7F_FFFF; // bits 22-0: 23 bits
        let high = (self.0 >> 25) & 0x7F; // bits 31-25: 7 bits
        (high << 23) | low
    }

    /// Install a forwarding pointer. The address **must** be 8-byte aligned.
    ///
    /// This overwrites the lower 32 bits. The upper 32 bits (klass) are
    /// preserved so the GC can still identify the class of the forwarded
    /// object. The shifted address is split around the lock-state bits
    /// (24-23) which are set to `11` as the forwarded tag.
    ///
    /// Addresses up to [`FORWARD_INLINE_MAX_ADDR`](Self::FORWARD_INLINE_MAX_ADDR)
    /// (~4 GB) are stored inline. Higher targets — which the 30-bit inline
    /// field cannot represent and which a naive encoder would silently truncate
    /// (corrupting the heap on 64-bit VMs whose arenas live above 8 GB) — are
    /// interned in a global side table; the inline field then holds the overflow
    /// tag plus a token. Either way [`forwarding_ptr`](Self::forwarding_ptr)
    /// recovers the *exact* address.
    pub fn set_forwarding_ptr(&mut self, addr: usize) {
        debug_assert!(addr & 0x7 == 0, "forwarding address must be 8-byte aligned");
        // Real range check (replaces the alignment-only debug_assert): the
        // inline encoding can only represent 8-byte-aligned addresses, so an
        // unaligned address would lose its low bits. Tolerate it in release by
        // routing through the (full-width) side table rather than truncating.
        let aligned = (addr & 0x7) == 0;
        if aligned && addr <= Self::FORWARD_INLINE_MAX_ADDR {
            // Fast path: representable inline. `shifted` fits in 29 bits, so the
            // overflow tag (bit 29) stays clear.
            let shifted = (addr >> 3) as u64;
            debug_assert_eq!(shifted & Self::FORWARD_OVERFLOW_TAG, 0);
            self.write_forward_field(shifted);
        } else {
            // Overflow path: stash the full address in the side table and store
            // the overflow tag plus its token inline. Never truncates.
            let token = forwarding_overflow().intern(addr);
            debug_assert!(token <= Self::FORWARD_MAX_TOKEN);
            self.write_forward_field(
                Self::FORWARD_OVERFLOW_TAG | (token & Self::FORWARD_PAYLOAD_MASK),
            );
        }
    }

    /// Read the forwarding pointer. Only valid when [`is_forwarded`] is true.
    pub fn forwarding_ptr(&self) -> usize {
        let field = self.read_forward_field();
        if field & Self::FORWARD_OVERFLOW_TAG != 0 {
            // Overflow: the payload is a side-table token, not an address.
            let token = field & Self::FORWARD_PAYLOAD_MASK;
            return forwarding_overflow().resolve(token);
        }
        let shifted = field;
        (shifted << 3) as usize
    }

    // -- Raw access ---------------------------------------------------------

    /// Get the raw 64-bit backing value.
    #[inline]
    pub fn raw(&self) -> u64 {
        self.0
    }

    /// Construct from a raw 64-bit value (for deserialization / testing).
    #[inline]
    pub fn from_raw(v: u64) -> Self {
        Self(v)
    }
}

// ---------------------------------------------------------------------------
// HashCodeTable -- side table for identity hash codes
// ---------------------------------------------------------------------------

/// Side table for identity hash codes.
///
/// Most Java objects never have `System.identityHashCode()` called on them.
/// Storing the hash in the header wastes 4 bytes per object. Instead we
/// lazily allocate an entry here only when the hash is first requested.
/// T10.9.B: FxHashMap — keys are object pointer addresses (internal).
pub struct HashCodeTable {
    table: RwLock<FxHashMap<usize, i32>>,
    next_hash: AtomicI32,
}

impl HashCodeTable {
    /// Create an empty hash code table. The first generated hash will be `1`.
    pub fn new() -> Self {
        Self {
            table: RwLock::new(FxHashMap::default()),
            next_hash: AtomicI32::new(1),
        }
    }

    /// Get or assign an identity hash code for the object at `obj_addr`.
    ///
    /// If no hash has been assigned yet a new one is generated atomically.
    /// Subsequent calls with the same address return the same value.
    pub fn get_or_assign(&self, obj_addr: usize) -> i32 {
        // Fast path: already assigned.
        {
            let read = self.table.read();
            if let Some(&hash) = read.get(&obj_addr) {
                return hash;
            }
        }

        // Slow path: generate and insert.
        let mut write = self.table.write();
        // Double-check after acquiring write lock.
        if let Some(&hash) = write.get(&obj_addr) {
            return hash;
        }
        let hash = self.next_hash.fetch_add(1, Ordering::Relaxed);
        write.insert(obj_addr, hash);
        hash
    }

    /// Does the object at `obj_addr` have a hash code in this table?
    pub fn has_hash(&self, obj_addr: usize) -> bool {
        self.table.read().contains_key(&obj_addr)
    }

    /// Return the hash code if one has been assigned.
    pub fn get(&self, obj_addr: usize) -> Option<i32> {
        self.table.read().get(&obj_addr).copied()
    }

    /// After a GC cycle, remap the addresses of objects that moved **and drop
    /// the entries of objects that did not survive**.
    ///
    /// * an entry in `pointer_map` — the object moved; re-key it to the new
    ///   address;
    /// * otherwise `survived(addr)` — the object is still live at the same
    ///   address (a stationary survivor, or an object in a generation this
    ///   cycle did not touch); keep it as it is;
    /// * otherwise — dead; drop it.
    ///
    /// # GCAUD-7 — why the survival predicate is a parameter
    ///
    /// This used to be `pointer_map.get(&addr).copied().unwrap_or(addr)`: an
    /// address the map did not mention kept its entry forever, and there was no
    /// sweep anywhere (`remove_dead` below has never had a caller). That is the
    /// classic address-keyed-cache failure — a dead key held at a recycled
    /// address, so the next object allocated there inherits the dead object's
    /// identity hash, and the table grows without bound.
    ///
    /// The obvious repair, "absent from the pointer map ⇒ did not survive", is
    /// the proof the reference processor uses — but it is only sound when the
    /// producer of the map guarantees an entry for **every** survivor, stationary
    /// ones included. The generational collector does that only for addresses
    /// the VM registered as watched (`gc_quiescence::set_watched_referents`;
    /// see the identity-entry arms in `OldGen::compact` and in the in-place old
    /// sweep), and a minor collection's map says nothing at all about old gen.
    /// Applying that proof unconditionally here would silently change a live
    /// object's `identityHashCode` mid-life, which is a different bug of the
    /// same size.
    ///
    /// So the caller must state it. `survived` makes the precondition a
    /// parameter instead of a convention, which is the whole point: this table
    /// has **no production consumer today** (it is exported from `lib.rs` and
    /// referenced only by tests — the generational heap mints identity hashes
    /// through `GenerationalHeap::mint_identity_hash_code` instead), and the
    /// hazard is entirely about what happens when someone wires it up. A
    /// signature that cannot be called without answering "which of these
    /// addresses are still alive?" cannot be wired up wrongly by accident.
    ///
    /// Pass `&|_| false` to sweep every unmoved entry; pass `&|_| true` for the
    /// old remap-only behaviour, and read the paragraph above first.
    pub fn update_after_gc(
        &self,
        pointer_map: &cratonvm_types::PointerMap,
        survived: &dyn Fn(usize) -> bool,
    ) {
        let mut write = self.table.write();
        let old: Vec<(usize, i32)> = write.drain().collect();
        for (addr, hash) in old {
            match pointer_map.get(&addr).copied() {
                Some(new_addr) => {
                    write.insert(new_addr, hash);
                }
                None if survived(addr) => {
                    write.insert(addr, hash);
                }
                // Dead: dropped. Keeping it would hand the next object
                // allocated at this address a stranger's identity hash.
                None => {}
            }
        }
    }

    /// Remove entries for dead objects. `is_live` returns `true` for addresses
    /// that survived the current GC cycle.
    pub fn remove_dead(&self, is_live: &dyn Fn(usize) -> bool) {
        let mut write = self.table.write();
        write.retain(|&addr, _| is_live(addr));
    }

    /// Number of hash codes stored.
    pub fn len(&self) -> usize {
        self.table.read().len()
    }

    /// `true` if no hash codes are stored.
    pub fn is_empty(&self) -> bool {
        self.table.read().is_empty()
    }

    /// Discard all entries.
    pub fn clear(&self) {
        self.table.write().clear();
    }

    /// Estimate the memory saved by *not* storing hash codes in the header.
    ///
    /// If every object kept a 4-byte hash in its header, the cost would be
    /// `total_objects * 4`. With the side table we only pay for objects that
    /// actually requested a hash: `self.len() * (size_of_entry)`. The savings
    /// are approximate.
    pub fn savings_bytes(&self, total_objects: usize) -> usize {
        let hash_size = 4usize; // i32
        let cost_if_inline = total_objects * hash_size;
        // Each HashMap entry: key (usize=8) + value (i32=4) + overhead (~16).
        let entry_overhead = 8 + 4 + 16;
        let actual_cost = self.len() * entry_overhead;
        cost_if_inline.saturating_sub(actual_cost)
    }
}

impl Default for HashCodeTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Legacy interop -- migration helpers
// ---------------------------------------------------------------------------

/// Fields extracted from a [`CompactHeader`] in a flat struct, matching the
/// shape expected by code written against the legacy [`ObjectHeader`].
#[derive(Debug, Clone)]
pub struct LegacyHeaderFields {
    pub narrow_klass: u32,
    pub is_array: bool,
    pub element_type: u8,
    pub gc_age: u8,
    pub lock_state: LockState,
    pub has_hash: bool,
    pub is_forwarded: bool,
    pub forwarding_addr: usize,
}

/// Convert a legacy (16-byte) [`ObjectHeader`](crate::heap::ObjectHeader) into
/// a [`CompactHeader`], migrating the identity hash code into `hash_table`
/// when present.
pub fn migrate_to_compact(
    old: &crate::heap::ObjectHeader,
    narrow_klass: u32,
    hash_table: &HashCodeTable,
    obj_addr: usize,
) -> CompactHeader {
    let is_array = old.kind() == crate::heap::ObjectKind::Array;
    let mut header = if is_array {
        CompactHeader::new_array(narrow_klass, old.element_type() as u8, old.gc_age())
    } else {
        CompactHeader::new_object(narrow_klass, old.gc_age())
    };

    // Migrate identity hash code to side table if non-zero.
    let old_hash = old.installed_identity_hash();
    if old_hash != 0 {
        // Force the same hash value into the table.
        let mut write = hash_table.table.write();
        write.insert(obj_addr, old_hash);
        drop(write);
        header.set_has_hash_code();
    }

    // Migrate GC flags into the lower 4 bits for non-arrays.
    // (For arrays the element_type already occupies those bits.)
    if !is_array {
        // Preserve gc_flags in the lowest nibble.
        let flags = (old.gc_flags() & 0xF) as u64;
        header.0 = (header.0 & !CompactHeader::ELEM_TYPE_MASK) | flags;
    }

    // Migrate forwarding pointer.
    if old.is_forwarded() {
        header.set_forwarding_ptr(old.forwarding_address() as usize);
    }

    header
}

/// Extract legacy-style fields from a compact header.
///
/// When `header.is_forwarded()` is true, the lower 32 bits hold the split
/// forwarding field rather than `is_array` / `element_type` / `gc_age` /
/// `has_hash`; the accessors report `false` / `0` for those fields.
pub fn to_legacy_fields(header: CompactHeader) -> LegacyHeaderFields {
    let forwarded = header.is_forwarded();
    LegacyHeaderFields {
        narrow_klass: header.narrow_klass(),
        is_array: !forwarded && header.is_array(),
        element_type: if forwarded { 0 } else { header.element_type() },
        gc_age: if forwarded { 0 } else { header.gc_age() },
        lock_state: header.lock_state(),
        has_hash: !forwarded && header.has_hash_code(),
        is_forwarded: forwarded,
        forwarding_addr: if forwarded {
            header.forwarding_ptr()
        } else {
            0
        },
    }
}

// ---------------------------------------------------------------------------
// NarrowKlassTable — bidirectional ClassId <-> u32 narrow klass mapping
// ---------------------------------------------------------------------------

/// Bidirectional mapping between [`ClassId`] (the VM's opaque class identifier)
/// and a 32-bit `NarrowKlass` value used inside [`CompactHeader`].
///
/// In a traditional JVM the narrow klass encodes a pointer into the metaspace
/// (compressed by a shift + base). Our synthetic model uses a simpler scheme:
/// each ClassId is assigned a sequential u32 narrow ID starting at 1.
/// ID 0 is reserved for "no class" / uninitialized.
/// T10.9.B: FxHashMap — internal u32 keys.
pub struct NarrowKlassTable {
    /// ClassId → NarrowKlass (sequential u32).
    to_narrow: RwLock<FxHashMap<u32, u32>>,
    /// NarrowKlass → ClassId (reverse lookup).
    to_class_id: RwLock<FxHashMap<u32, u32>>,
    /// Next narrow klass ID to assign.
    next_id: std::sync::atomic::AtomicU32,
}

impl NarrowKlassTable {
    /// Create an empty table. IDs start at 1 (0 is reserved).
    pub fn new() -> Self {
        Self {
            to_narrow: RwLock::new(FxHashMap::default()),
            to_class_id: RwLock::new(FxHashMap::default()),
            next_id: std::sync::atomic::AtomicU32::new(1),
        }
    }

    /// Get or assign a narrow klass ID for the given [`ClassId`].
    ///
    /// Thread-safe: concurrent calls for the same ClassId return the same value.
    pub fn get_or_assign(&self, class_id: cratonvm_types::ClassId) -> u32 {
        let raw = class_id.as_u32();
        // Fast path: already assigned.
        {
            let read = self.to_narrow.read();
            if let Some(&nk) = read.get(&raw) {
                return nk;
            }
        }
        // Slow path: assign a new ID.
        let mut write = self.to_narrow.write();
        if let Some(&nk) = write.get(&raw) {
            return nk;
        }
        let nk = self.next_id.fetch_add(1, Ordering::Relaxed);
        // gci i44/ch: the reverse entry is published BEFORE `to_narrow` is
        // released (lock order `to_narrow` -> `to_class_id`; `resolve` and
        // `remove_classes` never hold both). It used to be inserted after the
        // `drop`, which left two windows: a peer on the fast path above could
        // read `nk` and then `resolve(nk)` to `None` for a class it had just
        // been handed; and a `remove_classes` running between the two inserts
        // removed the forward entry, found no reverse entry to remove, and
        // then this call inserted one -- an orphan `nk -> class` that makes a
        // RETIRED narrow klass resolve to its unloaded class, the exact
        // answer `remove_classes` promises never to give.
        self.to_class_id.write().insert(nk, raw);
        write.insert(raw, nk);
        drop(write);
        nk
    }

    /// Look up the ClassId for a narrow klass value. Returns `None` if the
    /// narrow klass has never been assigned.
    pub fn resolve(&self, narrow_klass: u32) -> Option<cratonvm_types::ClassId> {
        self.to_class_id
            .read()
            .get(&narrow_klass)
            .map(|&raw| cratonvm_types::ClassId::new(raw))
    }

    /// Number of registered class mappings.
    pub fn len(&self) -> usize {
        self.to_narrow.read().len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.to_narrow.read().is_empty()
    }

    /// Check if a ClassId already has a narrow klass assignment.
    pub fn contains(&self, class_id: cratonvm_types::ClassId) -> bool {
        self.to_narrow.read().contains_key(&class_id.as_u32())
    }

    /// Release the narrow-klass assignments of classes that have been unloaded.
    /// Returns how many mappings were actually dropped.
    ///
    /// Both directions are keyed on a raw class id and, before this existed,
    /// had **no removal path of any kind** — no `remove`, `retain`, `prune` or
    /// `clear` anywhere in the impl. That satisfies neither arm of the
    /// "unload invalidation or hard bound" rule in
    /// `docs/architecture/class-loader-unloading.md`: the two
    /// maps grew one entry per class defined, forever, so any workload that
    /// spins loaders (CGLIB, ByteBuddy, Groovy, repeated app redeploys) would
    /// leak them without bound. It was latent only because
    /// [`CompactAllocator`] has no caller outside this file — exactly the
    /// condition under which such a table gets wired up without anyone
    /// rechecking the invariant. See
    /// `refs-metaspace-unloading.md` §3/§R3.
    ///
    /// # `next_id` is deliberately NOT rewound or recycled
    ///
    /// A narrow klass is embedded in the header word of every live object of
    /// that class. Handing a retired id back out would let a header the sweeper
    /// has not reached yet — or a stale copy captured by a concurrent
    /// scan — [`resolve`](Self::resolve) to a *different, live* class, which is
    /// silent heap-type confusion. Leaving `next_id` monotonic means a retired
    /// id resolves to `None` instead, which every reader already handles
    /// (`resolve` returns `Option`). The counter is a `u32` and only advances
    /// on genuinely new classes, so the bound it imposes is ~4.29e9 distinct
    /// class definitions per process — far beyond the map growth this fixes.
    ///
    /// # Locking
    ///
    /// `to_narrow` first, then `to_class_id`, never both held at once here.
    /// [`get_or_assign`](Self::get_or_assign) holds `to_narrow` across BOTH of
    /// its inserts (gci i44/ch; it used to release it between them, which
    /// broke the sentence below). A concurrent `get_or_assign` for a class
    /// being removed therefore either completes wholly before this call's
    /// `to_narrow` pass (and its pair is then removed in both directions) or
    /// wholly after it (and assigns a fresh id) — it can never leave a reverse
    /// entry behind a forward entry this call already removed.
    pub fn remove_classes(&self, class_ids: &[cratonvm_types::ClassId]) -> usize {
        if class_ids.is_empty() {
            return 0;
        }
        let retired: Vec<u32> = {
            let mut to_narrow = self.to_narrow.write();
            class_ids
                .iter()
                .filter_map(|cid| to_narrow.remove(&cid.as_u32()))
                .collect()
        };
        if retired.is_empty() {
            return 0;
        }
        let mut to_class_id = self.to_class_id.write();
        for nk in &retired {
            to_class_id.remove(nk);
        }
        retired.len()
    }
}

impl Default for NarrowKlassTable {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// HeaderView — unified read API for both header formats
// ---------------------------------------------------------------------------

/// A read-only view into an object's header, abstracting over the legacy
/// 16-byte [`ObjectHeader`] and the compact 8-byte [`CompactHeader`].
///
/// This allows VM code to be header-format-agnostic.
#[derive(Debug, Clone)]
pub enum HeaderView {
    /// The legacy 16-byte header (direct reference).
    Legacy {
        class_id: cratonvm_types::ClassId,
        is_array: bool,
        element_type: u8,
        array_length: u32,
        num_slots: u32,
        gc_age: u8,
        gc_flags: u8,
        is_forwarded: bool,
    },
    /// An 8-byte compact header with side-table hash.
    Compact {
        narrow_klass: u32,
        is_array: bool,
        element_type: u8,
        gc_age: u8,
        lock_state: LockState,
        has_hash: bool,
        is_forwarded: bool,
    },
}

impl HeaderView {
    /// Build a HeaderView from a legacy ObjectHeader.
    pub fn from_legacy(h: &crate::heap::ObjectHeader) -> Self {
        HeaderView::Legacy {
            class_id: h.class_id,
            is_array: h.kind() == crate::heap::ObjectKind::Array,
            element_type: h.element_type() as u8,
            array_length: h.array_length(),
            num_slots: h.num_slots(),
            gc_age: h.gc_age(),
            gc_flags: h.gc_flags(),
            is_forwarded: h.is_forwarded(),
        }
    }

    /// Build a HeaderView from a CompactHeader.
    ///
    /// When `h.is_forwarded()` is true, the lower 32 bits hold the split
    /// forwarding field rather than `is_array` / `element_type` / `gc_age` /
    /// `has_hash`; those fields are zeroed.
    pub fn from_compact(h: CompactHeader) -> Self {
        let forwarded = h.is_forwarded();
        HeaderView::Compact {
            narrow_klass: h.narrow_klass(),
            is_array: !forwarded && h.is_array(),
            element_type: if forwarded { 0 } else { h.element_type() },
            gc_age: if forwarded { 0 } else { h.gc_age() },
            lock_state: h.lock_state(),
            has_hash: !forwarded && h.has_hash_code(),
            is_forwarded: forwarded,
        }
    }

    /// Is this an array object?
    pub fn is_array(&self) -> bool {
        match self {
            HeaderView::Legacy { is_array, .. } => *is_array,
            HeaderView::Compact { is_array, .. } => *is_array,
        }
    }

    /// GC survivor age.
    pub fn gc_age(&self) -> u8 {
        match self {
            HeaderView::Legacy { gc_age, .. } => *gc_age,
            HeaderView::Compact { gc_age, .. } => *gc_age,
        }
    }

    /// Element type (meaningful only for arrays).
    pub fn element_type(&self) -> u8 {
        match self {
            HeaderView::Legacy { element_type, .. } => *element_type,
            HeaderView::Compact { element_type, .. } => *element_type,
        }
    }

    /// Is the object forwarded by GC?
    pub fn is_forwarded(&self) -> bool {
        match self {
            HeaderView::Legacy { is_forwarded, .. } => *is_forwarded,
            HeaderView::Compact { is_forwarded, .. } => *is_forwarded,
        }
    }

    /// Header size in bytes.
    pub fn header_size(&self) -> usize {
        match self {
            HeaderView::Legacy { .. } => crate::heap::HEADER_SIZE,
            HeaderView::Compact { .. } => CompactHeader::SIZE,
        }
    }
}

// ---------------------------------------------------------------------------
// CompactAllocator — bump-pointer allocator using 8-byte compact headers
// ---------------------------------------------------------------------------

/// A simple bump-pointer allocator that creates objects with 8-byte compact
/// headers instead of 16-byte legacy headers.
///
/// This demonstrates the compact header allocation path. Nothing in the VM
/// uses it: `use_compact_headers` (`-XX:+UseCompactObjectHeaders`) does not
/// select it, and no backend allocates through it -- see the module's
/// "NOT a reachable header mode" status.
pub struct CompactAllocator {
    /// Backing storage, 8-byte aligned (`u64` words) and wrapped in
    /// `UnsafeCell` so bump-pointer allocations via `&self` can write into
    /// disjoint sub-slices without violating Rust's shared-reference rules or
    /// `u64`/`u32` alignment requirements.
    storage: Vec<std::cell::UnsafeCell<u64>>,
    /// Requested capacity in bytes.
    capacity_bytes: usize,
    /// Bump pointer (offset in bytes into `storage`).
    offset: std::sync::atomic::AtomicUsize,
    /// Narrow klass table for ClassId compression.
    klass_table: NarrowKlassTable,
    /// Identity hash code side table.
    hash_table: HashCodeTable,
    /// Total number of objects allocated.
    object_count: std::sync::atomic::AtomicUsize,
    /// Total bytes saved vs. legacy headers -- `LEGACY_MINUS_COMPACT_HEADER`
    /// each. The legacy header shrank on 2026-08-06/07 (it is `HEADER_SIZE`,
    /// 16 bytes, now), so the figure is derived rather than written down here.
    bytes_saved: std::sync::atomic::AtomicUsize,
}

// SAFETY: `CompactAllocator` hands out disjoint 8-byte-aligned sub-slices of
// `storage` via atomic CAS on `offset`; each sub-slice is written only once by
// the allocating call before its pointer is returned, and subsequent reads use
// atomic loads on the header/length words after bounds-checking against the
// committed `offset`.
unsafe impl Sync for CompactAllocator {}

/// Size of each value slot (matches the legacy slot size for compatibility).
const COMPACT_SLOT_SIZE: usize = 16;

/// Header bytes a compact object saves over a legacy one. Was a bare `24`
/// twice below until the 2026-08-06 header shrink took the legacy header from
/// 32 to 24 and made it 16 — a literal that feeds `savings_report()` and four
/// assertions, so it drifts silently. Derived now.
const LEGACY_MINUS_COMPACT_HEADER: usize = cratonvm_types::HEADER_SIZE - CompactHeader::SIZE;

impl CompactAllocator {
    /// Create a new compact allocator with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        let words = capacity.div_ceil(8);
        let mut storage = Vec::with_capacity(words);
        storage.resize_with(words, || std::cell::UnsafeCell::new(0u64));
        Self {
            storage,
            capacity_bytes: capacity,
            offset: std::sync::atomic::AtomicUsize::new(0),
            klass_table: NarrowKlassTable::new(),
            hash_table: HashCodeTable::new(),
            object_count: std::sync::atomic::AtomicUsize::new(0),
            bytes_saved: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Raw base pointer of the backing storage as `*mut u8`.
    #[inline]
    fn base_ptr(&self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }

    /// Atomically bump `offset` by `aligned` bytes within `capacity_bytes`.
    /// Uses a CAS loop so a failed OOM attempt never transiently advances
    /// `offset` past `capacity_bytes` (which previously let two racing callers
    /// roll back and receive overlapping bytes).
    #[inline]
    fn bump_alloc(&self, aligned: usize) -> Option<usize> {
        let mut cur = self.offset.load(Ordering::Relaxed);
        loop {
            let end = cur.checked_add(aligned)?;
            if end > self.capacity_bytes {
                return None;
            }
            match self.offset.compare_exchange_weak(
                cur,
                end,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(cur),
                Err(actual) => cur = actual,
            }
        }
    }

    /// Check that `[ptr, ptr + need)` lies at an 8-byte-aligned offset within
    /// the already-allocated prefix of `storage`.
    #[inline]
    fn check_allocated_range(&self, ptr: *const u8, need: usize) -> bool {
        if ptr.is_null() || (ptr as usize) & 0x7 != 0 {
            return false;
        }
        let base = self.base_ptr() as usize;
        let addr = ptr as usize;
        if addr < base {
            return false;
        }
        let rel = addr - base;
        let used = self.offset.load(Ordering::Acquire).min(self.capacity_bytes);
        rel.checked_add(need).is_some_and(|end| end <= used)
    }

    /// Allocate an object with compact header + field slots.
    ///
    /// Returns `Some(ptr)` on success, `None` on OOM or arithmetic overflow.
    /// The returned pointer points to the start of the compact header (8 bytes)
    /// followed by `num_fields * COMPACT_SLOT_SIZE` bytes of field data.
    pub fn alloc_object(
        &self,
        class_id: cratonvm_types::ClassId,
        num_fields: usize,
    ) -> Option<*mut u8> {
        let field_bytes = num_fields.checked_mul(COMPACT_SLOT_SIZE)?;
        let total_size = CompactHeader::SIZE.checked_add(field_bytes)?;
        let aligned = total_size.checked_add(7)? & !7; // 8-byte align

        let start = self.bump_alloc(aligned)?;
        let nk = self.klass_table.get_or_assign(class_id);

        // SAFETY: `[start, start + aligned)` is exclusively claimed in
        // `storage` (which has at least `capacity_bytes.div_ceil(8) * 8` bytes
        // of `UnsafeCell<u64>` backing) and `start` is 8-byte aligned.
        let ptr = unsafe { self.base_ptr().add(start) };
        let header = CompactHeader::new_object(nk, 0);
        unsafe {
            (*(ptr as *const std::sync::atomic::AtomicU64))
                .store(header.raw(), Ordering::Release);
            std::ptr::write_bytes(ptr.add(CompactHeader::SIZE), 0, field_bytes);
        }

        self.object_count.fetch_add(1, Ordering::Relaxed);
        // Savings: HEADER_SIZE (16) - CompactHeader::SIZE (8) = 8 bytes per object
        self.bytes_saved
            .fetch_add(LEGACY_MINUS_COMPACT_HEADER, Ordering::Relaxed);

        Some(ptr)
    }

    /// Allocate an array with compact header + element data.
    ///
    /// Returns `Some(ptr)` on success, `None` on OOM or arithmetic/`u32`
    /// length overflow.
    pub fn alloc_array(
        &self,
        class_id: cratonvm_types::ClassId,
        element_type: crate::heap::ArrayElementType,
        length: usize,
    ) -> Option<*mut u8> {
        let len_u32 = u32::try_from(length).ok()?;
        let elem_size = element_byte_size(element_type);
        let data_size = length.checked_mul(elem_size)?;
        let data_aligned = data_size.checked_add(7)? & !7;
        // For arrays, we store the length as a u32 immediately after the 8-byte header
        // Layout: [CompactHeader:8][array_length:4][padding:4][data...]
        let total_size = CompactHeader::SIZE
            .checked_add(8)?
            .checked_add(data_aligned)?;

        let start = self.bump_alloc(total_size)?;
        let nk = self.klass_table.get_or_assign(class_id);

        // SAFETY: `[start, start + total_size)` is exclusively claimed in
        // `storage` and 8-byte aligned.
        let ptr = unsafe { self.base_ptr().add(start) };
        let header = CompactHeader::new_array(nk, element_type as u8, 0);
        unsafe {
            (*(ptr as *const std::sync::atomic::AtomicU64))
                .store(header.raw(), Ordering::Release);
            std::ptr::write(ptr.add(8) as *mut u32, len_u32);
            std::ptr::write(ptr.add(12) as *mut u32, 0);
            std::ptr::write_bytes(ptr.add(CompactHeader::SIZE + 8), 0, data_aligned);
        }

        self.object_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_saved
            .fetch_add(LEGACY_MINUS_COMPACT_HEADER, Ordering::Relaxed);

        Some(ptr)
    }

    /// Read the compact header at the given pointer.
    ///
    /// # Panics
    /// Panics if `ptr` is null, not 8-byte aligned, or outside the allocated
    /// region of this allocator's storage.
    pub fn read_header(&self, ptr: *const u8) -> CompactHeader {
        assert!(
            self.check_allocated_range(ptr, CompactHeader::SIZE),
            "CompactAllocator::read_header: pointer {:p} is outside allocated storage",
            ptr,
        );
        // SAFETY: `check_allocated_range` verified that `ptr` is non-null,
        // 8-byte aligned, and within the allocated prefix of `self.storage`.
        let raw = unsafe {
            (*(ptr as *const std::sync::atomic::AtomicU64)).load(Ordering::Acquire)
        };
        CompactHeader::from_raw(raw)
    }

    /// Get the array length stored after the compact header.
    ///
    /// # Panics
    /// Panics if `ptr` is null, not 8-byte aligned, or outside the allocated
    /// region of this allocator's storage.
    pub fn read_array_length(&self, ptr: *const u8) -> u32 {
        assert!(
            self.check_allocated_range(ptr, CompactHeader::SIZE + 4),
            "CompactAllocator::read_array_length: pointer {:p} is outside allocated storage",
            ptr,
        );
        // SAFETY: `check_allocated_range` verified that `ptr + 8..ptr + 12`
        // is 4-byte aligned and within the allocated prefix of `self.storage`.
        unsafe { (*(ptr.add(8) as *const std::sync::atomic::AtomicU32)).load(Ordering::Acquire) }
    }

    /// Resolve the ClassId from a compact header at the given pointer.
    pub fn class_id_at(&self, ptr: *const u8) -> Option<cratonvm_types::ClassId> {
        if !self.check_allocated_range(ptr, CompactHeader::SIZE) {
            return None;
        }
        let header = self.read_header(ptr);
        self.klass_table.resolve(header.narrow_klass())
    }

    /// Get or assign the identity hash code for the object at `obj_addr`.
    pub fn identity_hash_code(&self, obj_addr: usize) -> i32 {
        self.hash_table.get_or_assign(obj_addr)
    }

    /// Get a HeaderView for the object at the given pointer.
    pub fn header_view(&self, ptr: *const u8) -> HeaderView {
        HeaderView::from_compact(self.read_header(ptr))
    }

    /// Access the narrow klass table.
    pub fn klass_table(&self) -> &NarrowKlassTable {
        &self.klass_table
    }

    /// Access the hash code side table.
    pub fn hash_table(&self) -> &HashCodeTable {
        &self.hash_table
    }

    /// Total number of objects allocated.
    pub fn object_count(&self) -> usize {
        self.object_count.load(Ordering::Relaxed)
    }

    /// Total bytes saved vs. legacy headers -- `LEGACY_MINUS_COMPACT_HEADER`
    /// each. The legacy header shrank on 2026-08-06/07 (it is `HEADER_SIZE`,
    /// 16 bytes, now), so the figure is derived rather than written down here.
    pub fn bytes_saved(&self) -> usize {
        self.bytes_saved.load(Ordering::Relaxed)
    }

    /// Current allocation offset (bytes used).
    pub fn used_bytes(&self) -> usize {
        self.offset.load(Ordering::Relaxed)
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity_bytes
    }

    /// Generate a memory savings report.
    pub fn savings_report(&self) -> CompactHeaderSavingsReport {
        let obj_count = self.object_count();
        let bytes_saved = self.bytes_saved();
        let hash_entries = self.hash_table.len();
        let hash_savings = self.hash_table.savings_bytes(obj_count);
        CompactHeaderSavingsReport {
            object_count: obj_count,
            header_bytes_saved: bytes_saved,
            hash_table_entries: hash_entries,
            hash_table_savings: hash_savings,
            total_savings: bytes_saved + hash_savings,
            klass_table_entries: self.klass_table.len(),
        }
    }
}

/// Report of memory savings from compact headers.
#[derive(Debug, Clone)]
pub struct CompactHeaderSavingsReport {
    /// Number of objects allocated with compact headers.
    pub object_count: usize,
    /// Bytes saved from 16→8 byte header reduction (`LEGACY_MINUS_COMPACT_HEADER`).
    pub header_bytes_saved: usize,
    /// Number of identity hash codes in the side table.
    pub hash_table_entries: usize,
    /// Bytes saved by lazy hash code allocation.
    pub hash_table_savings: usize,
    /// Total bytes saved (header + hash).
    pub total_savings: usize,
    /// Number of class entries in the narrow klass table.
    pub klass_table_entries: usize,
}

impl CompactHeaderSavingsReport {
    /// Format the report as a human-readable string.
    pub fn format(&self) -> String {
        format!(
            "Compact Object Headers Report:\n  \
             Objects:           {}\n  \
             Header savings:    {} bytes ({} bytes/obj)\n  \
             Hash table:        {} entries ({} bytes saved)\n  \
             Klass table:       {} entries\n  \
             Total savings:     {} bytes",
            self.object_count,
            self.header_bytes_saved,
            if self.object_count > 0 {
                self.header_bytes_saved / self.object_count
            } else {
                0
            },
            self.hash_table_entries,
            self.hash_table_savings,
            self.klass_table_entries,
            self.total_savings,
        )
    }
}

/// Helper: element byte size for a given array element type.
fn element_byte_size(et: crate::heap::ArrayElementType) -> usize {
    match et {
        crate::heap::ArrayElementType::Boolean | crate::heap::ArrayElementType::Byte => 1,
        crate::heap::ArrayElementType::Char | crate::heap::ArrayElementType::Short => 2,
        crate::heap::ArrayElementType::Int | crate::heap::ArrayElementType::Float => 4,
        crate::heap::ArrayElementType::Long
        | crate::heap::ArrayElementType::Double
        | crate::heap::ArrayElementType::Reference => 8,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
