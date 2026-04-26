use std::fmt;

/// A JVM runtime value.
///
/// Represents any value that can be stored in a local variable or on the operand stack.
/// The JVM specification defines computational types that map to these variants.
#[derive(Debug, Clone, Copy, PartialEq)]
// Note: layout is verified at compile time (16 bytes on x86-64) via static assert in jit/src/lib.rs.
// Adding repr(C) would change size to 24 bytes and break JIT. The current Rust default
// layout is stable for this enum shape on x86-64 and is validated by the static assert.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef {
    ptr: *mut u8,
}

// SAFETY: ObjectRef is a Copy wrapper around a raw pointer.  We implement
// Hash based on the pointer value so ObjectRef can be used as a HashMap key
// (e.g. in SharedVm.class_mirrors_reverse).  Hashing a raw pointer is
// deterministic within a single process run.
impl std::hash::Hash for ObjectRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.ptr as usize).hash(state);
    }
}

impl ObjectRef {
    /// Create a new object reference from a raw pointer.
    ///
    /// # Safety
    /// The caller must ensure the pointer is valid and points to a properly allocated object.
    /// The pointer must be non-null and aligned to at least 8 bytes (heap object alignment).
    pub unsafe fn from_raw(ptr: *mut u8) -> Self {
        // T14: Check alignment in all builds, not just debug, to prevent
        // segfaults from corrupted heap slots during bootstrap.
        //
        // The panic hook itself can capture a backtrace via RUST_BACKTRACE=1;
        // we no longer print an extra forensic trail to stderr here to avoid
        // polluting stderr on every run.
        if ptr.is_null() {
            panic!("ObjectRef::from_raw called with null pointer");
        }
        if (ptr as usize) % 8 != 0 {
            panic!("ObjectRef::from_raw called with unaligned pointer: {ptr:p}");
        }
        Self { ptr }
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

// SAFETY: ObjectRef implements Send and Sync.
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

    /// Extract an object reference, or None if this isn't an Object.
    pub fn as_object(&self) -> Option<Option<ObjectRef>> {
        match self {
            Value::Object(r) => Some(*r),
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
            // T14: Gracefully handle corrupted or zero-initialized slots
            // that have VTAG_OBJECT but invalid pointer values.  This can
            // happen when a non-reference field is read via Unsafe reference
            // accessors, or during bootstrap when fields haven't been
            // properly initialized yet.
            if ptr.is_null() {
                Value::Object(None)
            } else if (ptr as usize) % 8 != 0 {
                // KC16 SIGSEGV audit: unaligned-but-nonzero pointers are
                // treated as null rather than crashing.  Silent to avoid
                // stderr pollution on hot read paths.
                Value::Object(None)
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
