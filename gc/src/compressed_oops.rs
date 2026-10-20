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
//! vs 1763 MB narrow, wall time indistinguishable. The reason was the then
//! 32 B `ObjectHeader` (16 B since 2026-08-07; HotSpot's is 12): halving the
//! reference *fields* of objects whose header already cost 32 bytes moved a
//! minority of the bytes, so the header shrink was the item with the leverage
//! here, not this one (the figure has not been re-measured at 16 B). Full
//! derivation in `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.
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
//!   Since gc-common w4-e (2026-09-23) they, and the two `narrow_klass_*`
//!   fields only they read, compile only under
//!   `cfg(any(test, doc, feature = "lilliput-prototype"))` — the same gate as
//!   the `compact_header` prototype — so a default build cannot name them.
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
///
/// Part of the `lilliput-prototype` feature: not a reachable mode.
#[cfg(any(test, doc, feature = "lilliput-prototype"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct NarrowKlass(u32);

#[cfg(any(test, doc, feature = "lilliput-prototype"))]
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
    #[cfg(any(test, doc, feature = "lilliput-prototype"))]
    narrow_klass_base: u64,
    /// Shift for compressed class pointers.
    #[cfg(any(test, doc, feature = "lilliput-prototype"))]
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
            #[cfg(any(test, doc, feature = "lilliput-prototype"))]
            narrow_klass_base: heap_base,
            #[cfg(any(test, doc, feature = "lilliput-prototype"))]
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
            #[cfg(any(test, doc, feature = "lilliput-prototype"))]
            narrow_klass_base: 0,
            #[cfg(any(test, doc, feature = "lilliput-prototype"))]
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
    #[cfg(any(test, doc, feature = "lilliput-prototype"))]
    #[inline]
    pub fn encode_klass(&self, addr: u64) -> NarrowKlass {
        if addr == 0 {
            return NarrowKlass::NULL;
        }
        NarrowKlass(((addr - self.narrow_klass_base) >> self.narrow_klass_shift) as u32)
    }

    /// Decode a [`NarrowKlass`] back to a 64-bit address.
    #[cfg(any(test, doc, feature = "lilliput-prototype"))]
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
                // `addr == base` is NOT encodable, and the `<` this replaces
                // said it was. `base` encodes to the narrow value 0, which this
                // module reserves for null — `encode` returns `CompressedOop`
                // 0 for it and `decode` turns that back into address 0, so an
                // object sitting exactly on the base would be silently read
                // back as a null reference. That is the one failure the
                // encodability guard exists to stop, and it is the failure that
                // leaves no trace: a truncated oop faults, a nulled one does
                // not. `enable_for_live_heap` puts the base a whole
                // `NARROW_OOP_HEADROOM` below the lowest arena precisely so no
                // live object can land there, and
                // `cratonvm_types::narrow_oop::is_encodable` — the mechanism
                // side of the same question — already spells the test
                // `addr <= base`. The two must not disagree about a boundary.
                if addr <= self.base {
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
    /// `cratonvm_types::ObjectHeader` is **16 bytes** (`HEADER_SIZE`; it was 32
    /// until the 2026-08-06/07 shrink, which is what the table below used to
    /// assume -- gc-common w3-e recomputed it), and the array length lives in
    /// the header, so element data starts at `ARRAY_DATA_OFFSET` = 16. Worked
    /// from the actual layouts (compact body, 8-byte object alignment):
    ///
    /// | object | wide | narrow | saving |
    /// |---|---|---|---|
    /// | `java.lang.Integer` (autoboxed ⇒ legacy 16-byte cell) | 32 | 32 | **0 %** |
    /// | `java.lang.String` `{byte[] value, byte coder, int hash}` | 32 | 32 | **0 %** — 13 B and 9 B both round to 16 |
    /// | `ArrayList` `{Object[], int, int}` | 32 | 32 | **0 %** — same rounding |
    /// | binary-tree node `{left, right}` | 32 | 24 | 25 % |
    /// | `HashMap.Node` `{int hash, K, V, next}` | 48 | 32 | 33 % |
    /// | `Object[16]` | 144 | 80 | 44 % |
    /// | `Object[n]`, large `n` | `16+8n` | `16+4n` | → 50 % |
    ///
    /// The pattern: **reference arrays are where compressed oops pay**, and
    /// they pay a lot. Reference-dense small objects pay 25-33 %. Boxed
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
///
/// Part of the `lilliput-prototype` feature: nothing stores narrow oops this
/// way (narrow reference SLOTS are `cratonvm_types::narrow_oop`).
#[cfg(any(test, doc, feature = "lilliput-prototype"))]
pub struct CompressedOopArray {
    data: Vec<u32>,
}

#[cfg(any(test, doc, feature = "lilliput-prototype"))]
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

    /// The base itself encodes to narrow 0, which is null — so it must be
    /// refused, not accepted. Accepting it made `encode(base)` a silent
    /// reference-to-null conversion that `decode` could not undo, and it
    /// disagreed with `cratonvm_types::narrow_oop::is_encodable`, which has
    /// always spelled this `addr <= base`.
    #[test]
    fn the_heap_base_itself_is_not_encodable_because_it_would_read_back_as_null() {
        let base = 0x1_0000_0000u64;
        let c = CompressedOops::new(base, 8 * GB);
        assert!(!c.is_encodable(base));
        // `encode(base)` is deliberately NOT exercised: the release fail-safe
        // returns `CompressedOop::NULL` for it, but the debug_assert above that
        // fail-safe fires first in a test build, and asserting on a panic would
        // be a test of the assertion rather than of the predicate.
        // The first encodable address is one object-alignment step above it.
        assert!(c.is_encodable(base + CompressedOops::OBJECT_ALIGNMENT));
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
    /// used to live here — CratonVM's header (16 bytes, 32 before 2026-08-06)
    /// makes a per-heap-size figure meaningless. Pins that it no longer varies
    /// with heap size.
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
        // gc-common w3-e: was a hard-coded 32, the pre-2026-08-06 header.
        const HEADER: usize = cratonvm_types::HEADER_SIZE;
        assert_eq!(HEADER, 16);
        assert_eq!(cratonvm_types::ARRAY_DATA_OFFSET, HEADER);
        fn round8(n: usize) -> usize {
            (n + 7) & !7
        }
        // (wide body bytes before rounding, narrow body bytes) -> (wide, narrow)
        fn total(wide_body: usize, narrow_body: usize) -> (usize, usize) {
            (HEADER + round8(wide_body), HEADER + round8(narrow_body))
        }

        // String {byte[] value, byte coder, int hash}: 8+4+1=13 vs 4+4+1=9.
        // Both round to 16 -- compressed oops save NOTHING here.
        assert_eq!(total(13, 9), (32, 32));

        // ArrayList {Object[] elementData, int size, int modCount}: same shape.
        assert_eq!(total(16, 12), (32, 32));

        // Binary-tree node {left, right}: 16 -> 8.
        assert_eq!(total(16, 8), (32, 24));

        // HashMap.Node {int hash, K key, V value, Node next}: 4+pad4+8+8+8 = 32
        // vs 4+4+4+4 = 16.
        assert_eq!(total(32, 16), (48, 32));

        // Reference arrays are the real win and are not eroded by rounding.
        assert_eq!((HEADER + 8 * 16, HEADER + 4 * 16), (144, 80));
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

// ---------------------------------------------------------------------------
// Admission: one reference width per PROCESS, checked for EVERY VM
// ---------------------------------------------------------------------------
//
// gc-common w8-f (`common-w7f-compressed-oops-second-vm-skips-the-fit-check`).
//
// The reference width is not a per-VM property and cannot be made one here:
// `cratonvm_types::narrow_oop::{ENABLED, BASE, SHIFT}` are process statics read
// on every reference access, and `ref_field_size()` decides the layout of every
// class any VM lays out. So a process that hosts several VMs (an embedder --
// `libcratonvm`, `cratonvm-embed` -- or the `vm` test binary) runs ALL of them
// at one width, and before this block nothing made the second VM agree to it:
//
// * narrow ON (VM A asked), VM B arrives: `enable_for_live_heap` answered A's
//   window from its one-shot latch without looking at B's heap, and a VM B that
//   did not ask never called it at all -- yet B's accesses were narrow;
// * narrow OFF with VM A live at 8 bytes, VM B asks: B's call flipped the
//   process to 4-byte slots under A's already-laid-out classes and live
//   objects.
//
// Both are silent heap corruption. `admit_heap` is the one door every VM goes
// through, whatever it asked for, and it decides under one lock. That width is
// the ONE piece of GC state this crate keeps per process on purpose: it is the
// mechanism's own granularity (see `cratonvm_types::narrow_oop`), not
// compatibility state, and making it per heap is the longer-term fix the
// known-issues page names.

/// What the VM being created asks of the narrow-oop gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NarrowOopRequest {
    /// `-XX:+UseCompressedOops` / `CRATONVM_COMPRESSED_OOPS=1` for THIS VM.
    pub wants_narrow: bool,
    /// Whether this VM's collector has been audited for 4-byte reference slots.
    /// Today only the generational collector has; G1 is unmigrated and ZGC is
    /// incompatible by construction (module doc, item 6).
    pub backend_audited: bool,
}

/// The answer [`admit_heap`] gives the VM being created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NarrowOopAdmission {
    /// 64-bit references. The VM did not ask for narrow oops and the process
    /// runs wide.
    Wide,
    /// The VM asked for narrow oops and runs with 64-bit references anyway, for
    /// the stated reason. Nothing else changes; the caller reports it.
    WideDeclined(String),
    /// Narrow oops at `(base, shift)`, and this VM's heap lies inside the
    /// window. The caller re-lays its loaded classes out at the narrow width
    /// (harmless when they already are). `fixed_by_this_vm` is `false` when an
    /// earlier VM in the process fixed the window and this one only joined it.
    Narrow {
        base: u64,
        shift: u8,
        fixed_by_this_vm: bool,
    },
    /// This VM cannot run in this process: the process runs narrow references
    /// and this heap cannot join them. The caller must refuse to create the VM
    /// -- there is no fallback, because the width is process-wide.
    Refuse(String),
}

