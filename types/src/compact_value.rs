// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Compact 8-byte tagged value representation for JVM operand stacks.
//!
//! `CompactValue` uses a NaN-boxing scheme to represent all JVM value types
//! in exactly 8 bytes (one `u64`), halving the memory footprint compared to
//! the 16-byte `Value` enum.
//!
//! # Encoding
//!
//! **Double**: stored as raw IEEE 754 f64 bits. A double whose bits collide
//! with the tagged encoding space (a NEGATIVE quiet NaN with mantissa bit 50
//! set — `bits & 0xFFFC_0000_0000_0000 == 0xFFFC_0000_0000_0000`) cannot be
//! told apart from a tagged slot by the bits alone, so there are two
//! constructors and the caller picks by what it can prove:
//!
//! * [`CompactValue::double_raw`] stores the bits verbatim and is for a slot
//!   whose store also writes an out-of-band "this is a double" mark
//!   (`ValueStack::kinds`, `Frame::local_kinds`, `SlotType::Double`). This is
//!   what every operand-stack, local-variable and argument store uses, so a
//!   NaN payload survives `f2d` / `dstore` / `dload` / a call boundary
//!   bit-exact, the way HotSpot's does.
//! * [`CompactValue::double`] canonicalizes the colliding patterns to the
//!   canonical quiet NaN and counts the loss
//!   ([`nan_payload_collapse_count`]). It is the context-free constructor:
//!   correct anywhere, lossy only for the collision set.
//!
//! **Long**: stored as raw i64 bits (reinterpreted as u64). Since Long and
//! Double both use all 64 bits, they cannot be distinguished by bit pattern
//! alone. The caller must know from JVM instruction context whether an
//! untagged value is a Long or a Double. `tag()` returns `CompactTag::Double`
//! for any untagged value; use `as_long_unchecked()` when the context
//! indicates a Long.
//!
//! **All other types** (Int, Float, Object, Null, Uninitialized,
//! ReturnAddress): encoded as a quiet NaN with a marker bit, a 3-bit sub-tag,
//! and a 47-bit payload:
//!
//! ```text
//! Bits 63    : 1 (sign, always set for tagged values)
//! Bits 62-52 : all 1s (NaN exponent)
//! Bit  51    : 1 (quiet NaN)
//! Bit  50    : 1 (our tag marker — distinguishes from canonical NaN)
//! Bits 49-47 : 3-bit type sub-tag
//! Bits 46-0  : 47-bit payload
//! ```

use crate::{ObjectRef, Value};
use std::fmt;

// ---------------------------------------------------------------------------
// CompactValueError — fallible-API failures
// ---------------------------------------------------------------------------

/// Failure modes for the checked `CompactValue` mutators / constructors.
///
/// Currently emitted only by [`CompactValue::update_object_ptr`] when the
/// supplied pointer would not fit in the 47-bit payload — but exposed as a
/// public enum so future fallible variants can extend it without churning
/// call-sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactValueError {
    /// `update_object_ptr` was called with a pointer that has bits set above
    /// bit 46 (i.e. it doesn't fit in the 47-bit NaN-box payload).
    ///
    /// The original slot is left unchanged so callers can choose to ignore,
    /// retry with a checked constructor, or fail upwards.
    PointerOutOfRange { ptr: u64 },

    /// `update_object_ptr` was called with a pointer that fits in the 47-bit
    /// payload but cannot be a VM heap object pointer by the same
    /// context-free plausibility rules enforced by `CompactValue::object`.
    InvalidObjectPointer { ptr: u64 },
}

impl fmt::Display for CompactValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompactValueError::PointerOutOfRange { ptr } => write!(
                f,
                "CompactValue: pointer {ptr:#x} exceeds 47-bit address space",
            ),
            CompactValueError::InvalidObjectPointer { ptr } => write!(
                f,
                "CompactValue: pointer {ptr:#x} is not a plausible heap object pointer",
            ),
        }
    }
}

impl std::error::Error for CompactValueError {}

// ---------------------------------------------------------------------------
// NaN-boxing constants
// ---------------------------------------------------------------------------
//
// The NaN-boxing scheme below assumes a 64-bit address space where user-mode
// object pointers fit within the lower 47 bits.  This holds on x86-64
// (canonical lower-half) and AArch64 (typically 39/42/48-bit user VA).

// Refuse to build the NaN-boxed CompactValue on 32-bit targets: `u64` long
// bits cannot be stored as a `usize` round-trip, and the address-space
// assumptions below are not met.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("CompactValue NaN-boxing requires a 64-bit target pointer width");

// The 47-bit-payload pointer assumption (see SUBTAG_SHIFT / PAYLOAD_MASK below)
// is only verified for x86-64 and AArch64. A 64-bit target that is neither
// (e.g. riscv64) would pass the `target_pointer_width = "64"` gate above while
// using an unaudited address-space layout, so reject it explicitly here rather
// than silently miscompiling.
#[cfg(all(
    target_pointer_width = "64",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
compile_error!(
    "CompactValue NaN-boxing's 47-bit pointer assumption is only verified for \
     x86_64 and aarch64; this 64-bit target is unsupported"
);

/// Mask covering bits 63 + 62-50 (sign + exponent + quiet + marker).
/// When all these bits are set, the value is a tagged non-double.
const NANBOX_BITS: u64 = 0xFFFC_0000_0000_0000;
// bit 63 = 1, bits 62-52 = all 1, bit 51 = 1, bit 50 = 1

/// Canonical quiet NaN used when a stored f64 happens to collide with our
/// tagged encoding space.  This is the standard hardware quiet NaN
/// (sign=0, exponent all-1, bit 51=1, rest 0).
const CANONICAL_NAN: u64 = 0x7FF8_0000_0000_0000;

/// Shift amount: sub-tag starts at bit 47.
///
/// Only x86-64 / AArch64 reach this point (the `compile_error!` above rejects
/// every other 64-bit target), and on both the user-mode address space fits in
/// 47 bits, so the value is unconditional.
const SUBTAG_SHIFT: u32 = 47;

/// Mask for the 47-bit payload (bits 46-0).
///
/// Only x86-64 / AArch64 reach this point (the `compile_error!` above rejects
/// every other 64-bit target), where user-mode object pointers are known to
/// fit in 47 bits.  See [`CompactValue::try_from_pointer`] for a checked
/// constructor that returns `None` when this assumption is violated (e.g.
/// AArch64 LVA 52-bit VA, x86-64 5-level paging 57-bit VA, `mmap(MAP_FIXED)`
/// above `0x0000_7FFF_FFFF_FFFF`).
const PAYLOAD_MASK: u64 = (1u64 << 47) - 1;

/// Mask for the 3-bit sub-tag (bits 49-47) after the value has been confirmed
/// as NaN-tagged.
const SUBTAG_MASK: u64 = 0x7; // applied after shifting right by SUBTAG_SHIFT

/// Lowest address a real heap object can occupy.
///
/// Context-free hardening for the HIGH long↔object type-confusion finding: a
/// `SUB_OBJECT` slot whose 47-bit payload lands in the platform null-guard page
/// (`[0, NULL_GUARD_PAGE)`) cannot be a live heap reference. Every supported
/// target reserves the first page as unmapped — on x86-64 / AArch64 Linux the
/// default `mmap_min_addr` is 64 KiB and the lowest page is never returned by
/// the VM's allocator — so an aligned, non-null payload below this bound is
/// provably a primitive `long` whose verbatim bits (see [`CompactValue::long`])
/// collided into the object sub-tag, not a fabricated-but-plausible pointer.
///
/// Sized at one 4 KiB page (the conservative minimum across x86-64 4 KiB and
/// AArch64 4/16/64 KiB pages) so it never rejects a genuine object: the
/// allocator's arenas always sit far above it. This is deliberately *cheap and
/// conservative* — it shrinks, but does not close, the unchecked fabrication
/// window (a long whose low 47 bits alias a live arena address still decodes as
/// an object); the only complete defense remains the live-heap predicate in
/// [`CompactValue::to_value_checked`] / [`CompactValue::is_object_checked`].
const NULL_GUARD_PAGE: u64 = 0x1000; // 4 KiB

/// Context-free "could this `SUB_OBJECT` payload be a real heap reference?"
/// filter, shared by every unchecked decode of a `SUB_OBJECT` slot
/// ([`CompactValue::to_value`] and [`CompactValue::to_value_checked`]) so they
/// can never drift apart.
///
/// Returns `true` only for a payload that a genuine [`CompactValue::object`]
/// could have produced: **non-null**, **8-byte aligned**, and **at or above the
/// [`NULL_GUARD_PAGE`]**. A payload failing any of these is provably a
/// primitive `long` whose verbatim bits collided into the object sub-tag, so
/// the caller degrades it to `Value::Long` (counting the reclassification via
/// [`note_object_degradation`]).
///
/// HARD-AUDIT: this is a *necessary but not sufficient* gate. It rejects the
/// large, cheaply-detectable class of impossible references (null, unaligned,
/// null-page) at zero cost, but a long whose low 47 bits happen to form an
/// aligned, above-guard-page address is still indistinguishable from a real
/// pointer here. Any caller that does not already know — from JVM type context
/// — that the slot is a reference MUST additionally validate the payload
/// against the live heap (see [`CompactValue::to_value_checked`] /
/// [`CompactValue::is_object_checked`]). Do not treat a `true` from this helper
/// as proof of a live object.
#[inline(always)]
fn object_payload_is_plausible(payload: u64) -> bool {
    crate::plausible_heap_pointer(payload)
}

// Sub-tag values (3 bits)
const SUB_INT: u64 = 0;
const SUB_FLOAT: u64 = 1;
const SUB_OBJECT: u64 = 2;
const SUB_NULL: u64 = 3;
const SUB_UNINIT: u64 = 4;
const SUB_RETADDR: u64 = 5;
const SUB_LONG_LO: u64 = 6; // lower 47 bits of a long
const SUB_LONG_HI: u64 = 7; // upper 17 bits of a long (stored in payload bits 16-0)

// ---------------------------------------------------------------------------
// CompactTag — the logical type tag
// ---------------------------------------------------------------------------

/// Type tag extracted from a `CompactValue`.
///
/// `Double` and `Long` both use untagged (raw 64-bit) storage and cannot be
/// distinguished by inspecting a single `CompactValue` alone.  `tag()` returns
/// `Double` for any untagged value.  When the JVM execution context indicates
/// a Long, use `as_long_unchecked()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompactTag {
    Int,
    Long,
    Float,
    Double,
    Object,
    Null,
    Uninitialized,
    ReturnAddress,
}

// ---------------------------------------------------------------------------
// CompactValue
// ---------------------------------------------------------------------------

/// An 8-byte compact JVM value using NaN-boxing.
///
/// See module-level documentation for the encoding scheme.
///
/// `repr(transparent)` over `u64` guarantees `Vec<CompactValue>` and
/// `Vec<u64>` share identical layout, enabling zero-copy transmute between
/// the operand-stack's compact slot storage and pool `Vec<u64>` buffers.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct CompactValue(u64);

// Ensure the struct is exactly 8 bytes.
const _: () = assert!(std::mem::size_of::<CompactValue>() == 8);
// Ensure alignment matches u64 so Vec<CompactValue> / Vec<u64> are interchangeable.
const _: () = assert!(std::mem::align_of::<CompactValue>() == std::mem::align_of::<u64>());

/// Helper: build a NaN-tagged value from a sub-tag and payload.
#[inline(always)]
const fn make_tagged(sub: u64, payload: u64) -> u64 {
    debug_assert!(sub <= 7, "sub-tag out of range");
    debug_assert!(payload <= PAYLOAD_MASK, "payload too large for 47 bits");
    NANBOX_BITS | (sub << SUBTAG_SHIFT) | (payload & PAYLOAD_MASK)
}

#[inline(always)]
fn is_nan_tagged(v: u64) -> bool {
    (v & NANBOX_BITS) == NANBOX_BITS
}

// ---------------------------------------------------------------------------
// Silent-degradation observability (HIGH NaN-box long↔object audit)
// ---------------------------------------------------------------------------
//
// Several decode paths "degrade" a slot whose bit pattern *looks* like a
// SUB_OBJECT reference but cannot be a real one (null payload, unaligned
// payload, or — via the checked decoders — a payload that fails live-heap
// validation) back to a primitive `Value::Long`. Historically those
// degradations were guarded only by `debug_assert!`, so in release builds
// they were completely invisible: a long whose bits collide with the
// SUB_OBJECT tag space would silently be reclassified with no trace.
//
// To make the danger countable in release without adding a hot logging path,
// every such degradation bumps this relaxed atomic counter. A `pub fn`
// accessor lets the GC / diagnostics layer poll it (e.g. to assert the count
// stays at zero in a fuzz corpus, or to surface "N long↔object collisions
// degraded" in a crash report). A relaxed increment is a single `lock xadd`
// with no ordering constraints — negligible on the cold degrade path and
// never touched on the hot well-formed-reference path.
//
// THIS MODULE IS THE CANONICAL SINK FOR THE WHOLE VM, not just for the
// interpreter. The same "refuse an implausible reference word rather than
// dereference it" decision is taken on three paths, and for a long time only
// this one was counted:
//
//   1. the NaN-box / SoA decoders in this crate               (Interpreter)
//   2. the JIT read helpers in vm/src/jit/helpers.rs          (Jit)
//   3. gc::heap::read_prim_element's reference arm            (ArrayElement)
//
// (2) and (3) grew private counters of their own precisely because
// `note_object_degradation` was `pub(crate)` and neither crate could reach it —
// which meant `object_degradation_count()` reported a reassuring 0 for the
// configuration where a GC root-coverage gap is MOST likely to fire (a JIT'd
// frame is the frame a deposited root snapshot misses). The sink is now `pub`
// and source-tagged: one total, plus a per-source breakdown so "jit: 17" is
// distinguishable from a harmless long↔object collision.
//
// THE ONE RULE A NEW SOURCE MUST NOT GET WRONG: an ordinary null is not a
// degradation. `plausible_heap_pointer(0)` is false, so on a raw-word path a
// null reference reaches the same cold arm as garbage; counting it puts the
// counter in the millions on a clean run. Raw-word callers must therefore call
// `note_ref_word_degradation`, which applies the filter for them. Callers
// holding a TAGGED slot (this file, `crate::value`) are exempt and must not
// filter: a null has its own tag there, so a zero payload under an object tag
// is an impossible encoding and counting it is the point.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Once;

