//! Compact 8-byte tagged value representation for JVM operand stacks.
//!
//! `CompactValue` uses a NaN-boxing scheme to represent all JVM value types
//! in exactly 8 bytes (one `u64`), halving the memory footprint compared to
//! the 16-byte `Value` enum.
//!
//! # Encoding
//!
//! **Double**: stored as raw IEEE 754 f64 bits. Any quiet NaN produced by
//! floating-point operations is canonicalized to avoid collisions with the
//! tagged encoding space.
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

/// Round-8 branch-hint: the SUB_OBJECT degraded path (null or unaligned
/// pointer arising from a stale slot) returns `Value::Object(None)` and
/// is taken essentially never in steady-state interpretation. Splitting
/// it out as a `#[cold]` non-inlined function gives LLVM permission to
/// place it off the hot path, freeing icache for the well-formed
/// branch in [`CompactValue::to_value`].
#[cold]
#[inline(never)]
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
    /// Most longs are stored as **raw i64 bits with no embedded tag** — the
    /// untagged fast path.  `tag()` returns `CompactTag::Double` for these and
    /// the caller uses `as_long_unchecked()` (or a descriptor-aware decode)
    /// when JVM instruction context indicates a Long.
    ///
    /// # NaN-box collision handling (heap-safety critical)
    ///
    /// A raw i64 whose top 14 bits coincide with the `NANBOX_BITS` marker
    /// (sign + exponent + quiet + tag-marker — i.e. `v as u64` in
    /// `0xFFFC_0000_0000_0000..=u64::MAX`, the negative longs in
    /// `[-2^50, -1]`) is **NaN-tagged by bit pattern**.  For such a value
    /// `tag()` decodes bits 49-47 as a 3-bit sub-tag.  If those bits land on
    /// `SUB_OBJECT` the slot is misclassified as an object reference:
    /// `is_object()` returns `true`, and the GC root scanner would then
    /// dereference raw integer data as an object pointer — heap corruption.
    /// Sub-tag `SUB_RETADDR` is a milder misclassification but still wrong.
    ///
    /// To prevent that, any colliding long whose natural sub-tag is **not**
    /// already one of the long sub-tags is re-tagged into the
    /// `SUB_LONG_LO` / `SUB_LONG_HI` space so that:
    /// * `is_object()` is **always `false`** for a value produced by `long`,
    /// * `tag()` **never** reports `Object` or `ReturnAddress` for a long,
    /// * the descriptor-aware J-decode path always sees `CompactTag::Long`
    ///   (never an `Object`/`Float` tag, which would raise a hard error).
    ///
    /// Colliding longs whose natural sub-tag is already `SUB_LONG_LO` /
    /// `SUB_LONG_HI` (bits 49-48 both set — every long in `[-2^48, -1]`,
    /// which covers `-1`, `-2`, … and all small-magnitude negatives) keep the
    /// verbatim representation and round-trip bit-exactly through
    /// `as_long()` / `as_long_unchecked()` / `to_value()`.
    ///
    /// The re-tagged path is reached only by large-magnitude negative longs
    /// in `[-2^50, -2^48)` whose three would-be sub-tag bits cannot be
    /// reconstructed from a single 8-byte slot (an 8-byte NaN-boxed slot
    /// physically cannot injectively hold all `2^50` colliding longs while
    /// also reserving the `SUB_OBJECT` pattern).  For those rare values the
    /// slot is guaranteed heap-safe (`is_object() == false`,
    /// `tag() == Long`); they decode verbatim from the re-tagged bits.
    #[inline]
    pub fn long(v: i64) -> Self {
        let bits = v as u64;
        if !is_nan_tagged(bits) {
            // Untagged fast path: the overwhelmingly common case.  `tag()`
            // reports `Double`; decoders reinterpret the raw bits as i64.
            return Self(bits);
        }
        // The bit pattern collides with the NaN-tag space.  Inspect the
        // would-be sub-tag.
        let sub = (bits >> SUBTAG_SHIFT) & SUBTAG_MASK;
        if sub == SUB_LONG_LO || sub == SUB_LONG_HI {
            // Natural sub-tag is already a long sub-tag (bits 49-48 set).
            // `tag()` => Long, `is_object()` => false, and the verbatim bits
            // round-trip exactly.  Keep them.
            return Self(bits);
        }
        // Natural sub-tag is one of Int/Float/Object/Null/Uninit/RetAddr.
        // Re-tag into the SUB_LONG space so the slot is never mistaken for an
        // object reference (heap-safety) and never raises a descriptor-decode
        // error.  Route on the would-be sub-tag's high bit so the two long
        // sub-tags are both exercised; the low 47 bits carry the payload.
        let long_sub = if sub < 3 { SUB_LONG_LO } else { SUB_LONG_HI };
        Self(make_tagged(long_sub, bits & PAYLOAD_MASK))
    }

    /// Create a CompactValue holding a 32-bit float.
    #[inline]
    pub fn float(v: f32) -> Self {
        Self(make_tagged(SUB_FLOAT, v.to_bits() as u64))
    }

    /// Create a CompactValue holding a 64-bit double.
    ///
    /// If the bit pattern of `v` collides with our NaN-tagged encoding space,
    /// it is replaced with the canonical quiet NaN.  This is lossless for all
    /// non-NaN doubles and for the standard quiet NaN; only exotic NaN payloads
    /// that happen to set our marker bits are canonicalized (Java mandates a
    /// single NaN anyway).
    #[inline]
    pub fn double(v: f64) -> Self {
        let bits = v.to_bits();
        if is_nan_tagged(bits) {
            // Collision: this double's bit pattern looks like a tagged value.
            // Replace with canonical NaN.
            Self(CANONICAL_NAN)
        } else {
            Self(bits)
        }
    }

    /// Create a CompactValue holding a non-null object reference.
    ///
    /// The pointer is stored in the 47-bit payload.  On x86-64 user-space
    /// addresses fit in 47 bits (bit 47 is always 0 for canonical lower-half
    /// addresses) and on AArch64 user-space the high bits are likewise zero.
    ///
    /// # Panics
    /// Panics (in **both** debug and release builds) if `ptr` is zero or has
    /// bits set outside the 47-bit payload range.  A pointer that does not fit
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
        assert!(
            ptr & !PAYLOAD_MASK == 0,
            "CompactValue::object: pointer {:#x} exceeds 47-bit address space",
            ptr
        );
        Self(make_tagged(SUB_OBJECT, ptr & PAYLOAD_MASK))
    }

    /// Checked constructor: returns `Some(CompactValue)` if `ptr` fits in the
    /// 47-bit payload, or `None` if it is null or has any bits set above bit
    /// 46.
    ///
    /// Safe counterpart to [`object`](Self::object) for callers that derive
    /// pointers from platform-supplied addresses (e.g. `mmap(MAP_FIXED)`,
    /// AArch64 LVA, x86-64 5-level paging) where the 47-bit assumption may
    /// not hold.
    #[inline]
    pub fn try_from_pointer(ptr: u64) -> Option<Self> {
        if ptr == 0 {
            return None;
        }
        if ptr & !PAYLOAD_MASK != 0 {
            return None;
        }
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
        match (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK {
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
    #[inline]
    pub fn as_int(&self) -> Option<i32> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_INT {
            return None;
        }
        // Payload is the zero-extended u32; reinterpret as i32.
        Some((self.0 & PAYLOAD_MASK) as u32 as i32)
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

    /// Reinterpret the raw bits as i64 without checking the tag.
    ///
    /// Use when the JVM instruction context guarantees this slot is a Long.
    #[inline]
    pub fn as_long_unchecked(&self) -> i64 {
        self.0 as i64
    }

    /// Extract an f32 if this value is tagged as Float.
    #[inline]
    pub fn as_float(&self) -> Option<f32> {
        if !is_nan_tagged(self.0) {
            return None;
        }
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_FLOAT {
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
        if (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK != SUB_OBJECT {
            return None;
        }
        Some(self.0 & PAYLOAD_MASK)
    }

    /// Returns `true` if this value is tagged as Null.
    #[inline]
    pub fn is_null(&self) -> bool {
        is_nan_tagged(self.0) && (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK == SUB_NULL
    }

    /// Returns `true` if this value is tagged as Uninitialized.
    #[inline]
    pub fn is_uninitialized(&self) -> bool {
        is_nan_tagged(self.0) && (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK == SUB_UNINIT
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
            Value::Object(Some(r)) => CompactValue::object(r.as_ptr() as u64),
            Value::Object(None) => CompactValue::null(),
            Value::ReturnAddress(pc) => CompactValue::return_address(pc),
            Value::Uninitialized => CompactValue::uninitialized(),
        }
    }

    // -- Conversion to Value ------------------------------------------------

    /// Convert back to the full `Value` enum.
    ///
    /// **Long vs Double ambiguity:** untagged values are decoded as `Double`.
    /// To decode as `Long`, use [`to_value_as_long`](Self::to_value_as_long).
    ///
    /// **Object references:** because `CompactValue` stores only the raw
    /// pointer (not a full `ObjectRef`), conversion back to
    /// `Value::Object(Some(_))` requires reconstructing the `ObjectRef` via
    /// unsafe `from_raw`.
    #[inline(always)]
    pub fn to_value(&self) -> Value {
        if !is_nan_tagged(self.0) {
            return Value::Double(f64::from_bits(self.0));
        }
        match (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK {
            SUB_INT => Value::Int((self.0 & PAYLOAD_MASK) as u32 as i32),
            SUB_FLOAT => Value::Float(f32::from_bits((self.0 & PAYLOAD_MASK) as u32)),
            SUB_OBJECT => {
                let ptr = (self.0 & PAYLOAD_MASK) as *mut u8;
                // Mirror the safety net in `decode_value` — null or unaligned
                // pointers arising from stale slots degrade to Object(None)
                // rather than panicking. The degraded paths are off the hot
                // line: every well-formed object slot satisfies both
                // predicates, so the branch predictor (and LLVM's basic-
                // block layout) should treat them as cold.
                if ptr.is_null() || (ptr as usize) % 8 != 0 {
                    cold_degraded_object_ptr()
                } else {
                    Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
                }
            }
            SUB_NULL => Value::Object(None),
            SUB_UNINIT => Value::Uninitialized,
            SUB_RETADDR => Value::ReturnAddress((self.0 & PAYLOAD_MASK) as u32),
            SUB_LONG_LO | SUB_LONG_HI => {
                // KC26 K1: `CompactValue::long(v)` stores the raw i64 bits
                // untagged.  For "normal" longs the slot is untagged and
                // `tag()` returns `Double`; for bit-pattern collisions
                // (e.g. -1_i64, i64::MIN) the NaN-tag pattern matches and
                // `tag()` returns `Long` via SUB_LONG_LO/HI.  The stored
                // bits are always the raw i64 value, so decode as such.
                // Returning `Value::Uninitialized` here (the prior behavior)
                // made every fast-path that did
                //   `if let (Value::Long(_), Value::Long(_)) = (a, b)`
                // fail for those collision values, breaking KC26 boot on
                // `io/quarkus/bootstrap/runner/Timing.staticInitStarted`
                // where `bootStartTime == -1L`.
                Value::Long(self.0 as i64)
            }
            _ => unreachable!(),
        }
    }

    /// Returns `true` if this slot carries an object reference (non-null).
    ///
    /// Used by the GC scanner to find root set entries without decoding the
    /// full `Value` enum.
    #[inline(always)]
    pub fn is_object(&self) -> bool {
        is_nan_tagged(self.0) && (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK == SUB_OBJECT
    }

    /// Replace the object-pointer payload if this slot holds an Object reference.
    ///
    /// Used by the GC compaction scanner to update roots after heap relocation.
    /// No-op for non-object slots.
    #[inline(always)]
    pub fn update_object_ptr(&mut self, new_ptr: u64) {
        if self.is_object() {
            debug_assert!(
                new_ptr & !PAYLOAD_MASK == 0,
                "CompactValue::update_object_ptr: pointer {:#x} exceeds 47-bit address space",
                new_ptr
            );
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
        let sub = (self.0 >> SUBTAG_SHIFT) & SUBTAG_MASK;
        match desc_byte {
            b'J' => match sub {
                // An explicit long pair decodes to the stored i64.
                SUB_LONG_LO | SUB_LONG_HI => Value::Long(self.0 as i64),
                // Int widens (JVMS i2l semantics) — defensive for bytecode
                // that forgot the conversion.
                SUB_INT => Value::Long((self.0 & PAYLOAD_MASK) as u32 as i32 as i64),
                // Null / Uninitialized → JVMS §2.3 default 0L.
                SUB_NULL | SUB_UNINIT => Value::Long(0),
                // Object / Float / ReturnAddress landing in a J slot is
                // upstream drift; reinterpret the raw bits so downstream
                // native code still gets a long.  Non-panicking fallback.
                _ => Value::Long(self.0 as i64),
            },
            b'D' => match sub {
                SUB_LONG_LO | SUB_LONG_HI => Value::Double(f64::from_bits(self.0)),
                SUB_INT => Value::Double(((self.0 & PAYLOAD_MASK) as u32 as i32) as f64),
                SUB_NULL | SUB_UNINIT => Value::Double(0.0),
                _ => Value::Double(f64::from_bits(self.0)),
            },
            b'F' => match sub {
                SUB_FLOAT => self.to_value(),
                // An Int landing in an F slot widens via JVMS i2f
                // numeric conversion — NOT a raw bit reinterpret.  The
                // sibling b'D'/SUB_INT and b'J'/SUB_INT branches already
                // do numeric conversion; `f32::from_bits` here would have
                // turned e.g. Int(1) into a denormal 1.4e-45 instead of 1.0.
                SUB_INT => Value::Float(
                    (self.0 & PAYLOAD_MASK) as u32 as i32 as f32,
                ),
                SUB_NULL | SUB_UNINIT => Value::Float(0.0),
                _ => self.to_value(),
            },
            b'I' | b'B' | b'C' | b'S' | b'Z' => match sub {
                SUB_INT => self.to_value(),
                SUB_NULL | SUB_UNINIT => Value::Int(0),
                _ => self.to_value(),
            },
            b'L' | b'[' => match sub {
                SUB_OBJECT | SUB_NULL => self.to_value(),
                // A non-reference value landing in a reference slot becomes
                // null — the JVM verifier would have caught this pre-runtime,
                // so this is a defensive fallback. `SUB_RETADDR` and
                // `SUB_UNINIT` are included alongside the numeric primitives:
                // a `ReturnAddress` or `Uninitialized` is not a valid object
                // reference either, and routing them through `to_value()`
                // would leak `Value::ReturnAddress` / `Value::Uninitialized`
                // into a reference slot — inconsistent with the numeric arms.
                SUB_INT | SUB_FLOAT | SUB_LONG_LO | SUB_LONG_HI
                | SUB_RETADDR | SUB_UNINIT => Value::Object(None),
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
            return write!(f, "CompactValue(raw={:#018x}, as_double={}, as_long={})", self.0, dval, lval);
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
        for v in [i64::MIN, -1, 0, 1, i64::MAX, -42, i64::MIN + 1, i64::MAX - 1] {
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

    // -- CRITICAL: NaN-box collision corruption -----------------------------
    //
    // `CompactValue::long` must NEVER produce a slot that `is_object()`
    // classifies as an object reference, and `tag()` must never report
    // `Object`/`ReturnAddress` for it.  Otherwise the GC root scanner
    // dereferences raw integer data as an object pointer => heap corruption.

    /// Helper: a colliding long bit pattern with a chosen 3-bit would-be
    /// sub-tag in bits 49-47 and a chosen 47-bit low payload.
    fn collide_with_subtag(sub: u64, low: u64) -> i64 {
        (NANBOX_BITS | (sub << SUBTAG_SHIFT) | (low & PAYLOAD_MASK)) as i64
    }

    #[test]
    fn long_never_classified_as_object() {
        // Every would-be sub-tag (0..=7) in the colliding space, including
        // SUB_OBJECT (2) which is the heap-corruption vector, and a couple
        // of payloads (0, all-ones, an aligned-pointer-shaped value).
        for sub in 0..8u64 {
            for &low in &[0u64, PAYLOAD_MASK, 0x1_0000, 0x0BAD_BEEF] {
                let v = collide_with_subtag(sub, low);
                let cv = CompactValue::long(v);
                assert!(
                    !cv.is_object(),
                    "is_object() must be false for long {v:#018x} (sub={sub})",
                );
                assert_ne!(
                    cv.tag(),
                    CompactTag::Object,
                    "tag() must not be Object for long {v:#018x} (sub={sub})",
                );
                assert_ne!(
                    cv.tag(),
                    CompactTag::ReturnAddress,
                    "tag() must not be ReturnAddress for long {v:#018x} (sub={sub})",
                );
            }
        }
    }

    #[test]
    fn long_object_subtag_collision_is_not_an_object() {
        // The exact corruption case: a long whose bits 49-47 == SUB_OBJECT.
        let v = collide_with_subtag(SUB_OBJECT, 0x1_0000);
        let cv = CompactValue::long(v);
        assert!(!cv.is_object());
        assert_eq!(cv.tag(), CompactTag::Long);
        // as_object_ptr must not hand the GC a fake pointer.
        assert_eq!(cv.as_object_ptr(), None);
    }

    #[test]
    fn long_retaddr_subtag_collision_is_not_a_retaddr() {
        let v = collide_with_subtag(SUB_RETADDR, 0x2_0000);
        let cv = CompactValue::long(v);
        assert!(!cv.is_object());
        assert_ne!(cv.tag(), CompactTag::ReturnAddress);
        assert_eq!(cv.tag(), CompactTag::Long);
        assert_eq!(cv.as_return_address(), None);
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

    /// A re-tagged colliding long must never decode through the J-descriptor
    /// path to an Object/Float tag (which `pop_static_field_value` rejects
    /// with a hard error) — its tag is always Long.
    #[test]
    fn long_collision_descriptor_decode_never_errors_tag() {
        for sub in 0..8u64 {
            let v = collide_with_subtag(sub, 0x55_5555);
            let cv = CompactValue::long(v);
            // Must be a long-carrying tag for the J-descriptor fast path.
            assert!(
                matches!(cv.tag(), CompactTag::Long),
                "re-tagged long must report CompactTag::Long (sub={sub})",
            );
            // Decoding as J yields a Long, never an Object/Float variant.
            assert!(matches!(cv.decode_by_descriptor(b'J'), Value::Long(_)));
        }
    }

    // -- Float round-trips ---------------------------------------------------

    #[test]
    fn float_roundtrip_basic() {
        let cv = CompactValue::float(3.14f32);
        assert_eq!(cv.tag(), CompactTag::Float);
        let f = cv.as_float().unwrap();
        assert!((f - 3.14f32).abs() < 1e-6);
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
        assert!((d - 2.718_281_828_459_045).abs() < 1e-12);
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
        assert_eq!(CompactValue::uninitialized().tag(), CompactTag::Uninitialized);
        assert_eq!(CompactValue::return_address(0).tag(), CompactTag::ReturnAddress);
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
        cv.update_object_ptr(0x2000);
        assert_eq!(cv.as_object_ptr(), Some(0x2000));
    }

    #[test]
    fn update_object_ptr_noop_for_non_object() {
        let mut cv = CompactValue::int(42);
        cv.update_object_ptr(0x9999);
        assert_eq!(cv.as_int(), Some(42));
    }

    #[test]
    fn repr_transparent_layout_matches_u64() {
        // Critical for the Vec<CompactValue> ↔ Vec<u64> transmute used by
        // the operand-stack pool integration.
        assert_eq!(std::mem::size_of::<CompactValue>(), std::mem::size_of::<u64>());
        assert_eq!(std::mem::align_of::<CompactValue>(), std::mem::align_of::<u64>());
    }

    #[test]
    fn to_value_null_ptr_degrades_to_null_obj() {
        // CompactValue never produces a null-pointer SUB_OBJECT via its
        // normal constructors (debug_assert in `object()`), but a manually-
        // crafted bit pattern that passes the NaN-tag check must degrade to
        // Value::Object(None) rather than panic.
        let raw_null_object = make_tagged(SUB_OBJECT, 0);
        let cv = CompactValue::from_bits(raw_null_object);
        match cv.to_value() {
            Value::Object(None) => {}
            other => panic!("expected Object(None), got {other:?}"),
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
    fn to_value_unaligned_ptr_degrades_to_null_obj() {
        // An unaligned pointer in a stale slot must not crash the interpreter.
        let raw_unaligned = make_tagged(SUB_OBJECT, 0x1001);
        let cv = CompactValue::from_bits(raw_unaligned);
        match cv.to_value() {
            Value::Object(None) => {}
            other => panic!("expected Object(None), got {other:?}"),
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
            other => panic!("expected untagged long to decode as Double via to_value; got {other:?}"),
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
        for val in [0.0f64, 1.0, -1.0, std::f64::consts::PI, f64::INFINITY, f64::NEG_INFINITY] {
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
            Value::Float(f) => assert!((f - std::f32::consts::E).abs() < 1e-6),
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
                Value::Float(f) => assert_eq!(
                    f, v as f32,
                    "i2f numeric conversion expected for Int({v})",
                ),
                other => panic!("expected Float({}), got {other:?}", v as f32),
            }
        }
        // Mirror the sibling descriptors to confirm consistent semantics.
        assert_eq!(CompactValue::int(7).decode_by_descriptor(b'D'), Value::Double(7.0));
        assert_eq!(CompactValue::int(7).decode_by_descriptor(b'F'), Value::Float(7.0));
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
}
