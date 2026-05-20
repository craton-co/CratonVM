use std::fmt;
use std::ptr::NonNull;

/// A JVM runtime value.
///
/// Represents any value that can be stored in a local variable or on the operand stack.
/// The JVM specification defines computational types that map to these variants.
///
/// **Layout invariant:** this enum is compile-time asserted below to be
/// exactly 16 bytes with alignment ≤ 8 on x86-64 / AArch64.  Adding `repr(C)`
/// would change size to 24 bytes and break JIT slot layout.  The static
/// asserts at the bottom of this file own the invariant; the `jit` crate
/// replicates them as belt-and-suspenders.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// A 32-bit integer (also used for boolean, byte, char, short).
    Int(i32),

    /// A 64-bit long integer. Occupies two stack/local slots.
    Long(i64),

    /// A 32-bit IEEE 754 float.
    Float(f32),

    /// A 64-bit IEEE 754 double. Occupies two stack/local slots.
    Double(f64),

    /// A reference to an object or array. Represented as a raw pointer internally.
    /// `None` represents the `null` reference.
    Object(Option<ObjectRef>),

    /// A return address for `jsr`/`ret` instructions (used by older `finally` implementations).
    ReturnAddress(u32),

    /// Uninitialized slot placeholder (e.g., second slot of a long/double, or unset local).
    Uninitialized,
}

/// An opaque reference to a heap-allocated Java object.
///
/// This will be replaced with a proper GC-managed pointer in Phase 6.
/// For now it's a simple wrapper around a raw pointer.
///
/// **A4 (architectural improvement):** the inner pointer is `NonNull<u8>`,
/// not `*mut u8`. This gives `Option<ObjectRef>` (and thus
/// `Value::Object(Option<ObjectRef>)`) the niche optimization: the `None`
/// case occupies the all-zero bit pattern, so the option is pointer-sized
/// and tag-free. Layout-compatible with `*mut u8` for FFI / casting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    ptr: NonNull<u8>,
}

// SAFETY: ObjectRef is a Copy wrapper around a non-null pointer.  We implement
// Hash based on the pointer value so ObjectRef can be used as a HashMap key
// (e.g. in SharedVm.class_mirrors_reverse).  Hashing a raw pointer is
// deterministic within a single process run.
impl std::hash::Hash for ObjectRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.ptr.as_ptr() as usize).hash(state);
    }
}

/// Round-5: shared helper used by both `ObjectRef::from_raw` and
/// `ObjectRef::from_raw_nonnull` to debug-assert 8-byte alignment.
///
/// Why a single helper: prior to this change the two constructors used
/// different assertion mechanisms (`from_raw` panicked unconditionally,
/// `from_raw_nonnull` only `debug_assert!`-ed). Either both are load-bearing
/// in release or neither is — and per the `from_raw_nonnull` analysis, the
/// post-condition is already guaranteed by callers (and by the heap layout
/// invariants). The helper makes both call sites use identical wording so a
/// future refactor can't accidentally re-introduce the asymmetry.
#[inline]
fn debug_assert_aligned(ptr: *mut u8) {
    debug_assert!(
        (ptr as usize) % 8 == 0,
        "ObjectRef pointer not 8-byte aligned: {ptr:p}"
    );
}

