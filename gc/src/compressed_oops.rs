// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compressed object pointers (CompressedOops) for heaps under 32 GB.
//!
//! When the Java heap fits in 32 GB, every object reference can be stored as a
//! 32-bit value instead of a full 64-bit pointer. In HotSpot this typically
//! saves 20-30 % of total heap for reference-heavy workloads.
//!
//! **In CratonVM it saves 4.7 %.** Measured 2026-08-06, interleaved A-B-B-A on
//! spring-beans `BeanRegistrationsAotContributionTests`: peak RSS 1850 MB wide
//! vs 1763 MB narrow, wall time indistinguishable. The reason is the 32-byte
//! `ObjectHeader` (HotSpot's is 12): halving the reference *fields* of objects
//! whose header already costs 32 bytes moves a minority of the bytes, so the
//! header shrink is the item with the leverage here, not this one. Full
//! derivation in
//! `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.
//!
//! Three modes are supported:
//!
//! | Mode | Heap base | Shift | Encoding |
//! |--------------|-----------|-------|------------------------------|
//! | Uncompressed | n/a | n/a | raw 64-bit pointer |
//! | ZeroBased | 0 | 0 / 3 | `addr >> shift` |
//! | HeapBased | > 0 | 0 / 3 | `(addr - base) >> shift` |
//!
//! # Wiring status (re-audited 2026-07-26 — the previous note was wrong)
//!
//! This module is wired into the live heap behind the **opt-in**
//! `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1` gate, which is
//! **off by default**. See [`enable_for_live_heap`] for the geometry the VM
//! fixes at init, and `cratonvm_types::narrow_oop` for the mechanism the hot
//! paths use (that module owns the base/shift pair because the compact field
//! accessors live below this crate in the dependency graph).
//!
//! What is narrowed when the gate is on: **reference instance fields** and
//! **reference array elements**, from 8 bytes to 4. Nothing else.
//!
//! ## The gate is NOT off for throughput reasons. Two correctness holes — both
//! ## now CLOSED (2026-08-06); the gate stays off for the reasons below them.
//!
//! This header previously claimed the only reason the gate stays off is that
//! the JIT's inline compact-field fast paths are disabled, i.e. "a throughput
//! regression, not incompleteness". A full sweep of every reference-slot access
//! in the workspace found that to be false. Under the *generational* backend
//! (the only one the gate permits — the `gc_backend != GcBackend::Generational`
//! check in `vm/src/vm/vm_init.rs`) two paths read
//! or wrote a reference slot at the wrong width:
//!
//! 1. **`jit/src/x64/objects.rs` `emit_load_string_value_ptr`.** It emitted an
//!    unconditional 64-bit `MOV dst, [base + compact_offset]` for the
//!    `String.value` `byte[]` field. Unlike the `getfield`/`putfield` arms it
//!    was **not** gated on `jit::x64::narrow_oops_block_inline_fields`, so under
//!    narrow oops it loaded 4 bytes of narrow oop plus 4 bytes of the adjacent
//!    `coder`/`hash` field and dereferenced the result: a deterministic
//!    wild-pointer SIGSEGV on every inlined
//!    `charAt`/`length`/`indexOf`/`hashCode`/`equals`/`compareTo`.
//!
//!    **CLOSED 2026-08-06.** The emitter has a narrow arm
//!    (`emit_load_narrow_ref_field`) mirroring `emit_narrow_ref_aload_regs`,
//!    selected per call site by `StringFieldLayout::value_compact_is_narrow` so
//!    the no-registered-layout fallback (which points the compact offset at the
//!    LEGACY 8-byte cell payload) keeps its wide load. The stopgap that refused
//!    `try_resolve_string_intrinsic` outright under narrow oops — and the
//!    throughput it cost — is gone.
//! 2. **`gc/src/gen_heap.rs` `mark_young_to_old_refs` and
//!    `rewrite_stretch_conservatively`.** Both scanned an unparseable heap
//!    stretch in aligned 8-byte words looking for old-gen object bases. A pair
//!    of adjacent narrow oops never matches, so marks were missed (premature
//!    reclamation) and refs to moved objects were left unrewritten (dangling).
//!    Fallback paths — but they are the paths that run when the parseable walk
//!    has already failed, which is exactly when correctness matters most.
//!
//!    **CLOSED 2026-08-06.** Both now go through
//!    `gen_heap::for_each_conservative_ref_slot`, which visits each aligned
//!    32-bit half decoded as a narrow oop *in addition to* the 64-bit word.
//!    Both widths, not one or the other: only reference fields and reference
//!    array elements are narrowed, so such a stretch can still hold full-width
//!    pointers. Sharing one helper is what keeps the mark walk and the rewrite
//!    walk agreeing about width — marking at one width and rewriting at
//!    another would itself leave a dangling reference.
//!
//! Both holes are now closed by a fix, not by refusal. `enable_for_live_heap`
//! still warns and the default stays OFF, for two reasons that are NOT
//! "unsound on this backend":
//!
//! * items 4 and 6 below are unmigrated, and item 6 means the G1/ZGC backends
//!   would corrupt the heap outright — `vm/src/vm/vm_init.rs`'s backend check
//!   is what stands between them and a user, and it is load-bearing;
//! * **it is not worth much here.** 4.7 % of peak RSS, measured — see the
//!   header note above. That is the honest reason not to spend a corpus run on
//!   it yet, and it reorders the roadmap: the `ObjectHeader` shrink is the item
//!   with the leverage, and this one is a prerequisite for it rather than a win
//!   on its own.
//!
//! "No known hole" is still a weaker claim than "measured sound" — the sweep
//! that found these two found them by reading, not by running — so a corpus
//! run with `-XX:+UseCompressedOops` remains the next real step for the
//! feature, just not an urgent one.
//!
//! For what closing hole 1 did and did not buy on the workload that prompted
//! it, see `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.
//!
//! ## Verified NOT needed (contrary to the older remaining-work list)
//!
//! * **Klass-pointer compression.** `ObjectHeader::class_id` is already a `u32`,
//!   so there is nothing to narrow. [`CompressedOops::encode_klass`] /
//!   [`CompressedOops::decode_klass`], [`NarrowKlass`] and
//!   [`CompressedOopArray`] have **zero callers** anywhere in the workspace and
//!   exist only for their unit tests; they are dead weight, not pending work.
//! * **GC root re-encoding.** Frame locals, operand-stack slots and JIT stack
//!   spills are *not* heap slots and must stay 64-bit: they are bounded by stack
//!   depth, not live-set size, and narrowing them would add an encode/decode to
//!   every `aload`/`astore` for no footprint win. `vm/src/jit/conservative_roots.rs`
//!   and `vm/src/jit/xt_root_scan.rs` walk 8-byte words and are correct as-is.
//!   This item must never be actioned.
//! * **`Unsafe.arrayIndexScale`.** `native-builtins/src/lib.rs:25404` reports 8
//!   for reference arrays. This looks like a bug and is not: `arrayBaseOffset`
//!   (16) and the scale form a *self-consistent fiction* that
//!   `unsafe_array_index_from_offset` (`unsafe_natives_ext.rs:1401`) decodes back
//!   to an element index, which then goes through the narrow-aware
//!   `get_array_element`. The synthetic offset is never dereferenced. Changing
//!   the scale to 4 without changing the decoder in lockstep would **break**
//!   `ConcurrentHashMap` and `AtomicReferenceArray`.
//!
//! ## Still open, in priority order
//!
//! 3. `jit/src/x64.rs:2119` `narrow_oops_block_inline_fields` and its four call
//!    sites — the inline compact `getfield`/`putfield` fast paths bail out, so
//!    field access falls back to the (correct but slower) helpers. This is the
//!    *throughput* item, and it is item 3, not item 1.
//! 4. `jit/src/x64.rs:17141` / `:17178` hardcode shift 3 in the narrow
//!    `aaload`/`aastore` emitters instead of reading `narrow_shift()`. Correct
//!    today only because [`enable_for_live_heap`] pins shift 3 — but
//!    [`CompressedOops::determine_shift`] returns 0 for heaps under 4 GiB, so
//!    wiring that in would silently miscompile every reference-array access.
//! 5. `vm/src/runtime/serviceability.rs:1460` — the hprof *instance* dump reads
//!    ref fields as `*const u64` and emits garbage object ids. (The sibling
//!    *array* dump at `:1548` was migrated; the instance path was missed.)
//!    Diagnostic-only.
//! 6. `gc/src/g1.rs` (~20 sites, including the main mark loop at `:5211` and
//!    every evacuation/remembered-set path), `gc/src/zgc.rs:1788`, and
//!    `gc/src/region.rs:957`/`:993` are entirely unmigrated. These are held off
//!    solely by the `gc_backend != GcBackend::Generational` check in
//!    `vm/src/vm/vm_init.rs`, which is therefore load-bearing and must not be
//!    relaxed before they are.
//!
//!    G1 and ZGC are NOT the same case, and this item used to blur them.
//!    G1 is *unmigrated*: narrow oops and a moving-but-uncolored collector are
//!    compatible in principle, so those ~20 sites are work someone could do.
//!    ZGC is *incompatible by construction* and must stay refused permanently.
//!    Its colored pointers spend bits 42-45 on mark/remap/finalizable metadata
//!    plus bit 63 on CratonVM's escaped-word tag (see `gc/src/zgc/vaddr.rs`);
//!    a 32-bit narrow slot has no room for them, and — the part that actually
//!    bites — a 32-bit slot cannot represent a value above `2^47`, so the
//!    `plausible_heap_pointer` tripwire that catches an escaped colored word
//!    has nothing left to trip on. OpenJDK refuses the same combination.
//!
//! Full derivation, including the honest footprint arithmetic and how this
//! interacts with shrinking `ObjectHeader` from 32 to 16 bytes, is in
//! `value-repr-and-compressed-oops.md`.