/// The distinct code paths that can degrade a would-be reference, i.e. the
/// sources that feed the single canonical sink [`note_object_degradation_at`].
///
/// All three mean the same thing — *a reference-shaped word was refused rather
/// than dereferenced* — but they do NOT carry the same diagnostic weight, which
/// is why the sink keeps a per-source breakdown alongside the total:
///
/// * [`Interpreter`](Self::Interpreter) — the NaN-box / SoA decoders in this
///   crate (`CompactValue::to_value`, `to_value_checked`, `is_object_checked`,
///   `decode_by_descriptor`, `from_value`, and `crate::value::decode_value`).
///   Historically the only counted source, and the only one this file calls.
/// * [`Jit`](Self::Jit) — the compiled-code read helpers in
///   `vm/src/jit/helpers.rs` (`jit_getfield`, `jit_aaload`, `jit_getstatic`, …).
///   **Diagnostically special, do not fold it into an opaque total.** A JIT'd
///   frame is exactly the frame a deposited root snapshot can miss, so the GC
///   root-coverage gap fires here first; a total that says "17" tells you
///   nothing, a breakdown that says "jit: 17" names the configuration.
/// * [`ArrayElement`](Self::ArrayElement) — `gc::heap::read_prim_element`'s
///   reference arm, shared by all three collectors (Generational, G1, ZGC).
///
/// Adding a variant requires widening [`DEGRADATION_COUNTS`] and
/// [`FIRST_DEGRADATION_DIAG`]; the `const` assertion below and the exhaustive
/// matches in [`Self::index`] / [`Self::name`] make forgetting a compile error,
/// not a silently-dropped source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DegradationSource {
    /// This crate's NaN-box / SoA decode paths.
    Interpreter,
    /// The JIT read helpers (`vm::jit::helpers`).
    Jit,
    /// The array-element read path (`gc::heap::read_prim_element`).
    ArrayElement,
}

impl DegradationSource {
    /// Number of variants — the width of every per-source table below.
    pub const COUNT: usize = 3;

    /// Every variant in table order, for callers rendering the breakdown.
    pub const ALL: [DegradationSource; Self::COUNT] = [
        DegradationSource::Interpreter,
        DegradationSource::Jit,
        DegradationSource::ArrayElement,
    ];

    /// Index into the per-source counter table.
    #[inline(always)]
    pub const fn index(self) -> usize {
        match self {
            DegradationSource::Interpreter => 0,
            DegradationSource::Jit => 1,
            DegradationSource::ArrayElement => 2,
        }
    }

    /// Short, stable, machine-greppable name used by the one-shot diagnostic
    /// and by any caller rendering [`object_degradation_breakdown`].
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            DegradationSource::Interpreter => "interpreter",
            DegradationSource::Jit => "jit",
            DegradationSource::ArrayElement => "array-element",
        }
    }
}

/// Guards the one-time-per-process-**per-source** diagnostic emitted by
/// [`note_object_degradation_at`] when a source records its *first*
/// degradation. A `Once` keeps each source to a single line no matter how many
/// subsequent events its counter records — surfacing the danger early without
/// spamming a hot crash log.
///
/// **One line per source, not one per process.** The three sources are three
/// independent failure stories, and the interpreter's is by far the most likely
/// to fire first *and* the least alarming (a primitive long colliding with the
/// tag space is harmless). A single process-wide `Once` would let one benign
/// interpreter collision permanently mute the JIT line — silencing precisely
/// the signal the JIT counter was added to surface. The bound is
/// [`DegradationSource::COUNT`] lines per process (3 today), which is not spam.
static FIRST_DEGRADATION_DIAG: [Once; DegradationSource::COUNT] =
    [Once::new(), Once::new(), Once::new()];

/// Per-source count of reference-shaped slots that were degraded rather than
/// dereferenced. Indexed by [`DegradationSource::index`].
///
/// The process-wide total reported by [`object_degradation_count`] is *defined*
/// as the sum of this table rather than kept as a fourth atomic: a separate
/// total would be a second thing to keep in step with the breakdown, and any
/// skew between them would be indistinguishable from a real miscount.
static DEGRADATION_COUNTS: [AtomicU64; DegradationSource::COUNT] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

// Widening `DegradationSource` without widening the two tables above would
// silently drop a source (index out of range at runtime is the *good* case);
// make it a compile error instead.
const _: () = assert!(
    DegradationSource::COUNT == 3,
    "DEGRADATION_COUNTS / FIRST_DEGRADATION_DIAG must be widened alongside DegradationSource"
);

/// Read the running **total** of degraded reference-shaped slots across every
/// [`DegradationSource`].
///
/// The counter is incremented (relaxed) every time one of the registered
/// sources refuses a reference-shaped word: this crate's decode paths
/// ([`CompactValue::to_value`], [`CompactValue::to_value_checked`],
/// [`CompactValue::is_object_checked`], [`CompactValue::decode_by_descriptor`],
/// [`CompactValue::from_value`] and the SoA [`crate::decode_value`]), the JIT
/// read helpers, and the array-element read path.
///
/// # What "zero is the only good value" does and does not mean
///
/// A non-zero total is always worth investigating, but it is not by itself a
/// bug: an `Interpreter` count can be a primitive `long` whose verbatim bits
/// collided into the `SUB_OBJECT` tag space, which is harmless and is exactly
/// what the degrade exists to handle. Read
/// [`object_degradation_breakdown`] before drawing a conclusion — a non-zero
/// [`DegradationSource::Jit`] or [`DegradationSource::ArrayElement`] count is
/// the alarming one, because those sources see *untagged* reference words that
/// carry no long/object ambiguity, so a refusal there means a word that should
/// have been a live pointer was handed to Java as `null`.
///
/// # What is NOT counted: an ordinary null
///
/// A null reference is **not** a degradation and must never reach this counter,
/// because no reference was lost — there was none to lose. This is a real trap:
/// [`crate::plausible_heap_pointer(0)`](crate::plausible_heap_pointer) is
/// `false` (it requires `raw >= 0x1000`), so on any path that inspects a *raw,
/// untagged* reference word an ordinary null falls into the same
/// "implausible" arm as genuine garbage. Counting it would put this number in
/// the millions on a perfectly clean run and falsify the contract above on
/// first use. Two sibling counters had to rediscover this independently, so the
/// rule now lives in the sink: raw-word callers must use
/// [`note_ref_word_degradation`], which applies the filter for them. See that
/// function for why this crate's own six call sites are exempt.
///
/// Uses `Relaxed` ordering: the value is advisory and carries no
/// happens-before relationship with the slot it counts. Summing the per-source
/// table is likewise non-atomic — a concurrent reader can miss an increment
/// landing in a slot it has already read, so the total is a lower bound under
/// concurrency, never an over-count. Tests that need an exact figure serialise
/// on [`degrade_counter_test_lock`] and assert deltas.
#[inline]
pub fn object_degradation_count() -> u64 {
    let mut total: u64 = 0;
    let mut i: usize = 0;
    while i < DegradationSource::COUNT {
        total = total.wrapping_add(DEGRADATION_COUNTS[i].load(Ordering::Relaxed));
        i += 1;
    }
    total
}

/// Read the running count for a single [`DegradationSource`].
///
/// See [`object_degradation_count`] for the ordering and null-exclusion
/// contract, which is identical.
#[inline]
pub fn object_degradation_count_from(source: DegradationSource) -> u64 {
    DEGRADATION_COUNTS[source.index()].load(Ordering::Relaxed)
}

/// Snapshot every per-source count, indexed by [`DegradationSource::index`].
///
/// This is the accessor a crash report or `print_gc_summary` line should use:
/// `interpreter=N jit=N array-element=N` distinguishes a benign long↔object
/// collision from a live GC root-coverage failure, which a single total cannot.
/// Pair with [`DegradationSource::ALL`] / [`DegradationSource::name`] to render
/// it without hard-coding the order.
///
/// The snapshot is taken with three independent relaxed loads, so under
/// concurrency it is a set of per-slot lower bounds rather than a consistent
/// instant; that is adequate for diagnostics and avoids putting a lock on a
/// path that exists to observe a failure.
#[inline]
pub fn object_degradation_breakdown() -> [u64; DegradationSource::COUNT] {
    [
        DEGRADATION_COUNTS[0].load(Ordering::Relaxed),
        DEGRADATION_COUNTS[1].load(Ordering::Relaxed),
        DEGRADATION_COUNTS[2].load(Ordering::Relaxed),
    ]
}

/// Count of NaN payloads destroyed by the tag collision in
/// [`CompactValue::double`].
///
/// Separate from [`DEGRADATION_COUNTS`] on purpose: a degradation is a
/// reference-shaped word that was refused (a memory-safety event), this is a
/// double that lost its NaN payload (a fidelity event). Folding them would make
/// a benign number and an alarming one indistinguishable.
static NAN_PAYLOAD_COLLAPSES: AtomicU64 = AtomicU64::new(0);

/// One-shot diagnostic the first time a payload is lost in a process.
static FIRST_NAN_COLLAPSE_DIAG: Once = Once::new();

/// Record one NaN payload collapsed by the tag collision.
///
/// Called from the cold arm of [`CompactValue::double`], which is already
/// branch-predicted away for every ordinary double.
#[inline]
pub(crate) fn note_nan_payload_collapse() {
    NAN_PAYLOAD_COLLAPSES.fetch_add(1, Ordering::Relaxed);
    // One line per process, matching `emit_first_degradation_diag`'s shape and
    // gated the same way — by a `Once`, not by a flag, so a workload that hits
    // this says so without anyone having to know to ask. Deliberately worded as
    // the fidelity event it is: nothing here is unsafe, and a reader who greps
    // this line should not go hunting for a memory bug.
    FIRST_NAN_COLLAPSE_DIAG.call_once(|| {
        eprintln!(
            "CompactValue: first NaN payload collapsed by the tag collision. \
             The double is a negative quiet NaN with mantissa bit 50 set, which \
             is bit-for-bit the NaN-box tag pattern, so it is stored as the \
             canonical quiet NaN instead. It still IS NaN — isNaN, compare, \
             equals, hashCode and toString are all unaffected — but \
             doubleToRawLongBits reports 7ff8000000000000 where HotSpot reports \
             the original payload. Subsequent collapses are counted by \
             nan_payload_collapse_count() but not logged."
        );
    });
}

/// How many NaN payloads this process has lost to the tag collision.
///
/// The number a workload run should report. Zero means the encoding's known
/// lossy case was never reached, which is the answer the write-up wanted and
/// could not get.
#[must_use]
pub(crate) fn nan_payload_collapse_count() -> u64 {
    NAN_PAYLOAD_COLLAPSES.load(Ordering::Relaxed)
}

/// Reset the NaN-payload counter, returning the previous value.
///
/// TEST-ONLY, and `pub(crate)` since round 10 to say so in the type system:
/// its four callers are all in this file's own test module, which is why the
/// orphan-instrument gate read it as a public reporting entry point with no
/// reader. A reset is not a reading.
pub(crate) fn reset_nan_payload_collapse_count() -> u64 {
    NAN_PAYLOAD_COLLAPSES.swap(0, Ordering::Relaxed)
}

// (`reset_object_degradation_count()` was DELETED in round 10. Its own doc
// already said the counter is process-wide, that a reset destroys whatever
// window a concurrent observer had open, that callers should prefer a delta,
// and that "they now take `degrade_counter_test_lock` *and* assert deltas, and
// no longer call this". Nothing in the tree called it. See
// docs/feature-designs/real-jdk-mode.md.)

/// Record one degraded reference-shaped slot from
/// [`DegradationSource::Interpreter`].
///
/// Convenience alias for
/// `note_object_degradation_at(DegradationSource::Interpreter, "types::compact_value")`,
/// kept because it is the name this crate's six decode sites and
/// `crate::value::cold_decode_degraded_object_ptr` already call.
///
/// # This entry point does NOT filter null — and must not
///
/// Its callers hold a **tagged** slot, where "no reference here" has its own
/// encoding (`SUB_NULL` / `VTAG_NULL`, both produced by
/// [`CompactValue::null`] / `encode_value(Value::Object(None))`). A `SUB_OBJECT`
/// slot whose payload is `0` is therefore *not* a Java null — it is an
/// unconstructible encoding ([`CompactValue::object`] asserts `ptr != 0`,
/// [`CompactValue::try_from_pointer`] returns `None` for it), so it can only
/// have arrived as a colliding primitive long or as memory corruption, and
/// counting it is correct. Raw-word callers are in the opposite position and
/// must use [`note_ref_word_degradation`] instead.
#[cold]
#[inline]
pub fn note_object_degradation() {
    note_object_degradation_at(DegradationSource::Interpreter, "types::compact_value");
}

/// Record one degraded reference-shaped slot from `source`, without a site tag.
///
/// Use when the caller has no meaningful static site name to attach (the
/// array-element path has one cold arm and does not need one).
#[cold]
#[inline]
pub fn note_object_degradation_from(source: DegradationSource) {
    note_object_degradation_at(source, "");
}