impl ObjectRef {
    /// Create a new object reference from a raw pointer.
    ///
    /// # Safety
    /// The caller must ensure the pointer is valid and points to a properly allocated object.
    /// The pointer must be non-null and aligned to at least 8 bytes (heap object alignment).
    ///
    /// **MED-1:** the null check here is `debug_assert!`-only. All callers
    /// in the value layer (notably `decode_value` for `VTAG_OBJECT`) already
    /// null-check upstream and route null bit patterns to `Value::Object(None)`
    /// without entering this constructor, so the release build elides the
    /// redundant branch. Debug builds still trip the assert if a caller
    /// violates the precondition.
    // SAFETY: callers guarantee `ptr` is non-null and aligned to 8 bytes
    // (heap object alignment). Both preconditions are checked via
    // `debug_assert!` only — release builds elide the branch.
    //
    // Round-7: alignment check downgraded from unconditional panic to
    // `debug_assert!` for consistency with `from_raw_nonnull`. The hot
    // path (object dereference) does not check alignment in release
    // anyway — a misaligned slot would corrupt the heap before reaching
    // this constructor — so the release-build panic was redundant.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut u8) -> Self {
        debug_assert!(
            !ptr.is_null(),
            "ObjectRef::from_raw called with null pointer"
        );
        // Round-5: shared alignment helper. Both `from_raw` and
        // `from_raw_nonnull` go through `debug_assert_aligned`, so the
        // assertion text and check site are identical.
        // SAFETY: callers guarantee `ptr` is non-null and 8-byte aligned.
        debug_assert_aligned(ptr);
        Self {
            ptr: unsafe { NonNull::new_unchecked(ptr) },
        }
    }

    /// Create a new object reference from an already-validated `NonNull<u8>`.
    ///
    /// Prefer this over [`from_raw`] when the caller already holds a
    /// `NonNull` — it avoids re-checking nullness in any build.
    ///
    /// # Safety
    /// The pointer must point to a properly allocated, 8-byte-aligned
    /// heap object for the lifetime that the returned `ObjectRef` is used.
    // SAFETY: callers guarantee `ptr` is aligned to 8 bytes (heap object
    // alignment); checked in debug only.
    #[inline]
    pub unsafe fn from_raw_nonnull(ptr: NonNull<u8>) -> Self {
        // SAFETY: callers guarantee 8-byte alignment; checked in debug only
        // via the shared `debug_assert_aligned` helper for consistency with
        // `from_raw`.
        debug_assert_aligned(ptr.as_ptr());
        Self { ptr }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// Return the underlying `NonNull<u8>` without going through a raw pointer round-trip.
    pub fn as_nonnull(&self) -> NonNull<u8> {
        self.ptr
    }
}

// SAFETY: ObjectRef implements Send and Sync.
//
// `NonNull<u8>` is `!Send + !Sync` by default (same as `*mut u8`), so the
// `unsafe impl`s below are still required after the A4 switch from
// `*mut u8` to `NonNull<u8>` — the niche optimization is a layout change,
// not an auto-trait change.
//
// Soundness argument:
//
// 1. **Lifetime guarantee** — The pointer refers to a GC-managed heap object.
//    The collector will not free an object while any root (thread stack, global
//    root set) holds an ObjectRef to it. Roots are scanned at safepoints.
//
// 2. **Mutation protocol** — All field reads/writes go through the VM's
//    field-access helpers (`get_field` / `set_field` on `NativeContext`), which
//    hold the appropriate monitor lock or use atomic operations for volatile
//    fields. Direct pointer mutation is never performed outside the GC.
//
// 3. **Compaction safety** — GC stop-the-world pauses ensure no thread
//    observes a half-moved object during relocation. Pointer updates happen
//    atomically from each thread's perspective.
//
// 4. **Current execution model** — The VM currently executes Java threads
//    on a single OS thread with cooperative scheduling. This makes the
//    Send+Sync bounds trivially sound. When true OS-thread parallelism is
//    added (threading/jvm_thread.rs), the monitor protocol in (2) provides
//    the necessary synchronization.
unsafe impl Send for ObjectRef {}
unsafe impl Sync for ObjectRef {}

