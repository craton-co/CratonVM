// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Compressed object pointers (CompressedOops) for heaps under 32 GB.
//!
//! When the Java heap fits in 32 GB, every object reference can be stored as a
//! 32-bit value instead of a full 64-bit pointer. This cuts reference footprint
//! in half and typically saves 20-30 % of total heap for reference-heavy
//! workloads.
//!
//! Three modes are supported:
//!
//! | Mode | Heap base | Shift | Encoding |
//! |--------------|-----------|-------|------------------------------|
//! | Uncompressed | n/a | n/a | raw 64-bit pointer |
//! | ZeroBased | 0 | 0 / 3 | `addr >> shift` |
//! | HeapBased | > 0 | 0 / 3 | `(addr - base) >> shift` |
//!
//! # Wiring status
//!
//! This module is wired into the live heap behind the **opt-in**
//! `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1` gate, which is
//! **off by default**. See [`enable_for_live_heap`] for the geometry the VM
//! fixes at init, and `cratonvm_types::narrow_oop` for the mechanism the hot
//! paths use (that module owns the base/shift pair because the compact field
//! accessors live below this crate in the dependency graph).
//!
//! What is narrowed when the gate is on: **reference instance fields** and
//! **reference array elements**, from 8 bytes to 4. The class pointer in the
//! object header is deliberately NOT narrowed — `ObjectHeader::class_id` is
//! already a `u32`, so `encode_klass`/`decode_klass` remain unwired and would
//! buy nothing here.
//!
//! Known gap: the JIT's inline compact-field fast paths are *disabled* while
//! compressed oops are active (they bake an 8-byte reference load/store);
//! `getfield`/`putfield` fall back to the always-correct helpers. That costs
//! throughput and is the main reason the gate stays off by default.

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

    /// Estimate percent savings for a given heap size.
    ///
    /// * Heaps within 32 GB: ~20-30 % savings depending on reference density.
    ///   We use 25 % as a reasonable default.
    /// * Larger heaps: no savings (compressed oops disabled).
    pub fn estimate_savings_percent(heap_size: u64) -> f64 {
        if heap_size > Self::MAX_HEAP_FOR_COMPRESSED {
            return 0.0;
        }
        // Smaller heaps have higher reference density → higher savings.
        if heap_size <= Self::MAX_HEAP_ZERO_SHIFT {
            30.0
        } else {
            25.0
        }
    }
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

    #[test]
    fn savings_small_heap() {
        assert_eq!(CompressedOops::estimate_savings_percent(2 * GB), 30.0);
    }

    #[test]
    fn savings_medium_heap() {
        assert_eq!(CompressedOops::estimate_savings_percent(16 * GB), 25.0);
    }

    #[test]
    fn savings_large_heap() {
        assert_eq!(CompressedOops::estimate_savings_percent(64 * GB), 0.0);
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
        let base = crate::gen_heap::JIT_REGION_BOUNDS.words[i * 2].load(AtomicOrdering::Acquire)
            as u64;
        let end = crate::gen_heap::JIT_REGION_BOUNDS.words[i * 2 + 1]
            .load(AtomicOrdering::Acquire) as u64;
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
    Ok((base, shift))
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