/// **The single canonical sink for reference-degradation events**, shared by
/// the interpreter, the JIT and the array-element paths.
///
/// Bumps `source`'s slot in [`DEGRADATION_COUNTS`] (hence the total read by
/// [`object_degradation_count`]) and, the first time that *source* records an
/// event in this process, emits one advisory stderr line naming it. `site` is a
/// static caller tag (e.g. `"jit_aaload"`) used only in that one-shot line;
/// pass `""` when there is nothing useful to say.
///
/// Cold by construction: it is only reached once a decode has already decided
/// the word cannot be a live reference.
///
/// # Counts
///
/// A word that is reference-*shaped* (an object tag, or a non-zero pointer-sized
/// value in a reference slot) but provably cannot be a live heap pointer:
/// unaligned, inside the null-guard page, above the 47-bit address range, not
/// in the reference-provenance bitmap, or rejected by a caller-supplied
/// live-heap predicate.
///
/// # Does not count
///
/// * **An ordinary null.** See [`note_ref_word_degradation`].
/// * A slot correctly typed as a primitive, or a well-formed reference that
///   merely moved — neither is a degradation.
///
/// `Relaxed` on the increment: the counter is advisory and establishes no
/// happens-before relationship with the slot it describes, so any stronger
/// ordering would buy a guarantee no reader needs and put a fence on a path
/// that a stress run can take often.
#[cold]
#[inline]
pub(crate) fn note_object_degradation_at(source: DegradationSource, site: &'static str) {
    let prev = DEGRADATION_COUNTS[source.index()].fetch_add(1, Ordering::Relaxed);
    // The first event *from this source* (its slot transitioning 0 -> 1) is the
    // one worth shouting about. Subsequent events are tracked by the counter.
    if prev == 0 {
        emit_first_degradation_diag(source, site);
    }
}

/// Sink for callers holding a **raw, untagged reference word** — the JIT read
/// helpers and the array-element read path.
///
/// Returns `true` if the event was counted, `false` if `raw` was an ordinary
/// null and therefore ignored. The caller degrades to `null` either way; the
/// boolean exists so a caller can skip further reporting for the null case.
///
/// # Why this exists instead of "just call [`note_object_degradation_at`]"
///
/// [`crate::plausible_heap_pointer(0)`](crate::plausible_heap_pointer) is
/// **`false`** — it requires `raw >= 0x1000` — so on a raw-word path an
/// ordinary null reference lands in the very same "implausible pointer" arm as
/// genuine garbage. A caller that forwards that arm straight to the counter
/// counts every null `aaload` / `getfield` / `getstatic` in the program: the
/// counter reads in the millions on a clean run, its "non-zero means a live
/// reference was lost" contract is false on first use, and an atomic RMW lands
/// on the *common* path for null field reads rather than on a cold one.
///
/// A null is not a degradation: no reference was lost, because there was none
/// to lose. Two sibling counters (`vm::jit::helpers::JIT_REF_DEGRADATIONS` and
/// `gc::heap::REF_ELEMENT_DEGRADATIONS`) each had to discover this
/// independently, which is one time too many — so the rule is encoded here,
/// where it cannot be forgotten by the next path that needs a sink.
///
/// This crate's own six call sites deliberately do **not** route through here:
/// they hold tagged slots in which a null has a distinct encoding, so for them
/// a zero payload under an object tag is an impossible encoding and counting it
/// is correct. See [`note_object_degradation`].
#[inline]
pub fn note_ref_word_degradation(raw: u64, source: DegradationSource, site: &'static str) -> bool {
    // Kept out-of-#[cold] so this early-out stays a single predicted-taken
    // compare even if a caller inlines it onto a warm path.
    if raw == 0 {
        return false;
    }
    note_object_degradation_at(source, site);
    true
}

/// Test-only mutex serialising every test that exercises a **degrading**
/// decode path.
///
/// [`DEGRADATION_COUNTS`] is one process-wide table and `cargo test`
/// runs the whole crate's tests in one process, in parallel — so a test that
/// merely *triggers* a degradation perturbs any concurrently-running test that
/// *counts* them. Both kinds must hold this lock, and that includes the
/// degrading tests over in `crate::value` (`decode_value`'s `VTAG_OBJECT`
/// cold path feeds this same counter through `note_object_degradation`), which
/// is why the mutex lives here at module scope rather than inside
/// `compact_value::tests` where it started: a lock only half the degraders can
/// name is not a lock. Missing it is a flaky `assert_eq!(count, N)` under
/// load, not a deterministic failure.
#[cfg(test)]
pub(crate) fn degrade_counter_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Per-source first-degradation diagnostics are disabled.
fn degradation_diag_verbose() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| false)
}

/// Cold one-shot diagnostic for a source's first observed degradation.
///
/// Kept out-of-line and `#[cold]` so the branch in
/// [`note_object_degradation_at`] stays a single predicted-not-taken compare on
/// the (already cold) degrade path. The crate has no logging dependency, so
/// this writes one line to stderr behind that source's `Once` — at most
/// [`DegradationSource::COUNT`] lines per process, one per source. See
/// [`FIRST_DEGRADATION_DIAG`] for why the `Once` is per-source rather than
/// per-process.
///
/// This is purely advisory: a degradation is a deliberately-handled,
/// *recoverable* fallback (a SUB_OBJECT-patterned primitive long, or a
/// heap-denied slot, is reclassified to `Value::Long` / `false` instead of
/// being dereferenced as a fabricated pointer — see the [`to_value`] Safety
/// contract). It is therefore NOT an invariant violation, so this must not
/// `panic!`/`debug_assert!(false)`: doing so would turn a normal counted event
/// into a test/fuzz abort and contradict the observability counter
/// (`object_degradation_count`), whose entire purpose is to make these events
/// visible without crashing. The genuinely alarming case (a long↔object
/// bit-pattern collision reaching a context-free decoder) is surfaced via this
/// one-shot stderr line plus the counter, both of which are non-fatal.
///
/// [`to_value`]: CompactValue::to_value
#[cold]
#[inline(never)]
fn emit_first_degradation_diag(source: DegradationSource, site: &'static str) {
    // FIX: removed the always-false `debug_assert!(false, ...)` that aborted in
    // debug/test builds on the very first degradation. Degradation is a counted,
    // recoverable fallback (returns Value::Long / false rather than dereferencing
    // a bogus pointer), not UB — see note_object_degradation / object_degradation_count
    // and the to_value Safety contract. Keep only the one-shot non-fatal diagnostic.
    // 2026-09-01: the prose is now behind a debug flag and the numbers moved to
    // `degradation_exit_summary`. This block was never the loud one -- a `Once`
    // per source caps it at three lines per process -- but it is three lines of
    // paragraph-length prose on the default stderr of a program that never asked
    // for a diagnostic, and the goal is a `cratonvm Hello` whose stderr is empty.
    // NOTHING about what is counted, or about what is returned to Java, changes
    // here: the `fetch_add` in `note_object_degradation_at` runs exactly as
    // before, and `degradation_exit_summary` reports all three sources rather
    // than only the first event of each.
    //
    // Three gates rather than one, because three lanes want this and none is a
    // superset of the others: `CRATONVM_DBG_COERCION` is the descriptor-loss
    // lane (a degraded reference and a coerced reference slot are one defect
    // seen from two sides), `CRATONVM_DBG_OVERLAY` is the native-overlay hunter
    // that reads slot-level damage, and `CRATONVM_DBG_CORRUPT_CELL` arms the
    // corrupt-cell census, whose whole subject is a word that cannot be a live
    // pointer.
    if !degradation_diag_verbose() {
        return;
    }
    FIRST_DEGRADATION_DIAG[source.index()].call_once(|| {
        match source {
            // Unchanged text for the interpreter source: it is the only source
            // reachable from this crate, so a default build's stderr is
            // byte-identical to what it printed before the sink was widened.
            DegradationSource::Interpreter => eprintln!(
                "CompactValue: first long↔object NaN-box collision degraded to \
                 Value::Long. The slot carries the SUB_OBJECT NaN-box pattern \
                 but its payload is not in the reference-provenance bitmap, which \
                 means EITHER a primitive long whose bits collide with the tag \
                 (harmless) OR a genuine reference whose payload was never \
                 recorded (NOT harmless: the degraded value takes LKIND_LONG, and \
                 Frame::scan_local_objects skips LONG slots, so the GC stops \
                 seeing the root). Subsequent collisions are counted by \
                 object_degradation_count() but not logged."
            ),
            // The raw-word sources carry no long/object ambiguity: the word came
            // out of a slot the JVM type system already says is a reference, and
            // a null was filtered out before this point. There is no harmless
            // reading of these two.
            DegradationSource::Jit | DegradationSource::ArrayElement => eprintln!(
                "CompactValue: first reference degradation from source '{}'{}{}. \
                 A non-null reference word that cannot be a live heap pointer was \
                 handed to Java as null. Unlike the interpreter source there is no \
                 benign long↔object reading here — the word came from a slot the \
                 JVM type system says is a reference — so this is a GC \
                 root-coverage failure until proven otherwise. Subsequent events \
                 are counted by object_degradation_count_from() but not logged.",
                source.name(),
                if site.is_empty() { "" } else { " at " },
                site,
            ),
        }
    });
}

/// The G30 descriptor-coercion census: per-species counts, a sampled locator,
/// and one line at process exit.
///
/// # Why the counters live in this crate and not beside the guard
///
/// They are bumped in `cratonvm_gc::heap::note_field_coercion_loss` and
/// printed from `native-builtins`' shutdown trailer, and
/// `cratonvm-native-builtins` does not depend on `cratonvm-gc`. This is the
/// same crate-reachability argument that put [`crate::cell_census`] in this
/// crate: a JUnit runner leaves through `System.exit` and never reaches
/// `vm-cli`'s normal-return arm, so a summary printed anywhere the shutdown
/// trailer cannot call appears in ZERO logs of a whole suite sweep -- and an
/// absent line reads exactly like a clean run.
///
/// # Why this is a count now and was a log before
///
/// Until 2026-09-01 every occurrence went to `tracing::warn!` under an
/// escalating `n < 4 || n.is_power_of_two()` sample. MEASURED on a stock
/// `cratonvm Hello`: 24 of the 28 lines that one-line program printed to
/// stderr came from this one guard, whose `primitive-into-reference` read
/// species reached occurrence 2048 during boot. An instrument that IS the
/// background cannot be the thing you notice, so a default run is now silent
/// and pays one line at exit. The per-occurrence rows are unchanged and still
/// reachable: `CRATONVM_DBG_COERCION` gives every occurrence with a
/// backtrace, `CRATONVM_DBG_OVERLAY` keeps the escalating sample for the
/// overlay-hunting lane that reads these rows beside its own.
///
/// # A quiet run is not a clean run
///
/// THIS CENSUS SEES DESCRIPTOR MISMATCHES ONLY. A write that lands on the
/// WRONG slot whose value nonetheless fits that field's own descriptor is
/// invisible to it, so "no line printed" means "no writer contradicted a
/// descriptor", not "every field was written correctly". G59-1 measured both
/// halves of one defect at once: two writes tripped this guard, and two more
/// from the same source line landed silently on an `int` and set a 1ms
/// connect timeout. The caveat is repeated inside the exit line itself,
/// because the reader of that line is exactly the person who did not read
/// this comment.
pub mod coercion_census {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Row count: one per `cratonvm_gc::heap::FieldCoercionLoss` variant.
    pub const SPECIES: usize = 4;
    /// Column count: one per `cratonvm_gc::heap::FieldAccessKind` variant.
    pub const KINDS: usize = 3;

    /// Species names in `FieldCoercionLoss::index()` order.
    ///
    /// Duplicated from `cratonvm_gc::heap::FieldCoercionLoss::name` rather
    /// than imported, because `gc` depends on this crate and not the other
    /// way round. Drift would silently mislabel the exit line, so it is
    /// pinned from the `gc` side by
    /// `gc::heap::tests::the_census_tables_match_the_coercion_enums`, which
    /// is the only reason duplicating them is acceptable.
    pub const SPECIES_NAMES: [&str; SPECIES] = [
        "primitive-into-reference",
        "primitive-into-reference-uncoerced",
        "null-into-primitive",
        "pointer-into-primitive",
    ];

    /// Access-kind names in `FieldAccessKind::index()` order. Same pinning
    /// argument as [`SPECIES_NAMES`].
    pub const KIND_NAMES: [&str; KINDS] = ["read", "store", "unattributed"];

    /// `class_id` value meaning "the caller carried no provenance". A real
    /// `ClassId` never reaches `u32::MAX`.
    pub const NO_CLASS: u32 = u32::MAX;
    /// Slot index meaning "unknown". Sixteen bits are kept for the index, so
    /// this is also what a (nonexistent in practice) slot 65535 reports as.
    pub const NO_INDEX: u32 = 0xFFFF;

    /// One counter per (species, access kind). Flat so the increment is a
    /// single relaxed `fetch_add` on an already-cold path.
    static COUNTS: [[AtomicU64; KINDS]; SPECIES] = [
        [const { AtomicU64::new(0) }; KINDS],
        [const { AtomicU64::new(0) }; KINDS],
        [const { AtomicU64::new(0) }; KINDS],
        [const { AtomicU64::new(0) }; KINDS],
    ];

    /// How many distinct `(kind, species, descriptor, slot, class)` tuples the
    /// census can name.
    ///
    /// Sixteen, not one: the first-seen locator is not the most common one,
    /// and a count with no locator is not actionable -- "2048 coercions" and
    /// "2048 coercions, all of them class 64 slot 2 at descriptor `[`" are
    /// different findings, and only the second can be repaired. Sixteen slots
    /// covers the observed boot population (one dominant tuple plus a short
    /// tail) at 256 bytes of static, and anything past it is counted in
    /// [`locator_spill`] rather than silently dropped.
    const LOCATOR_SLOTS: usize = 16;

