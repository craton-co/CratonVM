// SPDX-License-Identifier: Apache-2.0
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
pub fn note_nan_payload_collapse() {
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
pub fn nan_payload_collapse_count() -> u64 {
    NAN_PAYLOAD_COLLAPSES.load(Ordering::Relaxed)
}

/// Reset the NaN-payload counter, returning the previous value. Test-only.
pub fn reset_nan_payload_collapse_count() -> u64 {
    NAN_PAYLOAD_COLLAPSES.swap(0, Ordering::Relaxed)
}

/// Reset the degradation counter to zero, returning the previous value.
///
/// Intended for test harnesses and fuzzers that want to measure degradations
/// over a bounded window. Not used on any hot path.
///
/// **Prefer a delta** (`count_after - count_before`) over reset-then-assert.
/// The counter is process-wide, so a reset is not "my window starts here" — it
/// destroys whatever window any concurrent observer had already opened. This
/// crate's own tests measured absolute counts after a reset and were flaky for
/// exactly that reason; they now take
/// [`degrade_counter_test_lock`] *and* assert deltas, and no longer call this.
///
/// Resets **every** per-source slot, so the total and the breakdown stay
/// consistent. The one-shot diagnostics are deliberately NOT re-armed: they are
/// once-per-process by contract, and a fuzzer resetting the counter in a loop
/// must not turn them into a log flood.
#[inline]
pub fn reset_object_degradation_count() -> u64 {
    let mut prev_total: u64 = 0;
    let mut i: usize = 0;
    while i < DegradationSource::COUNT {
        prev_total = prev_total.wrapping_add(DEGRADATION_COUNTS[i].swap(0, Ordering::Relaxed));
        i += 1;
    }
    prev_total
}

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
pub fn note_object_degradation_at(source: DegradationSource, site: &'static str) {
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
        if crate::flags::runtime_var_os("CRATONVM_DBG_COMPACTVALUE").is_some() {
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
mod tests {
    use super::*;

    // -- Size assertion ------------------------------------------------------

    #[test]
    fn compact_value_is_8_bytes() {
        assert_eq!(std::mem::size_of::<CompactValue>(), 8);
    }

    // -- Int round-trips -----------------------------------------------------

    #[test]
    fn int_roundtrip_positive() {
        let cv = CompactValue::int(42);
        assert_eq!(cv.tag(), CompactTag::Int);
        assert_eq!(cv.as_int(), Some(42));
    }

    #[test]
    fn int_roundtrip_zero() {
        let cv = CompactValue::int(0);
        assert_eq!(cv.tag(), CompactTag::Int);
        assert_eq!(cv.as_int(), Some(0));
    }

    #[test]
    fn int_roundtrip_negative() {
        let cv = CompactValue::int(-1);
        assert_eq!(cv.as_int(), Some(-1));
        let cv = CompactValue::int(-999_999);
        assert_eq!(cv.as_int(), Some(-999_999));
    }

    #[test]
    fn int_roundtrip_min_max() {
        let cv_min = CompactValue::int(i32::MIN);
        assert_eq!(cv_min.as_int(), Some(i32::MIN));
        let cv_max = CompactValue::int(i32::MAX);
        assert_eq!(cv_max.as_int(), Some(i32::MAX));
    }

    // -- Long round-trips ----------------------------------------------------

    #[test]
    fn long_roundtrip_basic() {
        let cv = CompactValue::long(123_456_789_012_345i64);
        assert_eq!(cv.as_long_unchecked(), 123_456_789_012_345i64);
    }

    #[test]
    fn long_roundtrip_zero() {
        let cv = CompactValue::long(0);
        assert_eq!(cv.as_long_unchecked(), 0);
    }

    #[test]
    fn long_roundtrip_negative() {
        let cv = CompactValue::long(-1);
        assert_eq!(cv.as_long_unchecked(), -1);
        let cv = CompactValue::long(-999_999_999_999i64);
        assert_eq!(cv.as_long_unchecked(), -999_999_999_999i64);
    }

    #[test]
    fn long_roundtrip_min_max() {
        let cv_min = CompactValue::long(i64::MIN);
        assert_eq!(cv_min.as_long_unchecked(), i64::MIN);
        let cv_max = CompactValue::long(i64::MAX);
        assert_eq!(cv_max.as_long_unchecked(), i64::MAX);
    }

    // -- CRIT-1: `as_long()` must return `Some(_)` for every value that
    // `to_value()` resolves to `Value::Long(_)`, including longs whose
    // bit pattern collides with the NaN-tag space (e.g. `-1`, `i64::MIN`).
    // Prior behavior returned `None` for those collisions because
    // `as_long()` only handled the untagged path.

    #[test]
    fn as_long_handles_nan_tag_collisions() {
        // i64::MIN — high bit set, looks NaN-tagged.
        assert_eq!(CompactValue::long(i64::MIN).as_long(), Some(i64::MIN));
        // -1 — all-ones, the canonical collision case.
        assert_eq!(CompactValue::long(-1).as_long(), Some(-1));
        // 0 — untagged path; the baseline that always worked.
        assert_eq!(CompactValue::long(0).as_long(), Some(0));
    }

    #[test]
    fn as_long_handles_positive_longs() {
        for v in [1_i64, 42, 1_000_000, 123_456_789_012_345, i64::MAX] {
            assert_eq!(
                CompactValue::long(v).as_long(),
                Some(v),
                "as_long() failed for {v}"
            );
        }
    }

    #[test]
    fn as_long_handles_negative_longs() {
        for v in [-2_i64, -1000, -999_999_999_999, i64::MIN + 1] {
            assert_eq!(
                CompactValue::long(v).as_long(),
                Some(v),
                "as_long() failed for {v}"
            );
        }
    }

    /// `as_long()` and `to_value()` must agree: every bit pattern that
    /// decodes to `Value::Long(x)` must also yield `Some(x)` from
    /// `as_long()`.
    #[test]
    fn as_long_agrees_with_to_value_for_long_collisions() {
        for v in [
            i64::MIN,
            -1,
            0,
            1,
            i64::MAX,
            -42,
            i64::MIN + 1,
            i64::MAX - 1,
        ] {
            let cv = CompactValue::long(v);
            match cv.to_value() {
                Value::Long(x) => assert_eq!(
                    cv.as_long(),
                    Some(x),
                    "as_long()/to_value() disagree for {v}"
                ),
                Value::Double(_) => assert_eq!(
                    cv.as_long(),
                    Some(v),
                    "untagged long decoded as Double should still expose i64 via as_long()"
                ),
                other => panic!("unexpected Value variant for long {v}: {other:?}"),
            }
        }
    }

    /// `as_long()` must still return `None` for genuinely non-long tagged
    /// values (Int, Float, Object, Null, Uninitialized, ReturnAddress).
    #[test]
    fn as_long_returns_none_for_non_longs() {
        assert_eq!(CompactValue::int(42).as_long(), None);
        assert_eq!(CompactValue::float(1.5).as_long(), None);
        assert_eq!(CompactValue::object(0x1000).as_long(), None);
        assert_eq!(CompactValue::null().as_long(), None);
        assert_eq!(CompactValue::uninitialized().as_long(), None);
        assert_eq!(CompactValue::return_address(7).as_long(), None);
    }

    // -- NaN-box collision: bit-exact round-trip -----------------------------
    //
    // `CompactValue::long` stores longs verbatim, including bit patterns
    // that collide with the NaN-tagged sub-tag space (BC SM2 / LongArray
    // bug: lossy re-tag was corrupting `0xfffd…` longs to `0xffff…`).
    // Heap safety for slots whose bit pattern incidentally looks like
    // `SUB_OBJECT` is now enforced by the GC root scanners filtering
    // through `VmHeap::is_object_address`, not by the encoder.

    /// Helper: a colliding long bit pattern with a chosen 3-bit would-be
    /// sub-tag in bits 49-47 and a chosen 47-bit low payload.
    fn collide_with_subtag(sub: u64, low: u64) -> i64 {
        (NANBOX_BITS | (sub << SUBTAG_SHIFT) | (low & PAYLOAD_MASK)) as i64
    }

    #[test]
    fn long_collision_round_trip_exact_for_every_subtag() {
        // Bit-exact round-trip across every would-be sub-tag (0..=7) in the
        // colliding space, including SUB_OBJECT (2) — the exact BC SM2
        // regression case where the prior lossy re-tag was corrupting bits.
        for sub in 0..8u64 {
            for &low in &[0u64, PAYLOAD_MASK, 0x1_0000, 0x0BAD_BEEF] {
                // Skip a few ambiguous (sub, payload) pairs whose bit
                // pattern is identical to a real non-long value:
                //   * (sub=INT, payload < 2^32) — indistinguishable from a
                //     real int slot, where descriptor-J applies JVMS i2l
                //     widening (sign-extends the low 32 bits).
                //   * (sub=NULL/UNINIT, payload=0) — identical to
                //     `CompactValue::null()` / `uninitialized()`; J-decode
                //     honors JVMS §2.3 default 0L.
                // The verifier disambiguates in real bytecode (lstore never
                // writes a non-long into a J slot).
                if sub == SUB_INT && low >> 32 == 0 {
                    continue;
                }
                if low == 0 && (sub == SUB_NULL || sub == SUB_UNINIT) {
                    continue;
                }
                let v = collide_with_subtag(sub, low);
                let cv = CompactValue::long(v);
                assert_eq!(
                    cv.as_long_unchecked(),
                    v,
                    "long must round-trip bit-exact for {v:#018x} (sub={sub})",
                );
                assert_eq!(
                    cv.decode_by_descriptor(b'J'),
                    Value::Long(v),
                    "J-descriptor decode must round-trip bit-exact for {v:#018x}",
                );
            }
        }
    }

    /// Colliding longs whose natural sub-tag is already a long sub-tag
    /// (bits 49-48 set) round-trip bit-exactly — this covers `-1`, `-2`
    /// and every small-magnitude negative.
    #[test]
    fn long_natural_long_subtag_collisions_round_trip_exact() {
        for v in [
            -1i64,
            -2,
            -3,
            -1000,
            -999_999_999_999,
            -123_456,
            collide_with_subtag(SUB_LONG_LO, 0x12_3456),
            collide_with_subtag(SUB_LONG_HI, 0x7F_FFFF),
        ] {
            let cv = CompactValue::long(v);
            assert!(!cv.is_object());
            assert_eq!(cv.tag(), CompactTag::Long);
            assert_eq!(cv.as_long(), Some(v), "as_long mismatch for {v:#018x}");
            assert_eq!(cv.as_long_unchecked(), v);
            assert_eq!(cv.to_value(), Value::Long(v));
            assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(v));
            assert!(cv.is_category2());
        }
    }

    /// Non-colliding longs (the untagged fast path) — including the named
    /// boundary values — round-trip bit-exactly and are never objects.
    #[test]
    fn long_non_colliding_round_trip_exact() {
        for v in [
            0i64,
            1,
            42,
            -42,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
            i64::MAX - 1,
            123_456_789_012_345,
            0x0BAD_BEEF_DEAD_CAFEu64 as i64,
        ] {
            let cv = CompactValue::long(v);
            assert!(!cv.is_object(), "is_object() true for {v:#018x}");
            assert_ne!(cv.tag(), CompactTag::Object);
            assert_ne!(cv.tag(), CompactTag::ReturnAddress);
            assert_eq!(cv.as_long_unchecked(), v);
            assert_eq!(cv.as_long(), Some(v));
            assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(v));
        }
    }

    /// `i64::MIN` / `i64::MAX` are NOT in the colliding space (bit 63 alone,
    /// or bit 63 clear, does not satisfy the full NaN-box marker) — they take
    /// the untagged fast path.  Documented invariant guard.
    #[test]
    fn long_min_max_are_non_colliding() {
        assert!(!is_nan_tagged(i64::MIN as u64));
        assert!(!is_nan_tagged(i64::MAX as u64));
        assert!(!CompactValue::long(i64::MIN).is_object());
        assert!(!CompactValue::long(i64::MAX).is_object());
        assert_eq!(CompactValue::long(i64::MIN).as_long_unchecked(), i64::MIN);
        assert_eq!(CompactValue::long(i64::MAX).as_long_unchecked(), i64::MAX);
    }

    /// A colliding long routed through the J descriptor must always decode
    /// to `Value::Long(_)` regardless of its natural sub-tag (the operand
    /// stack's verifier-derived typing disambiguates). `pop_static_field_value`
    /// would reject any other variant. `SUB_INT` widens (i2l) rather than
    /// reinterprets bits.
    #[test]
    fn long_collision_descriptor_decode_always_returns_long() {
        for sub in 0..8u64 {
            let v = collide_with_subtag(sub, 0x55_5555);
            let cv = CompactValue::long(v);
            assert!(
                matches!(cv.decode_by_descriptor(b'J'), Value::Long(_)),
                "J-descriptor decode must return Value::Long for sub={sub}",
            );
            // SUB_INT applies JVMS i2l widening (sign-extend the low 32
            // bits) rather than reinterpreting the full 64-bit pattern.
            let expected = if sub == SUB_INT {
                Value::Long((v as u64 & PAYLOAD_MASK) as u32 as i32 as i64)
            } else {
                Value::Long(v)
            };
            assert_eq!(
                cv.decode_by_descriptor(b'J'),
                expected,
                "J-descriptor decode mismatch for sub={sub}",
            );
        }
    }

    /// `int_tag_collision_long` distinguishes a real int (payload < 2^32)
    /// from a long whose bits collide into the SUB_INT space (payload bits
    /// 32-46 set). Drives the JIT/OSR local-slot transfer fix.
    #[test]
    fn int_tag_collision_long_discriminates() {
        const NANBOX: u64 = 0xFFFC_0000_0000_0000;
        // Genuine ints: payload < 2^32 → None (kept as int).
        for n in [0i32, 1, -1, 42, i32::MIN, i32::MAX] {
            assert_eq!(
                CompactValue::int(n).int_tag_collision_long(),
                None,
                "int {n}"
            );
        }
        // SUB_INT-pattern longs with payload bits 32-46 set → Some(raw bits).
        for &bits in &[
            NANBOX | (1u64 << 32),
            NANBOX | 0x7FFF_FFFF_FFFF,
            0xFFFC_0001_ABCD_1234u64,
            0xFFFC_5555_5555_5555u64,
        ] {
            assert_eq!(
                CompactValue::long(bits as i64).int_tag_collision_long(),
                Some(bits as i64),
                "collision long {bits:#018x}",
            );
        }
        // Non-Int tags → None.
        assert_eq!(CompactValue::long(-1).int_tag_collision_long(), None); // SUB_LONG_HI
        assert_eq!(CompactValue::long(i64::MIN).int_tag_collision_long(), None); // untagged
        assert_eq!(CompactValue::float(1.5).int_tag_collision_long(), None);
        assert_eq!(CompactValue::null().int_tag_collision_long(), None);
        // Residual ambiguous case: payload < 2^32 with SUB_INT pattern is
        // indistinguishable from a real int → None (documented limitation).
        assert_eq!(
            CompactValue::long(NANBOX as i64).int_tag_collision_long(),
            None
        );
    }

    /// MEDIUM (2026-06-17): `to_value()` / `as_int()` SUB_INT/SUB_FLOAT
    /// long-collision guard. A primitive long whose verbatim bits collide
    /// into the SUB_INT/SUB_FLOAT sub-tag space with payload bits 32..46 set
    /// must NOT be truncated to a 32-bit Int/Float — it must be preserved
    /// bit-exact as `Value::Long`, and `as_int()` must decline it.
    #[test]
    fn to_value_int_float_collision_preserves_long() {
        // Genuine ints / floats (32-bit payload) still decode normally and
        // round-trip via as_int / as_float.
        for n in [0i32, 1, -1, 42, i32::MIN, i32::MAX] {
            let cv = CompactValue::int(n);
            assert_eq!(cv.to_value(), Value::Int(n), "real int {n}");
            assert_eq!(cv.as_int(), Some(n), "as_int real int {n}");
        }
        for f in [0.0f32, 1.5, -3.25, f32::MAX] {
            assert_eq!(
                CompactValue::float(f).to_value(),
                Value::Float(f),
                "real float {f}"
            );
        }

        // Colliding longs: SUB_INT / SUB_FLOAT sub-tag with payload bits
        // 32..46 set. These cannot be real int/float slots, so to_value()
        // must preserve the full i64 bit pattern (not truncate to 32 bits),
        // and as_int() must return None for the SUB_INT case.
        for &low in &[
            1u64 << 32,
            0x7FFF_FFFF_FFFF,
            0x1_ABCD_1234,
            0x5555_5555_5555,
        ] {
            if low >> 32 == 0 {
                continue; // not actually a collision payload
            }
            for &sub in &[SUB_INT, SUB_FLOAT] {
                let v = collide_with_subtag(sub, low);
                let cv = CompactValue::long(v);
                assert_eq!(
                    cv.to_value(),
                    Value::Long(v),
                    "collision long must be preserved bit-exact (sub={sub}, low={low:#x})",
                );
                // as_int must decline a SUB_INT-patterned collision long
                // rather than hand back a truncated low-32-bits int.
                if sub == SUB_INT {
                    assert_eq!(
                        cv.as_int(),
                        None,
                        "as_int must decline SUB_INT collision long (low={low:#x})",
                    );
                }
            }
        }
    }

    // -- Float round-trips ---------------------------------------------------

    #[test]
    fn float_roundtrip_basic() {
        let cv = CompactValue::float(3.14f32);
        assert_eq!(cv.tag(), CompactTag::Float);
        let f = cv.as_float().unwrap();
        assert_eq!(f.to_bits(), 3.14f32.to_bits());
    }

    #[test]
    fn float_roundtrip_zero() {
        let cv = CompactValue::float(0.0f32);
        assert_eq!(cv.as_float().unwrap().to_bits(), 0.0f32.to_bits());
    }

    #[test]
    fn float_roundtrip_negative_zero() {
        let cv = CompactValue::float(-0.0f32);
        assert_eq!(cv.as_float().unwrap().to_bits(), (-0.0f32).to_bits());
    }

    #[test]
    fn float_roundtrip_nan() {
        let cv = CompactValue::float(f32::NAN);
        assert!(cv.as_float().unwrap().is_nan());
    }

    // -- Double round-trips --------------------------------------------------

    #[test]
    fn double_roundtrip_basic() {
        let cv = CompactValue::double(2.718_281_828_459_045);
        assert_eq!(cv.tag(), CompactTag::Double);
        let d = cv.as_double().unwrap();
        // Bit equality: a stored value that is read back has been through NO rounding step, so the round trip is bit-exact or the slot corrupted it.
        assert_eq!(d.to_bits(), 2.718_281_828_459_045f64.to_bits());
    }

    #[test]
    fn double_roundtrip_zero() {
        let cv = CompactValue::double(0.0f64);
        assert_eq!(cv.as_double().unwrap().to_bits(), 0.0f64.to_bits());
    }

    #[test]
    fn double_roundtrip_infinity() {
        let cv = CompactValue::double(f64::INFINITY);
        assert_eq!(cv.as_double().unwrap(), f64::INFINITY);
    }

    #[test]
    fn double_roundtrip_neg_infinity() {
        let cv = CompactValue::double(f64::NEG_INFINITY);
        assert_eq!(cv.as_double().unwrap(), f64::NEG_INFINITY);
    }

    #[test]
    fn double_canonical_nan() {
        // Storing a NaN should produce the canonical NaN or preserve the bits
        // if they don't collide with our tag space.
        let cv = CompactValue::double(f64::NAN);
        let d = cv.as_double().unwrap();
        assert!(d.is_nan());
    }

    // -- Object pointer round-trips ------------------------------------------

    #[test]
    fn object_roundtrip() {
        let ptr: u64 = 0x0000_1234_5678_ABC0;
        let cv = CompactValue::object(ptr);
        assert_eq!(cv.tag(), CompactTag::Object);
        assert_eq!(cv.as_object_ptr(), Some(ptr));
    }

    #[test]
    fn object_large_pointer_47bit() {
        // Maximum 47-bit user-space pointer (bit 46 set, etc.)
        let ptr: u64 = 0x0000_7FFF_FFFF_FFF8; // 47-bit, 8-byte aligned
        let cv = CompactValue::object(ptr);
        assert_eq!(cv.as_object_ptr(), Some(ptr));
    }

    /// Pointer with bit 47+ set (out of 47-bit payload range) must panic via
    /// the unconditional `assert!` rather than silently truncate the high
    /// bits and produce a corrupted reference.  Regression test for the
    /// "47-bit pointer truncation" vulnerability.
    #[test]
    #[should_panic(expected = "exceeds 47-bit address space")]
    fn object_pointer_above_47bit_panics() {
        let ptr: u64 = 0x0001_0000_0000_0000; // bit 48 set — out of range
        let _ = CompactValue::object(ptr);
    }

    #[test]
    #[should_panic(expected = "not a plausible heap object pointer")]
    fn object_pointer_unaligned_panics() {
        let _ = CompactValue::object(0x1001);
    }

    #[test]
    #[should_panic(expected = "not a plausible heap object pointer")]
    fn object_pointer_null_page_panics() {
        let _ = CompactValue::object(0x8);
    }

    /// The checked constructor returns `None` instead of panicking for
    /// out-of-range pointers.
    #[test]
    fn try_from_pointer_rejects_above_47bit() {
        let ptr: u64 = 0x0001_0000_0000_0000;
        assert!(CompactValue::try_from_pointer(ptr).is_none());
    }

    #[test]
    fn try_from_pointer_rejects_null() {
        assert!(CompactValue::try_from_pointer(0).is_none());
    }

    #[test]
    fn try_from_pointer_rejects_unaligned() {
        assert!(CompactValue::try_from_pointer(0x1001).is_none());
    }

    #[test]
    fn try_from_pointer_rejects_null_page() {
        assert!(CompactValue::try_from_pointer(0x8).is_none());
        assert!(CompactValue::try_from_pointer(0xff8).is_none());
    }

    #[test]
    fn try_from_pointer_accepts_valid_pointer() {
        let ptr: u64 = 0x0000_1234_5678_ABC0;
        let cv = CompactValue::try_from_pointer(ptr).expect("valid 47-bit pointer");
        assert_eq!(cv.as_object_ptr(), Some(ptr));
    }

    #[test]
    fn try_from_pointer_accepts_max_47bit() {
        let ptr: u64 = 0x0000_7FFF_FFFF_FFF8;
        let cv = CompactValue::try_from_pointer(ptr).expect("max 47-bit pointer");
        assert_eq!(cv.as_object_ptr(), Some(ptr));
    }

    // -- Null ----------------------------------------------------------------

    #[test]
    fn null_roundtrip() {
        let cv = CompactValue::null();
        assert_eq!(cv.tag(), CompactTag::Null);
        assert!(cv.is_null());
        assert!(!cv.is_uninitialized());
        assert_eq!(cv.as_int(), None);
        assert_eq!(cv.as_object_ptr(), None);
    }

    // -- Uninitialized -------------------------------------------------------

    #[test]
    fn uninitialized_roundtrip() {
        let cv = CompactValue::uninitialized();
        assert_eq!(cv.tag(), CompactTag::Uninitialized);
        assert!(cv.is_uninitialized());
        assert!(!cv.is_null());
    }

    // -- ReturnAddress -------------------------------------------------------

    #[test]
    fn return_address_roundtrip() {
        let cv = CompactValue::return_address(999);
        assert_eq!(cv.tag(), CompactTag::ReturnAddress);
        assert_eq!(cv.as_return_address(), Some(999));
    }

    #[test]
    fn return_address_zero() {
        let cv = CompactValue::return_address(0);
        assert_eq!(cv.as_return_address(), Some(0));
    }

    #[test]
    fn return_address_max() {
        let cv = CompactValue::return_address(u32::MAX);
        assert_eq!(cv.as_return_address(), Some(u32::MAX));
    }

    // -- Tag extraction correctness ------------------------------------------

    #[test]
    fn tag_extraction_all_types() {
        assert_eq!(CompactValue::int(0).tag(), CompactTag::Int);
        assert_eq!(CompactValue::float(0.0).tag(), CompactTag::Float);
        assert_eq!(CompactValue::double(0.0).tag(), CompactTag::Double);
        assert_eq!(CompactValue::object(0x1000).tag(), CompactTag::Object);
        assert_eq!(CompactValue::null().tag(), CompactTag::Null);
        assert_eq!(
            CompactValue::uninitialized().tag(),
            CompactTag::Uninitialized
        );
        assert_eq!(
            CompactValue::return_address(0).tag(),
            CompactTag::ReturnAddress
        );
        // Long is untagged — tag() returns Double for untagged values.
        // The caller must know from JVM context that it is a Long.
        // We verify as_long_unchecked works correctly instead.
        let cv = CompactValue::long(42);
        assert_eq!(cv.as_long_unchecked(), 42);
    }

    // -- Cross-type rejection ------------------------------------------------

    #[test]
    fn int_not_extractable_as_float() {
        let cv = CompactValue::int(42);
        assert_eq!(cv.as_float(), None);
        assert_eq!(cv.as_double(), None);
        assert_eq!(cv.as_object_ptr(), None);
        assert!(!cv.is_null());
    }

    #[test]
    fn double_not_extractable_as_int() {
        let cv = CompactValue::double(1.0);
        assert_eq!(cv.as_int(), None);
        assert_eq!(cv.as_float(), None);
        assert_eq!(cv.as_object_ptr(), None);
    }

    // -- From<&Value> conversion ---------------------------------------------

    #[test]
    fn from_value_int() {
        let v = Value::Int(42);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_int(), Some(42));
    }

    #[test]
    fn from_value_long() {
        let v = Value::Long(i64::MAX);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_long_unchecked(), i64::MAX);
    }

    #[test]
    fn from_value_float() {
        let v = Value::Float(1.5);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_float(), Some(1.5f32));
    }

    #[test]
    fn from_value_double() {
        let v = Value::Double(2.5);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_double(), Some(2.5));
    }

    #[test]
    fn from_value_null() {
        let v = Value::Object(None);
        let cv = CompactValue::from(&v);
        assert!(cv.is_null());
    }

    #[test]
    fn from_value_uninit() {
        let v = Value::Uninitialized;
        let cv = CompactValue::from(&v);
        assert!(cv.is_uninitialized());
    }

    #[test]
    fn from_value_retaddr() {
        let v = Value::ReturnAddress(123);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_return_address(), Some(123));
    }

    #[test]
    fn from_value_object_ref() {
        let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        let v = Value::Object(Some(obj));
        let cv = CompactValue::from(&v);
        assert_eq!(cv.as_object_ptr(), Some(0x1234_5678_ABC0));
    }

    // -- to_value round-trip -------------------------------------------------

    #[test]
    fn to_value_int_roundtrip() {
        let v = Value::Int(-42);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.to_value(), v);
    }

    #[test]
    fn to_value_double_roundtrip() {
        let v = Value::Double(std::f64::consts::PI);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.to_value().as_double(), Some(std::f64::consts::PI));
    }

    #[test]
    fn to_value_as_long_roundtrip() {
        let v = Value::Long(i64::MIN);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.to_value_as_long(), v);
    }

    #[test]
    fn to_value_null_roundtrip() {
        let v = Value::Object(None);
        let cv = CompactValue::from(&v);
        assert!(cv.to_value().is_null());
    }

    #[test]
    fn to_value_uninit_roundtrip() {
        let v = Value::Uninitialized;
        let cv = CompactValue::from(&v);
        assert_eq!(cv.to_value(), v);
    }

    #[test]
    fn to_value_retaddr_roundtrip() {
        let v = Value::ReturnAddress(456);
        let cv = CompactValue::from(&v);
        assert_eq!(cv.to_value(), v);
    }

    // -- Equality ------------------------------------------------------------

    #[test]
    fn equality_same_values() {
        assert_eq!(CompactValue::int(10), CompactValue::int(10));
        assert_eq!(CompactValue::null(), CompactValue::null());
        assert_eq!(CompactValue::uninitialized(), CompactValue::uninitialized());
        assert_eq!(CompactValue::long(77), CompactValue::long(77));
        assert_eq!(CompactValue::double(1.0), CompactValue::double(1.0));
    }

    #[test]
    fn inequality_different_values() {
        assert_ne!(CompactValue::int(1), CompactValue::int(2));
        assert_ne!(CompactValue::null(), CompactValue::uninitialized());
        assert_ne!(CompactValue::int(0), CompactValue::float(0.0));
    }

    // -- T10.6 API additions ------------------------------------------------

    #[test]
    fn from_bits_to_bits_roundtrip() {
        let cv = CompactValue::int(42);
        let bits = cv.to_bits();
        let round = CompactValue::from_bits(bits);
        assert_eq!(round.as_int(), Some(42));
    }

    #[test]
    fn from_value_ctor_matches_from_ref() {
        let v = Value::Int(99);
        let a = CompactValue::from_value(v);
        let b = CompactValue::from(&v);
        assert_eq!(a, b);
    }

    #[test]
    fn from_value_owned() {
        let v = Value::Double(3.25);
        let a: CompactValue = v.into();
        assert_eq!(a.as_double(), Some(3.25));
    }

    #[test]
    fn zero_is_all_bits_zero() {
        let z = CompactValue::zero();
        assert_eq!(z.to_bits(), 0);
        // 0 is untagged → decoded as Double(0.0)
        assert_eq!(z.tag(), CompactTag::Double);
    }

    #[test]
    fn is_object_discriminates() {
        assert!(CompactValue::object(0x1000).is_object());
        assert!(!CompactValue::null().is_object());
        assert!(!CompactValue::int(0).is_object());
        assert!(!CompactValue::double(1.0).is_object());
    }

    #[test]
    fn update_object_ptr_rewrites_pointer() {
        let mut cv = CompactValue::object(0x1000);
        assert!(cv.update_object_ptr(0x2000).is_ok());
        assert_eq!(cv.as_object_ptr(), Some(0x2000));
    }

    #[test]
    fn update_object_ptr_noop_for_non_object() {
        let mut cv = CompactValue::int(42);
        // Non-object slot: even an out-of-range pointer is silently ignored —
        // there is no reference to corrupt.
        assert!(cv.update_object_ptr(0x9999).is_ok());
        assert_eq!(cv.as_int(), Some(42));
    }

    // -- MED audit 2026-05-24: hardened `update_object_ptr` -----------------

    /// `update_object_ptr` with a pointer that has bits set above bit 46
    /// must reject with `PointerOutOfRange` rather than silently masking the
    /// high bits — symmetric to the constructor `object()` which panics on
    /// the same condition. The slot is left unchanged so the caller can
    /// recover or escalate.
    #[test]
    fn update_object_ptr_rejects_above_47bit() {
        let original = 0x0000_1234_5678_ABC0;
        let mut cv = CompactValue::object(original);
        let bad: u64 = 0x0001_0000_0000_0000; // bit 48 set — out of range
        match cv.update_object_ptr(bad) {
            Err(CompactValueError::PointerOutOfRange { ptr }) => assert_eq!(ptr, bad),
            other => panic!("expected PointerOutOfRange, got {other:?}"),
        }
        // Slot untouched on failure.
        assert_eq!(cv.as_object_ptr(), Some(original));
    }

    #[test]
    fn update_object_ptr_rejects_implausible_pointer() {
        let original = 0x0000_1234_5678_ABC0;
        for bad in [0x1001u64, 0x8, 0xff8] {
            let mut cv = CompactValue::object(original);
            match cv.update_object_ptr(bad) {
                Err(CompactValueError::InvalidObjectPointer { ptr }) => assert_eq!(ptr, bad),
                other => panic!("expected InvalidObjectPointer, got {other:?}"),
            }
            assert_eq!(cv.as_object_ptr(), Some(original));
        }
    }

    /// `update_object_ptr` round-trips an in-range pointer — the canonical
    /// GC compaction path.
    #[test]
    fn update_object_ptr_round_trips_in_range() {
        let mut cv = CompactValue::object(0x0000_1000);
        // Maximum-magnitude in-range pointer.
        let new_ptr: u64 = 0x0000_7FFF_FFFF_FFF8;
        cv.update_object_ptr(new_ptr)
            .expect("47-bit pointer must succeed");
        assert_eq!(cv.as_object_ptr(), Some(new_ptr));
        assert!(cv.is_object());
        assert_eq!(cv.tag(), CompactTag::Object);
    }

    /// The `_unchecked` variant masks-and-stores in release builds; on a
    /// happy-path in-range pointer it behaves identically to the checked
    /// API. Used by the GC scanner where the address space is bounded by
    /// construction.
    #[test]
    fn update_object_ptr_unchecked_round_trips_in_range() {
        let mut cv = CompactValue::object(0x0000_1000);
        cv.update_object_ptr_unchecked(0x0000_2000);
        assert_eq!(cv.as_object_ptr(), Some(0x0000_2000));
    }

    /// `as_long_unchecked` returns the original i64 bit-exactly for every
    /// i64 value — including every NaN-box collision sub-tag, since
    /// `CompactValue::long` now stores verbatim.
    #[test]
    fn as_long_unchecked_returns_original_i64() {
        // Untagged fast path — original equals stored bits.
        for v in [0i64, 1, 2, 42, -42, 100, i64::MIN, i64::MAX] {
            let cv = CompactValue::long(v);
            assert_eq!(cv.as_long_unchecked(), v, "untagged long {v}");
            assert_eq!(cv.as_long_bits_unchecked(), v);
        }
        // Natural `SUB_LONG_LO`/`SUB_LONG_HI` collisions (small-magnitude
        // negatives whose top sub-tag bits are already 110 or 111).
        for v in [-1i64, -2, -3, -1000, -123_456_789_012_345] {
            let cv = CompactValue::long(v);
            assert_eq!(cv.as_long_unchecked(), v, "natural-collision long {v}");
            assert_eq!(cv.as_long_bits_unchecked(), v);
        }
        // Every collision sub-tag (0..=7) round-trips bit-exact — including
        // SUB_OBJECT (2), the BC SM2 regression case that motivated the
        // lossless verbatim encoding.
        for sub in 0u64..8 {
            let v = collide_with_subtag(sub, 0x12_3456);
            let cv = CompactValue::long(v);
            assert_eq!(cv.as_long_unchecked(), v, "collision sub={sub}");
            assert_eq!(cv.as_long_bits_unchecked(), v);
        }
    }

    /// Round-trip sanity for the new `as_long_bits_unchecked` name — same
    /// value as `as_long_unchecked` for every long pattern. Guards against
    /// accidental divergence if either method's body is edited.
    #[test]
    fn as_long_bits_unchecked_matches_as_long_unchecked() {
        for v in [
            0i64,
            1,
            -1,
            42,
            -42,
            i64::MIN,
            i64::MAX,
            i64::MIN + 1,
            i64::MAX - 1,
            -123_456_789_012_345,
        ] {
            let cv = CompactValue::long(v);
            assert_eq!(cv.as_long_unchecked(), cv.as_long_bits_unchecked());
        }
        // Also for the rare re-tag bit patterns.
        for sub in 0u64..8 {
            let v = collide_with_subtag(sub, 0x55_5555);
            let cv = CompactValue::long(v);
            assert_eq!(cv.as_long_unchecked(), cv.as_long_bits_unchecked());
        }
    }

    #[test]
    fn repr_transparent_layout_matches_u64() {
        // Critical for the Vec<CompactValue> ↔ Vec<u64> transmute used by
        // the operand-stack pool integration.
        assert_eq!(
            std::mem::size_of::<CompactValue>(),
            std::mem::size_of::<u64>()
        );
        assert_eq!(
            std::mem::align_of::<CompactValue>(),
            std::mem::align_of::<u64>()
        );
    }

    #[test]
    fn to_value_null_ptr_subobject_decodes_as_long() {
        let _guard = super::degrade_counter_test_lock();
        // BC SM2 fix (2026-05-28): `CompactValue::long` stores long bits
        // verbatim, so a slot with SUB_OBJECT bits and a null payload
        // could equally be a primitive long whose pattern landed here.
        // Real object slots are never null (`CompactValue::object` panics
        // on null) and never SUB_OBJECT-with-payload-zero, so `to_value`
        // returns `Value::Long` for safety — preserving the long bits
        // bit-exact rather than discarding them as `Object(None)`.
        let raw = make_tagged(SUB_OBJECT, 0);
        let cv = CompactValue::from_bits(raw);
        match cv.to_value() {
            Value::Long(x) => assert_eq!(x as u64, raw),
            other => panic!("expected Long, got {other:?}"),
        }
    }

    // ── T10.9.D regression tests for direct CompactValue hot path ────────

    #[test]
    fn t10_9_d_iadd_isub_imul_round_trip() {
        // Simulate a small loop of int arithmetic through CompactValue
        // push/pop to assert that the direct hot-path math is stable.
        let mut x: i32 = 0;
        for _ in 0..10 {
            // x = x + 1
            let a = CompactValue::int(x);
            let b = CompactValue::int(1);
            x = a.as_int().unwrap().wrapping_add(b.as_int().unwrap());
            // x = x * 2
            let a = CompactValue::int(x);
            let b = CompactValue::int(2);
            x = a.as_int().unwrap().wrapping_mul(b.as_int().unwrap());
            // x = x - 3
            let a = CompactValue::int(x);
            let b = CompactValue::int(3);
            x = a.as_int().unwrap().wrapping_sub(b.as_int().unwrap());
        }
        // Closed form: after each iter, x = 2*(x+1) - 3 = 2x - 1.
        // Starting at 0: -1, -3, -7, -15, -31, -63, -127, -255, -511, -1023.
        assert_eq!(x, -1023);
    }

    #[test]
    fn t10_9_d_lload_lstore_preserves_sign() {
        // Round-trip a signed long through the CompactValue helpers; this
        // guards against a mis-shifted payload decode on the hot path.
        //
        // Note: large-magnitude negative longs can have an upper bit
        // pattern that collides with the NaN-boxed tag; CompactValue does
        // not canonicalise long bits (only doubles), so the JVM context
        // that guarantees "this slot is a long" allows as_long_unchecked
        // to reproduce the exact i64 value without regard to tag().
        for original in [
            -1i64,
            -2i64,
            -1000i64,
            -123_456_789_012_345i64,
            i64::MIN,
            i64::MIN + 1,
            -123_456i64,
        ] {
            let cv = CompactValue::long(original);
            assert_eq!(
                cv.as_long_unchecked(),
                original,
                "round-trip failed for {original}"
            );
            // Through from_bits, the value survives too.
            let round = CompactValue::from_bits(cv.to_bits());
            assert_eq!(round.as_long_unchecked(), original);
        }
        // A small positive long should be category-2 and untagged:
        let cv_pos = CompactValue::long(42);
        assert!(cv_pos.is_category2());
        assert_eq!(cv_pos.tag(), CompactTag::Double); // untagged → Double
    }

    #[test]
    fn t10_9_d_fmul_fdiv_roundtrip() {
        // Float arithmetic through CompactValue: simulate a sequence
        // of fmul/fdiv on the hot path.
        let a = CompactValue::float(6.5f32);
        let b = CompactValue::float(2.0f32);
        // fmul: 6.5 * 2.0 = 13.0
        let prod = a.as_float().unwrap() * b.as_float().unwrap();
        let cv_prod = CompactValue::float(prod);
        assert!((cv_prod.as_float().unwrap() - 13.0f32).abs() < 1e-6);
        // fdiv: 13.0 / 4.0 = 3.25
        let c = CompactValue::float(4.0f32);
        let quot = cv_prod.as_float().unwrap() / c.as_float().unwrap();
        let cv_quot = CompactValue::float(quot);
        assert!((cv_quot.as_float().unwrap() - 3.25f32).abs() < 1e-6);
    }

    #[test]
    fn t10_9_d_dup2_preserves_long_category() {
        // Simulate the dup2 operation on a Long (a single CompactValue in
        // the compact slot model).  The slot must remain category-2 and
        // the long value round-trip across bit-wise copies.
        let cv = CompactValue::long(0x0BAD_BEEF_DEAD_CAFEu64 as i64);
        assert!(cv.is_category2());
        // "Dup" the slot bit-for-bit and confirm both copies decode.
        let copy1 = cv;
        let copy2 = cv;
        assert_eq!(copy1.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFEu64 as i64);
        assert_eq!(copy2.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFEu64 as i64);
        assert!(copy1.is_category2());
        assert!(copy2.is_category2());
    }

    #[test]
    fn t10_9_d_getfield_int_primitive() {
        // Simulate loading an int primitive from the heap (Value::Int)
        // and pushing it as a CompactValue on the stack.
        let heap_val = Value::Int(42);
        let cv = CompactValue::from_value(heap_val);
        assert_eq!(cv.as_int(), Some(42));
        assert_eq!(cv.tag(), CompactTag::Int);
        // Round-trip back to a Value for putfield-style paths.
        match cv.to_value() {
            Value::Int(v) => assert_eq!(v, 42),
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn t10_9_d_fibonacci_smoke() {
        // Iterative fibonacci using CompactValue for every operand; fib(10)
        // = 55.  Stresses iadd/istore/iload through the compact helpers.
        let mut a = CompactValue::int(0);
        let mut b = CompactValue::int(1);
        for _ in 0..10 {
            let sum = a.as_int().unwrap().wrapping_add(b.as_int().unwrap());
            a = b;
            b = CompactValue::int(sum);
        }
        assert_eq!(a.as_int(), Some(55));
    }

    #[test]
    fn is_category2_long_and_double() {
        // Both Long and Double are JVM category-2 types: is_category2
        // must return true for all "normal" cat-2 bit patterns.
        assert!(CompactValue::long(0).is_category2());
        assert!(CompactValue::long(1).is_category2());
        assert!(CompactValue::long(42).is_category2());
        assert!(CompactValue::long(i64::MAX).is_category2());
        // Negative longs that happen to have bits 49-47 = 111 also
        // decode as cat-2 via the SUB_LONG_HI branch.
        assert!(CompactValue::long(-1).is_category2());
        assert!(CompactValue::long(-2).is_category2());
        assert!(CompactValue::long(i64::MIN).is_category2());
        assert!(CompactValue::double(0.0).is_category2());
        assert!(CompactValue::double(std::f64::consts::PI).is_category2());

        // Non-cat2 types must all return false.
        assert!(!CompactValue::int(0).is_category2());
        assert!(!CompactValue::int(-1).is_category2());
        assert!(!CompactValue::float(1.5).is_category2());
        assert!(!CompactValue::null().is_category2());
        assert!(!CompactValue::uninitialized().is_category2());
        assert!(!CompactValue::object(0x1000).is_category2());
        assert!(!CompactValue::return_address(42).is_category2());
    }

    #[test]
    fn to_value_unaligned_ptr_subobject_decodes_as_long() {
        let _guard = super::degrade_counter_test_lock();
        // BC SM2 fix (2026-05-28): a NaN-tagged slot with SUB_OBJECT bits
        // but an unaligned payload cannot be a real object reference
        // (`CompactValue::object` panics on unaligned pointers), so it
        // must be a long-bit-pattern collision. `to_value()` preserves the
        // long bits — replacing the prior `Object(None)` degrade that
        // would silently drop the value.
        let raw_unaligned = make_tagged(SUB_OBJECT, 0x1001);
        let cv = CompactValue::from_bits(raw_unaligned);
        match cv.to_value() {
            Value::Long(x) => assert_eq!(x as u64, raw_unaligned),
            other => panic!("expected Long, got {other:?}"),
        }
    }

    // -- Debug ---------------------------------------------------------------

    #[test]
    fn debug_format_smoke() {
        // Just ensure Debug doesn't panic for each variant.
        let _ = format!("{:?}", CompactValue::int(42));
        let _ = format!("{:?}", CompactValue::long(99));
        let _ = format!("{:?}", CompactValue::float(1.0));
        let _ = format!("{:?}", CompactValue::double(2.0));
        let _ = format!("{:?}", CompactValue::object(0x1000));
        let _ = format!("{:?}", CompactValue::null());
        let _ = format!("{:?}", CompactValue::uninitialized());
        let _ = format!("{:?}", CompactValue::return_address(0));
    }

    // ── T10.9.E decode_by_descriptor tests ───────────────────────────────

    #[test]
    fn decode_by_descriptor_j_roundtrips_small_long() {
        // This is the exact failure mode reported in Session 93: a small
        // long value (5) stored via CompactValue::long(5) is untagged,
        // and to_value() decodes it as Value::Double(2.47e-323). The
        // descriptor-aware decoder must yield Value::Long(5).
        let cv = CompactValue::long(5);
        // Control: raw to_value() still returns Double for untagged.
        match cv.to_value() {
            Value::Double(_) => {}
            other => {
                panic!("expected untagged long to decode as Double via to_value; got {other:?}")
            }
        }
        // Descriptor-aware decode picks the correct type.
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(5));
    }

    #[test]
    fn decode_by_descriptor_j_handles_large_magnitude() {
        // Exercise values where the bit pattern has high bits collide with
        // the NaN-tag space; those use the SUB_LONG_LO/SUB_LONG_HI branch
        // inside decode_by_descriptor.
        for original in [
            0i64,
            1i64,
            42i64,
            -1i64,
            i64::MIN,
            i64::MAX,
            i64::MIN + 1,
            -123_456i64,
            123_456_789_012_345i64,
        ] {
            let cv = CompactValue::long(original);
            assert_eq!(
                cv.decode_by_descriptor(b'J'),
                Value::Long(original),
                "round-trip failed for {original}"
            );
        }
    }

    #[test]
    fn decode_by_descriptor_d_preserves_doubles() {
        // Doubles must decode as Value::Double, never as Long — symmetric
        // to the long path.
        for val in [
            0.0f64,
            1.0,
            -1.0,
            std::f64::consts::PI,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let cv = CompactValue::double(val);
            match cv.decode_by_descriptor(b'D') {
                Value::Double(d) => assert_eq!(d, val),
                other => panic!("expected Double({val}), got {other:?}"),
            }
        }
    }

    #[test]
    fn decode_by_descriptor_d_nan_is_canonical() {
        // A NaN double's bit pattern may collide with the tagged space —
        // CompactValue canonicalises it. decode_by_descriptor must still
        // return a Value::Double (NaN).
        let cv = CompactValue::double(f64::NAN);
        match cv.decode_by_descriptor(b'D') {
            Value::Double(d) => assert!(d.is_nan()),
            other => panic!("expected Double(NaN), got {other:?}"),
        }
    }

    #[test]
    fn decode_by_descriptor_f_on_float_roundtrips() {
        let cv = CompactValue::float(std::f32::consts::E);
        match cv.decode_by_descriptor(b'F') {
            Value::Float(f) => assert_eq!(f.to_bits(), std::f32::consts::E.to_bits()),
            other => panic!("expected Float(E), got {other:?}"),
        }
    }

    /// An Int landing in an `F` slot must widen via JVMS i2f *numeric*
    /// conversion, not a raw bit reinterpret.  Regression test for the
    /// `f32::from_bits` bug: `Value::Int(1)` decoded as `b'F'` previously
    /// produced the denormal `1.4e-45` (bit pattern 0x1) instead of `1.0`.
    /// The sibling `b'D'` / `b'J'` Int branches already convert numerically.
    #[test]
    fn decode_by_descriptor_f_int_widens_numerically() {
        for v in [0i32, 1, -1, 42, -42, 100, i16::MAX as i32, -12345] {
            let cv = CompactValue::int(v);
            match cv.decode_by_descriptor(b'F') {
                Value::Float(f) => {
                    assert_eq!(f, v as f32, "i2f numeric conversion expected for Int({v})",)
                }
                other => panic!("expected Float({}), got {other:?}", v as f32),
            }
        }
        // Mirror the sibling descriptors to confirm consistent semantics.
        assert_eq!(
            CompactValue::int(7).decode_by_descriptor(b'D'),
            Value::Double(7.0)
        );
        assert_eq!(
            CompactValue::int(7).decode_by_descriptor(b'F'),
            Value::Float(7.0)
        );
    }

    #[test]
    fn decode_by_descriptor_i_int_roundtrips() {
        for v in [0, 1, -1, i32::MAX, i32::MIN, 42] {
            let cv = CompactValue::int(v);
            assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(v));
        }
    }

    #[test]
    fn decode_by_descriptor_byte_short_char_boolean_use_int() {
        // JVMS: byte/short/char/boolean are represented as Int on the stack.
        let cv = CompactValue::int(127);
        assert_eq!(cv.decode_by_descriptor(b'B'), Value::Int(127));
        assert_eq!(cv.decode_by_descriptor(b'S'), Value::Int(127));
        assert_eq!(cv.decode_by_descriptor(b'C'), Value::Int(127));
        assert_eq!(cv.decode_by_descriptor(b'Z'), Value::Int(127));
    }

    #[test]
    fn decode_by_descriptor_reference_keeps_object() {
        let ptr: u64 = 0x0000_1234_5678_ABC0;
        let _obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
        let cv = CompactValue::object(ptr);
        match cv.decode_by_descriptor(b'L') {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
            other => panic!("expected Object(Some), got {other:?}"),
        }
        match cv.decode_by_descriptor(b'[') {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
            other => panic!("expected Object(Some), got {other:?}"),
        }
    }

    #[test]
    fn decode_by_descriptor_null_is_zero_for_primitive() {
        // A null slot in a primitive field decodes to the typed JVMS zero.
        let cv = CompactValue::null();
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(0));
        assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(0.0));
        assert_eq!(cv.decode_by_descriptor(b'F'), Value::Float(0.0));
        assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(0));
    }

    #[test]
    fn decode_by_descriptor_uninit_is_zero_for_primitive() {
        let cv = CompactValue::uninitialized();
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(0));
        assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(0.0));
        assert_eq!(cv.decode_by_descriptor(b'F'), Value::Float(0.0));
        assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(0));
    }

    #[test]
    fn decode_by_descriptor_int_widens_to_long() {
        // Upstream bytecode that forgot an i2l conversion landed an Int
        // on a J slot — descriptor-aware decode widens rather than
        // panics.
        let cv = CompactValue::int(-42);
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(-42));
    }

    #[test]
    fn decode_by_descriptor_unknown_falls_through() {
        // An unrecognized descriptor byte preserves legacy `to_value`
        // behavior so the defensive fallback cannot regress callers
        // that accidentally pass something like 'V' (void).
        let cv = CompactValue::int(99);
        assert_eq!(cv.decode_by_descriptor(b'V'), cv.to_value());
    }

    #[test]
    fn decode_by_descriptor_session93_reproduction() {
        // Exact bit pattern that broke ConcurrentHashMap.SIZECTL on the
        // KC16/KC26 boot path. Without descriptor awareness this slot
        // decodes as Value::Double(2.47e-323) — which `unsafe_offset`
        // previously fell through to 0 on, livelocking the CAS loop.
        let cv = CompactValue::long(5);
        // Not a tagged slot.
        assert!(!is_nan_tagged(cv.raw_bits()));
        // Raw to_value is Double — legacy behavior preserved.
        match cv.to_value() {
            Value::Double(_) => {}
            other => panic!("expected Double, got {other:?}"),
        }
        // Descriptor-aware decode returns Long(5) — the real fix.
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(5));
    }

    #[test]
    fn decode_by_descriptor_d_with_int_widens() {
        let cv = CompactValue::int(7);
        assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(7.0));
    }

    #[test]
    fn decode_by_descriptor_l_rejects_primitive() {
        // If a primitive is somehow stored in a reference slot, the
        // verifier would catch it pre-runtime — but defensively we
        // yield null so native code doesn't see a bogus pointer.
        let cv = CompactValue::int(99);
        assert!(matches!(cv.decode_by_descriptor(b'L'), Value::Object(None)));
    }

    // ── HIGH: NaN-box long↔object type-confusion — checked decoders ──────

    // The lock that serialises every degrading test lives at module scope
    // (`super::degrade_counter_test_lock`) so `crate::value`'s degrading tests
    // can take the same one — see its doc comment.

    #[test]
    fn decode_by_descriptor_l_rejects_unseen_subobject_payload() {
        let _guard = super::degrade_counter_test_lock();
        let raw = make_tagged(SUB_OBJECT, 0x0000_6CCC_DDDD_E000);
        let cv = CompactValue::from_bits(raw);
        assert!(matches!(cv.decode_by_descriptor(b'L'), Value::Object(None)));
    }

    /// A primitive long whose bits land in the `SUB_OBJECT` space with an
    /// aligned, non-null payload must not be decoded as a fabricated object by
    /// the context-free `to_value`. `to_value_checked` with a heap predicate
    /// that rejects the address also degrades it back to the bit-exact long
    /// and counts the degradation.
    /// Every SUB_OBJECT *encoder* must leave the payload decodable as a
    /// reference by the context-free decoder.
    ///
    /// The provenance bitmap (`crate::value::object_ref_payload_is_known`) is
    /// what `to_value` consults to tell a real reference from a primitive long
    /// whose bits collide with the SUB_OBJECT NaN-box pattern. Before these
    /// four encoders recorded, they could mint a slot their own decoder
    /// refused: `to_value` degraded the live reference to `Value::Long`, the
    /// interpreter then marked the local `LKIND_LONG`, and
    /// `Frame::scan_local_objects` skips LONG slots -- so the young sweep
    /// reclaimed an object a running frame still held. That is the
    /// `ClassId(0)` / `"result" is null` family in
    /// `fixed-suite-bugs/h2-suite-bugs/bug-h2-classid0-stale-address-family-FIXED.md`; two
    /// sites (`Frame::update_object_refs`, `ValueStack::update_object_refs`)
    /// had already been hand-patched with a `ObjectRef::from_raw` round-trip
    /// for exactly this reason, which is the symptom of a missing invariant
    /// one level down.
    ///
    /// Addresses here are deliberately outside any test heap so the assertion
    /// is about the ENCODER recording, not about some other test having
    /// happened to touch the same 64-byte granule.
    #[test]
    fn every_sub_object_encoder_records_decodable_provenance_cv() {
        let _guard = degrade_counter_test_lock();
        let before = object_degradation_count();

        // 1. `object(raw)` -- the raw-pointer constructor.
        let a1 = 0x0000_5A5A_0001_0000u64;
        assert!(
            matches!(CompactValue::object(a1).to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a1),
            "object() minted a slot to_value() refuses",
        );

        // 2. `try_from_pointer(raw)` -- the fallible constructor.
        let a2 = 0x0000_5A5A_0002_0000u64;
        let cv2 = CompactValue::try_from_pointer(a2).expect("plausible payload");
        assert!(
            matches!(cv2.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a2),
            "try_from_pointer() minted a slot to_value() refuses",
        );

        // 3. `update_object_ptr` -- the checked GC relocation writer.
        let a3 = 0x0000_5A5A_0003_0000u64;
        let mut cv3 = CompactValue::object(a1);
        cv3.update_object_ptr(a3).expect("plausible payload");
        assert!(
            matches!(cv3.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a3),
            "update_object_ptr() left a relocated reference undecodable",
        );

        // 4. `update_object_ptr_unchecked` -- the GC compaction hot path.
        let a4 = 0x0000_5A5A_0004_0000u64;
        let mut cv4 = CompactValue::object(a1);
        cv4.update_object_ptr_unchecked(a4);
        assert!(
            matches!(cv4.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a4),
            "update_object_ptr_unchecked() left a relocated reference undecodable",
        );

        // None of the eight decodes above may have counted a degradation.
        assert_eq!(
            object_degradation_count() - before,
            0,
            "an encoded reference degraded to Long",
        );
    }

    #[test]
    fn to_value_checked_degrades_fabricated_pointer_to_long() {
        let _guard = super::degrade_counter_test_lock();
        // Aligned (multiple of 8), non-null payload inside a SUB_OBJECT slot.
        let aligned = 0x0000_6BAD_CAFE_D000u64; // % 8 == 0, non-zero
        let raw = make_tagged(SUB_OBJECT, aligned);
        let cv = CompactValue::from_bits(raw);

        // Context-free path rejects the never-seen payload instead of
        // fabricating an object reference. Assert the DELTA rather than
        // resetting: the counter is process-wide, and zeroing it would
        // silently break whatever other test is mid-count.
        let base = object_degradation_count();
        match cv.to_value() {
            Value::Long(x) => assert_eq!(x as u64, raw),
            other => panic!("unchecked to_value should degrade to Long, got {other:?}"),
        }
        assert_eq!(object_degradation_count() - base, 1);

        // Checked path with a heap that says "not a live object" degrades to
        // the bit-exact long and records the reclassification.
        let base = object_degradation_count();
        match cv.to_value_checked(|_addr| false) {
            Value::Long(x) => assert_eq!(x as u64, raw),
            other => panic!("checked to_value should degrade to Long, got {other:?}"),
        }
        assert_eq!(object_degradation_count() - base, 1);
    }

    /// When the heap predicate confirms the address is live, the checked
    /// decoder returns the same object reference as `to_value` and does NOT
    /// count a degradation.
    #[test]
    fn to_value_checked_keeps_live_object() {
        let _guard = super::degrade_counter_test_lock();
        let aligned = 0x1234_5678_ABC0u64;
        let cv = CompactValue::object(aligned);
        let base = object_degradation_count();
        match cv.to_value_checked(|addr| addr == aligned) {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, aligned),
            other => panic!("expected live Object, got {other:?}"),
        }
        assert_eq!(object_degradation_count() - base, 0);
    }

    /// `to_value_checked` is identical to `to_value` for every non-object
    /// tag (and never invokes the heap closure for them).
    #[test]
    fn to_value_checked_matches_to_value_for_non_objects() {
        let samples = [
            CompactValue::int(-42),
            CompactValue::float(1.5),
            CompactValue::double(std::f64::consts::PI),
            CompactValue::null(),
            CompactValue::uninitialized(),
            CompactValue::return_address(7),
            CompactValue::long(5),  // untagged → Double
            CompactValue::long(-1), // SUB_LONG_HI collision
            CompactValue::long(i64::MIN),
        ];
        for cv in samples {
            // Closure must never be consulted for non-object slots.
            let checked = cv.to_value_checked(|_| panic!("heap closure called for non-object"));
            assert_eq!(checked, cv.to_value(), "mismatch for {cv:?}");
        }
    }

    /// `is_object_checked` returns true only when the payload is live and
    /// counts a degradation when a SUB_OBJECT slot fails heap validation.
    #[test]
    fn is_object_checked_validates_against_heap() {
        let _guard = super::degrade_counter_test_lock();
        let aligned = 0x4000u64;
        let real = CompactValue::object(aligned);
        assert!(real.is_object()); // unchecked pattern test
        assert!(real.is_object_checked(|addr| addr == aligned));

        let base = object_degradation_count();
        // Same bit pattern, but the heap denies it → false + degradation.
        assert!(!real.is_object_checked(|_| false));
        assert_eq!(object_degradation_count() - base, 1);

        // Non-object slots short-circuit without consulting the heap.
        assert!(!CompactValue::int(1).is_object_checked(|_| panic!("called")));
        assert!(!CompactValue::long(-1).is_object_checked(|_| panic!("called")));
    }

    /// The unchecked `to_value` SUB_OBJECT degrade (null / unaligned payload)
    /// bumps the observability counter so release-mode reclassifications are
    /// countable rather than invisible.
    #[test]
    fn to_value_unchecked_degrade_increments_counter() {
        let _guard = super::degrade_counter_test_lock();
        let base = object_degradation_count();
        // Null payload SUB_OBJECT → Long, counted.
        let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0)).to_value();
        // Unaligned payload SUB_OBJECT → Long, counted.
        let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x1001)).to_value();
        let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x0000_6AAA_BBBB_C000)).to_value();
        assert_eq!(object_degradation_count() - base, 3);
    }

    // ── HIGH: null-page plausibility guard for the unchecked SUB_OBJECT decode ──

    /// `object_payload_is_plausible` accepts only what a genuine
    /// `CompactValue::object` could produce — non-null, 8-byte aligned, and at
    /// or above the null-guard page — and rejects everything else.
    #[test]
    fn object_payload_plausibility_contract() {
        // Rejected: null, unaligned, and aligned-but-in-the-null-guard-page.
        assert!(!object_payload_is_plausible(0));
        assert!(!object_payload_is_plausible(4)); // unaligned
        assert!(!object_payload_is_plausible(0x1001)); // unaligned, above guard
        assert!(!object_payload_is_plausible(8)); // aligned but below guard
        assert!(!object_payload_is_plausible(NULL_GUARD_PAGE - 8)); // aligned, just under guard
                                                                    // Accepted: the guard-page boundary is inclusive, and anything aligned
                                                                    // above it.
        assert!(object_payload_is_plausible(NULL_GUARD_PAGE)); // 0x1000, aligned
        assert!(object_payload_is_plausible(0x1234_5678_ABC0));
        assert!(object_payload_is_plausible(PAYLOAD_MASK & !0b111)); // max aligned 47-bit
    }

    /// HIGH finding — crafted i64 attack: a primitive `long` whose verbatim
    /// bits set `NANBOX_BITS`, the `SUB_OBJECT` sub-tag, and an **aligned,
    /// non-null but null-page** payload must NOT fabricate an `ObjectRef`. The
    /// hardened unchecked `to_value` now treats it as a long-bit-pattern
    /// collision (the new null-page guard) and degrades it bit-exact, counting
    /// the reclassification. Without the guard this slot would have decoded as
    /// `Value::Object(Some(ObjectRef::from_raw(0x8)))` — a fabricated pointer.
    #[test]
    fn to_value_unchecked_null_page_subobject_degrades_to_long() {
        let _guard = super::degrade_counter_test_lock();
        for &aligned_low in &[0x8u64, 0x10, 0x100, NULL_GUARD_PAGE - 8] {
            // Aligned + non-null but inside the null-guard page → cannot be a
            // real heap reference.
            assert_eq!(aligned_low % 8, 0, "test payload must be 8-byte aligned");
            assert!(aligned_low != 0 && aligned_low < NULL_GUARD_PAGE);
            let raw = make_tagged(SUB_OBJECT, aligned_low);
            let cv = CompactValue::from_bits(raw);
            // The slot is bit-pattern-classified as an object (pure test)...
            assert!(cv.is_object());
            // ...but the hardened decoder refuses to fabricate a pointer.
            let base = object_degradation_count();
            match cv.to_value() {
                Value::Long(x) => assert_eq!(
                    x as u64, raw,
                    "null-page SUB_OBJECT long must round-trip bit-exact ({raw:#018x})",
                ),
                other => panic!(
                    "null-page SUB_OBJECT payload {aligned_low:#x} must NOT fabricate \
                     an object; got {other:?}",
                ),
            }
            assert_eq!(
                object_degradation_count() - base,
                1,
                "null-page degrade must be counted ({raw:#018x})",
            );
        }
    }

    /// The crafted-i64 attack expressed as a real `i64` flowing through the
    /// public `CompactValue::long` constructor (mirroring an `lxor`/`ladd`
    /// result an attacker could `lstore`): a long whose bits form an aligned,
    /// null-page SUB_OBJECT pattern is preserved as a long, never fabricated
    /// into an object reference.
    #[test]
    fn crafted_long_with_null_page_object_bits_is_not_an_object() {
        let _guard = super::degrade_counter_test_lock();
        // Construct the exact colliding i64: NANBOX | (SUB_OBJECT<<47) | 0x8.
        let crafted = (NANBOX_BITS | (SUB_OBJECT << SUBTAG_SHIFT) | 0x8) as i64;
        let cv = CompactValue::long(crafted);
        // Bit-exact storage (verbatim long encoding).
        assert_eq!(cv.as_long_unchecked(), crafted);
        // The unchecked decode degrades to the bit-exact long, not an object.
        match cv.to_value() {
            Value::Long(x) => assert_eq!(x, crafted),
            other => panic!("crafted long must not decode as object; got {other:?}"),
        }
        // The descriptor-aware path (the type-safe route) also yields a long.
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(crafted));
        // And the heap-checked path degrades it without ever calling the heap
        // closure for an implausible payload.
        match cv.to_value_checked(|_| panic!("heap closure called for null-page payload")) {
            Value::Long(x) => assert_eq!(x, crafted),
            other => panic!("checked decode of crafted long must be Long; got {other:?}"),
        }
    }

    /// Regression guard: the hardening must not reject object pointers that
    /// already crossed the unsafe `ObjectRef` construction boundary.
    #[test]
    fn to_value_unchecked_keeps_known_object() {
        for &ptr in &[NULL_GUARD_PAGE, 0x4000u64, 0x1234_5678_ABC0u64] {
            let _obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
            let cv = CompactValue::object(ptr);
            match cv.to_value() {
                Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
                other => panic!("known object {ptr:#x} must decode as Object; got {other:?}"),
            }
        }
    }

    // ── The shared degradation sink: null exclusion and per-source breakdown ──

    /// **The headline contract.** An ordinary Java null must never reach the
    /// counter from a *raw-word* source, because
    /// `plausible_heap_pointer(0) == false` puts it in the same cold arm as
    /// genuine garbage — counting it would read in the millions on a clean run.
    ///
    /// The asymmetry with the tagged sites below is deliberate and is the whole
    /// reason there are two entry points; assert both halves in one test so
    /// nobody "fixes" one of them into agreement with the other.
    #[test]
    fn null_ref_word_is_not_a_degradation_but_a_null_tagged_payload_is() {
        let _guard = super::degrade_counter_test_lock();

        // Raw-word source: null is ignored, non-null garbage is counted.
        let base_total = object_degradation_count();
        let base_jit = object_degradation_count_from(DegradationSource::Jit);
        assert!(
            !note_ref_word_degradation(0, DegradationSource::Jit, "test_null"),
            "a null reference word must not be reported as a degradation",
        );
        assert_eq!(
            object_degradation_count_from(DegradationSource::Jit) - base_jit,
            0,
            "note_ref_word_degradation counted an ordinary null",
        );
        assert!(
            note_ref_word_degradation(0x8D8D_8D8D, DegradationSource::Jit, "test_garbage"),
            "non-null implausible word must be reported as a degradation",
        );
        assert_eq!(
            object_degradation_count_from(DegradationSource::Jit) - base_jit,
            1,
        );
        assert_eq!(object_degradation_count() - base_total, 1);

        // Tagged source: a zero payload under SUB_OBJECT is an *unconstructible*
        // encoding (`object()` asserts non-null, `try_from_pointer` refuses it,
        // and a real null is SUB_NULL), so it is a collision and MUST count.
        let base_interp = object_degradation_count_from(DegradationSource::Interpreter);
        let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0)).to_value();
        assert_eq!(
            object_degradation_count_from(DegradationSource::Interpreter) - base_interp,
            1,
            "SUB_OBJECT with a zero payload is a collision, not a Java null",
        );
    }

    /// The six in-file call sites must not fire for an *ordinary* null, which
    /// in this encoding is `SUB_NULL` and never reaches a `SUB_OBJECT` arm.
    /// This is what keeps the interpreter's counter honest without needing the
    /// `raw == 0` filter the raw-word sources require.
    #[test]
    fn interpreter_sites_do_not_count_an_ordinary_null() {
        let _guard = super::degrade_counter_test_lock();
        let base = object_degradation_count();

        let null_slot = CompactValue::null();
        assert!(matches!(null_slot.to_value(), Value::Object(None)));
        assert!(matches!(
            null_slot.decode_by_descriptor(b'L'),
            Value::Object(None)
        ));
        assert!(matches!(
            null_slot.decode_by_descriptor(b'['),
            Value::Object(None)
        ));
        // The heap closure must never be consulted for a null slot either.
        assert!(matches!(
            null_slot.to_value_checked(|_| panic!("heap closure called for null slot")),
            Value::Object(None)
        ));
        assert!(!null_slot.is_object_checked(|_| panic!("heap closure called for null slot")));
        // `from_value(Object(None))` takes the dedicated null arm; `Object(Some)`
        // carries a `NonNull`, so the degrading arm cannot see a null at all.
        assert_eq!(
            CompactValue::from_value(Value::Object(None)).raw_bits(),
            null_slot.raw_bits(),
        );

        assert_eq!(
            object_degradation_count() - base,
            0,
            "an ordinary null was counted as a reference degradation",
        );
    }

    /// The total is *defined* as the sum of the per-source table, so a bump on
    /// any source shows up in both, and the breakdown attributes it correctly.
    #[test]
    fn degradation_total_is_the_sum_of_the_per_source_breakdown() {
        let _guard = super::degrade_counter_test_lock();
        let before = object_degradation_breakdown();
        let before_total = object_degradation_count();
        assert_eq!(
            before.iter().copied().sum::<u64>(),
            before_total,
            "total must equal the sum of the breakdown",
        );

        for source in DegradationSource::ALL {
            note_object_degradation_from(source);
        }

        let after = object_degradation_breakdown();
        for source in DegradationSource::ALL {
            assert_eq!(
                after[source.index()] - before[source.index()],
                1,
                "source '{}' did not record exactly one event",
                source.name(),
            );
        }
        assert_eq!(
            object_degradation_count() - before_total,
            DegradationSource::COUNT as u64,
        );
        assert_eq!(after.iter().copied().sum::<u64>(), object_degradation_count());
    }

    /// `note_object_degradation()` is the interpreter alias, so this crate's own
    /// sites are attributed to `Interpreter` and never to a sibling source.
    #[test]
    fn in_file_sites_are_attributed_to_the_interpreter_source() {
        let _guard = super::degrade_counter_test_lock();
        let before = object_degradation_breakdown();

        // A never-seen aligned payload: the `to_value` SUB_OBJECT degrade arm.
        let cv = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x0000_7EEE_1111_8000));
        assert!(matches!(cv.to_value(), Value::Long(_)));
        // The heap-denied arm of `is_object_checked`.
        assert!(!cv.is_object_checked(|_| false));

        let after = object_degradation_breakdown();
        assert_eq!(
            after[DegradationSource::Interpreter.index()]
                - before[DegradationSource::Interpreter.index()],
            2,
        );
        assert_eq!(
            after[DegradationSource::Jit.index()] - before[DegradationSource::Jit.index()],
            0,
        );
        assert_eq!(
            after[DegradationSource::ArrayElement.index()]
                - before[DegradationSource::ArrayElement.index()],
            0,
        );
    }

    /// `DegradationSource` table invariants: indices are dense, distinct, and
    /// within the tables' bounds, and every variant has a distinct name.
    #[test]
    fn degradation_source_table_is_dense_and_named() {
        for (i, source) in DegradationSource::ALL.into_iter().enumerate() {
            assert_eq!(source.index(), i, "ALL must be in index order");
            assert!(source.index() < DegradationSource::COUNT);
            assert!(!source.name().is_empty());
        }
        assert_eq!(DegradationSource::ALL.len(), DegradationSource::COUNT);
        assert_ne!(
            DegradationSource::Jit.name(),
            DegradationSource::ArrayElement.name(),
        );
    }

    /// The collapse set is exactly the negative quiet NaNs with mantissa bit 50
    /// set, the counter sees every one of them, and it sees nothing else.
    ///
    /// This is a characterisation test, not an aspiration: it pins the KNOWN
    /// lossy case of the encoding so that a future change to `NANBOX_BITS`
    /// cannot widen it unnoticed. If the encoding is ever made lossless, this
    /// test is what has to be rewritten — deliberately, rather than a silent
    /// count going up.
    #[test]
    fn the_nan_collapse_set_is_exactly_the_tag_pattern_and_is_counted() {
        // Not affected: every ordinary value, +/-0, the infinities, the
        // canonical quiet NaN, a POSITIVE payload NaN, and a negative NaN whose
        // mantissa bit 50 is CLEAR.
        let survivors: [u64; 8] = [
            0x3ff0_0000_0000_0000, // 1.0
            0xbff0_0000_0000_0000, // -1.0
            0x0000_0000_0000_0000, // +0.0
            0x8000_0000_0000_0000, // -0.0
            0x7ff0_0000_0000_0000, // +inf
            0xfff0_0000_0000_0000, // -inf
            0x7ff8_0000_0000_0001, // positive quiet NaN with a payload
            0xfff8_0000_0000_0000, // NEGATIVE quiet NaN, marker bit 50 clear
        ];
        let before = reset_nan_payload_collapse_count();
        let _ = before; // whatever earlier tests in this process did
        for bits in survivors {
            let cv = CompactValue::double(f64::from_bits(bits));
            assert_eq!(
                cv.0, bits,
                "{bits:#018x} must round-trip verbatim through CompactValue::double",
            );
        }
        assert_eq!(
            nan_payload_collapse_count(),
            0,
            "no survivor may be counted as a collapse",
        );

        // Affected: sign + exponent + quiet + marker all set. `0xFFFC…` is the
        // boundary; `0xFFFF_FFFF_FFFF_FFFF` is the pattern the commons-math NaN
        // census tripped over, and `Math.sqrt(-1.0)` widened from a float NaN
        // with mantissa bits 22 and 21 set lands here too.
        let collapsed: [u64; 4] = [
            0xFFFC_0000_0000_0000,
            0xFFFC_0000_0000_0001,
            0xFFFE_5E0E_8000_0000,
            0xFFFF_FFFF_FFFF_FFFF,
        ];
        for bits in collapsed {
            let cv = CompactValue::double(f64::from_bits(bits));
            assert_eq!(
                cv.0, CANONICAL_NAN,
                "{bits:#018x} collides with the tag space and must canonicalize",
            );
            // ...and it is still a NaN, which is why nothing above the encoding
            // notices: this is a payload loss, not a value change.
            assert!(f64::from_bits(cv.0).is_nan());
        }
        assert_eq!(
            nan_payload_collapse_count(),
            collapsed.len() as u64,
            "every collapse must be counted exactly once",
        );
        reset_nan_payload_collapse_count();
    }

    /// The other half of the pair: the SAME patterns `double()` collapses go
    /// through `double_raw()` verbatim, and nothing is counted.
    ///
    /// This is what the interpreter actually uses. Every store that reaches it
    /// writes a `KIND_DOUBLE` / `LKIND_DOUBLE` / `SlotType::Double` mark beside
    /// the slot, which is why dropping the canonicalization here is a fidelity
    /// win rather than a type-confusion bug — see `double_raw`'s contract, and
    /// `cratonvm_vm::runtime::frame`'s
    /// `tag_colliding_double_local_round_trips_and_is_never_a_root` for the
    /// GC half of the proof.
    #[test]
    fn double_raw_keeps_every_tag_colliding_payload_and_counts_nothing() {
        reset_nan_payload_collapse_count();
        // The four `double()` flattens, plus the boundary of the collision set
        // and the two `f2d` samples the census printed.
        let colliding: [u64; 7] = [
            0xFFFC_0000_0000_0000,
            0xFFFC_0000_0000_0001,
            0xFFFE_5E0E_8000_0000,
            0xFFFF_FFFF_FFFF_FFFF,
            0xFFFC_541A_8000_0000,
            0xFFFD_A846_C000_0000,
            0xFFFF_AF30_A000_0000,
        ];
        for bits in colliding {
            let cv = CompactValue::double_raw(f64::from_bits(bits));
            assert_eq!(
                cv.0, bits,
                "{bits:#018x} must round-trip verbatim through double_raw",
            );
            // The descriptor-aware read agrees, EXCEPT inside the one
            // sub-region a descriptor alone cannot separate: a `SUB_INT`
            // pattern whose payload is under 2^32 is bit-for-bit what
            // `CompactValue::int` produces for a small int, and a `SUB_NULL` /
            // `SUB_UNINIT` pattern with payload 0 is bit-for-bit a real null or
            // an unwritten slot. `decode_by_descriptor` has no kind mark to
            // consult, so it keeps answering `Int`-widened / `0.0d` there. Every
            // slot the interpreter actually stores a double in DOES carry that
            // mark, which is why the two exceptions below cost nothing at
            // runtime -- see `decode_arg_kind_aware` and `peek_kind_is_double`.
            let payload = bits & PAYLOAD_MASK;
            let sub = (bits >> SUBTAG_SHIFT) & SUBTAG_MASK;
            let descriptor_alone_is_ambiguous = (sub == SUB_INT && payload >> 32 == 0)
                || ((sub == SUB_NULL || sub == SUB_UNINIT) && payload == 0);
            if !descriptor_alone_is_ambiguous {
                match cv.decode_by_descriptor(b'D') {
                    Value::Double(d) => assert_eq!(d.to_bits(), bits),
                    other => panic!("D-descriptor decode of {bits:#018x} gave {other:?}"),
                }
            }
            // And `from_value_kinded` is the same encode by another door.
            assert_eq!(
                CompactValue::from_value_kinded(Value::Double(f64::from_bits(bits))).0,
                bits,
            );
        }
        assert_eq!(
            nan_payload_collapse_count(),
            0,
            "double_raw must never reach the collapse counter",
        );

        // Name the ambiguous sub-region explicitly: exactly the patterns a
        // *descriptor-only* decode still reads as something else. Both are
        // SUB_INT with a payload a real int could have had.
        for (bits, as_int) in [
            (0xFFFC_0000_0000_0000u64, 0i32),
            (0xFFFC_0000_0000_0001u64, 1i32),
        ] {
            assert_eq!(
                CompactValue::double_raw(f64::from_bits(bits)).0,
                bits,
                "the slot still holds the bits",
            );
            assert_eq!(
                CompactValue::int(as_int).0,
                bits,
                "…and they are bit-for-bit CompactValue::int({as_int}), which is                  why a decoder with no kind mark cannot tell them apart",
            );
        }

        // Non-colliding doubles are byte-identical through both constructors,
        // so nothing that already worked changes shape.
        for bits in [
            0x3ff0_0000_0000_0000u64,
            0x7ff8_0000_0000_0000,
            0xfff8_0000_0000_0000,
            0x0000_0000_0000_0000,
        ] {
            let d = f64::from_bits(bits);
            assert_eq!(CompactValue::double(d).0, CompactValue::double_raw(d).0);
        }
        assert_eq!(nan_payload_collapse_count(), 0);
        reset_nan_payload_collapse_count();
    }
}