/// Process-wide width decision. `narrow` mirrors [`ACTIVE`]; `wide_admitted`
/// records that some VM has already been created at 8 bytes, after which the
/// process can never switch to narrow (that VM's classes and objects are laid
/// out wide, and nothing tracks when it is gone).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WidthState {
    narrow: Option<(u64, u8)>,
    wide_admitted: bool,
}

/// Serialises every [`admit_heap`] call. This also closes the concurrent-init
/// residual `enable_for_live_heap` used to state: two VMs initialising on two
/// threads can no longer both pass the "not yet enabled" test and both publish
/// a geometry.
static WIDTH: parking_lot::Mutex<WidthState> = parking_lot::Mutex::new(WidthState {
    narrow: None,
    wide_admitted: false,
});

/// What [`plan`] tells [`admit_heap`] to do. Pure data, so the whole decision
/// is unit-testable with two synthetic heap geometries and without touching
/// the process-global narrow-oop statics (which a unit test must never flip;
/// see `cratonvm_types::narrow_oop::disable_for_test`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AdmissionPlan {
    /// Answer without changing the process width. `commits_wide` marks the
    /// process as having a wide VM.
    Answer {
        admission: NarrowOopAdmission,
        commits_wide: bool,
    },
    /// Switch narrow oops on at `(base, shift)` for the whole process.
    Enable { base: u64, shift: u8 },
}