use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Mode
// ---------------------------------------------------------------------------

/// Compression mode in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressedOopsMode {
    /// Disabled — use raw 64-bit pointers.
    Uncompressed,
    /// Zero-based: heap starts at address 0, shift only.
    /// Encoding: `compressed = addr >> shift`
    /// Decoding: `addr = compressed << shift`
    ZeroBased,
    /// Non-zero base: heap starts at `base_addr`.
    /// Encoding: `compressed = (addr - base) >> shift`
    /// Decoding: `addr = base + (compressed << shift)`
    HeapBased,
}

// ---------------------------------------------------------------------------
// CompressedOop (32-bit reference)
// ---------------------------------------------------------------------------

/// A compressed object pointer — fits in 32 bits.
/// Use [`CompressedOops::encode`] / [`CompressedOops::decode`] to convert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct CompressedOop(u32);

impl CompressedOop {
    pub const NULL: CompressedOop = CompressedOop(0);

    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn from_raw(v: u32) -> Self {
        CompressedOop(v)
    }
}

// ---------------------------------------------------------------------------
// NarrowKlass (compressed class pointer)
// ---------------------------------------------------------------------------

/// A compressed class pointer — fits in 32 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NarrowKlass(u32);

impl NarrowKlass {
    pub const NULL: NarrowKlass = NarrowKlass(0);