    /// Packed locator keys; `0` means "slot never claimed". Claimed exactly
    /// once, by CAS, and never evicted -- see [`note_locator`].
    static LOCATOR_KEYS: [AtomicU64; LOCATOR_SLOTS] = [const { AtomicU64::new(0) }; LOCATOR_SLOTS];
    /// Hit count for the key in the same position of [`LOCATOR_KEYS`].
    static LOCATOR_HITS: [AtomicU64; LOCATOR_SLOTS] = [const { AtomicU64::new(0) }; LOCATOR_SLOTS];
    /// Events whose locator found the table full. Reported, because a large
    /// spill beside a small `hits` means the hottest name printed is a sample
    /// rather than the maximum.
    static LOCATOR_SPILL: AtomicU64 = AtomicU64::new(0);

    /// Pack a site into a locator key.
    ///
    /// The key is never zero because `descriptor` is a JVMS descriptor byte
    /// and no descriptor is NUL, which is what lets `0` mean "empty slot" in
    /// [`LOCATOR_KEYS`] without a second flag word per slot.
    #[inline]
    fn locator_key(species: usize, kind: usize, descriptor: u8, class_id: u32, index: u32) -> u64 {
        (((kind as u64) & 0xF) << 60)
            | (((species as u64) & 0xF) << 56)
            | ((descriptor as u64) << 48)
            | (((index as u64) & 0xFFFF) << 32)
            | (class_id as u64)
    }

    /// Inverse of [`locator_key`]: `(species, kind, descriptor, class_id, index)`.
    #[inline]
    fn locator_parts(key: u64) -> (usize, usize, u8, u32, u32) {
        (
            ((key >> 56) & 0xF) as usize,
            ((key >> 60) & 0xF) as usize,
            ((key >> 48) & 0xFF) as u8,
            (key & 0xFFFF_FFFF) as u32,
            ((key >> 32) & 0xFFFF) as u32,
        )
    }

    /// Bump the hit count for one locator, claiming a slot for it if the
    /// table has one free.
    ///
    /// Open-addressed with linear probing over the whole table and NO
    /// eviction: the table exists to name the DOMINANT producer, and any
    /// eviction policy lets a long tail of one-shot sites push that producer
    /// out -- which is the failure mode that makes a locator worthless. Work
    /// is bounded by `LOCATOR_SLOTS` relaxed loads plus at most one CAS, on a
    /// path that only runs once a coercion has already destroyed a value, so
    /// this is affordable in a default build. That matters: a locator you have
    /// to know to ask for is how a run gets read without one.
    #[inline]
    fn note_locator(key: u64) {
        // Fibonacci hash of the key, then take the top bits: the low bits are
        // the class id, and class ids are dense small integers, so indexing on
        // them directly would pile every species of one class into adjacent
        // slots.
        let mut i = ((key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60) as usize) % LOCATOR_SLOTS;
        for _ in 0..LOCATOR_SLOTS {
            let cur = LOCATOR_KEYS[i].load(Ordering::Relaxed);
            if cur == key {
                LOCATOR_HITS[i].fetch_add(1, Ordering::Relaxed);
                return;
            }
            if cur == 0 {
                match LOCATOR_KEYS[i].compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                {
                    Ok(_) => {
                        LOCATOR_HITS[i].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    // Lost the race. If the winner wrote OUR key the slot is
                    // still ours to count in; otherwise keep probing.
                    Err(other) if other == key => {
                        LOCATOR_HITS[i].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Err(_) => {}
                }
            }
            i = (i + 1) % LOCATOR_SLOTS;
        }
        LOCATOR_SPILL.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one value-destroying descriptor coercion.
    ///
    /// Returns the number of events already recorded for this
    /// `(species, kind)` pair -- i.e. the occurrence ordinal the caller's
    /// escalating `n < 4 || n.is_power_of_two()` sample needs when a debug
    /// flag has asked for the per-occurrence rows.
    ///
    /// Pass [`NO_CLASS`] / [`NO_INDEX`] when the caller carries no
    /// provenance; those render as `-1` in the exit line, the same way the
    /// demoted per-occurrence WARN rendered them, so old and new logs grep
    /// alike.
    ///
    /// An out-of-range `species`/`kind` is ignored rather than panicking:
    /// this is a diagnostic on a path that has already lost a value, and
    /// turning a table-width mismatch into an abort would be the instrument
    /// doing more damage than the defect. The widths are pinned by a `const`
    /// assertion on the `gc` side instead.
    #[inline]
    pub fn note(species: usize, kind: usize, descriptor: u8, class_id: u32, index: u32) -> u64 {
        if species >= SPECIES || kind >= KINDS {
            return 0;
        }
        let n = COUNTS[species][kind].fetch_add(1, Ordering::Relaxed);
        note_locator(locator_key(species, kind, descriptor, class_id, index));
        n
    }

    /// How many times each (species, access kind) pair has fired, indexed the
    /// same way as [`SPECIES_NAMES`] and [`KIND_NAMES`].
    #[must_use]
    pub fn counts() -> [[u64; KINDS]; SPECIES] {
        let mut out = [[0u64; KINDS]; SPECIES];
        for (r, row) in COUNTS.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                out[r][c] = cell.load(Ordering::Relaxed);
            }
        }
        out
    }

    /// Total value-destroying descriptor coercions in this process.
    #[must_use]
    pub fn total() -> u64 {
        COUNTS
            .iter()
            .flatten()
            .map(|c| c.load(Ordering::Relaxed))
            .sum()
    }

    /// Events the locator table had no room for. Non-zero means the tuple
    /// named by [`hottest_locator`] is the busiest of the sixteen TRACKED
    /// tuples, not provably the busiest overall.
    #[must_use]
    pub(crate) fn locator_spill() -> u64 {
        LOCATOR_SPILL.load(Ordering::Relaxed)
    }

    /// The busiest tracked locator:
    /// `(species, kind, descriptor, class_id, index, hits)`, or `None` when
    /// nothing has fired.
    #[must_use]
    pub fn hottest_locator() -> Option<(usize, usize, u8, u32, u32, u64)> {
        let mut best: Option<(usize, u64)> = None;
        for i in 0..LOCATOR_SLOTS {
            if LOCATOR_KEYS[i].load(Ordering::Relaxed) == 0 {
                continue;
            }
            let hits = LOCATOR_HITS[i].load(Ordering::Relaxed);
            // An explicit match rather than `map_or` / `is_none_or`: the
            // workspace MSRV is 1.80 and `Option::is_none_or` is 1.82, while
            // recent clippy rewrites `map_or(true, ..)` into it. This spells
            // the same thing and is not a lint battleground.
            let better = match best {
                None => true,
                Some((_, b)) => hits > b,
            };
            if better {
                best = Some((i, hits));
            }
        }
        let (i, hits) = best?;
        let (species, kind, descriptor, class_id, index) =
            locator_parts(LOCATOR_KEYS[i].load(Ordering::Relaxed));
        Some((species, kind, descriptor, class_id, index, hits))
    }

    /// Render `class_id`/`index` the way the demoted per-occurrence WARN did:
    /// `-1` for "the caller has no provenance", so the two are greppable
    /// together across old and new logs.
    fn or_minus_one(v: u32, absent: u32) -> i64 {
        if v == absent {
            -1
        } else {
            i64::from(v)
        }
    }

    /// One line on the exit path, and only when something fired.
    ///
    /// Not behind a debug flag, deliberately, and not printed when the count
    /// is zero -- the two decisions answer different questions. Unconditional
    /// on non-zero, because a count you have to know to ask for is how a run
    /// gets read without one. Silent on zero, because the whole point of the
    /// change is that a hello-world prints nothing, and because this census
    /// CANNOT certify a clean run anyway: it sees descriptor mismatches only
    /// (see the module comment), so a printed `total=0` would be a stronger
    /// claim than the instrument is able to make.
    ///
    /// Idempotent -- `Once`-guarded -- because two exit arms call it: the
    /// `System.exit` shutdown trailer in `native-builtins::lang_system`, and
    /// `vm-cli`'s normal-return arm, which is the one a program returning
    /// from `main` takes instead.
    pub fn exit_summary() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let counts = counts();
        let total: u64 = counts.iter().flatten().sum();
        if total == 0 || !super::exit_census_asked() {
            return;
        }
        ONCE.call_once(|| {
            let mut by_species = String::new();
            for (s, row) in counts.iter().enumerate() {
                if row.iter().all(|&n| n == 0) {
                    continue;
                }
                by_species.push(' ');
                by_species.push_str(SPECIES_NAMES[s]);
                by_species.push('[');
                let mut first = true;
                for (k, &n) in row.iter().enumerate() {
                    if n == 0 {
                        continue;
                    }
                    if !first {
                        by_species.push(' ');
                    }
                    first = false;
                    by_species.push_str(KIND_NAMES[k]);
                    by_species.push('=');
                    by_species.push_str(&n.to_string());
                }
                by_species.push(']');
            }
            let spill = match locator_spill() {
                0 => String::new(),
                n => format!(
                    " (+{n} untracked locator(s): this is the busiest TRACKED site, \
                     not provably the busiest)"
                ),
            };
            let hottest = match hottest_locator() {
                Some((s, k, d, c, i, hits)) => format!(
                    " hottest={}/{} class_id={} index={} descriptor={} hits={}{}",
                    SPECIES_NAMES.get(s).copied().unwrap_or("?"),
                    KIND_NAMES.get(k).copied().unwrap_or("?"),
                    or_minus_one(c, NO_CLASS),
                    or_minus_one(i, NO_INDEX),
                    d as char,
                    hits,
                    spill,
                ),
                None => String::new(),
            };
            crate::exit_report::emit(format_args!(
                "[cratonvm] descriptor-coercion census: total={total}{by_species}{hottest} \
                 -- field accesses whose value contradicted the slot's declared descriptor \
                 and was DESTROYED \
                 (G30-1-the-silent-reference-slot-coercion-20260817.md). This is NOT the \
                 W7-84 autobox guard: that one boxes and fires only for the class mirror; \
                 this one nulls (or zeroes). class_id=-1/index=-1 means the caller has no \
                 provenance yet (G30 NOMINATION 1). CRATONVM_DBG_LAYOUT=1 resolves a \
                 class_id to a name; CRATONVM_DBG_COERCION=1 restores the per-occurrence \
                 warning with a backtrace, CRATONVM_DBG_OVERLAY=1 the rate-limited sample. \
                 THIS COUNTS DESCRIPTOR MISMATCHES ONLY: a wrong slot whose value happens \
                 to fit the field's own descriptor is invisible here, so the ABSENCE of \
                 this line is not a clean run."
            ));
        });
    }
}

/// Whether the two exit censuses of this module print (nio21 wave 8 lane L4):
/// under `CRATONVM_DBG=exit-census` (`CRATONVM_DBG_EXIT_CENSUS`), or under the
/// per-occurrence switches they summarise (`CRATONVM_DBG_COERCION`,
/// `CRATONVM_DBG_OVERLAY`, `CRATONVM_DBG_CORRUPT_CELL`). They used to print on
/// any run with a non-zero count, which an ordinary probe has, and HotSpot's
/// stderr holds no such line. The counts are unchanged.
fn exit_census_asked() -> bool {
    // r23 w1 L11: the one exit-report policy (`crate::exit_report`).
    crate::exit_report::exit_report_on()
        || false
        || false
        || None::<std::ffi::OsString>.is_some()
}

/// One line at exit naming every reference degradation this process counted.
///
/// The per-source `Once` diagnostics in `emit_first_degradation_diag` used
/// to be the only report and printed unconditionally on a default run. They
/// are now behind the same debug flags as the coercion guard's per-occurrence
/// rows, for the same reason: a one-line program's stderr must be empty, and
/// a paragraph of prose about a counter reaching 1 is not something a reader
/// at boot can act on.
///
/// What replaces them carries MORE, not less. Those lines fired on the first
/// event per source and never said how many followed; this prints all three
/// counters, so "interpreter=1" (the benign long-versus-object collision) and
/// "interpreter=1 jit=4200" stop looking alike.
///
/// `jit=` and `array-element=` are the diagnostically loaded halves: a JIT'd
/// frame is exactly the frame a deposited root snapshot can miss, and neither
/// of those two sources has the interpreter's benign reading. See
/// [`DegradationSource`].
pub fn degradation_exit_summary() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let breakdown = object_degradation_breakdown();
    let total: u64 = breakdown.iter().sum();
    if total == 0 || !exit_census_asked() {
        return;
    }
    ONCE.call_once(|| {
        let mut parts = String::new();
        for source in DegradationSource::ALL {
            parts.push(' ');
            parts.push_str(source.name());
            parts.push('=');
            parts.push_str(&breakdown[source.index()].to_string());
        }
        crate::exit_report::emit(format_args!(
            "[cratonvm] reference-degradation census: total={total}{parts} -- \
             reference-shaped words that could not be live heap pointers and were handed \
             to Java as null. `interpreter` has a benign reading (a primitive long whose \
             bits collide with the SUB_OBJECT tag); `jit` and `array-element` do not -- \
             the word came from a slot the JVM type system says IS a reference, so a \
             non-zero count there is a GC root-coverage failure until proven otherwise. \
             Set CRATONVM_DBG_COERCION=1, CRATONVM_DBG_OVERLAY=1 or \
             CRATONVM_DBG_CORRUPT_CELL=1 for the per-source first-event prose."
        ));
    });
}

/// Round-8 branch-hint: the SUB_OBJECT degraded path (null or unaligned
/// pointer arising from a stale slot) is no longer reached by
/// `to_value()` — it now treats unaligned/null SUB_OBJECT slots as
/// long-bit-pattern collisions and returns `Value::Long(_)` (BC SM2 fix,
/// 2026-05-28). Kept as a no-op helper to avoid orphaning callers of the
/// old name in test code; the body is now `Value::Object(None)` and is
/// only invoked by the legacy null-ptr-degrade unit test.
#[cold]
#[inline(never)]
#[allow(dead_code)]
fn cold_degraded_object_ptr() -> Value {
    Value::Object(None)
}