impl Value {
    /// Returns true if this value is the null reference.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Object(None))
    }

    /// Returns true if this value occupies two stack slots (long or double).
    pub fn is_category2(&self) -> bool {
        matches!(self, Value::Long(_) | Value::Double(_))
    }

    /// Extract an int value, or None if this isn't an Int.
    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a long value, or None if this isn't a Long.
    pub fn as_long(&self) -> Option<i64> {
        match self {
            Value::Long(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a float value, or None if this isn't a Float.
    pub fn as_float(&self) -> Option<f32> {
        match self {
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a double value, or None if this isn't a Double.
    pub fn as_double(&self) -> Option<f64> {
        match self {
            Value::Double(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract a non-null object reference.
    ///
    /// **MED-P3 flatten:** returns `Some(r)` only when this is a
    /// `Value::Object(Some(r))` (a real, non-null reference). Returns
    /// `None` for both `Value::Object(None)` (the JVM `null`) and any
    /// non-Object variant. Callers that need to distinguish "is this an
    /// Object slot at all" from "is this null" should match `Value::Object(_)`
    /// directly or use [`Value::is_null`] together with this accessor.
    pub fn as_object(&self) -> Option<ObjectRef> {
        match self {
            Value::Object(r) => *r,
            _ => None,
        }
    }
}

// -- Compact encoding: Value <-> (u64, u8) for SoA storage --
// Reduces per-slot memory from 16 bytes (enum) to 9 bytes (u64 + u8 tag).

/// Type tags for compact SoA (Structure of Arrays) value storage.
pub const VTAG_INT: u8 = 0;
pub const VTAG_LONG: u8 = 1;
pub const VTAG_FLOAT: u8 = 2;
pub const VTAG_DOUBLE: u8 = 3;
pub const VTAG_OBJECT: u8 = 4;
pub const VTAG_NULL: u8 = 5;
pub const VTAG_UNINIT: u8 = 6;
pub const VTAG_RETADDR: u8 = 7;

/// Encode a Value into a compact (u64, u8) pair for SoA storage.
#[inline(always)]
pub fn encode_value(v: Value) -> (u64, u8) {
    match v {
        Value::Int(i) => (i as u32 as u64, VTAG_INT),
        Value::Long(l) => (l as u64, VTAG_LONG),
        Value::Float(f) => (f.to_bits() as u64, VTAG_FLOAT),
        Value::Double(d) => (d.to_bits(), VTAG_DOUBLE),
        Value::Object(Some(r)) => (r.as_ptr() as u64, VTAG_OBJECT),
        Value::Object(None) => (0, VTAG_NULL),
        Value::Uninitialized => (0, VTAG_UNINIT),
        Value::ReturnAddress(a) => (a as u64, VTAG_RETADDR),
    }
}

/// Cold path for [`decode_value`]'s `VTAG_OBJECT` branch: a `VTAG_OBJECT`
/// slot whose pointer is null or unaligned (a corrupted or zero-initialized
/// stale slot). This degrades to `Value::Object(None)` in release builds and
/// panics via `debug_assert!` in debug builds so the underlying bug surfaces.
///
/// Splitting this out as a `#[cold]` non-inlined function mirrors
/// `compact_value::cold_degraded_object_ptr` and gives LLVM permission to
/// place it off the hot path, freeing icache for the well-formed object
/// branch in `decode_value`. The well-formed slot satisfies both predicates,
/// so this is taken essentially never in steady-state interpretation.
#[cold]
#[inline(never)]
fn cold_decode_degraded_object_ptr(ptr: *mut u8) -> Value {
    // T14: Gracefully handle corrupted or zero-initialized slots that have
    // VTAG_OBJECT but invalid pointer values in release builds. In debug
    // builds we panic via `debug_assert!(false, ...)` so tests catch the
    // underlying bug (writing non-reference bits through a reference
    // accessor) instead of silent degradation.
    if ptr.is_null() {
        debug_assert!(
            false,
            "decode_value: VTAG_OBJECT with null pointer — likely a non-reference bit pattern \
             written through a reference accessor; degrading to Object(None) in release"
        );
    } else {
        // KC16 SIGSEGV audit: unaligned-but-nonzero pointers are treated as
        // null in release rather than crashing; debug builds panic so the
        // underlying bug surfaces.
        debug_assert!(
            false,
            "decode_value: VTAG_OBJECT with unaligned pointer {ptr:p} — likely a non-reference \
             bit pattern written through a reference accessor; degrading to Object(None) in release"
        );
    }
    Value::Object(None)
}

/// Decode a compact (u64, u8) pair back into a Value.
#[inline(always)]
pub fn decode_value(val: u64, tag: u8) -> Value {
    match tag {
        VTAG_INT => Value::Int(val as i32),
        VTAG_LONG => Value::Long(val as i64),
        VTAG_FLOAT => Value::Float(f32::from_bits(val as u32)),
        VTAG_DOUBLE => Value::Double(f64::from_bits(val)),
        VTAG_OBJECT => {
            let ptr = val as *mut u8;
            // The degraded paths (null or unaligned pointer arising from a
            // stale slot) are split into a `#[cold]` helper: every
            // well-formed object slot satisfies both predicates, so the
            // branch predictor and LLVM's basic-block layout treat them as
            // cold.
            if ptr.is_null() || (ptr as usize) % 8 != 0 {
                cold_decode_degraded_object_ptr(ptr)
            } else {
                Value::Object(Some(unsafe { ObjectRef::from_raw(ptr) }))
            }
        }
        VTAG_NULL => Value::Object(None),
        VTAG_RETADDR => Value::ReturnAddress(val as u32),
        _ => Value::Uninitialized,
    }
}

/// Check if a tag represents an Object reference (non-null) for GC scanning.
#[inline(always)]
pub fn is_object_tag(tag: u8) -> bool {
    tag == VTAG_OBJECT
}

/// Raw bits stored in a local/stack cell tagged [`VTAG_LONG`] that should be
/// treated as a non-null object pointer for GC rooting and pointer remapping.
///
/// JNI and internal bridges sometimes surface `jobject` handles as raw `i64`
/// (`Value::Long`). When those bits are written into a reference local without
/// widening to [`VTAG_OBJECT`], they must still be traced like
/// `coerce_value_for_return(..., b'L')` does on the read path: **non-zero** and
/// **8-byte aligned** (`usize` object pointers are always aligned in this VM).
///
/// Returns `None` for patterns that `coerce_value_for_return` maps to `null`.
#[inline(always)]
pub fn jlong_bits_as_aligned_object_ptr(bits: u64) -> Option<usize> {
    let p = bits as usize;
    if p != 0 && p % 8 == 0 {
        Some(p)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Layout invariants (owned by `types` since `Value` is defined here)
// ---------------------------------------------------------------------------
//
// The JIT slot layout depends on `Value` being exactly 16 bytes with align ≤ 8
// and on `ObjectRef` being pointer-sized.  These asserts live here (alongside
// the type definitions) so the invariant is owned by the crate that owns the
// types.  The `jit` crate replicates the same asserts as belt-and-suspenders
// so a JIT-only platform-port still fails to compile if the layout drifts.
const _: () = assert!(
    std::mem::size_of::<Value>() == 16,
    "Value must be exactly 16 bytes (JIT slot layout depends on this)"
);
const _: () = assert!(
    std::mem::align_of::<Value>() <= 8,
    "Value alignment must not exceed 8 bytes"
);
const _: () = assert!(
    std::mem::size_of::<ObjectRef>() == std::mem::size_of::<*mut u8>(),
    "ObjectRef must be pointer-sized"
);
// A4: confirm the NonNull niche optimization — Option<ObjectRef> must be
// pointer-sized (no discriminant tag) because `null` is the niche for the
// `None` case.  If this assert ever fires, something has been added to
// ObjectRef (e.g. a non-niche-aware field) that defeats the layout opt
// and silently inflates Value to 24 bytes — breaking the JIT slot layout.
const _: () = assert!(
    std::mem::size_of::<Option<ObjectRef>>() == std::mem::size_of::<*mut u8>(),
    "Option<ObjectRef> must be pointer-sized (NonNull niche optimization)"
);

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(v) => write!(f, "int({v})"),
            Value::Long(v) => write!(f, "long({v})"),
            Value::Float(v) => write!(f, "float({v})"),
            Value::Double(v) => write!(f, "double({v})"),
            Value::Object(None) => write!(f, "null"),
            Value::Object(Some(r)) => write!(f, "ref({:p})", r.as_ptr()),
            Value::ReturnAddress(addr) => write!(f, "retaddr({addr})"),
            Value::Uninitialized => write!(f, "<uninitialized>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_int() {
        let v = Value::Int(42);
        assert_eq!(v.as_int(), Some(42));
        assert!(!v.is_category2());
        assert!(!v.is_null());
    }

    #[test]
    fn value_long_is_category2() {
        let v = Value::Long(100);
        assert!(v.is_category2());
    }

    #[test]
    fn jlong_bits_as_aligned_object_ptr_matches_coerce_contract() {
        assert_eq!(jlong_bits_as_aligned_object_ptr(0), None);
        assert_eq!(jlong_bits_as_aligned_object_ptr(4), None);
        assert_eq!(jlong_bits_as_aligned_object_ptr(0x1000), Some(0x1000));
        assert_eq!(jlong_bits_as_aligned_object_ptr(0x1008), Some(0x1008));
    }

    #[test]
    fn value_null() {
        let v = Value::Object(None);
        assert!(v.is_null());
    }

    #[test]
    fn value_display() {
        assert_eq!(format!("{}", Value::Int(42)), "int(42)");
        assert_eq!(format!("{}", Value::Object(None)), "null");
        assert_eq!(format!("{}", Value::Uninitialized), "<uninitialized>");
    }

    #[test]
    fn encode_decode_int() {
        let (val, tag) = encode_value(Value::Int(42));
        assert_eq!(decode_value(val, tag).as_int(), Some(42));
        let (val, tag) = encode_value(Value::Int(-1));
        assert_eq!(decode_value(val, tag).as_int(), Some(-1));
        let (val, tag) = encode_value(Value::Int(i32::MAX));
        assert_eq!(decode_value(val, tag).as_int(), Some(i32::MAX));
        let (val, tag) = encode_value(Value::Int(i32::MIN));
        assert_eq!(decode_value(val, tag).as_int(), Some(i32::MIN));
    }

    #[test]
    fn encode_decode_long() {
        let (val, tag) = encode_value(Value::Long(123456789012345));
        assert_eq!(decode_value(val, tag).as_long(), Some(123456789012345));
        let (val, tag) = encode_value(Value::Long(-1));
        assert_eq!(decode_value(val, tag).as_long(), Some(-1));
        let (val, tag) = encode_value(Value::Long(i64::MAX));
        assert_eq!(decode_value(val, tag).as_long(), Some(i64::MAX));
        let (val, tag) = encode_value(Value::Long(i64::MIN));
        assert_eq!(decode_value(val, tag).as_long(), Some(i64::MIN));
    }

    #[test]
    fn encode_decode_float() {
        let (val, tag) = encode_value(Value::Float(3.15));
        let decoded = decode_value(val, tag).as_float().unwrap();
        assert!((decoded - 3.15).abs() < 1e-6);
        let (val, tag) = encode_value(Value::Float(-0.0));
        assert_eq!(
            decode_value(val, tag).as_float().unwrap().to_bits(),
            (-0.0f32).to_bits()
        );
    }

    #[test]
    fn encode_decode_double() {
        let (val, tag) = encode_value(Value::Double(2.719281828));
        let decoded = decode_value(val, tag).as_double().unwrap();
        assert!((decoded - 2.719281828).abs() < 1e-9);
    }

    #[test]
    fn encode_decode_null() {
        let (val, tag) = encode_value(Value::Object(None));
        assert!(decode_value(val, tag).is_null());
    }

    #[test]
    fn encode_decode_uninit() {
        let (val, tag) = encode_value(Value::Uninitialized);
        assert_eq!(decode_value(val, tag), Value::Uninitialized);
    }

    #[test]
    fn encode_decode_object_ref() {
        // Use an aligned pointer (multiple of 8)
        let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        let (val, tag) = encode_value(Value::Object(Some(obj)));
        assert!(is_object_tag(tag));
        let decoded = decode_value(val, tag);
        assert!(
            matches!(decoded, Value::Object(Some(r)) if r.as_ptr() as u64 == 0x1234_5678_ABC0)
        );
    }

    #[test]
    fn encode_decode_retaddr() {
        let (val, tag) = encode_value(Value::ReturnAddress(999));
        assert!(matches!(decode_value(val, tag), Value::ReturnAddress(999)));
    }

    #[test]
    fn decode_value_rejects_null_object() {
        // T14 graceful degradation: a VTAG_OBJECT tag paired with a null
        // pointer is treated as Value::Object(None) rather than panicking,
        // so corrupted or zero-initialized slots don't crash the VM.
        assert!(matches!(
            decode_value(0, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    #[test]
    fn decode_value_rejects_unaligned_object() {
        // T14 graceful degradation: a VTAG_OBJECT tag paired with an
        // unaligned (non-8-byte-aligned) pointer is treated as
        // Value::Object(None) rather than panicking (KC16 SIGSEGV audit).
        assert!(matches!(
            decode_value(0x1001, VTAG_OBJECT),
            Value::Object(None)
        ));
    }

    #[test]
    fn object_ref_as_ptr_roundtrip() {
        let ptr = 0xDEAD_BEE0_u64 as *mut u8;
        let obj = unsafe { ObjectRef::from_raw(ptr) };
        assert_eq!(obj.as_ptr(), ptr);
    }
}
