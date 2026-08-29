// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compact 64-bit object headers per JEP 519 / Project Lilliput.
//!
//! Reduces the per-object header from 32 bytes ([`ObjectHeader`]) to 8 bytes,
//! saving 24 bytes per live object. Identity hash codes are stored in a
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
    fn new() -> Self {
        Self {
            table: RwLock::new(FxHashMap::default()),
            next_token: AtomicU64::new(1),
        }
    }

    /// Intern a full address, returning a fresh token in `1..=MAX_TOKEN`.
    ///
    /// Panics if the 29-bit token space is exhausted, which would require more
    /// than half a billion *simultaneously live* overflow forwardings — far
    /// beyond any real GC pause. Panicking is strictly better than wrapping a
    /// token and aliasing two unrelated objects to the same heap address.
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
    fn len(&self) -> usize {
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
    /// GC forwarding pointer installed; bits 31-2 hold the target address >> 3.
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

    /// Bits 31-25: GC survivor age (0..=127).
    #[inline]
    pub fn gc_age(&self) -> u8 {
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

    /// Bit 22: whether this object has a hash code in the side table.
    #[inline]
    pub fn has_hash_code(&self) -> bool {
        self.0 & Self::HASH_BIT != 0
    }

    /// Set the identity-hash-code-present bit.
    #[inline]
    pub fn set_has_hash_code(&mut self) {
        self.0 |= Self::HASH_BIT;
    }

    // -- Array flag ---------------------------------------------------------

    /// Bit 21: 1 if this object is an array.
    #[inline]
    pub fn is_array(&self) -> bool {
        self.0 & Self::ARRAY_BIT != 0
    }

    /// Element type for arrays (lower 4 bits). Meaningless for non-arrays.
    #[inline]
    pub fn element_type(&self) -> u8 {
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
/// shape expected by code written against the old 32-byte header.
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

/// Convert a legacy 32-byte [`ObjectHeader`](crate::heap::ObjectHeader) into
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
    let old_hash = crate::heap::ObjectHeader::neutral_hash(
        old.mark_word.load(std::sync::atomic::Ordering::Relaxed),
    );
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
pub fn to_legacy_fields(header: CompactHeader) -> LegacyHeaderFields {
    LegacyHeaderFields {
        narrow_klass: header.narrow_klass(),
        is_array: header.is_array(),
        element_type: header.element_type(),
        gc_age: header.gc_age(),
        lock_state: header.lock_state(),
        has_hash: header.has_hash_code(),
        is_forwarded: header.is_forwarded(),
        forwarding_addr: if header.is_forwarded() {
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
        write.insert(raw, nk);
        drop(write);
        self.to_class_id.write().insert(nk, raw);
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
    /// `arch-2026-07-26/refs-metaspace-unloading.md` §3/§R3.
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
    /// Mirrors [`get_or_assign`](Self::get_or_assign): `to_narrow` first, then
    /// `to_class_id`, never both held at once. A concurrent `get_or_assign` for
    /// a class being removed therefore either completes wholly before this call
    /// (and is then removed) or wholly after (and re-assigns a fresh id) — it
    /// can never observe a half-removed pair in the direction it reads.
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
/// 32-byte [`ObjectHeader`] and the compact 8-byte [`CompactHeader`].
///
/// This allows VM code to be header-format-agnostic.
#[derive(Debug, Clone)]
pub enum HeaderView {
    /// The legacy 32-byte header (direct reference).
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
    pub fn from_compact(h: CompactHeader) -> Self {
        HeaderView::Compact {
            narrow_klass: h.narrow_klass(),
            is_array: h.is_array(),
            element_type: h.element_type(),
            gc_age: h.gc_age(),
            lock_state: h.lock_state(),
            has_hash: h.has_hash_code(),
            is_forwarded: h.is_forwarded(),
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
/// headers instead of 32-byte legacy headers.
///
/// This demonstrates the compact header allocation path. The VM can use this
/// alongside the existing `GenerationalHeap` when `use_compact_headers` is
/// enabled.
pub struct CompactAllocator {
    /// Backing storage.
    storage: Vec<u8>,
    /// Bump pointer (offset into storage).
    offset: std::sync::atomic::AtomicUsize,
    /// Narrow klass table for ClassId compression.
    klass_table: NarrowKlassTable,
    /// Identity hash code side table.
    hash_table: HashCodeTable,
    /// Total number of objects allocated.
    object_count: std::sync::atomic::AtomicUsize,
    /// Total bytes saved vs. legacy headers -- `LEGACY_MINUS_COMPACT_HEADER`
    /// each. The legacy header was 32 bytes until the 2026-08-06 shrink and is
    /// 24 now, so the figure is derived rather than written down here.
    bytes_saved: std::sync::atomic::AtomicUsize,
}

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
        Self {
            storage: vec![0u8; capacity],
            offset: std::sync::atomic::AtomicUsize::new(0),
            klass_table: NarrowKlassTable::new(),
            hash_table: HashCodeTable::new(),
            object_count: std::sync::atomic::AtomicUsize::new(0),
            bytes_saved: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Allocate an object with compact header + field slots.
    ///
    /// Returns `Some(ptr)` on success, `None` on OOM.
    /// The returned pointer points to the start of the compact header (8 bytes)
    /// followed by `num_fields * COMPACT_SLOT_SIZE` bytes of field data.
    pub fn alloc_object(
        &self,
        class_id: cratonvm_types::ClassId,
        num_fields: usize,
    ) -> Option<*mut u8> {
        let nk = self.klass_table.get_or_assign(class_id);
        let total_size = CompactHeader::SIZE + num_fields * COMPACT_SLOT_SIZE;
        let aligned = (total_size + 7) & !7; // 8-byte align

        let start = self.offset.fetch_add(aligned, Ordering::Relaxed);
        if start + aligned > self.storage.len() {
            // OOM — roll back (best-effort; doesn't handle races perfectly)
            self.offset.fetch_sub(aligned, Ordering::Relaxed);
            return None;
        }

        let ptr = unsafe { (self.storage.as_ptr() as *mut u8).add(start) };

        // Write the compact header
        let header = CompactHeader::new_object(nk, 0);
        unsafe {
            std::ptr::write(ptr as *mut u64, header.raw());
            // Zero field data
            std::ptr::write_bytes(
                ptr.add(CompactHeader::SIZE),
                0,
                num_fields * COMPACT_SLOT_SIZE,
            );
        }

        self.object_count.fetch_add(1, Ordering::Relaxed);
        // Savings: 32 - 8 = 24 bytes per object
        self.bytes_saved
            .fetch_add(LEGACY_MINUS_COMPACT_HEADER, Ordering::Relaxed);

        Some(ptr)
    }

    /// Allocate an array with compact header + element data.
    ///
    /// Returns `Some(ptr)` on success, `None` on OOM.
    pub fn alloc_array(
        &self,
        class_id: cratonvm_types::ClassId,
        element_type: crate::heap::ArrayElementType,
        length: usize,
    ) -> Option<*mut u8> {
        let nk = self.klass_table.get_or_assign(class_id);
        let elem_size = element_byte_size(element_type);
        let data_size = length.checked_mul(elem_size)?;
        let data_aligned = (data_size + 7) & !7;
        // For arrays, we store the length as a u32 immediately after the 8-byte header
        // Layout: [CompactHeader:8][array_length:4][padding:4][data...]
        let total_size = CompactHeader::SIZE + 8 + data_aligned; // 8 for length+padding

        let start = self.offset.fetch_add(total_size, Ordering::Relaxed);
        if start + total_size > self.storage.len() {
            self.offset.fetch_sub(total_size, Ordering::Relaxed);
            return None;
        }

        let ptr = unsafe { (self.storage.as_ptr() as *mut u8).add(start) };
        let header = CompactHeader::new_array(nk, element_type as u8, 0);
        unsafe {
            std::ptr::write(ptr as *mut u64, header.raw());
            // Write array length at offset 8
            std::ptr::write((ptr.add(8)) as *mut u32, length as u32);
            // Zero the data region
            std::ptr::write_bytes(ptr.add(CompactHeader::SIZE + 8), 0, data_aligned);
        }

        self.object_count.fetch_add(1, Ordering::Relaxed);
        self.bytes_saved
            .fetch_add(LEGACY_MINUS_COMPACT_HEADER, Ordering::Relaxed);

        Some(ptr)
    }

    /// Read the compact header at the given pointer.
    pub fn read_header(&self, ptr: *const u8) -> CompactHeader {
        unsafe { CompactHeader::from_raw(std::ptr::read(ptr as *const u64)) }
    }

    /// Get the array length stored after the compact header.
    pub fn read_array_length(&self, ptr: *const u8) -> u32 {
        unsafe { std::ptr::read(ptr.add(8) as *const u32) }
    }

    /// Resolve the ClassId from a compact header at the given pointer.
    pub fn class_id_at(&self, ptr: *const u8) -> Option<cratonvm_types::ClassId> {
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
    /// each. The legacy header was 32 bytes until the 2026-08-06 shrink and is
    /// 24 now, so the figure is derived rather than written down here.
    pub fn bytes_saved(&self) -> usize {
        self.bytes_saved.load(Ordering::Relaxed)
    }

    /// Current allocation offset (bytes used).
    pub fn used_bytes(&self) -> usize {
        self.offset.load(Ordering::Relaxed)
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.storage.len()
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
    /// Bytes saved from 32→8 byte header reduction.
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
mod tests {
    use super::*;

    /// Serialises every test that **touches** the global forwarding overflow
    /// side table — interning a high address counts, not just asserting the
    /// table's length.
    ///
    /// # The premise this used to carry was wrong
    ///
    /// It read: "Per-header invariants (exact address round-trip, tag bit) are
    /// deterministic regardless and don't need this." A round-trip through the
    /// *inline* encoding is indeed per-header. A round-trip through the
    /// **overflow** encoding is not: `set_forwarding_ptr` stores only a token in
    /// the header and parks the real address in this process-wide table, so
    /// `forwarding_ptr()` is a lookup in shared mutable state. Any test that
    /// interns is therefore exposed to `forwarding_overflow_table_clears`, whose
    /// `clear_forwarding_overflow_table()` wipes the whole table — every other
    /// test's tokens with it. `resolve` answers `0` for a missing token, so the
    /// victim fails with `0 != <its address>`.
    ///
    /// That is not theoretical. Before this guard was widened,
    /// `cargo test -p cratonvm-gc --lib -- forwarding` failed **15 runs out of
    /// 40**, spread across four different victims
    /// (`forwarding_ptr_distinct_high_targets`, `forwarding_ptr_above_8gb_exact`,
    /// `forwarding_ptr_mixed_sweep`,
    /// `forwarding_ptr_overflow_survives_raw_roundtrip`) — whichever happened to
    /// be between its intern and its read when the clear landed. In the full
    /// suite it diluted to roughly one run in four, which is exactly the rate at
    /// which a flake gets re-run rather than diagnosed.
    ///
    /// The clearing test analysed the race in one direction only and concluded
    /// its own assertions were safe. They are — concurrent interning only *adds*
    /// entries, and its token is never reissued. What it did not consider is the
    /// other direction: what its clear does to everyone else.
    ///
    /// # How to apply
    ///
    /// Hold this for the whole test if the test interns **any** address above
    /// [`CompactHeader::FORWARD_INLINE_MAX_ADDR`], including a sweep where only
    /// some entries are high. Tests that stay entirely inline do not need it.
    /// `--test-threads=1` is not the fix: it would hide this class of bug across
    /// the other 980 tests in the crate.
    fn overflow_table_guard() -> std::sync::MutexGuard<'static, ()> {
        static OVERFLOW_TABLE_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        OVERFLOW_TABLE_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // -- CompactHeader construction -----------------------------------------

    #[test]
    fn new_object_basic() {
        let h = CompactHeader::new_object(42, 0);
        assert_eq!(h.narrow_klass(), 42);
        assert_eq!(h.gc_age(), 0);
        assert!(!h.is_array());
        assert_eq!(h.lock_state(), LockState::Unlocked);
        assert!(!h.has_hash_code());
    }

    #[test]
    fn new_object_with_age() {
        let h = CompactHeader::new_object(100, 15);
        assert_eq!(h.narrow_klass(), 100);
        assert_eq!(h.gc_age(), 15);
    }

    #[test]
    fn new_object_max_klass() {
        let h = CompactHeader::new_object(u32::MAX, 0);
        assert_eq!(h.narrow_klass(), u32::MAX);
    }

    #[test]
    fn new_array_with_element_type() {
        let h = CompactHeader::new_array(7, 5, 3);
        assert!(h.is_array());
        assert_eq!(h.narrow_klass(), 7);
        assert_eq!(h.element_type(), 5);
        assert_eq!(h.gc_age(), 3);
    }

    // -- NarrowKlass roundtrip ----------------------------------------------

    #[test]
    fn narrow_klass_roundtrip() {
        for klass in [0, 1, 255, 65535, 0xDEAD_BEEF, u32::MAX] {
            let h = CompactHeader::new_object(klass, 0);
            assert_eq!(h.narrow_klass(), klass, "klass={klass:#x}");
        }
    }

    #[test]
    fn set_narrow_klass_preserves_lower_bits() {
        let mut h = CompactHeader::new_array(1, 9, 7);
        h.set_narrow_klass(0xABCD_1234);
        assert_eq!(h.narrow_klass(), 0xABCD_1234);
        // Lower bits unchanged.
        assert_eq!(h.gc_age(), 7);
        assert!(h.is_array());
        assert_eq!(h.element_type(), 9);
    }

    // -- GC age -------------------------------------------------------------

    #[test]
    fn gc_age_zero() {
        let h = CompactHeader::new_object(1, 0);
        assert_eq!(h.gc_age(), 0);
    }

    #[test]
    fn gc_age_fifteen() {
        let h = CompactHeader::new_object(1, 15);
        assert_eq!(h.gc_age(), 15);
    }

    #[test]
    fn gc_age_max_127() {
        let h = CompactHeader::new_object(1, 127);
        assert_eq!(h.gc_age(), 127);
    }

    #[test]
    fn gc_age_clamped_above_max() {
        let h = CompactHeader::new_object(1, 200);
        assert_eq!(h.gc_age(), CompactHeader::MAX_GC_AGE);
    }

    #[test]
    fn increment_age_caps_at_max() {
        let mut h = CompactHeader::new_object(1, 126);
        assert_eq!(h.increment_age(), 127);
        assert_eq!(h.increment_age(), 127); // stays capped
        assert_eq!(h.gc_age(), 127);
    }

    // -- Lock state ---------------------------------------------------------

    #[test]
    fn lock_state_transitions() {
        let mut h = CompactHeader::new_object(1, 0);
        assert_eq!(h.lock_state(), LockState::Unlocked);

        h.set_lock_state(LockState::ThinLocked);
        assert_eq!(h.lock_state(), LockState::ThinLocked);

        h.set_lock_state(LockState::Inflated);
        assert_eq!(h.lock_state(), LockState::Inflated);

        h.set_lock_state(LockState::Forwarded);
        assert_eq!(h.lock_state(), LockState::Forwarded);

        h.set_lock_state(LockState::Unlocked);
        assert_eq!(h.lock_state(), LockState::Unlocked);
    }

    // -- Hash code flag -----------------------------------------------------

    #[test]
    fn has_hash_code_flag() {
        let mut h = CompactHeader::new_object(1, 0);
        assert!(!h.has_hash_code());
        h.set_has_hash_code();
        assert!(h.has_hash_code());
        // Other fields untouched.
        assert_eq!(h.narrow_klass(), 1);
        assert_eq!(h.gc_age(), 0);
    }

    // -- Array flag ---------------------------------------------------------

    #[test]
    fn is_array_flag() {
        let obj = CompactHeader::new_object(1, 0);
        assert!(!obj.is_array());
        let arr = CompactHeader::new_array(1, 0, 0);
        assert!(arr.is_array());
    }

    // -- Element type -------------------------------------------------------

    #[test]
    fn element_type_values() {
        for et in 0u8..=15 {
            let h = CompactHeader::new_array(1, et, 0);
            assert_eq!(h.element_type(), et, "element_type={et}");
        }
    }

    // -- Forwarding pointer -------------------------------------------------

    #[test]
    fn forwarding_ptr_roundtrip() {
        let addrs: Vec<usize> = vec![0, 8, 64, 0x1000, 0x0FFF_FFF8];
        for &addr in &addrs {
            let mut h = CompactHeader::new_object(42, 5);
            h.set_forwarding_ptr(addr);
            assert!(h.is_forwarded(), "addr={addr:#x}");
            assert_eq!(h.forwarding_ptr(), addr, "addr={addr:#x}");
            // Klass preserved.
            assert_eq!(h.narrow_klass(), 42);
        }
    }

    #[test]
    fn forwarding_clears_other_lower_fields() {
        let mut h = CompactHeader::new_array(10, 7, 12);
        h.set_has_hash_code();
        // After installing forwarding pointer the lower 32 bits are overwritten.
        h.set_forwarding_ptr(0x8000);
        assert!(h.is_forwarded());
        assert_eq!(h.lock_state(), LockState::Forwarded);
        // Klass survives.
        assert_eq!(h.narrow_klass(), 10);
    }

    #[test]
    fn is_forwarded_check() {
        let mut h = CompactHeader::new_object(1, 0);
        assert!(!h.is_forwarded());
        h.set_lock_state(LockState::Forwarded);
        assert!(h.is_forwarded());
    }

    // -- Forwarding pointer: high addresses (>4 GB / >8 GB) -----------------

    /// Regression: a forwarding target above 8 GB must round-trip exactly.
    ///
    /// The old encoder masked `addr >> 3` to 30 bits and silently dropped every
    /// bit above ~8 GB, returning a *truncated* (wrong) address — a moving GC
    /// would then read/write the wrong memory and corrupt the heap. The fix
    /// routes such targets through the overflow side table.
    #[test]
    fn forwarding_ptr_above_8gb_exact() {
        // Interns into the shared overflow table — see `overflow_table_guard`.
        let _guard = overflow_table_guard();
        // 16 GB, 8-byte aligned. addr >> 3 needs 31 bits, so the 30-bit inline
        // field cannot hold it.
        let addr: usize = 16usize * 1024 * 1024 * 1024;
        assert!(addr & 0x7 == 0);
        assert!(addr > CompactHeader::FORWARD_INLINE_MAX_ADDR);

        let mut h = CompactHeader::new_object(0xABCD, 7);
        h.set_forwarding_ptr(addr);
        assert!(h.is_forwarded());
        assert_eq!(h.forwarding_ptr(), addr, "must not truncate >8 GB target");
        // Klass survives the overflow encoding.
        assert_eq!(h.narrow_klass(), 0xABCD);

        // Demonstrate that the OLD inline-only encoding *would* have truncated:
        let truncated = ((addr >> 3) as u64 & 0x3FFF_FFFF) << 3;
        assert_ne!(truncated as usize, addr, "old encoding loses high bits");
    }

    /// The overflow path survives a `raw()` / `from_raw()` round-trip, because
    /// the token lives inside the 64-bit header value itself.
    #[test]
    fn forwarding_ptr_overflow_survives_raw_roundtrip() {
        // Interns into the shared overflow table — see `overflow_table_guard`.
        let _guard = overflow_table_guard();
        let addr: usize = 0x7_0000_0008; // 28 GB + 8, aligned
        let mut h = CompactHeader::new_object(3, 0);
        h.set_forwarding_ptr(addr);
        let copy = CompactHeader::from_raw(h.raw());
        assert!(copy.is_forwarded());
        assert_eq!(copy.forwarding_ptr(), addr);
    }

    /// Two distinct high targets must not alias to the same address.
    #[test]
    fn forwarding_ptr_distinct_high_targets() {
        // Interns twice into the shared overflow table — see
        // `overflow_table_guard`. This was the most frequent victim of the clear
        // race, holding two live tokens across four reads.
        let _guard = overflow_table_guard();
        let a: usize = 0x10_0000_0000; // 64 GB
        let b: usize = 0x10_0000_0008; // 64 GB + 8
        let mut ha = CompactHeader::new_object(1, 0);
        let mut hb = CompactHeader::new_object(2, 0);
        ha.set_forwarding_ptr(a);
        hb.set_forwarding_ptr(b);
        assert_eq!(ha.forwarding_ptr(), a);
        assert_eq!(hb.forwarding_ptr(), b);
        assert_ne!(ha.forwarding_ptr(), hb.forwarding_ptr());
    }

    /// Boundary: the largest inline-representable address stays on the fast
    /// path (no overflow tag, no side-table entry) and round-trips exactly;
    /// the next aligned address up tips into the overflow path.
    #[test]
    fn forwarding_ptr_inline_boundary() {
        // The `just_over` half interns into the shared overflow table, so this
        // test needs the guard even though its first half is pure inline —
        // "mostly inline" is not "inline". See `overflow_table_guard`.
        let _guard = overflow_table_guard();
        let max_inline = CompactHeader::FORWARD_INLINE_MAX_ADDR;
        assert_eq!(max_inline & 0x7, 0, "inline max must be 8-byte aligned");

        let mut h = CompactHeader::new_object(9, 1);
        h.set_forwarding_ptr(max_inline);
        assert_eq!(h.forwarding_ptr(), max_inline);
        // Fast path: the overflow tag is clear -> encoded inline, not in the table.
        assert_eq!(
            h.read_forward_field() & CompactHeader::FORWARD_OVERFLOW_TAG,
            0
        );

        // One object slot (8 bytes) higher cannot be encoded inline.
        let just_over = max_inline + 8;
        let mut h2 = CompactHeader::new_object(9, 1);
        h2.set_forwarding_ptr(just_over);
        assert_eq!(h2.forwarding_ptr(), just_over);
        assert_ne!(
            h2.read_forward_field() & CompactHeader::FORWARD_OVERFLOW_TAG,
            0
        );
    }

    /// Sweep of representative addresses straddling the inline/overflow split,
    /// including 0, small, near-boundary, and several multi-GB targets.
    #[test]
    fn forwarding_ptr_mixed_sweep() {
        // Four of the eight addresses below are above the inline ceiling and
        // intern into the shared overflow table — see `overflow_table_guard`.
        let _guard = overflow_table_guard();
        let addrs: Vec<usize> = vec![
            0,
            8,
            0x1000,
            0x0FFF_FFF8,
            CompactHeader::FORWARD_INLINE_MAX_ADDR, // last inline
            CompactHeader::FORWARD_INLINE_MAX_ADDR + 8, // first overflow
            12usize * 1024 * 1024 * 1024,           // 12 GB
            0xFF_FFFF_FFF8,                         // ~1 TB, aligned
        ];
        for &addr in &addrs {
            let mut h = CompactHeader::new_object(0x5151, 4);
            h.set_forwarding_ptr(addr);
            assert!(h.is_forwarded(), "addr={addr:#x}");
            assert_eq!(h.forwarding_ptr(), addr, "addr={addr:#x}");
            assert_eq!(
                h.narrow_klass(),
                0x5151,
                "klass clobbered at addr={addr:#x}"
            );
        }
    }

    /// The overflow side table can be cleared between GC cycles.
    ///
    /// This test's *own* assertions are race-safe and its doc used to say so:
    /// tokens are monotonic and never reissued, so a token dropped by `clear()`
    /// resolves to 0 afterwards regardless of concurrent interning, and the
    /// `>= 1` length check only ever gains entries. That reasoning was correct
    /// and it concluded "correctness does not depend on the guard" — of the one
    /// direction it looked at.
    ///
    /// The direction it missed is what this test does **to** its neighbours.
    /// `clear_forwarding_overflow_table()` is not scoped to this test's token;
    /// it empties the process-wide table, so every concurrently-running test
    /// holding an overflow forwarding loses its address. The guard is now
    /// load-bearing here — it is what keeps this clear from landing between some
    /// other test's intern and its read. See `overflow_table_guard`.
    #[test]
    fn forwarding_overflow_table_clears() {
        let _guard = overflow_table_guard();

        let addr = 20usize * 1024 * 1024 * 1024; // 20 GB -> overflow path
        let mut h = CompactHeader::new_object(1, 0);
        h.set_forwarding_ptr(addr);
        assert!(forwarding_overflow_table_len() >= 1);
        // The freshly interned high target resolves before clearing.
        assert_eq!(h.forwarding_ptr(), addr);

        clear_forwarding_overflow_table();
        // After clearing, this header's token is gone and never reissued, so it
        // resolves to 0 (the "unknown token" sentinel) — independent of any
        // concurrent interning.
        assert_eq!(h.forwarding_ptr(), 0);
    }

    // -- Raw roundtrip ------------------------------------------------------

    #[test]
    fn raw_roundtrip() {
        let h = CompactHeader::new_array(0xCAFE, 11, 99);
        let raw = h.raw();
        let h2 = CompactHeader::from_raw(raw);
        assert_eq!(h2.narrow_klass(), 0xCAFE);
        assert_eq!(h2.element_type(), 11);
        assert_eq!(h2.gc_age(), 99);
        assert!(h2.is_array());
    }

    // -- Size constant ------------------------------------------------------

    #[test]
    fn size_is_8_bytes() {
        assert_eq!(CompactHeader::SIZE, 8);
        assert_eq!(std::mem::size_of::<CompactHeader>(), 8);
    }

    #[test]
    fn memory_savings_vs_old_header() {
        // Old ObjectHeader is 32 bytes, compact is 8 => 24 bytes saved.
        let old_size: usize = 32;
        let new_size = CompactHeader::SIZE;
        assert_eq!(old_size - new_size, 24);
    }

    // -- HashCodeTable ------------------------------------------------------

    #[test]
    fn hash_table_get_or_assign_unique() {
        let ht = HashCodeTable::new();
        let h1 = ht.get_or_assign(0x1000);
        let h2 = ht.get_or_assign(0x2000);
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_table_get_or_assign_stable() {
        let ht = HashCodeTable::new();
        let h1 = ht.get_or_assign(0x1000);
        let h2 = ht.get_or_assign(0x1000);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_table_has_hash() {
        let ht = HashCodeTable::new();
        assert!(!ht.has_hash(0x1000));
        ht.get_or_assign(0x1000);
        assert!(ht.has_hash(0x1000));
        assert!(!ht.has_hash(0x2000));
    }

    #[test]
    fn hash_table_get() {
        let ht = HashCodeTable::new();
        assert_eq!(ht.get(0x1000), None);
        let assigned = ht.get_or_assign(0x1000);
        assert_eq!(ht.get(0x1000), Some(assigned));
    }

    #[test]
    fn hash_table_update_after_gc() {
        let ht = HashCodeTable::new();
        let hash = ht.get_or_assign(0x1000);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000usize, 0x5000usize);
        ht.update_after_gc(&map, &|_| false);
        // Old address gone, new address has the same hash.
        assert!(!ht.has_hash(0x1000));
        assert_eq!(ht.get(0x5000), Some(hash));
    }

    /// GCAUD-7 — `update_after_gc` must remap **and sweep**.
    ///
    /// Before this it was remap-only (`unwrap_or(addr)`), so an address the
    /// pointer map did not mention kept its entry forever. With no sweep
    /// anywhere (`remove_dead` has never had a caller), the next object
    /// allocated at that recycled address inherits a dead object's identity
    /// hash — and the table grows without bound.
    ///
    /// All three outcomes are asserted together, because a sweep that drops
    /// stationary survivors is just as wrong as one that keeps dead keys: an
    /// object's `identityHashCode` must not change while it is alive.
    #[test]
    fn hash_table_update_after_gc_sweeps_dead_keys_and_keeps_stationary_survivors() {
        let ht = HashCodeTable::new();
        let moved = ht.get_or_assign(0x1000);
        let stayed = ht.get_or_assign(0x2000);
        let died = ht.get_or_assign(0x3000);
        assert_ne!(stayed, died);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x1000usize, 0x5000usize);
        // Only 0x2000 is a stationary survivor; 0x3000 was reclaimed.
        ht.update_after_gc(&map, &|addr| addr == 0x2000);

        assert_eq!(ht.get(0x5000), Some(moved), "a moved object keeps its hash");
        assert!(!ht.has_hash(0x1000), "the pre-move key must not linger");
        assert_eq!(
            ht.get(0x2000),
            Some(stayed),
            "a live object that did not move must keep its identity hash — \
             changing it mid-life breaks every hash container holding it",
        );
        assert!(
            !ht.has_hash(0x3000),
            "a dead key must be swept: an object later allocated at 0x3000 \
             would otherwise inherit this hash",
        );
        assert_eq!(ht.len(), 2, "the table must not grow without bound");
    }

    #[test]
    fn hash_table_remove_dead() {
        let ht = HashCodeTable::new();
        ht.get_or_assign(0x1000);
        ht.get_or_assign(0x2000);
        ht.get_or_assign(0x3000);
        assert_eq!(ht.len(), 3);

        ht.remove_dead(&|addr| addr == 0x2000); // only 0x2000 is live
        assert_eq!(ht.len(), 1);
        assert!(ht.has_hash(0x2000));
        assert!(!ht.has_hash(0x1000));
    }

    #[test]
    fn hash_table_savings_bytes() {
        let ht = HashCodeTable::new();
        // 1000 objects, no hashes requested: saves 1000*4 = 4000 bytes.
        assert_eq!(ht.savings_bytes(1000), 4000);

        // Assign one hash -- savings decrease slightly.
        ht.get_or_assign(0x1000);
        let savings = ht.savings_bytes(1000);
        assert!(savings < 4000);
        assert!(savings > 0);
    }

    #[test]
    fn hash_table_len_and_empty() {
        let ht = HashCodeTable::new();
        assert!(ht.is_empty());
        assert_eq!(ht.len(), 0);
        ht.get_or_assign(0x1000);
        assert!(!ht.is_empty());
        assert_eq!(ht.len(), 1);
    }

    #[test]
    fn hash_table_clear() {
        let ht = HashCodeTable::new();
        ht.get_or_assign(0x1000);
        ht.get_or_assign(0x2000);
        assert_eq!(ht.len(), 2);
        ht.clear();
        assert!(ht.is_empty());
    }

    // -- Migration helpers --------------------------------------------------

    #[test]
    fn migrate_preserves_class_and_age() {
        let mut old = crate::heap::ObjectHeader::new(
            cratonvm_types::ClassId::new(77),
            crate::heap::ObjectKind::Object,
            crate::heap::ArrayElementType::Reference,
            0,
            2,
        );
        old.set_gc_age(9);
        let ht = HashCodeTable::new();
        let compact = migrate_to_compact(&old, 77, &ht, 0x4000);
        assert_eq!(compact.narrow_klass(), 77);
        assert_eq!(compact.gc_age(), 9);
        assert!(!compact.is_array());
        assert!(!compact.has_hash_code());
    }

    #[test]
    fn migrate_array_preserves_element_type() {
        let old = crate::heap::ObjectHeader::new(
            cratonvm_types::ClassId::new(10),
            crate::heap::ObjectKind::Array,
            crate::heap::ArrayElementType::Int,
            5,
            5,
        );
        let ht = HashCodeTable::new();
        let compact = migrate_to_compact(&old, 10, &ht, 0x5000);
        assert!(compact.is_array());
        assert_eq!(
            compact.element_type(),
            crate::heap::ArrayElementType::Int as u8
        );
    }

    #[test]
    fn migrate_hash_code_to_side_table() {
        let old = crate::heap::ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            crate::heap::ObjectKind::Object,
            crate::heap::ArrayElementType::Reference,
            0,
            0,
        );
        // The hash rides in the mark word now, not in a header field, so the
        // migration source has to be set up the way a real hashed object is.
        old.mark_word.store(
            crate::heap::ObjectHeader::make_neutral_hashed(cratonvm_types::MARK_NEUTRAL, 42),
            std::sync::atomic::Ordering::Relaxed,
        );
        let ht = HashCodeTable::new();
        let compact = migrate_to_compact(&old, 1, &ht, 0x6000);
        assert!(compact.has_hash_code());
        assert_eq!(ht.get(0x6000), Some(42));
    }

    #[test]
    fn to_legacy_fields_roundtrip() {
        let h = CompactHeader::new_array(55, 3, 10);
        let lf = to_legacy_fields(h);
        assert_eq!(lf.narrow_klass, 55);
        assert!(lf.is_array);
        assert_eq!(lf.element_type, 3);
        assert_eq!(lf.gc_age, 10);
        assert_eq!(lf.lock_state, LockState::Unlocked);
        assert!(!lf.has_hash);
        assert!(!lf.is_forwarded);
        assert_eq!(lf.forwarding_addr, 0);
    }

    #[test]
    fn to_legacy_fields_forwarded() {
        let mut h = CompactHeader::new_object(99, 0);
        h.set_forwarding_ptr(0x8000);
        let lf = to_legacy_fields(h);
        assert!(lf.is_forwarded);
        assert_eq!(lf.forwarding_addr, 0x8000);
        assert_eq!(lf.narrow_klass, 99);
    }

    #[test]
    fn legacy_fields_all_populated() {
        let mut h = CompactHeader::new_array(200, 7, 42);
        h.set_has_hash_code();
        h.set_lock_state(LockState::Inflated);
        let lf = to_legacy_fields(h);
        assert_eq!(lf.narrow_klass, 200);
        assert!(lf.is_array);
        assert_eq!(lf.element_type, 7);
        assert_eq!(lf.gc_age, 42);
        assert_eq!(lf.lock_state, LockState::Inflated);
        assert!(lf.has_hash);
        assert!(!lf.is_forwarded);
    }

    // =======================================================================
    // Session 54 — Compact Object Headers (JEP 450)
    // =======================================================================

    // -- 54.1: NarrowKlassTable --

    #[test]
    fn s54_narrow_klass_table_new_empty() {
        let t = NarrowKlassTable::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn s54_narrow_klass_table_assign_sequential() {
        let t = NarrowKlassTable::new();
        let nk1 = t.get_or_assign(cratonvm_types::ClassId::new(100));
        let nk2 = t.get_or_assign(cratonvm_types::ClassId::new(200));
        assert_ne!(nk1, nk2);
        assert!(nk1 >= 1);
        assert!(nk2 >= 1);
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn s54_narrow_klass_table_idempotent() {
        let t = NarrowKlassTable::new();
        let nk1 = t.get_or_assign(cratonvm_types::ClassId::new(42));
        let nk2 = t.get_or_assign(cratonvm_types::ClassId::new(42));
        assert_eq!(nk1, nk2);
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn s54_narrow_klass_table_resolve() {
        let t = NarrowKlassTable::new();
        let nk = t.get_or_assign(cratonvm_types::ClassId::new(77));
        let resolved = t.resolve(nk);
        assert_eq!(resolved, Some(cratonvm_types::ClassId::new(77)));
    }

    #[test]
    fn s54_narrow_klass_table_resolve_unknown() {
        let t = NarrowKlassTable::new();
        assert_eq!(t.resolve(999), None);
    }

    #[test]
    fn s54_narrow_klass_table_contains() {
        let t = NarrowKlassTable::new();
        let cid = cratonvm_types::ClassId::new(55);
        assert!(!t.contains(cid));
        t.get_or_assign(cid);
        assert!(t.contains(cid));
    }

    #[test]
    fn s54_narrow_klass_table_many_classes() {
        let t = NarrowKlassTable::new();
        for i in 0..500 {
            let cid = cratonvm_types::ClassId::new(i);
            let nk = t.get_or_assign(cid);
            assert!(nk > 0);
            assert_eq!(t.resolve(nk), Some(cid));
        }
        assert_eq!(t.len(), 500);
    }

    #[test]
    fn s54_narrow_klass_table_thread_safe() {
        use std::sync::Arc;
        let t = Arc::new(NarrowKlassTable::new());
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let t = t.clone();
                std::thread::spawn(move || {
                    for j in 0..50 {
                        let cid = cratonvm_types::ClassId::new(i * 50 + j);
                        let nk = t.get_or_assign(cid);
                        assert_eq!(t.resolve(nk), Some(cid));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(t.len(), 500);
    }

    // -- NarrowKlassTable removal path (refs-metaspace-unloading.md §R3) --

    #[test]
    fn narrow_klass_table_remove_classes_drops_both_directions() {
        let t = NarrowKlassTable::new();
        let keep = cratonvm_types::ClassId::new(7);
        let drop_a = cratonvm_types::ClassId::new(8);
        let drop_b = cratonvm_types::ClassId::new(9);

        let nk_keep = t.get_or_assign(keep);
        let nk_a = t.get_or_assign(drop_a);
        let nk_b = t.get_or_assign(drop_b);
        assert_eq!(t.len(), 3);

        assert_eq!(t.remove_classes(&[drop_a, drop_b]), 2);

        // Forward direction gone...
        assert!(!t.contains(drop_a));
        assert!(!t.contains(drop_b));
        // ...and the reverse direction too. A leak here is the one that
        // matters: `to_class_id` is what keeps the raw class id reachable.
        assert_eq!(t.resolve(nk_a), None);
        assert_eq!(t.resolve(nk_b), None);

        // The survivor is untouched in both directions.
        assert!(t.contains(keep));
        assert_eq!(t.resolve(nk_keep), Some(keep));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn narrow_klass_table_remove_classes_is_idempotent_and_ignores_unknowns() {
        let t = NarrowKlassTable::new();
        let cid = cratonvm_types::ClassId::new(3);
        t.get_or_assign(cid);

        assert_eq!(t.remove_classes(&[cid]), 1);
        // Second removal of the same id, and of an id never assigned, are both
        // no-ops rather than panics or phantom counts — an unloader that runs
        // twice over the same batch must not corrupt the count it reports.
        assert_eq!(t.remove_classes(&[cid]), 0);
        assert_eq!(t.remove_classes(&[cratonvm_types::ClassId::new(999)]), 0);
        assert_eq!(t.remove_classes(&[]), 0);
        assert!(t.is_empty());
    }

    /// A retired narrow klass must NEVER be handed to a different class: a
    /// header the sweeper has not reached yet still carries the old value, and
    /// recycling would make it resolve to a live, wrong class. `resolve`
    /// returning `None` is the required failure mode.
    #[test]
    fn narrow_klass_table_does_not_recycle_retired_ids() {
        let t = NarrowKlassTable::new();
        let old = cratonvm_types::ClassId::new(11);
        let nk_old = t.get_or_assign(old);
        assert_eq!(t.remove_classes(&[old]), 1);

        let fresh = cratonvm_types::ClassId::new(12);
        let nk_fresh = t.get_or_assign(fresh);
        assert_ne!(
            nk_fresh, nk_old,
            "a retired narrow klass was reissued: a stale header for class {old:?} \
             would now resolve to {fresh:?}"
        );
        assert_eq!(t.resolve(nk_old), None);
        assert_eq!(t.resolve(nk_fresh), Some(fresh));
    }

    /// The point of the whole change: repeated define/unload cycles must reach
    /// a steady state instead of growing one entry per cycle. This is the shape
    /// of any CGLIB/ByteBuddy/redeploy workload.
    #[test]
    fn narrow_klass_table_repeated_unload_cycles_leave_no_residue() {
        let t = NarrowKlassTable::new();
        for cycle in 0..64u32 {
            let batch: Vec<_> = (0..16)
                .map(|i| cratonvm_types::ClassId::new(cycle * 16 + i))
                .collect();
            for cid in &batch {
                t.get_or_assign(*cid);
            }
            assert_eq!(t.len(), 16);
            assert_eq!(t.remove_classes(&batch), 16);
            assert!(
                t.is_empty(),
                "residue after cycle {cycle}: {} entries",
                t.len()
            );
        }
    }

    #[test]
    fn narrow_klass_table_remove_classes_is_thread_safe() {
        use std::sync::Arc;
        let t = Arc::new(NarrowKlassTable::new());
        let all: Vec<_> = (0..500u32).map(cratonvm_types::ClassId::new).collect();
        for cid in &all {
            t.get_or_assign(*cid);
        }
        // Ten threads each retire a disjoint slice concurrently; the totals
        // must add up to exactly one removal per class.
        let handles: Vec<_> = (0..10usize)
            .map(|i| {
                let t = Arc::clone(&t);
                let slice: Vec<_> = all[i * 50..(i + 1) * 50].to_vec();
                std::thread::spawn(move || t.remove_classes(&slice))
            })
            .collect();
        let removed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(removed, 500);
        assert!(t.is_empty());
    }

    // -- 54.2: HeaderView --

    #[test]
    fn s54_header_view_from_legacy() {
        let mut old = crate::heap::ObjectHeader::new(
            cratonvm_types::ClassId::new(10),
            crate::heap::ObjectKind::Object,
            crate::heap::ArrayElementType::Reference,
            0,
            3,
        );
        old.set_gc_age(5);
        let view = HeaderView::from_legacy(&old);
        assert!(!view.is_array());
        assert_eq!(view.gc_age(), 5);
        assert!(!view.is_forwarded());
        assert_eq!(view.header_size(), crate::heap::HEADER_SIZE);
    }

    #[test]
    fn s54_header_view_from_compact() {
        let h = CompactHeader::new_array(99, 10, 7);
        let view = HeaderView::from_compact(h);
        assert!(view.is_array());
        assert_eq!(view.gc_age(), 7);
        assert_eq!(view.element_type(), 10);
        assert!(!view.is_forwarded());
        assert_eq!(view.header_size(), 8);
    }

    #[test]
    fn s54_header_view_legacy_forwarded() {
        let mut old = crate::heap::ObjectHeader::new(
            cratonvm_types::ClassId::new(1),
            crate::heap::ObjectKind::Object,
            crate::heap::ArrayElementType::Reference,
            0,
            0,
        );
        // Raw bits, not `set_forwarding_address`: the constructor asserts the
        // target is a plausible, 8-byte-aligned heap pointer, and 0x1234 is
        // deliberately neither. Writing the word directly is how a test says
        // "pretend the collector already forwarded this" without pretending the
        // address is real.
        old.mark_word.store(
            0x1234u64 | cratonvm_types::MARK_FORWARDED,
            std::sync::atomic::Ordering::Relaxed,
        );
        let view = HeaderView::from_legacy(&old);
        assert!(view.is_forwarded());
    }

    #[test]
    fn s54_header_view_compact_forwarded() {
        let mut h = CompactHeader::new_object(50, 3);
        h.set_forwarding_ptr(0x8000);
        let view = HeaderView::from_compact(h);
        assert!(view.is_forwarded());
    }

    // -- 54.3: CompactAllocator --

    #[test]
    fn s54_compact_allocator_new() {
        let alloc = CompactAllocator::new(1024);
        assert_eq!(alloc.capacity(), 1024);
        assert_eq!(alloc.used_bytes(), 0);
        assert_eq!(alloc.object_count(), 0);
    }

    #[test]
    fn s54_compact_allocator_alloc_object() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(42);
        let ptr = alloc.alloc_object(cid, 3);
        assert!(ptr.is_some());
        let ptr = ptr.unwrap();

        // Verify the header
        let header = alloc.read_header(ptr);
        assert!(!header.is_array());
        assert_eq!(header.gc_age(), 0);

        // Verify class ID resolution
        let resolved = alloc.class_id_at(ptr);
        assert_eq!(resolved, Some(cid));

        assert_eq!(alloc.object_count(), 1);
        // Derived, not restated: this delta moved 24 -> 16 when the legacy
        // header shrank 32 -> 24 on 2026-08-06, and it is the number the
        // savings report is FOR — a literal here reports the wrong saving
        // as confidently as the right one.
        assert_eq!(alloc.bytes_saved(), LEGACY_MINUS_COMPACT_HEADER);
    }

    #[test]
    fn s54_compact_allocator_alloc_array() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(10);
        let ptr = alloc.alloc_array(cid, crate::heap::ArrayElementType::Int, 5);
        assert!(ptr.is_some());
        let ptr = ptr.unwrap();

        let header = alloc.read_header(ptr);
        assert!(header.is_array());
        assert_eq!(
            header.element_type(),
            crate::heap::ArrayElementType::Int as u8
        );

        let length = alloc.read_array_length(ptr);
        assert_eq!(length, 5);

        assert_eq!(alloc.object_count(), 1);
    }

    #[test]
    fn s54_compact_allocator_oom() {
        let alloc = CompactAllocator::new(16); // Too small for a 3-field object
        let cid = cratonvm_types::ClassId::new(1);
        // 8 header + 3*16 fields = 56 bytes, won't fit in 16
        let ptr = alloc.alloc_object(cid, 3);
        assert!(ptr.is_none());
    }

    #[test]
    fn s54_compact_allocator_multiple_objects() {
        let alloc = CompactAllocator::new(1024 * 1024);
        for i in 0..100 {
            let cid = cratonvm_types::ClassId::new(i);
            let ptr = alloc.alloc_object(cid, 2);
            assert!(ptr.is_some(), "Failed to alloc object {}", i);
        }
        assert_eq!(alloc.object_count(), 100);
        assert_eq!(alloc.bytes_saved(), 100 * LEGACY_MINUS_COMPACT_HEADER);
    }

    #[test]
    fn s54_compact_allocator_identity_hash() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(1);
        let ptr = alloc.alloc_object(cid, 1).unwrap();

        let h1 = alloc.identity_hash_code(ptr as usize);
        let h2 = alloc.identity_hash_code(ptr as usize);
        assert_eq!(h1, h2, "Identity hash should be stable");

        let ptr2 = alloc.alloc_object(cid, 1).unwrap();
        let h3 = alloc.identity_hash_code(ptr2 as usize);
        assert_ne!(h1, h3, "Different objects should have different hashes");
    }

    #[test]
    fn s54_compact_allocator_header_view() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(5);
        let ptr = alloc.alloc_object(cid, 2).unwrap();

        let view = alloc.header_view(ptr);
        assert!(!view.is_array());
        assert_eq!(view.gc_age(), 0);
        assert_eq!(view.header_size(), 8);
    }

    // -- 54.4: Savings report --

    #[test]
    fn s54_savings_report_empty() {
        let alloc = CompactAllocator::new(4096);
        let report = alloc.savings_report();
        assert_eq!(report.object_count, 0);
        assert_eq!(report.header_bytes_saved, 0);
        assert_eq!(report.total_savings, 0);
    }

    #[test]
    fn s54_savings_report_with_objects() {
        let alloc = CompactAllocator::new(1024 * 1024);
        for i in 0..50 {
            alloc
                .alloc_object(cratonvm_types::ClassId::new(i), 1)
                .unwrap();
        }
        let report = alloc.savings_report();
        assert_eq!(report.object_count, 50);
        assert_eq!(report.header_bytes_saved, 50 * LEGACY_MINUS_COMPACT_HEADER);
        assert_eq!(report.klass_table_entries, 50);
        assert_eq!(report.hash_table_entries, 0); // no hash codes requested
    }

    #[test]
    fn s54_savings_report_format() {
        let alloc = CompactAllocator::new(4096);
        alloc
            .alloc_object(cratonvm_types::ClassId::new(1), 2)
            .unwrap();
        let report = alloc.savings_report();
        let formatted = report.format();
        assert!(formatted.contains("Compact Object Headers Report"));
        assert!(formatted.contains("Objects:"));
        assert!(formatted.contains("Header savings:"));
        assert!(formatted.contains("Total savings:"));
    }

    #[test]
    fn s54_savings_report_with_hash_codes() {
        let alloc = CompactAllocator::new(4096);
        let ptr = alloc
            .alloc_object(cratonvm_types::ClassId::new(1), 1)
            .unwrap();
        alloc.identity_hash_code(ptr as usize);
        let report = alloc.savings_report();
        assert_eq!(report.hash_table_entries, 1);
    }

    // -- 54.5: element_byte_size helper --

    #[test]
    fn s54_element_byte_size_values() {
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Boolean), 1);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Byte), 1);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Char), 2);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Short), 2);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Int), 4);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Float), 4);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Long), 8);
        assert_eq!(element_byte_size(crate::heap::ArrayElementType::Double), 8);
        assert_eq!(
            element_byte_size(crate::heap::ArrayElementType::Reference),
            8
        );
    }

    // -- 54.6: Integration with CompactHeader --

    #[test]
    fn s54_compact_header_8_vs_legacy_16() {
        assert_eq!(CompactHeader::SIZE, 8);
        // 32 -> 24 when `forwarding_ptr` folded into the mark word, 24 -> 16
        // when the identity hash followed and the kind/element_type/gc_age/
        // gc_flags quartet joined them in bits 48..62. The gap this type would
        // still close is now 8, not 24 -- most of the reason it existed has
        // been taken by the real header.
        assert_eq!(crate::heap::HEADER_SIZE, 16);
        assert_eq!(crate::heap::HEADER_SIZE - CompactHeader::SIZE, 8);
    }

    #[test]
    fn s54_compact_allocator_klass_table_access() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(99);
        alloc.alloc_object(cid, 1).unwrap();

        let kt = alloc.klass_table();
        assert_eq!(kt.len(), 1);
        assert!(kt.contains(cid));
    }

    #[test]
    fn s54_compact_allocator_hash_table_access() {
        let alloc = CompactAllocator::new(4096);
        let ht = alloc.hash_table();
        assert!(ht.is_empty());
    }

    // -- 54.7: Mixed array element types --

    #[test]
    fn s54_compact_array_all_element_types() {
        let alloc = CompactAllocator::new(1024 * 1024);
        let cid = cratonvm_types::ClassId::new(1);
        let types = [
            crate::heap::ArrayElementType::Boolean,
            crate::heap::ArrayElementType::Byte,
            crate::heap::ArrayElementType::Char,
            crate::heap::ArrayElementType::Short,
            crate::heap::ArrayElementType::Int,
            crate::heap::ArrayElementType::Float,
            crate::heap::ArrayElementType::Long,
            crate::heap::ArrayElementType::Double,
            crate::heap::ArrayElementType::Reference,
        ];
        for et in &types {
            let ptr = alloc.alloc_array(cid, *et, 10);
            assert!(ptr.is_some(), "Failed to alloc array of type {:?}", et);
            let ptr = ptr.unwrap();
            let header = alloc.read_header(ptr);
            assert!(header.is_array());
            assert_eq!(header.element_type(), *et as u8);
            assert_eq!(alloc.read_array_length(ptr), 10);
        }
    }

    // -- 54.8: Klass table + compact header roundtrip --

    #[test]
    fn s54_klass_roundtrip_through_header() {
        let alloc = CompactAllocator::new(4096);
        let cid = cratonvm_types::ClassId::new(12345);
        let ptr = alloc.alloc_object(cid, 0).unwrap();
        let header = alloc.read_header(ptr);
        let nk = header.narrow_klass();
        let resolved = alloc.klass_table().resolve(nk);
        assert_eq!(resolved, Some(cid));
    }

    // -- 54.9: Large allocation stress --

    #[test]
    fn s54_compact_allocator_stress() {
        let alloc = CompactAllocator::new(16 * 1024 * 1024); // 16 MB
        let mut ptrs = Vec::new();
        for i in 0..10_000 {
            let cid = cratonvm_types::ClassId::new(i % 100);
            if let Some(ptr) = alloc.alloc_object(cid, 2) {
                ptrs.push(ptr);
            } else {
                break;
            }
        }
        assert_eq!(ptrs.len(), 10_000);
        assert_eq!(alloc.object_count(), 10_000);
        assert_eq!(alloc.bytes_saved(), 10_000 * LEGACY_MINUS_COMPACT_HEADER);

        // Verify all objects have correct class IDs
        for (i, ptr) in ptrs.iter().enumerate() {
            let cid = alloc.class_id_at(*ptr);
            assert_eq!(cid, Some(cratonvm_types::ClassId::new(i as u32 % 100)));
        }
    }

    // -- 54.10: Memory layout verification --

    #[test]
    fn s54_compact_object_layout_size() {
        // Object with 2 fields: 8 (header) + 2*16 (slots) = 40, aligned to 8
        let alloc = CompactAllocator::new(4096);
        let p1 = alloc
            .alloc_object(cratonvm_types::ClassId::new(1), 2)
            .unwrap();
        let p2 = alloc
            .alloc_object(cratonvm_types::ClassId::new(1), 2)
            .unwrap();
        let diff = (p2 as usize) - (p1 as usize);
        // 8 + 2*16 = 40, 8-byte aligned
        assert_eq!(diff, 40);
    }

    #[test]
    fn s54_compact_array_layout_size() {
        let alloc = CompactAllocator::new(4096);
        // int[5]: 8 (header) + 8 (length+pad) + 5*4 = 36, aligned to 8 → 40
        let p1 = alloc
            .alloc_array(
                cratonvm_types::ClassId::new(1),
                crate::heap::ArrayElementType::Int,
                5,
            )
            .unwrap();
        let p2 = alloc
            .alloc_array(
                cratonvm_types::ClassId::new(1),
                crate::heap::ArrayElementType::Int,
                5,
            )
            .unwrap();
        let diff = (p2 as usize) - (p1 as usize);
        // 8 + 8 + ceil(20/8)*8 = 8 + 8 + 24 = 40
        assert_eq!(diff, 40);
    }

    /// T10.9.B — smoke test: HashCodeTable and NarrowKlassTable use FxHashMap.
    /// Verifies insert/lookup works after the HashMap → FxHashMap swap.
    #[test]
    fn t10_9_b_fx_hashmap_swap_smoke() {
        // HashCodeTable insertion
        let tbl = HashCodeTable::new();
        let h1 = tbl.get_or_assign(0x1000);
        let h2 = tbl.get_or_assign(0x2000);
        let h1_again = tbl.get_or_assign(0x1000);
        assert_eq!(h1, h1_again, "same address returns same hash");
        assert_ne!(h1, h2, "different addresses get different hashes");

        // NarrowKlassTable round-trip
        let nk = NarrowKlassTable::new();
        let id1 = nk.get_or_assign(cratonvm_types::ClassId::new(42));
        let id2 = nk.get_or_assign(cratonvm_types::ClassId::new(43));
        let id1_again = nk.get_or_assign(cratonvm_types::ClassId::new(42));
        assert_eq!(id1, id1_again, "same ClassId returns same narrow id");
        assert_ne!(id1, id2, "different ClassId gets distinct narrow id");
    }
}