impl CompactValue {
    // -- Constructors -------------------------------------------------------

    /// Create a CompactValue holding a 32-bit int.
    #[inline]
    pub fn int(v: i32) -> Self {
        // Store as zero-extended u32 in the 47-bit payload.
        Self(make_tagged(SUB_INT, v as u32 as u64))
    }

    /// Create a CompactValue holding a 64-bit long.
    ///
    /// # Encoding
    ///
    /// Longs are stored **bit-exact** as raw i64 bits with no transformation.
    /// `tag()` returns `CompactTag::Double` for non-NaN-tagged patterns and
    /// the natural sub-tag (Int/Float/Object/Null/Uninit/RetAddr/Long-Lo/Long-Hi)
    /// for NaN-tagged patterns. Callers use `as_long_unchecked()` (or a
    /// descriptor-aware decode) when JVM instruction context indicates a Long.
    ///
    /// # NaN-box collision: heap safety is owned by the GC, not the encoder
    ///
    /// A long whose top 14 bits coincide with `NANBOX_BITS` and whose bits
    /// 49-47 form the `SUB_OBJECT` pattern will report `is_object() == true`.
    /// Historic CratonVM re-tagged such longs into `SUB_LONG_LO` /
    /// `SUB_LONG_HI` to preserve the invariant "`CompactValue::long` never
    /// produces an Object-classified slot" — but the re-tag was lossy
    /// (original bits 49-47 destroyed) which corrupted the long value
    /// (BC SM2 / `LongArray.modSquare` regression, May 2026: `lxor` of two
    /// untagged operands producing `0xfffd…` got pushed back as `0xffff…`).
    /// JVM spec requires bit-exact long preservation, so the encoder now
    /// stores verbatim.
    ///
    /// The GC roots scanners (`Frame::scan_local_objects`,
    /// `ValueStack::scan_object_refs`, and their `update_*` counterparts)
    /// now filter every `is_object()`-classified slot through
    /// `VmHeap::is_object_address`. A long-bit-pattern that happens to look
    /// like `SUB_OBJECT` but whose payload is not a live heap address is
    /// dropped from the root set. The vanishingly-unlikely case of a long
    /// whose lower 47 bits coincide with a live object's address would still
    /// over-root the slot, but the GC's
    /// [`GenerationalHeap::forward_object`] `MAX_SANE_OBJECT_SIZE` guard
    /// degrades gracefully (over-retention, no SEGV).
    #[inline]
    pub fn long(v: i64) -> Self {
        Self(v as u64)
    }

    /// Create a CompactValue holding a 32-bit float.
    #[inline]
    pub fn float(v: f32) -> Self {
        Self(make_tagged(SUB_FLOAT, v.to_bits() as u64))
    }

    /// Create a CompactValue holding a 64-bit double.
    ///
    /// If the bit pattern of `v` collides with our NaN-tagged encoding space,
    /// it is replaced with the canonical quiet NaN. This is lossless for all
    /// non-NaN doubles and for the standard quiet NaN.
    ///
    /// It is NOT lossless for every NaN, and the parenthetical that used to
    /// stand here — "Java mandates a single NaN anyway" — is false.
    /// `Double.doubleToLongBits` canonicalizes by specification, but
    /// `doubleToRawLongBits` exists precisely so a program can observe the
    /// payload it was handed, and HotSpot carries payloads through `f2d`,
    /// `d2f`, `dmul`, `dadd`, array stores and field stores. So does this VM,
    /// everywhere except here.
    ///
    /// The cost is measurable, not hypothetical. `is_nan_tagged` is
    /// `(bits & 0xFFFC_0000_0000_0000) == 0xFFFC_0000_0000_0000` — sign,
    /// exponent, quiet bit and marker bit — so exactly the NEGATIVE QUIET NaNs
    /// with mantissa bit 50 set are destroyed. `probes/F2dCensus.java` widens
    /// random NaN floats and finds 49 667 of 200 000 flattened, because a float
    /// NaN's mantissa shifts left by 29 and its bits 22 and 21 land on the
    /// quiet and marker bits. Positives: 0 of 2048. Negatives: 1024 of 2048.
    ///
    /// Keeping the check is still right — without it a tagged slot would be
    /// indistinguishable from a double, which is a memory-safety problem rather
    /// than a payload one. What is wrong is calling the loss free. See
    /// `nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828` for the
    /// write-up and the candidate fixes, all of which are changes to this
    /// encoding.
    ///
    /// The loss is no longer *silent*: every collapse is counted by
    /// [`note_nan_payload_collapse`], so
    /// [`nan_payload_collapse_count`] answers "did this workload hit it at
    /// all?" without a rebuild. The counter is a single relaxed increment on
    /// a branch that is already taken, so the common path — every non-NaN
    /// double, and every positive NaN — pays nothing.
    ///
    /// **This is no longer the constructor the interpreter uses.** Every
    /// operand-stack push, local store and argument copy writes a kind mark
    /// beside the slot and therefore goes through
    /// [`double_raw`](Self::double_raw), which keeps the payload. What is
    /// left here is the *context-free* encode: a caller with no mark to
    /// offer, and any future one. So the counter is now a ratchet — a
    /// non-zero reading means a new context-free double encode has appeared,
    /// and the fix is to give that call site a mark, not to accept the loss.
    #[inline]
    pub fn double(v: f64) -> Self {
        let bits = v.to_bits();
        if is_nan_tagged(bits) {
            // Collision: this double's bit pattern looks like a tagged value.
            // Replace with canonical NaN, and record that a payload died here.
            note_nan_payload_collapse();
            Self(CANONICAL_NAN)
        } else {
            Self(bits)
        }
    }

    /// Store a double's bits **verbatim**, with no collision canonicalization.
    ///
    /// # Caller contract - the slot must carry an out-of-band double mark
    ///
    /// This is only sound where the store that writes this slot *also* writes
    /// a "this slot is a double" mark next to it, in the same operation:
    ///
    /// * `ValueStack` - `kinds[i] == KIND_DOUBLE`
    /// * `Frame` locals - `local_kinds[i] == LKIND_DOUBLE`
    /// * `RawSlot` - the caller-supplied `SlotType::Double`
    ///
    /// Those marks already exist and are already load-bearing: they were added
    /// for the *long* side of the same ambiguity (a `long` is stored verbatim
    /// by [`long`](Self::long), so a long whose bits land in the `SUB_OBJECT`
    /// sub-tag would otherwise be rooted and relocated as a reference). Every
    /// consumer that a raw 64-bit pattern could confuse is already gated on
    /// them:
    ///
    /// * the GC root scans (`ValueStack::scan_object_refs`,
    ///   `ValueStack::update_object_refs`, `Frame::scan_local_objects`,
    ///   `Frame::update_local_refs`) skip a `KIND_DOUBLE` / `LKIND_DOUBLE`
    ///   slot outright, so a verbatim double can never be mistaken for a root
    ///   or rewritten by a moving collector;
    /// * the decoders that matter (`ValueStack::value_at`,
    ///   `ValueStack::pop_double`, `Frame::get_local`, `Frame::get_local_raw`,
    ///   `decode_by_descriptor(b'D')`) read the raw bits when the mark says
    ///   double, rather than consulting the NaN-box sub-tag.
    ///
    /// A caller that does **not** write such a mark must use
    /// [`double`](Self::double) instead: without the mark the slot is
    /// context-free, and a `0xFFFC_...` double is bit-for-bit a tagged value.
    ///
    /// # Why this exists
    ///
    /// `double()` canonicalizes exactly the negative quiet NaNs with mantissa
    /// bit 50 set - 2^50 patterns, a quarter of the NaN space, and precisely
    /// the ones an `f2d` of a negative float NaN with mantissa bits 22 and 21
    /// set produces. `Double.doubleToRawLongBits` observes the loss and
    /// HotSpot does not lose it. See
    /// `nan-payloads-lost-to-the-compactvalue-tag-collision-FIXED-20260828` for the
    /// census: 49 667 / 200 000 widened NaNs flattened before this constructor
    /// existed, 0 after.
    #[inline]
    pub fn double_raw(v: f64) -> Self {
        Self(v.to_bits())
    }

    /// [`from_value`](Self::from_value) for a store that writes a kind mark
    /// alongside the slot.
    ///
    /// Identical to `from_value` for every variant except `Value::Double`,
    /// which is stored verbatim via [`double_raw`](Self::double_raw). Use it
    /// only where the call site also records `KIND_DOUBLE` / `LKIND_DOUBLE` -
    /// see `double_raw`'s contract, which this inherits wholesale.
    #[inline]
    pub fn from_value_kinded(v: Value) -> Self {
        match v {
            Value::Double(d) => Self::double_raw(d),
            other => Self::from_value(other),
        }
    }

    /// Create a CompactValue holding a non-null object reference.
    ///
    /// The pointer is stored in the 47-bit payload.  On x86-64 user-space
    /// addresses fit in 47 bits (bit 47 is always 0 for canonical lower-half
    /// addresses) and on AArch64 user-space the high bits are likewise zero.
    ///
    /// # Panics
    /// Panics (in **both** debug and release builds) if `ptr` is zero,
    /// unaligned, below the null-guard page, or has bits set outside the
    /// 47-bit payload range.  A pointer that does not fit
    /// would otherwise be silently truncated by `& PAYLOAD_MASK` into a bogus
    /// heap reference — an unrecoverable corruption — so an immediate panic is
    /// strictly better than producing a dangling object handle.  Callers that
    /// derive pointers from platform-supplied addresses where the 47-bit
    /// assumption may not hold (e.g. `mmap(MAP_FIXED)`, AArch64 LVA, x86-64
    /// 5-level paging) must use [`try_from_pointer`](Self::try_from_pointer)
    /// instead, which reports the failure as `None` rather than panicking.
    #[inline]
    pub fn object(ptr: u64) -> Self {
        // Release-active checks: a truncated pointer is unrecoverable, so we
        // must not let `& PAYLOAD_MASK` silently mask away high bits. The
        // checks are cheap relative to the cost of a corrupted heap reference.
        assert!(
            ptr != 0,
            "CompactValue::object called with null pointer; use null() instead"
        );
        if ptr & !PAYLOAD_MASK != 0 {
            Self::object_out_of_range(ptr);
        }
        if !object_payload_is_plausible(ptr) {
            Self::object_invalid_pointer(ptr);
        }
        // A SUB_OBJECT slot is only decodable as a reference if its payload
        // has crossed a reference-construction boundary in this process
        // (`crate::value::object_ref_payload_is_known`). Encoding one here IS
        // such a boundary: without this record, `to_value` / `decode_value`
        // would later find the payload unknown and DEGRADE a live reference to
        // `Value::Long` -- which also marks the slot `LKIND_LONG`, hiding it
        // from `Frame::scan_local_objects`, so the GC reclaims a live object.
        // See the module-level note above `object`.
        crate::value::record_object_ref_payload((ptr & PAYLOAD_MASK) as *mut u8);
        Self(make_tagged(SUB_OBJECT, ptr & PAYLOAD_MASK))
    }

    #[cold]
    #[inline(never)]
    fn object_invalid_pointer(ptr: u64) -> ! {
        panic!("CompactValue::object: pointer {ptr:#x} is not a plausible heap object pointer");
    }

    /// Cold out-of-line handler for an out-of-47-bit object pointer reaching
    /// [`object`](Self::object). Default behaviour is unchanged (panic, caught
    /// upstream and surfaced as `InternalError: JIT dispatch ... failed`).
    ///
    /// HUNT diagnostic (gated `CRATONVM_DBG_COMPACTVALUE`): such a value is
    /// almost always a NaN-double bit pattern (e.g. `0xfffc...`) or other
    /// primitive landing in a reference slot via a JIT dispatch arg-marshalling
    /// miscompile. Before panicking, dump the raw value + the Rust call path so
    /// the offending decode/getfield/dispatch site is identifiable. The panic
    /// (and its upstream catch) is preserved, so the run continues and every
    /// occurrence is logged. Built with debug symbols (`--config
    /// profile.release.debug=true`) for a readable backtrace.
    #[cold]
    #[inline(never)]
    fn object_out_of_range(ptr: u64) -> ! {
        if None::<std::ffi::OsString>.is_some() {
            eprintln!(
                "[CRATONVM_DBG_COMPACTVALUE] CompactValue::object out-of-range ptr={ptr:#x}\n{}",
                std::backtrace::Backtrace::force_capture()
            );
        }
        panic!("CompactValue::object: pointer {ptr:#x} exceeds 47-bit address space");
    }

    /// Checked constructor: returns `Some(CompactValue)` if `ptr` is a
    /// plausible compact heap object pointer, or `None` if it is null,
    /// unaligned, below the null-guard page, or has any bits set above bit 46.
    ///
    /// Safe counterpart to [`object`](Self::object) for callers that derive
    /// pointers from platform-supplied addresses (e.g. `mmap(MAP_FIXED)`,
    /// AArch64 LVA, x86-64 5-level paging) where the 47-bit assumption may
    /// not hold.
    #[inline]
    pub fn try_from_pointer(ptr: u64) -> Option<Self> {
        if !object_payload_is_plausible(ptr) {
            return None;
        }
        // A SUB_OBJECT slot is only decodable as a reference if its payload
        // has crossed a reference-construction boundary in this process
        // (`crate::value::object_ref_payload_is_known`). Encoding one here IS
        // such a boundary: without this record, `to_value` / `decode_value`
        // would later find the payload unknown and DEGRADE a live reference to
        // `Value::Long` -- which also marks the slot `LKIND_LONG`, hiding it
        // from `Frame::scan_local_objects`, so the GC reclaims a live object.
        // See the module-level note above `object`.
        crate::value::record_object_ref_payload(ptr as *mut u8);
        Some(Self(make_tagged(SUB_OBJECT, ptr)))
    }