const UNAUDITED_BACKEND: &str = "the selected GC backend has not been audited for 4-byte \
     reference slots (only the generational collector has); the heap geometry itself is \
     published by every backend -- this is the audit gate, not a missing base/shift";

/// Derive the narrow-oop window from one heap's reserved spans: `HeapBased`,
/// shift 3, base `NARROW_OOP_HEADROOM` below the lowest span, page-aligned
/// down. See [`enable_for_live_heap`] for why each of those.
///
/// GEOMETRY, NOT PERMISSION. The spans come from `crate::heap_geometry` or
/// `VmHeap::reserved_address_envelope`, never from `gen_heap::JIT_REGION_BOUNDS`:
/// that table's load-bearing job is "may an inline reference store skip the
/// write barrier", G1 and ZGC answer it by leaving it EMPTY, and reading it here
/// once made compressed oops "no live heap regions published" on two of the
/// three collectors. Whether a collector may run narrow is the separate
/// `backend_audited` question.
fn derive_geometry(spans: &[(u64, u64)]) -> Result<(u64, u8), String> {
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for (i, &(base, end)) in spans.iter().enumerate() {
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
        return Err("no live heap regions published by the selected GC backend".to_string());
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
    Ok((base, shift))
}

/// Does every span of a JOINING heap lie inside the window `(base, shift)` an
/// earlier VM fixed? The bounds are `assert_region_encodable`'s, which is the
/// check the heap runs again every time it republishes a region: strictly
/// above the base (the base encodes to the null narrow oop) and no further
/// than the exclusive limit.
fn check_fits(base: u64, shift: u8, spans: &[(u64, u64)]) -> Result<(), String> {
    let limit = base + ((u32::MAX as u64) << shift);
    if spans.is_empty() {
        return Err(format!(
            "this VM's heap published no reserved span, so it cannot be checked against \
             the process's narrow-oop window {base:#x}..{limit:#x}"
        ));
    }
    for (i, &(lo, hi)) in spans.iter().enumerate() {
        if lo % 8 != 0 {
            return Err(format!(
                "heap region {i} base {lo:#x} is not 8-byte aligned; shift-{shift} narrow \
                 oops would be misaligned"
            ));
        }
        if lo <= base || hi > limit {
            return Err(format!(
                "this VM's heap region {i} ({lo:#x}..{hi:#x}) lies outside the narrow-oop \
                 window {base:#x}..{limit:#x} an earlier VM in this process fixed; references \
                 into it cannot be encoded"
            ));
        }
    }
    Ok(())
}

/// The whole admission decision, as a pure function of the process state, the
/// joining heap's spans and what the VM asked for.
fn plan(state: WidthState, spans: &[(u64, u64)], req: NarrowOopRequest) -> AdmissionPlan {
    let answer = |admission, commits_wide| AdmissionPlan::Answer {
        admission,
        commits_wide,
    };
    if let Some((base, shift)) = state.narrow {
        // The process already runs 4-byte reference slots. A VM that cannot
        // run at that width cannot run here at all -- there is no per-VM
        // fallback to 8 bytes.
        let refuse = |why: String| {
            answer(
                NarrowOopAdmission::Refuse(format!(
                    "compressed oops are ON process-wide (base {base:#x}, shift {shift}, fixed \
                     by an earlier VM) and one process cannot run mixed reference widths: {why}"
                )),
                false,
            )
        };
        if !req.wants_narrow {
            return refuse(
                "this VM did not request -XX:+UseCompressedOops; start it with the flag (and the \
                 generational collector), or in a process of its own"
                    .to_string(),
            );
        }
        if !req.backend_audited {
            return refuse(UNAUDITED_BACKEND.to_string());
        }
        return match check_fits(base, shift, spans) {
            Ok(()) => answer(
                NarrowOopAdmission::Narrow {
                    base,
                    shift,
                    fixed_by_this_vm: false,
                },
                false,
            ),
            Err(why) => refuse(why),
        };
    }
    if !req.wants_narrow {
        return answer(NarrowOopAdmission::Wide, true);
    }
    if !req.backend_audited {
        return answer(
            NarrowOopAdmission::WideDeclined(UNAUDITED_BACKEND.to_string()),
            true,
        );
    }
    if state.wide_admitted {
        return answer(
            NarrowOopAdmission::WideDeclined(
                "an earlier VM in this process runs with 64-bit references, and the width is \
                 process-wide: compressed oops can only be fixed by the first VM a process \
                 creates"
                    .to_string(),
            ),
            true,
        );
    }
    match derive_geometry(spans) {
        Ok((base, shift)) => AdmissionPlan::Enable { base, shift },
        Err(why) => answer(NarrowOopAdmission::WideDeclined(why), true),
    }
}

/// Admit the heap of a VM being created into the process's reference width.
///
/// **Every VM calls this, whether or not it asked for compressed oops**, once,
/// at init: after its heap's backing stores exist and before its first
/// allocation. `spans` are THIS heap's reserved address spans -- pass
/// [`crate::vm_heap::VmHeap::reserved_address_envelope`], not
/// [`crate::heap_geometry::heap_spans`]: that table is process-global, and a
/// heap another VM built a moment later may have overwritten some of its slots.
///
/// See [`NarrowOopAdmission`] for what the caller must do with each answer. A
/// [`NarrowOopAdmission::Refuse`] means creating the VM would corrupt its heap
/// (or its neighbour's); the caller refuses creation.
pub fn admit_heap<I>(spans: I, req: NarrowOopRequest) -> NarrowOopAdmission
where
    I: IntoIterator<Item = (usize, usize)>,
{
    let spans: Vec<(u64, u64)> = spans
        .into_iter()
        .filter(|&(base, end)| base != 0 && end > base)
        .map(|(base, end)| (base as u64, end as u64))
        .collect();
    let mut state = WIDTH.lock();
    // A geometry fixed through `ACTIVE` without this lock (it cannot happen
    // any more, `enable_for_live_heap` routes here) must still be honoured.
    if state.narrow.is_none() {
        state.narrow = ACTIVE.get().map(|active| (active.base(), active.shift()));
    }
    match plan(*state, &spans, req) {
        AdmissionPlan::Answer {
            admission,
            commits_wide,
        } => {
            if commits_wide {
                state.wide_admitted = true;
            }
            admission
        }
        AdmissionPlan::Enable { base, shift } => {
            if !cratonvm_types::narrow_oop::enable(base, shift as usize) {
                state.wide_admitted = true;
                return NarrowOopAdmission::WideDeclined(
                    "cratonvm_types::narrow_oop::enable rejected the geometry".to_string(),
                );
            }
            let _ = ACTIVE.set(CompressedOops::new(
                base,
                CompressedOops::MAX_HEAP_FOR_COMPRESSED,
            ));
            state.narrow = Some((base, shift));
            warn_known_unsound();
            NarrowOopAdmission::Narrow {
                base,
                shift,
                fixed_by_this_vm: true,
            }
        }
    }
}

/// Fix the compressed-oop geometry from the live heap's published regions and
/// switch narrow oops on process-wide.
///
/// **Superseded by [`admit_heap`]** (gc-common w8-f), which every VM -- not
/// only one that asked for narrow oops -- must call with its OWN heap's spans.
/// This entry point stays for the one caller that predates it (the
/// compressed-oops block of `vm/src/vm/vm_init.rs`, reached only for a VM that
/// asked AND runs the generational collector); it is `admit_heap` with that
/// request and the process-global [`crate::heap_geometry`] spans.
///
/// **Mode: `HeapBased`, shift 3.** The base is
/// `lowest_region_base - NARROW_OOP_HEADROOM`, page-aligned down; a narrow oop
/// is `(addr - base) >> 3` and encoding `0` is reserved for null (the base sits
/// far below any object, so no live object can encode to zero). Shift 3 is
/// sound because every object starts on the 8-byte object grid, and it buys a
/// 32 GiB window — a shift of 0 would cap the encodable range at 4 GiB measured
/// from a base 8 GiB below the heap, i.e. nothing would be encodable at all.
///
/// Called at VM init: after the heap's backing stores exist (so their addresses
/// are known) and before the first object is allocated or the first class
/// layout is registered (so no object is ever read back under a different width
/// than it was written).
///
/// Returns the `(base, shift)` in force, or an error describing why this VM
/// runs with full 64-bit references.
///
/// "EXACTLY ONCE" is enforced, and it is the most dangerous thing in this
/// module to leave on trust: `cratonvm_types::narrow_oop::enable` stores
/// BASE/SHIFT/LIMIT unconditionally, so a second publish with a different base
/// would make every reference already written decode to a different address --
/// the process-global-geometry failure class `scripts/gc-flake-gate.sh` exists
/// for. A LATER call (a second VM) no longer answers the first VM's window
/// unchecked, which is what
/// `common-w7f-compressed-oops-second-vm-skips-the-fit-check` was: the joining
/// heap's spans are checked against the window first.
///
/// # Panics
///
/// When narrow oops are already on process-wide and the calling VM's heap does
/// not fit the window. There is no `Err` to return for that: an `Err` tells the
/// caller "running with 64-bit references", which would be false -- the width
/// is process-wide -- and the VM would then read its 8-byte slots through
/// 4-byte accessors. Refusing to create the VM is the only safe answer, and
/// `Vm::new` is infallible by signature (it refuses a `--jdk-only` run with no
/// JDK image the same way).
pub fn enable_for_live_heap() -> Result<(u64, u8), String> {
    match admit_heap(
        crate::heap_geometry::heap_spans(),
        NarrowOopRequest {
            wants_narrow: true,
            backend_audited: true,
        },
    ) {
        NarrowOopAdmission::Narrow { base, shift, .. } => Ok((base, shift)),
        NarrowOopAdmission::WideDeclined(why) => Err(why),
        // `wants_narrow` is true, so `plan` never answers plain `Wide`.
        NarrowOopAdmission::Wide => Err("narrow oops were not requested".to_string()),
        NarrowOopAdmission::Refuse(why) => panic!("[cratonvm] cannot create this VM: {why}"),
    }
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
         [cratonvm]   3. It was measured at 4.7% of peak RSS on this VM (with the \
         then-32-byte ObjectHeader; the header is {} bytes now and the figure \
         has not been re-measured), not the 20-30% narrow oops buy on HotSpot.\n\
         [cratonvm] See gc/src/compressed_oops.rs for the full list.",
        cratonvm_types::HEADER_SIZE,
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

/// gc-common w8-f: the admission decision for a process that hosts several VMs
/// (`common-w7f-compressed-oops-second-vm-skips-the-fit-check`). Every test
/// drives the pure [`plan`] with two synthetic heap geometries: nothing here
/// touches `cratonvm_types::narrow_oop`, which is process-global and must never
/// be flipped from a unit test (its `disable_for_test` doc). The one test that
/// does run the global path, `admit_heap` end to end, is its own binary:
/// `gc/tests/narrow_oop_second_vm_admission.rs`.
#[cfg(test)]
mod admission_tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    /// VM A's heap: 256 MiB at 256 GiB, far above the headroom.
    const A: (u64, u64) = (256 * GB, 256 * GB + 256 * MB);

    const NARROW_GEN: NarrowOopRequest = NarrowOopRequest {
        wants_narrow: true,
        backend_audited: true,
    };
    const WIDE_GEN: NarrowOopRequest = NarrowOopRequest {
        wants_narrow: false,
        backend_audited: true,
    };
    const NARROW_UNAUDITED: NarrowOopRequest = NarrowOopRequest {
        wants_narrow: true,
        backend_audited: false,
    };
    const WIDE_UNAUDITED: NarrowOopRequest = NarrowOopRequest {
        wants_narrow: false,
        backend_audited: false,
    };

    /// The state after VM A fixed its window: returns it and the window.
    fn after_first_vm() -> (WidthState, u64, u8) {
        let AdmissionPlan::Enable { base, shift } = plan(WidthState::default(), &[A], NARROW_GEN)
        else {
            panic!("a fitting first heap that asked must enable narrow oops");
        };
        (
            WidthState {
                narrow: Some((base, shift)),
                wide_admitted: false,
            },
            base,
            shift,
        )
    }

    fn refused(p: &AdmissionPlan) -> bool {
        matches!(
            p,
            AdmissionPlan::Answer {
                admission: NarrowOopAdmission::Refuse(_),
                commits_wide: false,
            }
        )
    }

    #[test]
    fn the_first_vm_fixes_the_window_below_its_own_heap() {
        let (_, base, shift) = after_first_vm();
        assert_eq!(shift, 3);
        assert_eq!(base, (A.0 - NARROW_OOP_HEADROOM) & !0xfff);
        assert!(check_fits(base, shift, &[A]).is_ok());
    }

    #[test]
    fn a_second_vm_whose_heap_fits_joins_the_window_without_refixing_it() {
        let (state, base, shift) = after_first_vm();
        // VM B: another 512 MiB, 1 GiB above A -- inside the 32 GiB window.
        let b = (A.1 + GB, A.1 + GB + 512 * MB);
        assert_eq!(
            plan(state, &[b], NARROW_GEN),
            AdmissionPlan::Answer {
                admission: NarrowOopAdmission::Narrow {
                    base,
                    shift,
                    fixed_by_this_vm: false,
                },
                commits_wide: false,
            }
        );
    }

    #[test]
    fn a_second_vm_whose_heap_is_outside_the_window_is_refused_not_handed_it() {
        let (state, base, shift) = after_first_vm();
        let limit = base + ((u32::MAX as u64) << shift);
        // Above the limit (the w7-f scenario: an unrelated mapping 100 GiB up).
        let above = (A.0 + 100 * GB, A.0 + 100 * GB + 64 * MB);
        assert!(above.1 > limit);
        assert!(refused(&plan(state, &[above], NARROW_GEN)));
        // Straddling the limit: its base fits, its end does not.
        let straddle = (limit - 16 * MB, limit + 16 * MB);
        assert!(refused(&plan(state, &[straddle], NARROW_GEN)));
        // Below the base, and exactly ON it (the base encodes to null).
        let below = (base - GB, base - GB + 64 * MB);
        assert!(refused(&plan(state, &[below], NARROW_GEN)));
        let on_base = (base, base + 64 * MB);
        assert!(refused(&plan(state, &[on_base], NARROW_GEN)));
        // One good span does not excuse a bad one.
        let good = (A.1 + GB, A.1 + GB + 64 * MB);
        assert!(refused(&plan(state, &[good, above], NARROW_GEN)));
        // A heap that published nothing cannot be checked, so it cannot join.
        assert!(refused(&plan(state, &[], NARROW_GEN)));
    }

    #[test]
    fn a_second_vm_that_did_not_ask_or_cannot_run_narrow_is_refused() {
        let (state, _, _) = after_first_vm();
        let inside = (A.1 + GB, A.1 + GB + 64 * MB);
        // Did not ask (the default flags; the w7-f failure scenario's VM B).
        assert!(refused(&plan(state, &[inside], WIDE_GEN)));
        // G1 / ZGC, asked or not: not audited for 4-byte slots.
        assert!(refused(&plan(state, &[inside], NARROW_UNAUDITED)));
        assert!(refused(&plan(state, &[inside], WIDE_UNAUDITED)));
    }

    #[test]
    fn once_a_wide_vm_exists_a_later_vm_cannot_switch_the_process_to_narrow() {
        // VM A wide (default flags).
        let first = plan(WidthState::default(), &[A], WIDE_GEN);
        assert_eq!(
            first,
            AdmissionPlan::Answer {
                admission: NarrowOopAdmission::Wide,
                commits_wide: true,
            }
        );
        let state = WidthState {
            narrow: None,
            wide_admitted: true,
        };
        // VM B asks, with a heap that would fit on its own: declined, runs wide.
        let b = (A.1 + GB, A.1 + GB + 64 * MB);
        match plan(state, &[b], NARROW_GEN) {
            AdmissionPlan::Answer {
                admission: NarrowOopAdmission::WideDeclined(why),
                commits_wide: true,
            } => assert!(why.contains("earlier VM"), "{why}"),
            other => panic!("a later VM must not flip the width under a wide VM: {other:?}"),
        }
    }

    #[test]
    fn a_first_vm_that_cannot_run_narrow_falls_back_to_wide_and_commits_the_process() {
        // Unaudited backend.
        assert!(matches!(
            plan(WidthState::default(), &[A], NARROW_UNAUDITED),
            AdmissionPlan::Answer {
                admission: NarrowOopAdmission::WideDeclined(_),
                commits_wide: true,
            }
        ));
        // Unusable geometry: below the headroom, no spans, misaligned.
        for spans in [
            vec![(4 * GB, 4 * GB + 64 * MB)],
            vec![],
            vec![(A.0 + 4, A.1)],
        ] {
            assert!(
                matches!(
                    plan(WidthState::default(), &spans, NARROW_GEN),
                    AdmissionPlan::Answer {
                        admission: NarrowOopAdmission::WideDeclined(_),
                        commits_wide: true,
                    }
                ),
                "{spans:?}"
            );
        }
    }

    /// `check_fits` and the runtime tripwire `assert_region_encodable` must
    /// agree about the window's two edges, or a heap admitted here panics on
    /// its first region republish (or one refused here would have been fine).
    #[test]
    fn the_fit_check_uses_the_runtime_tripwire_bounds() {
        let (_, base, shift) = after_first_vm();
        let limit = base + ((u32::MAX as u64) << shift);
        assert!(check_fits(base, shift, &[(base + 8, limit)]).is_ok());
        assert!(check_fits(base, shift, &[(base, limit)]).is_err());
        assert!(check_fits(base, shift, &[(base + 8, limit + 8)]).is_err());
    }
}