    #[inline]
    pub fn raw(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn is_null(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub fn from_raw(v: u32) -> Self {
        NarrowKlass(v)
    }
}

// ---------------------------------------------------------------------------
// CompressedOops configuration
// ---------------------------------------------------------------------------

/// Global compressed-oop configuration derived from heap geometry.
pub struct CompressedOops {
    /// Whether compressed oops are enabled.
    enabled: AtomicBool,
    /// Mode of compression.
    mode: CompressedOopsMode,
    /// Heap base address (0 for zero-based mode).
    base: u64,
    /// Shift amount (0 for heaps < 4 GB, 3 for heaps < 32 GB).
    shift: u8,
    /// Maximum heap size that can use compressed oops (32 GB).
    _max_heap_size: u64,
    /// Narrow oop base for class pointers (may differ from heap base).
    narrow_klass_base: u64,
    /// Shift for compressed class pointers.
    narrow_klass_shift: u8,
}

impl CompressedOops {
    /// Maximum heap size for which compressed oops are possible (32 GB).
    pub const MAX_HEAP_FOR_COMPRESSED: u64 = 32 * 1024 * 1024 * 1024;

    /// Object alignment in bytes — allows a 3-bit shift.
    pub const OBJECT_ALIGNMENT: u64 = 8;

    /// Maximum heap for zero shift (4 GB fits in 32 bits without shifting).
    pub const MAX_HEAP_ZERO_SHIFT: u64 = 4 * 1024 * 1024 * 1024;

    // -- constructors -------------------------------------------------------

    /// Create a new configuration.  The mode and shift are derived
    /// automatically from `heap_base` and `heap_size`.
    pub fn new(heap_base: u64, heap_size: u64) -> Self {
        if heap_size > Self::MAX_HEAP_FOR_COMPRESSED {
            return Self::disabled();
        }

        let mode = Self::determine_mode(heap_base, heap_size);
        let shift = Self::determine_shift(heap_size);

        CompressedOops {
            enabled: AtomicBool::new(true),
            mode,
            base: heap_base,
            shift,
            _max_heap_size: Self::MAX_HEAP_FOR_COMPRESSED,
            narrow_klass_base: heap_base,
            narrow_klass_shift: shift,
        }
    }

    /// Create a disabled (uncompressed) configuration.
    pub fn disabled() -> Self {
        CompressedOops {
            enabled: AtomicBool::new(false),
            mode: CompressedOopsMode::Uncompressed,
            base: 0,
            shift: 0,
            _max_heap_size: Self::MAX_HEAP_FOR_COMPRESSED,
            narrow_klass_base: 0,
            narrow_klass_shift: 0,
        }
    }

    // -- mode / shift determination -----------------------------------------

    /// Determine the optimal mode for the given heap parameters.
    pub fn determine_mode(heap_base: u64, heap_size: u64) -> CompressedOopsMode {
        if heap_size > Self::MAX_HEAP_FOR_COMPRESSED {
            CompressedOopsMode::Uncompressed
        } else if heap_base == 0 {
            CompressedOopsMode::ZeroBased
        } else {
            CompressedOopsMode::HeapBased
        }
    }

    /// Determine the shift amount.
    ///
    /// * `0` if `heap_size <= 4 GB` — 32 bits covers the full range.
    /// * `3` if `heap_size <= 32 GB` — shift by object alignment (8 bytes).
    pub fn determine_shift(heap_size: u64) -> u8 {
        if heap_size <= Self::MAX_HEAP_ZERO_SHIFT {
            0
        } else {
            3
        }
    }

    // -- encode / decode (oops) ---------------------------------------------

    /// Encode a 64-bit address into a [`CompressedOop`].
    ///
    /// Returns [`CompressedOop::NULL`] for address 0.
    ///
    /// # Encodability guard
    ///
    /// The address must lie within the encodable heap range and be aligned to
    /// the active shift (8-byte aligned when `shift == 3`). Encoding a
    /// non-encodable address would silently truncate it into the wrong narrow
    /// oop, so this method guards against that:
    ///
    /// * In debug builds a `debug_assert!` fires on a non-encodable / misaligned
    ///   address, catching wiring bugs at their origin.
    /// * In release builds it fails safe by returning [`CompressedOop::NULL`]
    ///   rather than a truncated, wrong-but-plausible narrow oop. NULL can never
    ///   alias a live object, so a downstream decode lands on address 0 (an
    ///   obvious fault) instead of silently pointing at an unrelated object.
    ///
    /// Use [`CompressedOops::is_encodable`] to check ahead of time when NULL is
    /// not an acceptable sentinel for the caller.
    #[inline]
    pub fn encode(&self, addr: u64) -> CompressedOop {
        if addr == 0 {
            return CompressedOop::NULL;
        }
        match self.mode {
            CompressedOopsMode::Uncompressed => {
                // Degenerate pass-through: compression is disabled, so there is
                // no narrow-oop range or shift to validate against. Preserve the
                // historic "shouldn't normally be called, but be safe" behavior.
                CompressedOop(addr as u32)
            }
            CompressedOopsMode::ZeroBased | CompressedOopsMode::HeapBased => {
                // Guard: the address must be representable as a narrow oop and
                // aligned to the shift. Misalignment (low `shift` bits set) is
                // discarded by `>> shift`, decoding back to a *different*
                // address — a silent corruption. Out-of-range addresses are
                // truncated by `as u32`. `is_encodable` covers the range check.
                let shift_mask = (1u64 << self.shift) - 1;
                let aligned = (addr & shift_mask) == 0;
                debug_assert!(
                    self.is_encodable(addr) && aligned,
                    "CompressedOops::encode: address {addr:#x} is not encodable \
                     (mode={:?}, base={:#x}, shift={}) — would silently truncate \
                     to a wrong narrow oop",
                    self.mode,
                    self.base,
                    self.shift,
                );
                if !self.is_encodable(addr) || !aligned {
                    // Release-build fail-safe: never emit a truncated/wrong
                    // narrow oop; NULL decodes to an obvious fault at address 0.
                    return CompressedOop::NULL;
                }
                match self.mode {
                    CompressedOopsMode::ZeroBased => CompressedOop((addr >> self.shift) as u32),
                    // HeapBased — `is_encodable` already proved `addr >= base`.
                    _ => CompressedOop(((addr - self.base) >> self.shift) as u32),
                }
            }
        }
    }

    /// Decode a [`CompressedOop`] back to a 64-bit address.
    ///
    /// Returns `0` for [`CompressedOop::NULL`].
    #[inline]
    pub fn decode(&self, oop: CompressedOop) -> u64 {
        if oop.is_null() {
            return 0;
        }
        match self.mode {
            CompressedOopsMode::Uncompressed => oop.0 as u64,
            CompressedOopsMode::ZeroBased => (oop.0 as u64) << self.shift,
            CompressedOopsMode::HeapBased => self.base + ((oop.0 as u64) << self.shift),
        }
    }

    // -- encode / decode (klass) --------------------------------------------

    /// Encode a 64-bit class metadata address into a [`NarrowKlass`].
    #[inline]
    pub fn encode_klass(&self, addr: u64) -> NarrowKlass {
        if addr == 0 {
            return NarrowKlass::NULL;
        }
        NarrowKlass(((addr - self.narrow_klass_base) >> self.narrow_klass_shift) as u32)
    }

    /// Decode a [`NarrowKlass`] back to a 64-bit address.
    #[inline]
    pub fn decode_klass(&self, klass: NarrowKlass) -> u64 {
        if klass.is_null() {
            return 0;
        }
        self.narrow_klass_base + ((klass.0 as u64) << self.narrow_klass_shift)
    }

    // -- queries ------------------------------------------------------------

    /// Check whether `addr` is encodable under the current configuration.
    pub fn is_encodable(&self, addr: u64) -> bool {
        if !self.is_enabled() {
            return false;
        }
        if addr == 0 {
            return true; // null is always representable
        }
        match self.mode {
            CompressedOopsMode::Uncompressed => false,
            CompressedOopsMode::ZeroBased => {
                // addr must fit after shifting
                (addr >> self.shift) <= u32::MAX as u64
            }
            CompressedOopsMode::HeapBased => {
                if addr < self.base {
                    return false;
                }
                ((addr - self.base) >> self.shift) <= u32::MAX as u64
            }
        }
    }

    /// Whether compressed oops are currently active.
    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Current compression mode.
    #[inline]
    pub fn mode(&self) -> CompressedOopsMode {
        self.mode
    }

    /// Heap base address.
    #[inline]
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Shift amount.
    #[inline]
    pub fn shift(&self) -> u8 {
        self.shift
    }

    // -- statistics ---------------------------------------------------------

    /// Produce a stats snapshot.
    pub fn stats(&self, heap_size: u64) -> CompressedOopsStats {
        CompressedOopsStats {
            mode: self.mode,
            heap_base: self.base,
            heap_size,
            shift: self.shift,
            estimated_savings_percent: Self::estimate_savings_percent(heap_size),
        }
    }

    /// Rough upper bound on the percent footprint saving, for reporting only.
    ///
    /// # This is deliberately NOT the HotSpot 20-30 % figure
    ///
    /// The old value here was a flat 25-30 %, copied from HotSpot's published
    /// range. That range assumes HotSpot's **12-byte** object header. CratonVM's
    /// `cratonvm_types::ObjectHeader` is **32 bytes** (`HEADER_SIZE`, see
    /// `types/src/heap_types.rs:18`), which changes the arithmetic completely — the header,
    /// not the references, dominates a small object. Worked from the actual
    /// layouts (compact body, 8-byte object alignment):
    ///
    /// | object | wide | narrow | saving |
    /// |---|---|---|---|
    /// | `java.lang.Integer` (autoboxed ⇒ legacy 16-byte cell) | 48 | 48 | **0 %** |
    /// | `java.lang.String` `{byte[] value, byte coder, int hash}` | 48 | 48 | **0 %** — 13 B and 9 B both round to 16 |
    /// | `ArrayList` `{Object[], int, int}` | 48 | 48 | **0 %** — same rounding |
    /// | binary-tree node `{left, right}` | 48 | 40 | 16.7 % |
    /// | `HashMap.Node` `{int hash, K, V, next}` | 64 | 48 | 25 % |
    /// | `Object[16]` | 160 | 96 | 40 % |
    /// | `Object[n]`, large `n` | `32+8n` | `32+4n` | → 50 % |
    ///
    /// The pattern: **reference arrays are where compressed oops pay**, and
    /// they pay a lot. Reference-dense small objects pay 17-25 %. Boxed
    /// primitives, `String` and single-reference containers pay **nothing at
    /// all**, because saving 4 bytes on an 8-byte-aligned body is frequently
    /// rounded straight back. Halving the header would help every one of those.
    ///
    /// A whole-heap number therefore depends entirely on the object mix and
    /// cannot be derived from `heap_size`. This function keeps the signature for
    /// its callers but reports a defensible ceiling rather than a fabricated
    /// point estimate: no live heap of ordinary Java objects reaches the 50 %
    /// asymptote, and many workloads land in single digits.
    pub fn estimate_savings_percent(heap_size: u64) -> f64 {
        if heap_size > Self::MAX_HEAP_FOR_COMPRESSED {
            return 0.0;
        }
        // Ceiling, not an expectation: the 50 % asymptote is reached only by a
        // heap that is entirely large reference arrays. See the table above.
        Self::MAX_SAVINGS_PERCENT
    }

    /// Upper bound reported by [`Self::estimate_savings_percent`]: a heap made
    /// entirely of large `Object[]` halves its reference bytes and nothing else.
    pub const MAX_SAVINGS_PERCENT: f64 = 50.0;
}

// ---------------------------------------------------------------------------
// CompressedOopArray
// ---------------------------------------------------------------------------

/// A compact array of compressed oops (4 bytes each instead of 8).
pub struct CompressedOopArray {
    data: Vec<u32>,
}

impl CompressedOopArray {
    /// Create a new array of the given length, initialized to null.
    pub fn new(len: usize) -> Self {
        CompressedOopArray { data: vec![0; len] }
    }

    /// Number of elements.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the array is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Get the compressed oop at `index`.
    ///
    /// # Panics
    /// Panics if `index >= self.len()`.
    #[inline]
    pub fn get(&self, index: usize) -> CompressedOop {
        CompressedOop(self.data[index])
    }

    /// Set the compressed oop at `index`.
    ///
    /// # Panics
    /// Panics if `index >= self.len()`.
    #[inline]
    pub fn set(&mut self, index: usize, oop: CompressedOop) {
        self.data[index] = oop.0;
    }

    /// Bytes saved compared to an uncompressed (64-bit) reference array of the
    /// same length.  Each entry saves 4 bytes (8 vs 4).
    pub fn savings_bytes(&self) -> usize {
        self.data.len() * 4
    }
}

// ---------------------------------------------------------------------------
// CompressedOopsStats
// ---------------------------------------------------------------------------

/// Snapshot of compressed-oop statistics.
pub struct CompressedOopsStats {
    pub mode: CompressedOopsMode,
    pub heap_base: u64,
    pub heap_size: u64,
    pub shift: u8,
    pub estimated_savings_percent: f64,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    // -- mode determination -------------------------------------------------

    #[test]
    fn zero_based_small_heap() {
        let c = CompressedOops::new(0, 2 * GB);
        assert_eq!(c.mode(), CompressedOopsMode::ZeroBased);
        assert_eq!(c.shift(), 0);
        assert!(c.is_enabled());
    }

    #[test]
    fn zero_based_large_heap() {
        let c = CompressedOops::new(0, 16 * GB);
        assert_eq!(c.mode(), CompressedOopsMode::ZeroBased);
        assert_eq!(c.shift(), 3);
        assert!(c.is_enabled());
    }

    #[test]
    fn heap_based_mode() {
        let base = 0x7000_0000_0000u64;
        let c = CompressedOops::new(base, 8 * GB);
        assert_eq!(c.mode(), CompressedOopsMode::HeapBased);
        assert_eq!(c.base(), base);
        assert_eq!(c.shift(), 3);
    }

    #[test]
    fn disabled_for_large_heap() {
        let c = CompressedOops::new(0, 64 * GB);
        assert_eq!(c.mode(), CompressedOopsMode::Uncompressed);
        assert!(!c.is_enabled());
    }

    #[test]
    fn explicitly_disabled() {
        let c = CompressedOops::disabled();
        assert!(!c.is_enabled());
        assert_eq!(c.mode(), CompressedOopsMode::Uncompressed);
    }

    // -- shift determination ------------------------------------------------

    #[test]
    fn shift_zero_for_small_heap() {
        assert_eq!(CompressedOops::determine_shift(GB), 0);
        assert_eq!(CompressedOops::determine_shift(4 * GB), 0);
    }

    #[test]
    fn shift_three_for_medium_heap() {
        assert_eq!(CompressedOops::determine_shift(4 * GB + 1), 3);
        assert_eq!(CompressedOops::determine_shift(32 * GB), 3);
    }

    // -- encode / decode roundtrip ------------------------------------------

    #[test]
    fn roundtrip_zero_shift() {
        let c = CompressedOops::new(0, 2 * GB);
        let addr = 0x1000u64;
        let encoded = c.encode(addr);
        assert_eq!(c.decode(encoded), addr);
    }

    #[test]
    fn roundtrip_shift_3() {
        let c = CompressedOops::new(0, 16 * GB);
        // Address must be 8-byte aligned (shift=3).
        let addr = 0x1000_0000u64;
        let encoded = c.encode(addr);
        assert_eq!(c.decode(encoded), addr);
    }

    #[test]
    fn roundtrip_with_base() {
        let base = 0x1_0000_0000u64; // 4 GB
        let c = CompressedOops::new(base, 8 * GB);
        let addr = base + 0x800_0000u64; // base + 128 MB, 8-byte aligned
        let encoded = c.encode(addr);
        assert_eq!(c.decode(encoded), addr);
    }

    #[test]
    fn null_encode_decode() {
        let c = CompressedOops::new(0, 4 * GB);
        let encoded = c.encode(0);
        assert_eq!(encoded, CompressedOop::NULL);
        assert!(encoded.is_null());
        assert_eq!(c.decode(CompressedOop::NULL), 0);
    }

    // -- NarrowKlass --------------------------------------------------------

    #[test]
    fn klass_roundtrip() {
        let base = 0x8_0000_0000u64;
        let c = CompressedOops::new(base, 8 * GB);
        let klass_addr = base + 0x100_0000u64; // base + 16 MB, 8-aligned
        let nk = c.encode_klass(klass_addr);
        assert_eq!(c.decode_klass(nk), klass_addr);
    }

    #[test]
    fn klass_null() {
        let c = CompressedOops::new(0, 4 * GB);
        let nk = c.encode_klass(0);
        assert!(nk.is_null());
        assert_eq!(c.decode_klass(NarrowKlass::NULL), 0);
    }

    // -- is_encodable -------------------------------------------------------

    #[test]
    fn encodable_valid_address() {
        let c = CompressedOops::new(0, 4 * GB);
        assert!(c.is_encodable(0));
        assert!(c.is_encodable(0x1000));
        assert!(c.is_encodable(4 * GB - 8));
    }

    #[test]
    fn encodable_out_of_range() {
        let base = 0x1_0000_0000u64;
        let c = CompressedOops::new(base, 8 * GB);
        // Address below base is not encodable.
        assert!(!c.is_encodable(base - 1));
    }

    #[test]
    fn not_encodable_when_disabled() {
        let c = CompressedOops::disabled();
        assert!(!c.is_encodable(0x1000));
    }

    // -- CompressedOopArray -------------------------------------------------

    #[test]
    fn array_basic_ops() {
        let mut arr = CompressedOopArray::new(10);
        assert_eq!(arr.len(), 10);
        assert!(!arr.is_empty());

        // Default is null.
        assert!(arr.get(0).is_null());

        arr.set(3, CompressedOop::from_raw(42));
        assert_eq!(arr.get(3).raw(), 42);
    }

    #[test]
    fn array_savings() {
        let arr = CompressedOopArray::new(1000);
        // Each entry saves 4 bytes.
        assert_eq!(arr.savings_bytes(), 4000);
    }

    #[test]
    fn array_empty() {
        let arr = CompressedOopArray::new(0);
        assert!(arr.is_empty());
        assert_eq!(arr.savings_bytes(), 0);
    }

    // -- constants ----------------------------------------------------------

    #[test]
    fn max_heap_constant() {
        assert_eq!(CompressedOops::MAX_HEAP_FOR_COMPRESSED, 32 * GB);
    }

    #[test]
    fn max_heap_zero_shift_constant() {
        assert_eq!(CompressedOops::MAX_HEAP_ZERO_SHIFT, 4 * GB);
    }

    #[test]
    fn object_alignment_constant() {
        assert_eq!(CompressedOops::OBJECT_ALIGNMENT, 8);
    }

    // -- stats / savings estimation -----------------------------------------

    #[test]
    fn stats_snapshot() {
        let c = CompressedOops::new(0, 8 * GB);
        let s = c.stats(8 * GB);
        assert_eq!(s.mode, CompressedOopsMode::ZeroBased);
        assert_eq!(s.shift, 3);
        assert_eq!(s.heap_base, 0);
        assert_eq!(s.heap_size, 8 * GB);
        assert!(s.estimated_savings_percent > 0.0);
    }

    /// The estimate is a *ceiling*, not the HotSpot 20-30 % point estimate that
    /// used to live here — CratonVM's 32-byte header makes a per-heap-size
    /// figure meaningless. Pins that it no longer varies with heap size.
    #[test]
    fn savings_is_a_size_independent_ceiling() {
        assert_eq!(
            CompressedOops::estimate_savings_percent(2 * GB),
            CompressedOops::MAX_SAVINGS_PERCENT
        );
        assert_eq!(
            CompressedOops::estimate_savings_percent(16 * GB),
            CompressedOops::MAX_SAVINGS_PERCENT
        );
        assert_eq!(CompressedOops::MAX_SAVINGS_PERCENT, 50.0);
    }

    #[test]
    fn savings_large_heap() {
        assert_eq!(CompressedOops::estimate_savings_percent(64 * GB), 0.0);
    }

    /// The documented footprint table is the load-bearing part of the savings
    /// story, so compute it rather than trusting the prose. `round8` models the
    /// 8-byte object alignment that erases the saving on one-reference objects.
    #[test]
    fn documented_object_footprint_arithmetic_holds() {
        const HEADER: usize = 32; // cratonvm_types::HEADER_SIZE
        fn round8(n: usize) -> usize {
            (n + 7) & !7
        }
        // (wide body bytes before rounding, narrow body bytes) -> (wide, narrow)
        fn total(wide_body: usize, narrow_body: usize) -> (usize, usize) {
            (HEADER + round8(wide_body), HEADER + round8(narrow_body))
        }

        // String {byte[] value, byte coder, int hash}: 8+4+1=13 vs 4+4+1=9.
        // Both round to 16 -- compressed oops save NOTHING here.
        assert_eq!(total(13, 9), (48, 48));

        // ArrayList {Object[] elementData, int size, int modCount}: same shape.
        assert_eq!(total(16, 12), (48, 48));

        // Binary-tree node {left, right}: 16 -> 8.
        assert_eq!(total(16, 8), (48, 40));

        // HashMap.Node {int hash, K key, V value, Node next}: 4+pad4+8+8+8 = 32
        // vs 4+4+4+4 = 16.
        assert_eq!(total(32, 16), (64, 48));

        // Reference arrays are the real win and are not eroded by rounding.
        assert_eq!((HEADER + 8 * 16, HEADER + 4 * 16), (160, 96));
        // ...and approach the 50 % ceiling as length grows.
        let big = 1_000_000usize;
        let wide = HEADER + 8 * big;
        let narrow = HEADER + 4 * big;
        let pct = 100.0 * (wide - narrow) as f64 / wide as f64;
        assert!(pct > 49.9 && pct < CompressedOops::MAX_SAVINGS_PERCENT);
    }

    // -- multiple addresses -------------------------------------------------

    #[test]
    fn multiple_encode_decode() {
        let c = CompressedOops::new(0, 16 * GB);
        let addrs: Vec<u64> = (1..=100).map(|i| i * 8).collect(); // all 8-aligned
        for &a in &addrs {
            assert_eq!(c.decode(c.encode(a)), a);
        }
    }

    // -- edge: address at max boundary --------------------------------------

    #[test]
    fn address_at_max_boundary_zero_shift() {
        let c = CompressedOops::new(0, 4 * GB);
        // Largest encodable: u32::MAX since shift=0
        let addr = u32::MAX as u64;
        let encoded = c.encode(addr);
        assert_eq!(c.decode(encoded), addr);
    }

    #[test]
    fn address_at_max_boundary_shift_3() {
        let c = CompressedOops::new(0, 32 * GB);
        // Largest encodable: u32::MAX << 3
        let addr = (u32::MAX as u64) << 3;
        assert!(c.is_encodable(addr));
        let encoded = c.encode(addr);
        assert_eq!(c.decode(encoded), addr);
    }

    // -- mode determination helper ------------------------------------------

    #[test]
    fn determine_mode_uncompressed() {
        assert_eq!(
            CompressedOops::determine_mode(0, 64 * GB),
            CompressedOopsMode::Uncompressed
        );
    }

    #[test]
    fn determine_mode_zero_based() {
        assert_eq!(
            CompressedOops::determine_mode(0, 4 * GB),
            CompressedOopsMode::ZeroBased
        );
    }

    #[test]
    fn determine_mode_heap_based() {
        assert_eq!(
            CompressedOops::determine_mode(0x1000, 4 * GB),
            CompressedOopsMode::HeapBased
        );
    }

    // -- CompressedOop / NarrowKlass structs ---------------------------------

    #[test]
    fn compressed_oop_from_raw() {
        let oop = CompressedOop::from_raw(0xDEAD);
        assert_eq!(oop.raw(), 0xDEAD);
        assert!(!oop.is_null());
    }

    #[test]
    fn narrow_klass_from_raw() {
        let nk = NarrowKlass::from_raw(0xBEEF);
        assert_eq!(nk.raw(), 0xBEEF);
        assert!(!nk.is_null());
    }
}

// ---------------------------------------------------------------------------
// Live-heap wiring
// ---------------------------------------------------------------------------

use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::OnceLock;

/// Virtual-address headroom reserved **below** the lowest live heap region when
/// the narrow-oop base is chosen.
///
/// The heap's backing stores are ordinary large allocations (`Vec<u8>`), so
/// their addresses come from the system allocator's `mmap` region rather than
/// from one reservation the VM controls. Linux hands successive large mappings
/// out *downwards*, so a young-gen arena that is grown after init typically
/// lands BELOW everything that existed at init. Anchoring the base a long way
/// under the initial low-water mark keeps those later mappings encodable
/// instead of turning heap growth into a hard failure.
///
/// 8 GiB of the 32 GiB shift-3 window is spent on this; the remaining 24 GiB is
/// far more than any heap this VM is used with.
pub const NARROW_OOP_HEADROOM: u64 = 8 * 1024 * 1024 * 1024;

static ACTIVE: OnceLock<CompressedOops> = OnceLock::new();

/// The configuration fixed at VM init, or `None` when compressed oops are off.
pub fn active() -> Option<&'static CompressedOops> {
    ACTIVE.get()
}

/// Fix the compressed-oop geometry from the live heap's published regions and
/// switch narrow oops on process-wide.
///
/// **Mode: `HeapBased`, shift 3.** The base is
/// `lowest_region_base - NARROW_OOP_HEADROOM`, page-aligned down; a narrow oop
/// is `(addr - base) >> 3` and encoding `0` is reserved for null (the base sits
/// far below any object, so no live object can encode to zero). Shift 3 is
/// sound because every object starts on the 8-byte object grid, and it buys a
/// 32 GiB window — a shift of 0 would cap the encodable range at 4 GiB measured
/// from a base 8 GiB below the heap, i.e. nothing would be encodable at all.
///
/// Must be called **exactly once**, at VM init: after the heap's backing stores
/// exist (so their addresses are known) and before the first object is
/// allocated or the first class layout is registered (so no object is ever read
/// back under a different width than it was written).
///
/// Returns the `(base, shift)` it fixed, or an error describing why the heap
/// geometry is unusable — in which case narrow oops stay OFF and the VM runs
/// with full 64-bit references exactly as before.
pub fn enable_for_live_heap() -> Result<(u64, u8), String> {
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for i in 0..3 {
        let base =
            crate::gen_heap::JIT_REGION_BOUNDS.words[i * 2].load(AtomicOrdering::Acquire) as u64;
        let end = crate::gen_heap::JIT_REGION_BOUNDS.words[i * 2 + 1].load(AtomicOrdering::Acquire)
            as u64;
        if base == 0 || end <= base {
            continue;
        }
        if base % 8 != 0 {
            return Err(format!(
                "heap region {i} base {base:#x} is not 8-byte aligned; shift-3 \
                 narrow oops would be misaligned"
            ));
        }
        lo = lo.min(base);
        hi = hi.max(end);
    }
    if lo == u64::MAX {
        return Err("no live heap regions published (non-generational backend?)".to_string());
    }
    if lo <= NARROW_OOP_HEADROOM {
        return Err(format!(
            "lowest heap region {lo:#x} sits below the {NARROW_OOP_HEADROOM:#x} base headroom"
        ));
    }
    let base = (lo - NARROW_OOP_HEADROOM) & !0xfff;
    let shift: u8 = 3;
    let limit = base + ((u32::MAX as u64) << shift);
    if hi >= limit {
        return Err(format!(
            "heap spans {lo:#x}..{hi:#x}, which does not fit the narrow-oop window \
             {base:#x}..{limit:#x}"
        ));
    }
    if !cratonvm_types::narrow_oop::enable(base, shift as usize) {
        return Err("cratonvm_types::narrow_oop::enable rejected the geometry".to_string());
    }
    let _ = ACTIVE.set(CompressedOops::new(
        base,
        CompressedOops::MAX_HEAP_FOR_COMPRESSED,
    ));
    warn_known_unsound();
    Ok((base, shift))
}

/// Announce, once, that this run has two known correctness holes.
///
/// The caller (the compressed-oops setup block in `vm/src/vm/vm_init.rs`, just
/// past the `GcBackend::Generational` gate) already prints a success line saying
/// narrow oops are on. That line reads like an endorsement, and it is not one:
/// anyone who flips `-XX:+UseCompressedOops` must be told what is still
/// unmigrated at the moment they opt in, not discover it from a crash dump.
///
/// The two blockers this warning was written for — the ungated 64-bit
/// `String.value` load and the 8-byte-word conservative heap rescans — were
/// **closed on 2026-08-06** (see the module header). What is left is narrower
/// and is what this now says.
///
/// This is **not** a new gate. The gate already exists and is already off by
/// default; this only makes the existing opt-in honest about what it buys.
fn warn_known_unsound() {
    eprintln!(
        "[cratonvm] WARNING: compressed oops are opt-in and not production-ready. \
         The two correctness holes this warning used to name (the ungated 64-bit \
         String.value load, and the 8-byte-word conservative heap rescans) were \
         closed 2026-08-06. What remains:\n\
         [cratonvm]   1. Only the GENERATIONAL backend is migrated. gc/src/g1.rs, \
         gc/src/zgc.rs and gc/src/region.rs still read reference slots at 8 bytes; \
         vm/src/vm/vm_init.rs refuses the combination, and that check is \
         load-bearing.\n\
         [cratonvm]   2. The narrow aaload/aastore emitters hardcode shift 3, which \
         is correct only because enable_for_live_heap pins it.\n\
         [cratonvm]   3. It is worth 4.7% of peak RSS on this VM, not the 20-30% \
         narrow oops buy on HotSpot -- the 32-byte ObjectHeader is the dominant \
         term and compression does not touch it.\n\
         [cratonvm] See gc/src/compressed_oops.rs for the full list."
    );
}

/// Panic if `[base, end)` has drifted outside the encodable window.
///
/// Called whenever the heap republishes its region bounds (young-gen growth
/// reallocates the arena, which can move it anywhere the allocator likes). A
/// region outside the window means references into it cannot be encoded, which
/// would be silent heap corruption — fail loudly at the moment it happens
/// instead.
#[inline]
pub fn assert_region_encodable(base: usize, end: usize) {
    if !cratonvm_types::narrow_oop::narrow_oops_enabled() || base == 0 || end <= base {
        return;
    }
    let lo = cratonvm_types::narrow_oop::narrow_base();
    let hi = cratonvm_types::narrow_oop::narrow_limit();
    if (base as u64) <= lo || (end as u64) > hi {
        panic!(
            "compressed oops: heap region {base:#x}..{end:#x} moved outside the \
             narrow-oop window {lo:#x}..{hi:#x} — references into it cannot be encoded"
        );
    }
}