    /// Create a CompactValue representing the null reference.
    #[inline]
    pub fn null() -> Self {
        Self(make_tagged(SUB_NULL, 0))
    }

    /// Create a CompactValue representing an uninitialized slot.
    #[inline]
    pub fn uninitialized() -> Self {
        Self(make_tagged(SUB_UNINIT, 0))
    }

    /// Create a CompactValue holding a return address (JSR/RET pc offset).
    #[inline]
    pub fn return_address(pc: u32) -> Self {
        Self(make_tagged(SUB_RETADDR, pc as u64))
    }

    // -- Tag query -----------------------------------------------------------

    /// Extract the 3-bit NaN-box sub-tag (bits 49-47) as a `u64`.
    ///
    /// This is **only** meaningful once the slot has been confirmed
    /// NaN-tagged via [`is_nan_tagged`]; on an untagged (Long/Double) slot the
    /// result is just the corresponding bits of the raw value. Callers always
    /// gate on `is_nan_tagged(self.0)` first. Centralizes the
    /// `(self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK` shift/mask so the accessors
    /// don't each recompute it — identical behavior, clearer intent.
    #[inline(always)]
    fn subtag(&self) -> u64 {
        (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK
    }

    /// Extract the logical type tag.
    ///
    /// **Important:** Long and Double are both stored as raw 64-bit values
    /// (untagged).  This method returns `CompactTag::Double` for any untagged
    /// value.  If the JVM instruction context indicates a Long, use
    /// `as_long_unchecked()` instead of relying on `tag()`.
    #[inline]
    pub fn tag(&self) -> CompactTag {
        if !is_nan_tagged(self.0) {
            return CompactTag::Double;
        }
        match self.subtag() {
            SUB_INT => CompactTag::Int,
            SUB_FLOAT => CompactTag::Float,
            SUB_OBJECT => CompactTag::Object,
            SUB_NULL => CompactTag::Null,
            SUB_UNINIT => CompactTag::Uninitialized,
            SUB_RETADDR => CompactTag::ReturnAddress,
            SUB_LONG_LO => CompactTag::Long,
            SUB_LONG_HI => CompactTag::Long,
            _ => unreachable!("3-bit sub-tag cannot exceed 7"),
        }
    }

    // -- Accessors -----------------------------------------------------------

    /// Extract an i32 if this value is tagged as Int.
    ///
    /// Returns `None` for a SUB_INT *bit-pattern* whose payload has any bit at
    /// position 32..46 set: `CompactValue::int` only ever stores a 32-bit
    /// payload (`v as u32 as u64`), so such a slot cannot be a genuine int —
    /// it is a primitive `long` whose verbatim bits collided into the SUB_INT
    /// sub-tag space (BC safegcd `0xFFFC_…` accumulators). Returning a
    /// truncated low-32-bits `i32` would silently corrupt that long, so we
    /// decline. This mirrors the SUB_INT collision guard now applied by
    /// [`to_value`](Self::to_value) and matches
    /// [`int_tag_collision_long`](Self::int_tag_collision_long), which exposes
    /// the i64 value for exactly these slots.
    #[inline]
    pub fn as_int(&self) -> Option<i32> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if self.subtag() != SUB_INT {
            return None;
        }
        let payload = self.0 & PAYLOAD_MASK;
        // SUB_INT long-collision guard (MEDIUM, 2026-06-17): a real int has
        // payload < 2^32; a payload with bits 32..46 set is a colliding long,
        // not an int — return None rather than a truncated value.
        if payload >> 32 != 0 {
            return None;
        }
        // Payload is the zero-extended u32; reinterpret as i32.
        Some(payload as u32 as i32)
    }

    /// Extract an i64.
    ///
    /// Returns `Some` for untagged values (Long or Double stored raw) as well
    /// as for NaN-tagged long pairs (`SUB_LONG_LO` / `SUB_LONG_HI`) that arise
    /// when a long's bit pattern collides with our NaN-tag space (e.g. `-1`,
    /// `i64::MIN`).  The caller must know from context that the slot actually
    /// holds a Long.  For tagged non-long values, returns `None`.
    ///
    /// This mirrors the discrimination logic in [`to_value`](Self::to_value):
    /// every bit pattern that `to_value` would resolve to `Value::Long(_)`
    /// here returns `Some(self.0 as i64)`.
    #[inline]
    pub fn as_long(&self) -> Option<i64> {
        if !is_nan_tagged(self.0) {
            // Untagged: raw i64 bits (Long or Double — caller decides).
            return Some(self.0 as i64);
        }
        // NaN-tagged: long bit-pattern collisions land in SUB_LONG_LO/HI and
        // are decoded as Long by `to_value`.  Mirror that here so `as_long`
        // doesn't lose `i64::MIN`, `-1`, or any other collision value.
        match (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK {
            SUB_LONG_LO | SUB_LONG_HI => Some(self.0 as i64),
            _ => None,
        }
    }

    /// For a slot that `tag()`s as `Int`, return `Some(raw_i64)` when its
    /// 47-bit payload has any bit at position 32..=46 set. A genuine
    /// [`int`](Self::int) stores `n as u32` (payload `< 2^32`), so such a slot
    /// cannot be a real int — it is a `long` whose bit pattern collides into
    /// the `SUB_INT` sub-tag space (e.g. BC safegcd `0xFFFC_…` accumulators).
    ///
    /// Returns `None` for genuine ints (payload `< 2^32`, bit-identical to a
    /// long with those exact bits and therefore unresolvable from a single
    /// slot) and for every non-`Int` tag. Used by the JIT / OSR local-slot
    /// transfer so a collision long in a local is handed to compiled code as
    /// its true i64 value rather than truncated to the low 32 bits. Mirrors
    /// the `SUB_INT` discrimination in
    /// [`decode_by_descriptor`](Self::decode_by_descriptor).
    #[inline]
    pub fn int_tag_collision_long(&self) -> Option<i64> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_INT {
            return None;
        }
        if (self.0 & PAYLOAD_MASK) >> 32 != 0 {
            Some(self.0 as i64)
        } else {
            None
        }
    }

    /// Reinterpret the raw stored bits as i64 without checking the tag.
    ///
    /// Since `CompactValue::long` stores longs verbatim, this round-trips
    /// bit-exactly for every i64 value — including longs whose bit pattern
    /// collides with the NaN-tag space (e.g. `-1`, `i64::MIN`, the BC SM2
    /// `0xfffd_…` collision that motivated removing the lossy re-tag).
    ///
    /// The companion accessor [`as_long_bits_unchecked`](Self::as_long_bits_unchecked)
    /// has the same implementation but a name that makes the "raw stored
    /// bits" semantic unambiguous; prefer it in new code where the caller
    /// is consuming the slot as a bit pattern (e.g. JNI long-as-jobject
    /// smuggling, NaN-aware double decode) rather than as a numeric value.
    ///
    /// Use when the JVM instruction context guarantees this slot is a Long.
    #[inline]
    pub fn as_long_unchecked(&self) -> i64 {
        self.0 as i64
    }

    /// Reinterpret the raw stored bits as i64 — the bits-semantic alias of
    /// [`as_long_unchecked`](Self::as_long_unchecked).
    ///
    /// Returns exactly what is stored in the 8-byte slot, with no attempt to
    /// "reverse" the [`long`](Self::long) re-tag transformation. For longs
    /// that took the re-tag branch (large-magnitude negatives in
    /// `[-2^50, -2^48)` whose top three sub-tag bits would have collided
    /// with `SUB_INT` / `SUB_FLOAT` / `SUB_OBJECT`) those original sub-tag
    /// bits are not recoverable from a single 8-byte slot — `make_tagged`
    /// overwrites them with `SUB_LONG_LO` / `SUB_LONG_HI`. This method
    /// returns the re-tagged bits verbatim, which is the same value
    /// [`to_value`](Self::to_value) hands back as `Value::Long(_)` and what
    /// the interpreter consumes for `ladd`/`lsub`/etc. on those slots.
    ///
    /// Prefer this over [`as_long_unchecked`](Self::as_long_unchecked) in
    /// new code: the `_bits_unchecked` suffix makes the "stored bits, not
    /// original i64" semantic obvious at the call-site.
    #[inline]
    pub fn as_long_bits_unchecked(&self) -> i64 {
        self.0 as i64
    }

    /// Extract an f32 if this value is tagged as Float.
    #[inline]
    pub fn as_float(&self) -> Option<f32> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if self.subtag() != SUB_FLOAT {
            return None;
        }
        Some(f32::from_bits((self.0 & PAYLOAD_MASK) as u32))
    }

    /// Extract an f64 if this value is an untagged double.
    #[inline]
    pub fn as_double(&self) -> Option<f64> {
        if is_nan_tagged(self.0) {
            return None;
        }
        Some(f64::from_bits(self.0))
    }

    /// Extract the raw object pointer (non-null) if tagged as Object.
    #[inline]
    pub fn as_object_ptr(&self) -> Option<u64> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if self.subtag() != SUB_OBJECT {
            return None;
        }
        Some(self.0 & PAYLOAD_MASK)
    }

    /// Returns `true` if this value is tagged as Null.
    #[inline]
    pub fn is_null(&self) -> bool {
        is_nan_tagged(self.0) && self.subtag() == SUB_NULL
    }

    /// Returns `true` if this value is tagged as Uninitialized.
    #[inline]
    pub fn is_uninitialized(&self) -> bool {
        is_nan_tagged(self.0) && self.subtag() == SUB_UNINIT
    }

    /// Returns `true` if this slot holds a category-2 JVM value
    /// (Long or Double — occupying two stack slots in the abstract JVM
    /// model, though CompactValue fits each into a single 8-byte slot).
    ///
    /// Long is stored untagged (raw i64 bits).  Most long bit-patterns
    /// don't collide with the NaN-tag space, so `is_nan_tagged` returns
    /// false and we correctly identify the slot as category-2.  A handful
    /// of long values (e.g. `-1`, `i64::MIN`) have high bits that coincide
    /// with our NaN-tag pattern; in those cases the sub-tag may be
    /// SUB_LONG_LO/SUB_LONG_HI, which we also accept.  For other collisions
    /// the result is consistent with the prior `to_value().is_category2()`
    /// path, which already returned false — so behavior is preserved.
    #[inline(always)]
    pub fn is_category2(&self) -> bool {
        if !is_nan_tagged(self.0) {
            // Untagged ⇒ Double or small/positive Long — always cat-2.
            return true;
        }
        let sub = (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK;
        sub == SUB_LONG_LO || sub == SUB_LONG_HI
    }

    /// Extract the return-address pc offset if tagged as ReturnAddress.
    #[inline]
    pub fn as_return_address(&self) -> Option<u32> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_RETADDR {
            return None;
        }
        Some((self.0 & PAYLOAD_MASK) as u32)
    }

    /// Return the raw u64 backing this compact value.
    #[inline]
    pub fn raw_bits(&self) -> u64 {
        self.0
    }

    /// Reconstruct a `CompactValue` from raw bits (no validation).
    ///
    /// Use this when you already have a `u64` that was produced by
    /// `raw_bits()` or `to_bits()` on a valid `CompactValue`.
    #[inline(always)]
    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Alias of [`raw_bits`] with the conventional "to_bits" name.
    #[inline(always)]
    pub fn to_bits(&self) -> u64 {
        self.0
    }

    /// Canonical zero-initialized slot (used when resizing a stack buffer).
    ///
    /// Decodes to `Value::Double(0.0)`; interpreter initialisation never
    /// reads uninitialised slots, so the concrete tag doesn't matter —
    /// all-zero bytes are simply the cheapest default.
    #[inline(always)]
    pub fn zero() -> Self {
        Self(0)
    }

    /// Convert a full [`Value`] into a `CompactValue` (boundary helper).
    ///
    /// Equivalent to `CompactValue::from(&v)` but avoids taking a reference
    /// at hot push sites and is guaranteed `#[inline(always)]`.
    #[inline(always)]
    pub fn from_value(v: Value) -> Self {
        match v {
            Value::Int(i) => CompactValue::int(i),
            Value::Long(l) => CompactValue::long(l),
            Value::Float(f) => CompactValue::float(f),
            Value::Double(d) => CompactValue::double(d),
            Value::Object(Some(r)) => {
                // Defense-in-depth for the named shared panic site
                // (`object()` -> object_out_of_range -> panic at line ~502): a
                // `Value::Object` carrying an out-of-47-bit pointer is corruption
                // — reused/garbage memory surfaced through a stale reference by a
                // GC root-coverage gap — never a live object on x86-64/AArch64
                // user space. Degrade it to null (counted) instead of aborting
                // the VM, so the corruption surfaces as a Java-level null rather
                // than a hard crash. `try_from_pointer` performs the exact same
                // null/47-bit checks `object()` does, so valid pointers take an
                // identical path at zero added cost and are never degraded.
                CompactValue::try_from_pointer(r.as_ptr() as u64).unwrap_or_else(|| {
                    note_object_degradation();
                    CompactValue::null()
                })
            }
            Value::Object(None) => CompactValue::null(),
            Value::ReturnAddress(pc) => CompactValue::return_address(pc),
            Value::Uninitialized => CompactValue::uninitialized(),
        }
    }

    // -- Conversion to Value ------------------------------------------------

    /// Convert back to the full `Value` enum (UNCHECKED — see contract below).
    ///
    /// **Long vs Double ambiguity:** untagged values are decoded as `Double`.
    /// To decode as `Long`, use [`to_value_as_long`](Self::to_value_as_long).
    ///
    /// **Object references:** because `CompactValue` stores only the raw
    /// pointer (not a full `ObjectRef`), conversion back to
    /// `Value::Object(Some(_))` requires reconstructing the `ObjectRef` via
    /// unsafe `from_raw`.
    ///
    /// # Safety / caller contract (HIGH: NaN-box long↔object confusion)
    ///
    /// **If the slot may have originated from a primitive `long` (anything not
    /// provably a reference by JVM type context), you MUST NOT trust an
    /// `Value::Object(Some(_))` returned here — route the decode through
    /// [`to_value_checked`](Self::to_value_checked) with a live-heap predicate,
    /// or through [`decode_by_descriptor`](Self::decode_by_descriptor) when the
    /// declared type is known. The context-free form now rejects merely
    /// plausible raw payloads without prior `ObjectRef` provenance.**
    ///
    /// `CompactValue::long` stores i64 bits **verbatim** (see its docs). A
    /// primitive long whose top bits coincide with `NANBOX_BITS`, whose
    /// sub-tag field equals `SUB_OBJECT`, and whose 47-bit payload is
    /// **non-zero and 8-byte aligned** is *indistinguishable from a real heap
    /// reference at the bit level*. For such a slot, see the current hardening
    /// note below.
    /// Prior versions could fabricate a heap pointer out of a primitive long;
    /// current code rejects merely plausible payloads without provenance.
    ///
    /// `to_value` therefore makes **no guarantee** that an
    /// `Value::Object(Some(_))` it returns points at a live heap object. The
    /// only context-free safety net it applies is the "aligned & non-null"
    /// filter (an unaligned or null `SUB_OBJECT` payload is treated as a long
    /// collision and returned as `Value::Long`, bumping
    /// [`object_degradation_count`]). That filter is necessary but **not
    /// sufficient**: a long whose low 47 bits happen to form an aligned,
    /// non-null address is still rejected here without prior `ObjectRef`
    /// provenance.
    ///
    /// Callers reading a slot that *may* hold a primitive long (operand-stack
    /// slots, locals, GC roots — anything not provably a reference by JVM
    /// type context) **MUST** do one of the following instead of trusting the
    /// `SUB_OBJECT` result of `to_value`:
    ///
    /// * route the decode through [`decode_by_descriptor`](Self::decode_by_descriptor)
    ///   when the declared JVM type is known — this is the type-safe path and
    ///   never fabricates a reference from a primitive; or
    /// * use [`to_value_checked`](Self::to_value_checked) (or
    ///   [`is_object_checked`](Self::is_object_checked)) with a closure that
    ///   validates the payload against the **live heap** (e.g.
    ///   `VmHeap::is_object_address`), so a fabricated pointer degrades to
    ///   `Value::Long` rather than being dereferenced or over-rooted.
    ///
    /// `to_value` is kept unchecked for backwards compatibility and for hot
    /// paths where the caller has *already* established by type context that
    /// the slot is a reference. Do not introduce new context-free
    /// `to_value()`/`is_object()` reference consumers without one of the
    /// guards above.
    ///
    /// Current hardening also requires prior `ObjectRef` provenance before
    /// this method recreates an object. A merely plausible raw payload is
    /// degraded to the bit-exact long.
    #[inline(always)]
    pub fn to_value(&self) -> Value {
        if !is_nan_tagged(self.0) {
            return Value::Double(f64::from_bits(self.0));
        }
        let payload = self.0 & PAYLOAD_MASK;
        match (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK {
            // SUB_INT/SUB_FLOAT long-collision guard (MEDIUM, 2026-06-17):
            // `CompactValue::int`/`float` only ever store a 32-bit payload
            // (`v as u32 as u64` / `v.to_bits() as u64`), so a genuine int or
            // float always has payload bits 32-46 clear. A SUB_INT/SUB_FLOAT
            // bit pattern whose payload has any bit at position 32..46 set
            // (`payload >> 32 != 0`) cannot be a real int/float — it is a
            // primitive `long` whose verbatim bits (CompactValue::long stores
            // them unmodified) collided into the SUB_INT/SUB_FLOAT sub-tag
            // space. Without this guard `to_value()` would `payload as u32`
            // and silently truncate such a long to its low 32 bits. Mirror the
            // identical discrimination already applied by
            // `decode_by_descriptor(b'J')` / `int_tag_collision_long` and by
            // the SUB_NULL/SUB_UNINIT/SUB_RETADDR collision arms below:
            // preserve the long bit-exact instead of truncating.
            SUB_INT => {
                if payload >> 32 == 0 {
                    Value::Int(payload as u32 as i32)
                } else {
                    Value::Long(self.0 as i64)
                }
            }
            SUB_FLOAT => {
                if payload >> 32 == 0 {
                    Value::Float(f32::from_bits(payload as u32))
                } else {
                    Value::Long(self.0 as i64)
                }
            }
            SUB_OBJECT => {
                // BC SM2 fix (2026-05-28): `CompactValue::long` now stores
                // long bits verbatim, so a slot with NaN-tagged bits and
                // SUB_OBJECT pattern may be a primitive long whose bits
                // happen to land in this sub-tag (BC LongArray
                // `0xfffd_…` regression). Real object pointers are always
                // 8-byte aligned and above the null-guard page
                // (`CompactValue::object` enforces non-null + alignment, and
                // the allocator never hands out null-page addresses), so any
                // unaligned, null, or null-page payload must be a long. Return
                // `Value::Long` for those to preserve bits; reserve the Object
                // decode for plausible payloads with recorded ObjectRef provenance.
                //
                // HARD-AUDIT (HIGH long↔object type-confusion): this remains a
                // CONTEXT-FREE decoder — `object_payload_is_plausible` rejects
                // only the cheaply-impossible payloads. An aligned, above-guard
                // long bit pattern is now degraded unless its payload has prior
                // `ObjectRef` provenance. Callers
                // that do not already KNOW the slot is a reference from JVM type
                // context MUST decode via `to_value_checked` / `decode_by_descriptor`
                // instead — see this method's Safety contract.
                if object_payload_is_plausible(payload)
                    && crate::value::object_ref_payload_is_known(payload)
                {
                    let ptr = payload as *mut u8;
                    Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
                } else {
                    // Provably-not-a-reference SUB_OBJECT slot (null, unaligned,
                    // null-page, or never-seen): count the degradation so the silent
                    // reclassification is visible in release builds.
                    note_object_degradation();
                    Value::Long(self.0 as i64)
                }
            }
            // SUB_NULL with non-zero payload is a long-collision (real null
            // always has payload=0); same for SUB_UNINIT. Preserve the long
            // bits in those cases.
            SUB_NULL => {
                if payload == 0 {
                    Value::Object(None)
                } else {
                    Value::Long(self.0 as i64)
                }
            }
            SUB_UNINIT => {
                if payload == 0 {
                    Value::Uninitialized
                } else {
                    Value::Long(self.0 as i64)
                }
            }
            SUB_RETADDR => {
                // Real return addresses fit in u32 (`CompactValue::return_address`
                // stores `pc as u64` from a u32 pc, so payload bits 32-46 are
                // always 0). A SUB_RETADDR bit pattern with payload bits 32-46
                // set is a long-bit-pattern collision (BC SM2 fix 2026-05-28)
                // — preserve the long bits rather than truncating to u32.
                if payload >> 32 == 0 {
                    Value::ReturnAddress(payload as u32)
                } else {
                    Value::Long(self.0 as i64)
                }
            }
            SUB_LONG_LO | SUB_LONG_HI => Value::Long(self.0 as i64),
            _ => unreachable!(),
        }
    }

    /// Returns `true` if this slot carries the `SUB_OBJECT` NaN-box pattern
    /// (UNCHECKED — see contract below).
    ///
    /// Used by the GC scanner to find root set entries without decoding the
    /// full `Value` enum.
    ///
    /// # Safety / caller contract (HIGH: NaN-box long↔object confusion)
    ///
    /// **A `true` here is NOT proof of a live reference. If the slot may have
    /// originated from a primitive `long`, you MUST validate the payload
    /// against the live heap before dereferencing or rooting it — prefer
    /// [`is_object_checked`](Self::is_object_checked) with a heap predicate.
    /// The unchecked form reports `true` for long bits that merely match the
    /// `SUB_OBJECT` pattern.**
    ///
    /// This is a **pure bit-pattern test**: it returns `true` for any slot
    /// whose NaN-box sub-tag is `SUB_OBJECT`, regardless of whether the
    /// payload is a live heap address. Because [`long`](Self::long) stores
    /// i64 bits verbatim, a primitive long whose bits land in the
    /// `SUB_OBJECT` space (e.g. an `lxor`/`ladd` result) reports
    /// `is_object() == true` here. A consumer that then treats the payload as
    /// a pointer (dereference, or adding it to the GC root set) is acting on a
    /// fabricated reference.
    ///
    /// GC root scanners and any other context-free consumer **MUST** validate
    /// the payload against the live heap before treating this slot as a
    /// reference. Prefer [`is_object_checked`](Self::is_object_checked), which
    /// folds the heap-validation closure in and returns `false` (counting the
    /// degradation via [`object_degradation_count`]) for a non-live payload.
    /// `is_object` is kept for callers that perform the heap check separately
    /// (e.g. CratonVM's root scanners filter every `is_object()` slot through
    /// `VmHeap::is_object_address`).
    ///
    /// HARD-AUDIT: unlike [`to_value`](Self::to_value), this predicate does not
    /// even apply the cheap [`object_payload_is_plausible`] filter (null /
    /// unaligned / null-page) — a `true` here covers strictly *more* impossible
    /// payloads than `to_value` would decode as an object. It is therefore the
    /// weakest of the decoders and must never be the sole gate before a
    /// dereference or a GC-root insertion; pair it with the live-heap predicate
    /// in [`is_object_checked`](Self::is_object_checked).
    #[inline(always)]
    pub fn is_object(&self) -> bool {
        is_nan_tagged(self.0) && self.subtag() == SUB_OBJECT
    }

    /// Heap-validated counterpart to [`is_object`](Self::is_object).
    ///
    /// Returns `true` only when this slot carries the `SUB_OBJECT` pattern
    /// **and** the closure `is_heap_object` confirms the payload is a live
    /// heap address. For a `SUB_OBJECT` slot whose payload is *not* a live
    /// heap object — i.e. a primitive long whose bits collided into the
    /// object sub-tag — this returns `false` and bumps
    /// [`object_degradation_count`] so the reclassification is observable.
    ///
    /// `is_heap_object` receives the **47-bit payload** (the candidate
    /// pointer), exactly as it would be handed to `ObjectRef::from_raw`. It
    /// should return `true` iff that address is a currently-live heap object
    /// (e.g. `VmHeap::is_object_address`). This is the single correct check
    /// callers should use instead of pairing a raw `is_object()` with an
    /// open-coded heap lookup.
    #[inline]
    pub fn is_object_checked(&self, is_heap_object: impl Fn(u64) -> bool) -> bool {
        if !self.is_object() {
            return false;
        }
        let payload = self.0 & PAYLOAD_MASK;
        if is_heap_object(payload) {
            true
        } else {
            note_object_degradation();
            false
        }
    }

    /// Heap-validated counterpart to [`to_value`](Self::to_value).
    ///
    /// Behaves exactly like [`to_value`](Self::to_value) for every tag except
    /// `SUB_OBJECT`. For a `SUB_OBJECT` slot that passes the context-free
    /// "aligned & non-null" filter, it additionally calls `is_heap_object`
    /// with the candidate pointer (the 47-bit payload). When that returns:
    ///
    /// * `true` — the payload is a live heap object, so this returns
    ///   `Value::Object(Some(ObjectRef::from_raw(payload)))` exactly as
    ///   `to_value` would; otherwise
    /// * `false` — the slot is a primitive long whose bits merely *look* like
    ///   a reference, so it degrades to `Value::Long(bits)` (bit-exact) and
    ///   bumps [`object_degradation_count`].
    ///
    /// This gives callers a single, correct primitive: instead of taking the
    /// unchecked `to_value()` result and re-validating the pointer themselves
    /// (and risking that the slot was already dereferenced), they get the
    /// heap-validated `Value` directly. Use this — or
    /// [`decode_by_descriptor`](Self::decode_by_descriptor) when the declared
    /// JVM type is known — for any slot that may hold a primitive long.
    ///
    /// `is_heap_object` should be a cheap, side-effect-free predicate such as
    /// `VmHeap::is_object_address`.
    #[inline]
    pub fn to_value_checked(&self, is_heap_object: impl Fn(u64) -> bool) -> Value {
        if !is_nan_tagged(self.0) {
            return Value::Double(f64::from_bits(self.0));
        }
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_OBJECT {
            // Non-object tags are unambiguous (or already long-collision
            // aware inside `to_value`); reuse the existing decode.
            return self.to_value();
        }
        // SUB_OBJECT: apply the shared context-free filter, then the heap
        // check. Routing through `object_payload_is_plausible` keeps this in
        // lock-step with the unchecked `to_value` SUB_OBJECT arm.
        let payload = self.0 & PAYLOAD_MASK;
        if !object_payload_is_plausible(payload) {
            note_object_degradation();
            return Value::Long(self.0 as i64);
        }
        let ptr = payload as *mut u8;
        if is_heap_object(payload) {
            Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
        } else {
            // Aligned, non-null, but not a live heap object: a long whose
            // bits collided into the object sub-tag. Degrade to the bit-exact
            // long and record the reclassification.
            note_object_degradation();
            Value::Long(self.0 as i64)
        }
    }

    /// Replace the object-pointer payload if this slot holds an Object
    /// reference. No-op for non-object slots.
    ///
    /// # Errors
    ///
    /// Returns [`CompactValueError::PointerOutOfRange`] if `new_ptr` has bits
    /// set above bit 46 (i.e. it does not fit in the 47-bit NaN-box payload),
    /// or [`CompactValueError::InvalidObjectPointer`] if it otherwise fails
    /// the compact object pointer plausibility rules. The slot is left
    /// unchanged in either case. Asymmetry note: the
    /// constructor [`object`](Self::object) refuses the same condition with a
    /// panic in both debug and release — historically `update_object_ptr`
    /// silently truncated in release, which would produce a corrupted
    /// reference. Reporting the failure via `Result` is strictly safer; the
    /// GC root scanner is the canonical caller and the addresses it threads
    /// here come from a `HashMap<usize, usize>` of live-heap pointers, all of
    /// which are 47-bit by construction — so [`update_object_ptr_unchecked`]
    /// is preferred on that hot path.
    ///
    /// Used by the GC compaction scanner to update roots after heap
    /// relocation.
    #[inline]
    pub fn update_object_ptr(&mut self, new_ptr: u64) -> Result<(), CompactValueError> {
        if !self.is_object() {
            return Ok(());
        }
        if new_ptr & !PAYLOAD_MASK != 0 {
            return Err(CompactValueError::PointerOutOfRange { ptr: new_ptr });
        }
        if !object_payload_is_plausible(new_ptr) {
            return Err(CompactValueError::InvalidObjectPointer { ptr: new_ptr });
        }
        // A SUB_OBJECT slot is only decodable as a reference if its payload
        // has crossed a reference-construction boundary in this process
        // (`crate::value::object_ref_payload_is_known`). Encoding one here IS
        // such a boundary: without this record, `to_value` / `decode_value`
        // would later find the payload unknown and DEGRADE a live reference to
        // `Value::Long` -- which also marks the slot `LKIND_LONG`, hiding it
        // from `Frame::scan_local_objects`, so the GC reclaims a live object.
        // See the module-level note above `object`.
        crate::value::record_object_ref_payload((new_ptr & PAYLOAD_MASK) as *mut u8);
        self.0 = make_tagged(SUB_OBJECT, new_ptr & PAYLOAD_MASK);
        Ok(())
    }

    /// Unchecked variant of [`update_object_ptr`].
    ///
    /// Replaces the object-pointer payload without release-mode verification.
    /// In debug builds an assertion still catches a pointer that fails the
    /// compact object pointer plausibility rules; in release the high bits are
    /// masked off, which would corrupt the reference.
    ///
    /// # Safety
    ///
    /// This function is **not** `unsafe` in the Rust sense (it cannot violate
    /// memory safety on its own — the truncated pointer would simply
    /// reference the wrong object or no object at all), but callers must
    /// guarantee one of the following invariants for the result to be
    /// correct:
    ///
    /// * `object_payload_is_plausible(new_ptr)` (the pointer is encodable as a
    ///   compact object pointer), OR
    /// * `self` is **not** an object slot (the call is a no-op).
    ///
    /// The canonical caller is the GC compaction scanner, where every
    /// address comes from a `HashMap<usize, usize>` whose values are live-
    /// heap pointers already validated to fit in 47 bits by the allocator.
    #[inline(always)]
    pub fn update_object_ptr_unchecked(&mut self, new_ptr: u64) {
        if self.is_object() {
            debug_assert!(
                object_payload_is_plausible(new_ptr),
                "CompactValue::update_object_ptr_unchecked: pointer {:#x} is not a plausible heap object pointer",
                new_ptr
            );
            // A SUB_OBJECT slot is only decodable as a reference if its payload
            // has crossed a reference-construction boundary in this process
            // (`crate::value::object_ref_payload_is_known`). Encoding one here IS
            // such a boundary: without this record, `to_value` / `decode_value`
            // would later find the payload unknown and DEGRADE a live reference to
            // `Value::Long` -- which also marks the slot `LKIND_LONG`, hiding it
            // from `Frame::scan_local_objects`, so the GC reclaims a live object.
            // See the module-level note above `object`.
            crate::value::record_object_ref_payload((new_ptr & PAYLOAD_MASK) as *mut u8);
            self.0 = make_tagged(SUB_OBJECT, new_ptr & PAYLOAD_MASK);
        }
    }

    /// Convert to `Value::Long` by reinterpreting the raw bits as i64.
    ///
    /// Use this when the JVM execution context indicates the slot holds a Long.
    #[inline]
    pub fn to_value_as_long(&self) -> Value {
        Value::Long(self.0 as i64)
    }

    /// T10.9.E — Descriptor-aware decode to a [`Value`].
    ///
    /// JVM field slots carry a **declared** type from the class file
    /// (`Ljava/lang/String;`, `J`, `D`, etc.).  Storage and operand-stack
    /// compaction both use NaN-boxing, which cannot distinguish a small
    /// long's bit pattern from a denormal double — so `to_value` on an
    /// untagged slot unconditionally yields `Value::Double`.  When the
    /// caller knows the declared type (from the constant pool, a native
    /// method descriptor, or a FieldReference), routing the decode through
    /// this helper guarantees the resulting `Value` variant matches the
    /// declared type, regardless of the bit pattern.
    ///
    /// Supported descriptor bytes:
    /// - `b'J'` — long (reinterpret raw bits as i64)
    /// - `b'D'` — double (reinterpret raw bits as f64)
    /// - `b'F'` — float (reinterpret low 32 bits as f32)
    /// - `b'I' | b'B' | b'C' | b'S' | b'Z'` — int (sign-extended low 32 bits)
    /// - `b'L' | b'['` — reference (falls through to [`to_value`])
    /// - anything else — falls through to [`to_value`] (preserves legacy
    ///   behavior so unknown descriptors cannot regress existing paths).
    ///
    /// Fast-path behavior:
    /// - if the slot is **tag-exact** (e.g. `SUB_INT` for `b'I'`) the
    ///   standard decode via [`to_value`] is used — no bit-level
    ///   reinterpretation, so all Debug/tracing invariants hold.
    /// - only **untagged** slots (raw 64-bit longs or doubles) are
    ///   reinterpreted according to the descriptor.  This is exactly
    ///   where the Long/Double ambiguity bites.
    /// - `Null` and `Uninitialized` slots keep their decoded `Value` so
    ///   the caller can zero-coerce per JVMS §2.3 if needed.
    #[inline]
    pub fn decode_by_descriptor(self, desc_byte: u8) -> Value {
        // For tagged slots (non-NaN-boxed) the decode is unambiguous — use the
        // regular path.  Only untagged slots carry the Long/Double ambiguity.
        if !is_nan_tagged(self.0) {
            return match desc_byte {
                b'J' => Value::Long(self.0 as i64),
                b'D' => Value::Double(f64::from_bits(self.0)),
                b'F' => Value::Float(f32::from_bits(self.0 as u32)),
                b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(self.0 as u32 as i32),
                // References and arrays: an untagged slot holding a raw ptr
                // pattern is not something the operand stack produces, so
                // fall through to `to_value()` which yields Value::Double
                // (the legacy behavior).  This branch is defensive only.
                _ => self.to_value(),
            };
        }
        // Tagged: inspect the sub-tag.  For J/D descriptors the SUB_LONG_*
        // and untagged-double paths must yield the declared type, not
        // whatever `to_value` happens to return.
        let sub = self.subtag();
        match desc_byte {
            b'J' => match sub {
                // An explicit long pair decodes to the stored i64.
                SUB_LONG_LO | SUB_LONG_HI => Value::Long(self.0 as i64),
                // SUB_INT: real int slots have payload < 2^32 (CompactValue::int
                // stores `v as u32 as u64`). For those, apply JVMS i2l widening.
                // A SUB_INT bit pattern whose payload has bits 32..46 set is a
                // long-bit-pattern collision (BC SM2 fix 2026-05-28) — reinterpret
                // the raw bits to preserve the long value.
                SUB_INT => {
                    let payload = self.0 & PAYLOAD_MASK;
                    if payload >> 32 == 0 {
                        Value::Long(payload as u32 as i32 as i64)
                    } else {
                        Value::Long(self.0 as i64)
                    }
                }
                // Null / Uninitialized with payload=0 → JVMS §2.3 default 0L.
                // With non-zero payload it's a long-bit-pattern collision
                // (BC SM2 fix 2026-05-28) — reinterpret the raw bits.
                SUB_NULL | SUB_UNINIT => {
                    if (self.0 & PAYLOAD_MASK) == 0 {
                        Value::Long(0)
                    } else {
                        Value::Long(self.0 as i64)
                    }
                }
                // Object / Float / ReturnAddress landing in a J slot is
                // upstream drift; reinterpret the raw bits so downstream
                // native code still gets a long.  Non-panicking fallback.
                _ => Value::Long(self.0 as i64),
            },
            b'D' => match sub {
                SUB_LONG_LO | SUB_LONG_HI => Value::Double(f64::from_bits(self.0)),
                // A genuine int slot has payload < 2^32 (`CompactValue::int`
                // stores `v as u32 as u64`), so widen it the way an `i2d`
                // would. A SUB_INT bit pattern whose payload has bits 32-46
                // set cannot be an int at all — under a `D` descriptor it is a
                // double whose raw bits collided into this sub-tag (see
                // `CompactValue::double_raw`), so reinterpret them. Exactly
                // the discrimination the `b'J'` arm above already applies for
                // the long side of the same collision.
                SUB_INT => {
                    let payload = self.0 & PAYLOAD_MASK;
                    if payload >> 32 == 0 {
                        Value::Double((payload as u32 as i32) as f64)
                    } else {
                        Value::Double(f64::from_bits(self.0))
                    }
                }
                // Null / Uninitialized with payload == 0 is JVMS §2.3's
                // default `0.0d` for an unwritten slot. A NON-ZERO payload is
                // impossible for a real null or uninitialized slot
                // (`CompactValue::null`/`uninitialized` both store payload 0),
                // so it is a collided double — again mirroring `b'J'`.
                SUB_NULL | SUB_UNINIT => {
                    if self.0 & PAYLOAD_MASK == 0 {
                        Value::Double(0.0)
                    } else {
                        Value::Double(f64::from_bits(self.0))
                    }
                }
                _ => Value::Double(f64::from_bits(self.0)),
            },
            b'F' => match sub {
                SUB_FLOAT => self.to_value(),
                // An Int landing in an F slot widens via JVMS i2f
                // numeric conversion — NOT a raw bit reinterpret.  The
                // sibling b'D'/SUB_INT and b'J'/SUB_INT branches already
                // do numeric conversion; `f32::from_bits` here would have
                // turned e.g. Int(1) into a denormal 1.4e-45 instead of 1.0.
                SUB_INT => Value::Float((self.0 & PAYLOAD_MASK) as u32 as i32 as f32),
                SUB_NULL | SUB_UNINIT => Value::Float(0.0),
                _ => self.to_value(),
            },
            b'I' | b'B' | b'C' | b'S' | b'Z' => match sub {
                SUB_INT => self.to_value(),
                SUB_NULL | SUB_UNINIT => Value::Int(0),
                _ => self.to_value(),
            },
            b'L' | b'[' => match sub {
                SUB_OBJECT => {
                    let payload = self.0 & PAYLOAD_MASK;
                    if object_payload_is_plausible(payload)
                        && crate::value::object_ref_payload_is_known(payload)
                    {
                        self.to_value()
                    } else {
                        note_object_degradation();
                        Value::Object(None)
                    }
                }
                SUB_NULL => self.to_value(),
                // A non-reference value landing in a reference slot becomes
                // null — the JVM verifier would have caught this pre-runtime,
                // so this is a defensive fallback. `SUB_RETADDR` and
                // `SUB_UNINIT` are included alongside the numeric primitives:
                // a `ReturnAddress` or `Uninitialized` is not a valid object
                // reference either, and routing them through `to_value()`
                // would leak `Value::ReturnAddress` / `Value::Uninitialized`
                // into a reference slot — inconsistent with the numeric arms.
                SUB_INT | SUB_FLOAT | SUB_LONG_LO | SUB_LONG_HI | SUB_RETADDR | SUB_UNINIT => {
                    Value::Object(None)
                }
                _ => self.to_value(),
            },
            // Unknown descriptor: keep legacy behavior.
            _ => self.to_value(),
        }
    }
}

// ---------------------------------------------------------------------------
// From<&Value> for CompactValue
// ---------------------------------------------------------------------------

impl From<&Value> for CompactValue {
    #[inline(always)]
    fn from(v: &Value) -> Self {
        CompactValue::from_value(*v)
    }
}

impl From<Value> for CompactValue {
    #[inline(always)]
    fn from(v: Value) -> Self {
        CompactValue::from_value(v)
    }
}

// ---------------------------------------------------------------------------
// Debug, PartialEq, Eq
// ---------------------------------------------------------------------------

impl fmt::Debug for CompactValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !is_nan_tagged(self.0) {
            // Untagged — could be Double or Long depending on context.
            let dval = f64::from_bits(self.0);
            let lval = self.0 as i64;
            return write!(
                f,
                "CompactValue(raw={:#018x}, as_double={}, as_long={})",
                self.0, dval, lval
            );
        }
        match (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK {
            SUB_INT => {
                let v = (self.0 & PAYLOAD_MASK) as u32 as i32;
                write!(f, "CompactValue::Int({})", v)
            }
            SUB_FLOAT => {
                let v = f32::from_bits((self.0 & PAYLOAD_MASK) as u32);
                write!(f, "CompactValue::Float({})", v)
            }
            SUB_OBJECT => {
                let ptr = self.0 & PAYLOAD_MASK;
                write!(f, "CompactValue::Object({:#x})", ptr)
            }
            SUB_NULL => write!(f, "CompactValue::Null"),
            SUB_UNINIT => write!(f, "CompactValue::Uninitialized"),
            SUB_RETADDR => {
                let pc = (self.0 & PAYLOAD_MASK) as u32;
                write!(f, "CompactValue::ReturnAddress({})", pc)
            }
            SUB_LONG_LO => write!(f, "CompactValue::LongLo({:#x})", self.0 & PAYLOAD_MASK),
            SUB_LONG_HI => write!(f, "CompactValue::LongHi({:#x})", self.0 & PAYLOAD_MASK),
            _ => write!(f, "CompactValue::Unknown({:#018x})", self.0),
        }
    }
}

impl PartialEq for CompactValue {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for CompactValue {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
