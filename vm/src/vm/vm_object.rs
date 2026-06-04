// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object helpers: Java String interning, Class mirrors, static field access.

use crate::classloading::ClassId;
use crate::memory::vm_heap::VmHeap;
use crate::memory::heap::ArrayElementType;
use crate::types::{ObjectRef, Value};

use super::SharedVm;

// ---------------------------------------------------------------------------
// Java String helpers
// ---------------------------------------------------------------------------

/// Default number of instance fields for synthetic java/lang/String objects.
/// Field 0: `value` — char[] (ObjectRef to a char array)
/// Field 1: `hash` — int (cached hash code, 0 = not yet computed)
/// When real JDK String is loaded, `num_total_fields` from the class is used instead.
const STRING_NUM_FIELDS_DEFAULT: usize = 2;

/// Coder constants for JDK 9+ compact strings.
const CODER_LATIN1: i32 = 0;
const CODER_UTF16: i32 = 1;

/// Check if a Rust string is entirely representable in Latin-1 (ISO 8859-1).
/// Latin-1 maps exactly to Unicode code points U+0000..U+00FF.
fn is_latin1(text: &str) -> bool {
    text.chars().all(|c| (c as u32) <= 0xFF)
}

/// Create a Java String object from a Rust `&str`.
///
/// Uses the VM's string pool for interning: if an identical string was already
/// created, the existing ObjectRef is returned. Otherwise a new String object
/// is allocated.
///
/// **Layout depends on compact_strings flag:**
/// - Compact (JDK 9+): `byte[] value` (field 0) + `byte coder` (field 1) +
///   `int hash` (field 2) + `boolean hashIsZero` (field 3).
///   LATIN1 coder (0) stores one byte per char; UTF16 coder (1) stores two
///   bytes per char in big-endian order.
/// - Legacy (pre-JDK 9 / synthetic): `char[] value` (field 0) + `int hash`
///   (field 1).
pub fn create_java_string(shared: &SharedVm, text: &str) -> ObjectRef {
    // Fast path: check pool
    if let Some(&obj) = shared.string_pool.read().get(text) {
        return obj;
    }

    // Slow path: create new string object
    let mut pool = shared.string_pool.write();
    // Double-check after acquiring write lock
    if let Some(&obj) = pool.get(text) {
        return obj;
    }

    let str_obj = alloc_java_string_object(shared, text);
    pool.insert(text.to_string(), str_obj);
    str_obj
}

/// Create a Java String object from a Rust `&str` **without** consulting or
/// populating the interned-string pool.
///
/// This is the correct constructor for *dynamically produced* strings —
/// `StringBuilder.toString()`, `String.substring()`, `String.concat(...)`,
/// etc. The JVM spec requires those to return a brand new, distinct object:
/// only string literals (`ldc`) and explicit `String.intern()` participate
/// in the constant pool. Routing dynamic producers through the pooled
/// `create_java_string` made `==` wrongly report identity between, e.g.,
/// `sb.toString()` and a literal of equal content — which breaks any code
/// that relies on `==` reference identity of distinct String objects.
pub fn create_java_string_uninterned(shared: &SharedVm, text: &str) -> ObjectRef {
    alloc_java_string_object(shared, text)
}

/// Allocate and populate a fresh `java/lang/String` object for `text`.
/// Performs no pool lookup or insertion — callers decide pooling policy.
fn alloc_java_string_object(shared: &SharedVm, text: &str) -> ObjectRef {
    // Load java/lang/String class and resolve field count (cached after first call).
    // The field count is cached in an AtomicUsize to avoid lock contention:
    // once resolved, subsequent calls skip the class_manager lock entirely.
    let cached = shared.cached_string_num_fields.load(std::sync::atomic::Ordering::Relaxed);
    let (string_class_id, field_count) = {
        let mut cm = shared.class_manager.write();
        let id = cm.load_class("java/lang/String").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm.get_class(id)
                .map(|cls| {
                    if cls.is_synthetic_stub {
                        // Synthetic stub — use the hardcoded default (value + hash).
                        STRING_NUM_FIELDS_DEFAULT
                    } else {
                        // Real class loaded from .class file — use its actual
                        // field layout (JDK 25 String has 4: value, coder, hash,
                        // hashIsZero).
                        cls.num_total_fields
                    }
                })
                .unwrap_or(STRING_NUM_FIELDS_DEFAULT);
            // Safety net: never allocate zero fields
            let c = if c == 0 { STRING_NUM_FIELDS_DEFAULT } else { c };
            shared.cached_string_num_fields.store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };
    let str_obj = shared.heap.alloc_object(string_class_id, field_count);

    let compact = shared.compact_strings.load(std::sync::atomic::Ordering::Relaxed);

    if compact {
        // ---- JDK 9+ compact string layout ----
        // Field 0: byte[] value
        // Field 1: byte coder (0=LATIN1, 1=UTF16)
        // Field 2: int hash (0 = not yet computed)
        // Field 3: boolean hashIsZero (false)
        if is_latin1(text) {
            // LATIN1: one byte per char
            let bytes: Vec<u8> = text.chars().map(|c| c as u8).collect();
            let byte_array = shared
                .heap
                .alloc_array(ClassId::new(0), ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                let _ = shared
                    .heap
                    .set_array_element(byte_array, i, Value::Int(b as i32));
            }
            shared.heap.set_field(str_obj, 0, Value::Object(Some(byte_array)));
            // write_barrier fires automatically inside set_field
            shared.heap.set_field(str_obj, 1, Value::Int(CODER_LATIN1));
        } else {
            // UTF16: two bytes per char. HotSpot's `StringUTF16` stores each
            // char in the host's native byte order — `StringUTF16.isBigEndian()`
            // is an intrinsic bound to the platform endianness. Every tier-1
            // target CratonVM runs on (x86_64, aarch64) is LITTLE-endian, so
            // the low byte is stored first. This MUST agree with both
            // `StringUTF16.isBigEndian()` (native_string_utf16_is_big_endian
            // in lang_string.rs) and the native String readers; a mismatch
            // byte-swaps every non-LATIN-1 char and corrupts e.g.
            // `CharacterData00`'s packed lookup tables.
            let utf16: Vec<u16> = text.encode_utf16().collect();
            let byte_len = utf16.len() * 2;
            let byte_array = shared
                .heap
                .alloc_array(ClassId::new(0), ArrayElementType::Byte, byte_len);
            for (i, &unit) in utf16.iter().enumerate() {
                // Little-endian: low byte at even index, high byte at odd index.
                let lo = (unit & 0xFF) as u8;
                let hi = (unit >> 8) as u8;
                let _ = shared
                    .heap
                    .set_array_element(byte_array, i * 2, Value::Int(lo as i32));
                let _ = shared
                    .heap
                    .set_array_element(byte_array, i * 2 + 1, Value::Int(hi as i32));
            }
            shared.heap.set_field(str_obj, 0, Value::Object(Some(byte_array)));
            // write_barrier fires automatically inside set_field
            shared.heap.set_field(str_obj, 1, Value::Int(CODER_UTF16));
        }
        shared.heap.set_field(str_obj, 2, Value::Int(0)); // hash
        shared.heap.set_field(str_obj, 3, Value::Int(0)); // hashIsZero
    } else {
        // ---- Legacy / synthetic layout ----
        // Field 0: char[] value (UTF-16)
        // Field 1: int hash
        let utf16: Vec<u16> = text.encode_utf16().collect();
        let char_array = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Char, utf16.len());
        for (i, &ch) in utf16.iter().enumerate() {
            let _ = shared
                .heap
                .set_array_element(char_array, i, Value::Int(ch as i32));
        }
        shared.heap.set_field(str_obj, 0, Value::Object(Some(char_array)));
        // write_barrier fires automatically inside set_field
        shared.heap.set_field(str_obj, 1, Value::Int(0));
    }

    str_obj
}

/// Decode the JDK `String.value` array payload (compact `byte[]` or legacy `char[]`).
pub fn decode_java_string_value_array(
    heap: &VmHeap,
    value_array: ObjectRef,
    coder: i32,
) -> Option<String> {
    if heap.kind_of(value_array) != crate::memory::heap::ObjectKind::Array {
        return None;
    }
    let elem_type = heap.array_element_type(value_array)?;
    match elem_type {
        ArrayElementType::Char => {
            let utf16 = heap.read_char_array_bulk(value_array);
            Some(String::from_utf16_lossy(&utf16))
        }
        ArrayElementType::Byte => {
            let len = heap.array_length(value_array);
            if coder == CODER_LATIN1 {
                let mut s = String::with_capacity(len);
                for i in 0..len {
                    let b = match heap.get_array_element(value_array, i) {
                        Ok(Value::Int(v)) => (v & 0xFF) as u8,
                        _ => 0,
                    };
                    s.push(b as char);
                }
                Some(s)
            } else {
                let num_units = len / 2;
                let mut utf16 = Vec::with_capacity(num_units);
                for i in 0..num_units {
                    // Little-endian: low byte at even index (matches
                    // StringUTF16.isBigEndian()==false and create_java_string).
                    let lo = match heap.get_array_element(value_array, i * 2) {
                        Ok(Value::Int(v)) => (v & 0xFF) as u16,
                        _ => 0,
                    };
                    let hi = match heap.get_array_element(value_array, i * 2 + 1) {
                        Ok(Value::Int(v)) => (v & 0xFF) as u16,
                        _ => 0,
                    };
                    utf16.push((hi << 8) | lo);
                }
                Some(String::from_utf16_lossy(&utf16))
            }
        }
        _ => None,
    }
}

/// Read a Java String object back to a Rust `String`.
///
/// Supports both compact string layout (JDK 9+: byte[] + coder) and legacy
/// layout (char[] value).
pub fn read_java_string(heap: &VmHeap, obj_ref: ObjectRef) -> Option<String> {
    read_java_string_inner(heap, obj_ref, false)
}

/// Inner implementation with explicit compact_strings flag override.
/// When `compact_override` is true, reads compact layout; when false, tries
/// to auto-detect by examining the array element type.
fn read_java_string_inner(heap: &VmHeap, obj_ref: ObjectRef, _compact_override: bool) -> Option<String> {
    // Undersized-receiver guard. This function is invoked speculatively by
    // hash-key / equality / toString helpers (`map_hash_key`,
    // `obj_to_display_string`, etc.) that don't know whether the receiver
    // is actually a `java/lang/String`. When called with a non-String
    // 0-slot or 1-slot object — e.g. a static-only class such as
    // `net/sf/cglib/proxy/MethodInterceptorGenerator` (0 instance fields)
    // used as a HashMap key — the unguarded `get_field(obj, 0)` and
    // `get_field(obj, 1)` reads below fired the `gen_heap::get_field`
    // out-of-bounds-read diagnostic for every probe. A String always has
    // at least 2 slots (value + coder), so anything smaller cannot be a
    // String and the right answer is `None`.
    if heap.num_fields(obj_ref) < 2 {
        return None;
    }
    // Read field 0 (the value array)
    let value_array = match heap.get_field(obj_ref, 0) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };

    // Guard: field 0 must actually be an array (not a regular Object)
    if heap.kind_of(value_array) != crate::memory::heap::ObjectKind::Array {
        return None;
    }

    // Detect the array element type to determine which layout is in use.
    let elem_type = heap.array_element_type(value_array);
    let coder = match elem_type {
        Some(ArrayElementType::Byte) => match heap.get_field(obj_ref, 1) {
            // Slot 1 of a real-JDK 9+ String is `coder:byte` (stored as Int
            // on the JVM stack/field) and is ALWAYS 0 (LATIN1) or 1 (UTF16).
            // Anything else means this receiver is NOT a String. Three common
            // collisions we MUST reject here:
            //
            //   • `ASN1ObjectIdentifier` (`contents:[B` + `identifier:String`)
            //     — slot 1 is a reference, falls through to the catch-all.
            //
            //   • `ASN1ObjectIdentifier$OidHandle` and similar
            //     byte-array-payload key classes (`contents:[B` +
            //     `contentsLength:I` + `key:I`) — slot 1 is an Int but its
            //     value is the array length, not a coder.
            //
            //   • `ASN1Integer` (`bytes:[B` + `start:I` where `start==0`
            //     for any small value) — coder check alone passes (0 ==
            //     LATIN1). The additional `num_fields >= 4` test below
            //     rejects it: real-JDK 9+ String has exactly 4 fields
            //     (value, coder, hash, hashIsZero) and ASN1Integer has 2.
            //
            // Without these guards, ASN1Integer's `toString()` for value 9
            // returned the tab character "\t" (Latin-1 decode of byte[9]),
            // surfacing in BC ASN.1's DLExternalTest as a mismatch between
            // the printed "\t" and the expected "9".
            Value::Int(c) if (c == CODER_LATIN1 || c == CODER_UTF16)
                && heap.num_fields(obj_ref) >= 4 =>
            {
                c
            }
            _ => return None,
        },
        _ => CODER_LATIN1,
    };
    match elem_type {
        Some(ArrayElementType::Char) | Some(ArrayElementType::Byte) => {
            decode_java_string_value_array(heap, value_array, coder)
        }
        _ => {
            // Field 0 is an array but it's neither a char[] nor a byte[].
            // This means `obj_ref` is NOT a java.lang.String — it's some
            // other class that happens to have a non-string array in its
            // first field (e.g. Guava's ImmutableList where field 0 is the
            // backing Object[] of elements).
            //
            // Returning Some(garbage) here causes value_to_string /
            // invoke_to_string to short-circuit and treat the object as a
            // pre-decoded String, skipping the toString() virtual call. The
            // garbage chars come from interpreting the reference array's
            // raw bytes as UTF-16 code units (Wave 2 string-decode bug).
            //
            // Return None so callers fall back to dispatching toString().
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Class mirror helpers
// ---------------------------------------------------------------------------

/// Number of instance fields we allocate for synthetic java/lang/Class mirror.
/// When running against real JDK 25 classes, the allocation is sized to the
/// real class's `num_total_fields` (≈19 in JDK 25) instead.
const CLASS_MIRROR_NUM_FIELDS: usize = 2;

/// Look up the ClassId backing a java.lang.Class mirror via the reverse
/// map in `SharedVm.class_mirrors_reverse`.  Returns `None` for primitive
/// mirrors and for non-mirror objects.
pub fn class_id_from_mirror(shared: &SharedVm, mirror: ObjectRef) -> Option<ClassId> {
    shared.class_mirrors_reverse.read().get(&mirror).copied()
}

/// Get or create a java.lang.Class mirror object for the given ClassId.
///
/// Class mirrors are cached in `SharedVm.class_mirrors` to ensure identity:
/// `a.getClass() == a.getClass()` is always true.
///
/// Layout: the mirror is sized to the real `java/lang/Class` class
/// (`num_total_fields`, ≈19 in JDK 25; 2 for the synthetic stub).  In the
/// real-JDK layout we populate the `name` (slot 1), `modifiers` (slot 6),
/// and `primitive` (slot 7) instance fields so JDK bytecode that reads
/// them directly via `getfield` sees correct values.
///
/// The class_id ↔ mirror mapping is maintained by `SharedVm.class_mirrors`
/// (forward) and `SharedVm.class_mirrors_reverse` (reverse), which lets
/// `mirror_class_id` recover the ClassId without encoding it in the mirror's
/// Java-visible fields.
pub fn get_or_create_class_mirror(shared: &SharedVm, class_id: ClassId) -> ObjectRef {
    if std::env::var("CRATONVM_DBG_TOARRAY").is_ok() {
        let nm = shared.class_manager.read().get_class(class_id).map(|c| c.name.to_string());
        eprintln!("[DBG_TOARRAY] get_or_create_class_mirror cid={:?} name={:?}", class_id, nm);
    }
    // Fast path: check cache
    if let Some(&mirror) = shared.class_mirrors.read().get(&class_id) {
        return mirror;
    }

    // Slow path: create new mirror
    let mut mirrors = shared.class_mirrors.write();
    // Double-check after acquiring write lock
    if let Some(&mirror) = mirrors.get(&class_id) {
        return mirror;
    }

    // Load java/lang/Class and resolve field count (cached after first call).
    let cached = shared.cached_class_mirror_num_fields.load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.class_manager.write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm.get_class(id)
                .map(|cls| {
                    if cls.is_synthetic_stub {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared.cached_class_mirror_num_fields.store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared.heap.alloc_object(class_class_id, mirror_field_count);

    // Populate the real JDK 25 `java.lang.Class` instance field layout:
    //
    //   Slot 0:  cachedConstructor  (Constructor<T>)  → null
    //   Slot 1:  name               (String)          → class name
    //   Slot 2:  module             (Module)           → null
    //   Slot 3:  classLoader        (ClassLoader)      → null
    //   Slot 4:  classData          (Object)           → null
    //   Slot 5:  signers            (Object[])         → null
    //   Slot 6:  modifiers          (char/int)         → access_flags
    //   Slot 7:  primitive          (boolean/int)      → false (0)
    //   Slot 8:  packageName        (String)           → null
    //   Slot 9:  componentType      (Class<?>)         → null
    //   Slot 10: protectionDomain   (ProtectionDomain) → null
    //   Slot 11: reflectionData     (SoftReference)    → null
    //   Slot 12: classRedefinedCount (int)             → 0
    //   Slot 13: genericInfo        (ClassRepository)  → null
    //   Slot 14: enumConstants      (T[])              → null
    //   Slot 15: enumConstantDirectory (Map)           → null
    //   Slot 16: annotationData     (AnnotationData)   → null
    //   Slot 17: annotationType     (AnnotationType)   → null
    //   Slot 18: classValueMap      (ClassValueMap)    → null
    //
    // Null Object fields default to zero/null in the heap already,
    // so we only need to explicitly set non-null / non-zero fields.

    // Slot 0: store class_id as Int for legacy compatibility (mirror_class_id
    // fallback and internal VM code that reads field 0). In real-JDK mode this
    // "occupies" the cachedConstructor slot, but JDK bytecode that reads
    // cachedConstructor will get an Int which it treats as an invalid reference
    // (effectively null) — safe because cachedConstructor is checked with `if
    // (cachedConstructor == null)` patterns.
    shared.heap.set_field(mirror, 0, Value::Int(class_id.as_u32() as i32));

    // Slot 1: name → class name String (same slot in both synthetic and real JDK)
    let (class_name, access_flags) = {
        let cm = shared.class_manager.read();
        let name = cm.get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
        let flags = cm.get_class(class_id)
            .map(|c| c.access_flags.bits())
            .unwrap_or(0u16);
        (name, flags)
    };
    let name_obj = create_java_string(shared, &class_name);
    shared.heap.set_field(mirror, 1, Value::Object(Some(name_obj)));

    // Populate the real JDK 25 `java.lang.Class` extended fields when the
    // mirror has the full field count (19 fields in real-JDK mode).
    // This is critical for Class.reflectionData() which reads
    // classRedefinedCount (slot 12) directly via bytecode.
    if mirror_field_count > 6 {
        // Slot 6: modifiers (access flags as int)
        shared.heap.set_field(mirror, 6, Value::Int(access_flags as i32));
    }
    if mirror_field_count > 7 {
        // Slot 7: primitive → false (0) for regular class mirrors
        shared.heap.set_field(mirror, 7, Value::Int(0));
    }
    if mirror_field_count > 12 {
        // Slot 11: reflectionData → null (SoftReference<ReflectionData>)
        shared.heap.set_field(mirror, 11, Value::Object(None));
        // Slot 12: classRedefinedCount → 0 (critical for Class.reflectionData())
        shared.heap.set_field(mirror, 12, Value::Int(0));
    }

    mirrors.insert(class_id, mirror);
    // Reverse map for mirror_class_id (`class_id_from_mirror`).
    shared.class_mirrors_reverse.write().insert(mirror, class_id);
    mirror
}

/// Get or create a Class mirror for a primitive type ("int", "boolean", etc.).
///
/// Primitive mirrors use ClassId(0) and store Int(-1) in field 0 as a marker.
/// Field 1 stores the primitive name as a String.
pub fn get_or_create_primitive_mirror(shared: &SharedVm, prim_name: &str) -> ObjectRef {
    // Fast path: check cache
    if let Some(&mirror) = shared.primitive_mirrors.read().get(prim_name) {
        return mirror;
    }

    // Slow path
    let mut mirrors = shared.primitive_mirrors.write();
    if let Some(&mirror) = mirrors.get(prim_name) {
        return mirror;
    }

    // Resolve the real field count for java/lang/Class (same logic as
    // get_or_create_class_mirror). This ensures primitive mirrors are
    // allocated with the correct number of fields when running against
    // real JDK classes. Falls back to CLASS_MIRROR_NUM_FIELDS for synthetic mode.
    let cached = shared.cached_class_mirror_num_fields.load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.class_manager.write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm.get_class(id)
                .map(|cls| {
                    if cls.is_synthetic_stub {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared.cached_class_mirror_num_fields.store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared.heap.alloc_object(class_class_id, mirror_field_count);

    // Slot 0: Int(-1) marks this as a primitive Class mirror (legacy convention).
    shared.heap.set_field(mirror, 0, Value::Int(-1));

    // Slot 1: primitive type name as String
    let name_obj = create_java_string(shared, prim_name);
    shared.heap.set_field(mirror, 1, Value::Object(Some(name_obj)));

    // Extended fields for real-JDK mode
    if mirror_field_count > 7 {
        // Slot 7: primitive → true (1) — this IS a primitive mirror
        shared.heap.set_field(mirror, 7, Value::Int(1));
    }
    if mirror_field_count > 12 {
        // Slot 12: classRedefinedCount → 0
        shared.heap.set_field(mirror, 12, Value::Int(0));
    }

    mirrors.insert(prim_name.to_string(), mirror);
    mirror
}

// ---------------------------------------------------------------------------
// Free functions: static field access
// ---------------------------------------------------------------------------

/// Get a static field value from the shared state.
pub fn get_static_shared(shared: &SharedVm, class_id: ClassId, field_index: usize) -> Value {
    shared
        .statics
        .read()
        .get(&class_id)
        .and_then(|fields| fields.get(field_index))
        .copied()
        .unwrap_or(Value::Int(0))
}

/// Set a static field value in the shared state.
pub fn set_static_shared(shared: &SharedVm, class_id: ClassId, field_index: usize, value: Value) {
    let mut statics = shared.statics.write();
    let fields = statics.entry(class_id).or_insert_with(|| {
        // Reading class_manager while holding statics write is fine because
        // class_manager read is non-exclusive, and no code path holds
        // class_manager write while trying to acquire statics.
        let num_fields = shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.fields.len())
            .unwrap_or(0);
        vec![Value::Int(0); num_fields]
    });
    if field_index >= fields.len() {
        fields.resize(field_index + 1, Value::Int(0));
    }
    fields[field_index] = value;
}

// ---------------------------------------------------------------------------
// Bootstrap helpers
// ---------------------------------------------------------------------------

/// Pre-initialize critical static fields for java/lang/String after bootstrap.
///
/// JDK 9+ String has a static initializer that sets:
///   - COMPACT_STRINGS = true
///   - serialPersistentFields = new ObjectStreamField[0]
///   - CASE_INSENSITIVE_ORDER = new CaseInsensitiveComparator()
///
/// Running the full <clinit> would cascade into loading many classes. Instead,
/// we pre-set the critical fields that the VM and bytecode depend on:
///   - COMPACT_STRINGS = true (field "COMPACT_STRINGS")
///   - LATIN1 = 0 (field "LATIN1")
///   - UTF16 = 1 (field "UTF16")
///
/// Also sets `SharedVm.compact_strings` flag so `create_java_string` and
/// `read_java_string` use the correct layout.
pub fn pre_init_string_statics(shared: &SharedVm) {
    let cm = shared.class_manager.read();
    let string_id = match cm.get_loaded_class_id("java/lang/String") {
        Some(id) => id,
        None => return, // String not loaded yet
    };
    let cls = match cm.get_class(string_id) {
        Some(c) => c,
        None => return,
    };

    // Only pre-init for real (non-synthetic) String class
    if cls.is_synthetic_stub {
        return;
    }

    // Find static field indices by name
    let mut compact_strings_idx = None;
    let mut latin1_idx = None;
    let mut utf16_idx = None;
    let mut static_count = 0usize;
    for field in &cls.fields {
        if field.is_static() {
            match &*field.name {
                "COMPACT_STRINGS" => compact_strings_idx = Some(static_count),
                "LATIN1" => latin1_idx = Some(static_count),
                "UTF16" => utf16_idx = Some(static_count),
                _ => {}
            }
            static_count += 1;
        }
    }
    drop(cm);

    // Set the static fields
    if let Some(idx) = compact_strings_idx {
        set_static_shared(shared, string_id, idx, Value::Int(1)); // true
    }
    if let Some(idx) = latin1_idx {
        set_static_shared(shared, string_id, idx, Value::Int(0)); // LATIN1 = 0
    }
    if let Some(idx) = utf16_idx {
        set_static_shared(shared, string_id, idx, Value::Int(1)); // UTF16 = 1
    }

    // Enable compact string mode in the VM
    shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("Pre-initialized String compact string statics (COMPACT_STRINGS=true, LATIN1=0, UTF16=1)");
}

/// Pre-initialize critical static fields for java/lang/Class after bootstrap.
///
/// JDK 25 Class has a `registerNatives()` call in its <clinit> plus
/// `runtimeSetup()`. We wire registerNatives as a no-op native. For the
/// static fields, we pre-set:
///   - EMPTY_CLASS_ARRAY = new Class[0] (so Class.getInterfaces() etc. work)
pub fn pre_init_class_statics(shared: &SharedVm) {
    let cm = shared.class_manager.read();
    let class_id = match cm.get_loaded_class_id("java/lang/Class") {
        Some(id) => id,
        None => return,
    };
    let cls = match cm.get_class(class_id) {
        Some(c) => c,
        None => return,
    };
    if cls.is_synthetic_stub {
        return;
    }

    // Find EMPTY_CLASS_ARRAY static field index
    let mut empty_class_array_idx = None;
    let mut static_count = 0usize;
    for field in &cls.fields {
        if field.is_static() {
            if &*field.name == "EMPTY_CLASS_ARRAY" {
                empty_class_array_idx = Some(static_count);
            }
            static_count += 1;
        }
    }
    drop(cm);

    // Pre-set EMPTY_CLASS_ARRAY to an empty Class[] array
    if let Some(idx) = empty_class_array_idx {
        let empty_array = shared.heap.alloc_array(
            class_id,
            ArrayElementType::Reference,
            0,
        );
        set_static_shared(shared, class_id, idx, Value::Object(Some(empty_array)));
    }

    tracing::info!("Pre-initialized Class statics (EMPTY_CLASS_ARRAY)");
}

/// Pre-initialize the `TYPE` static field on each primitive wrapper class.
///
/// JDK wrapper classes (Integer, Long, Boolean, etc.) each have a
/// `public static final Class<X> TYPE` field set by `<clinit>` to
/// `Class.getPrimitiveClass("int")`, etc.  Pre-setting these avoids running
/// the full `<clinit>` cascade during early bootstrap.
///
/// The wrapper→primitive mapping:
///   Integer→"int", Long→"long", Boolean→"boolean", Byte→"byte",
///   Short→"short", Character→"char", Float→"float", Double→"double",
///   Void→"void"
pub fn pre_init_wrapper_type_fields(shared: &SharedVm) {
    let wrappers: &[(&str, &str)] = &[
        ("java/lang/Integer",   "int"),
        ("java/lang/Long",      "long"),
        ("java/lang/Boolean",   "boolean"),
        ("java/lang/Byte",      "byte"),
        ("java/lang/Short",     "short"),
        ("java/lang/Character", "char"),
        ("java/lang/Float",     "float"),
        ("java/lang/Double",    "double"),
        ("java/lang/Void",      "void"),
    ];

    // Phase 1: Collect (class_id, static_field_index, prim_name) while holding read lock.
    let mut to_init: Vec<(ClassId, usize, &str)> = Vec::new();
    {
        let cm = shared.class_manager.read();
        for &(wrapper_name, prim_name) in wrappers {
            let class_id = match cm.get_loaded_class_id(wrapper_name) {
                Some(id) => id,
                None => continue,
            };
            let cls = match cm.get_class(class_id) {
                Some(c) => c,
                None => continue,
            };
            if cls.is_synthetic_stub {
                continue;
            }

            // Find the "TYPE" static field index
            let mut static_idx = 0usize;
            let mut type_idx = None;
            for field in &cls.fields {
                if field.is_static() {
                    if &*field.name == "TYPE" {
                        type_idx = Some(static_idx);
                        break;
                    }
                    static_idx += 1;
                }
            }

            if let Some(idx) = type_idx {
                to_init.push((class_id, idx, prim_name));
            }
        }
    } // cm read lock dropped

    // Phase 2: Create primitive mirrors and set TYPE fields (no read lock held).
    for (class_id, idx, prim_name) in &to_init {
        let mirror = get_or_create_primitive_mirror(shared, prim_name);
        set_static_shared(shared, *class_id, *idx, Value::Object(Some(mirror)));
    }

    if !to_init.is_empty() {
        tracing::info!("Pre-initialized TYPE fields for {} wrapper classes", to_init.len());
    }
}

// ---------------------------------------------------------------------------
// Native Method Bridge — Session 10
// ---------------------------------------------------------------------------

/// A native method descriptor: class name, method name, JVM descriptor.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeMethodInfo {
    pub class_name: String,
    pub method_name: String,
    pub descriptor: String,
}

impl std::fmt::Display for NativeMethodInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}{}", self.class_name, self.method_name, self.descriptor)
    }
}

/// Result of native method coverage validation.
#[derive(Debug)]
pub struct NativeCoverageReport {
    /// Native methods that have a Rust implementation registered.
    pub covered: Vec<NativeMethodInfo>,
    /// Native methods that have NO implementation (will fail at runtime).
    pub missing: Vec<NativeMethodInfo>,
}

impl NativeCoverageReport {
    /// Total number of ACC_NATIVE methods found across all loaded classes.
    pub fn total(&self) -> usize {
        self.covered.len() + self.missing.len()
    }

    /// Fraction of native methods that are covered (0.0 to 1.0).
    pub fn coverage_ratio(&self) -> f64 {
        if self.total() == 0 {
            return 1.0;
        }
        self.covered.len() as f64 / self.total() as f64
    }

    /// Pretty-print the report.
    pub fn log_report(&self) {
        let pct = (self.coverage_ratio() * 100.0) as u32;
        tracing::info!(
            "[NativeBridge] Coverage: {}/{} ({pct}%) — {} covered, {} missing",
            self.covered.len(),
            self.total(),
            self.covered.len(),
            self.missing.len(),
        );
        if !self.missing.is_empty() {
            tracing::warn!(
                "[NativeBridge] {} unregistered native methods:",
                self.missing.len()
            );
            for m in &self.missing {
                tracing::warn!("  MISSING: {m}");
            }
        }
    }
}

/// Scan ALL loaded (non-synthetic) classes for ACC_NATIVE methods and check
/// whether each has a corresponding entry in the native method registry.
///
/// Returns a `NativeCoverageReport` with covered and missing lists.
pub fn validate_native_coverage(shared: &SharedVm) -> NativeCoverageReport {
    let mut covered = Vec::new();
    let mut missing = Vec::new();

    let cm = shared.class_manager.read();
    // Iterate over all loaded classes in the ClassStore
    for class_id_u32 in 0..cm.class_store.len() as u32 {
        let class_id = ClassId::new(class_id_u32);
        let class = match cm.class_store.get(class_id) {
            Some(c) => c,
            None => continue,
        };
        // Only scan real (non-synthetic) classes
        if class.is_synthetic_stub {
            continue;
        }
        for method in &class.methods {
            if !method.is_native() {
                continue;
            }
            let info = NativeMethodInfo {
                class_name: class.name.to_string(),
                method_name: method.name.to_string(),
                descriptor: method.descriptor.to_string(),
            };
            if shared
                .native_methods
                .find(&class.name, &method.name, &method.descriptor)
                .is_some()
            {
                covered.push(info);
            } else {
                missing.push(info);
            }
        }
    }

    covered.sort();
    missing.sort();
    NativeCoverageReport { covered, missing }
}

/// Scan a single class for its ACC_NATIVE methods.
pub fn scan_class_natives(shared: &SharedVm, class_name: &str) -> Vec<NativeMethodInfo> {
    let cm = shared.class_manager.read();
    let class_id = match cm.get_loaded_class_id(class_name) {
        Some(id) => id,
        None => return Vec::new(),
    };
    let class = match cm.class_store.get(class_id) {
        Some(c) => c,
        None => return Vec::new(),
    };
    class
        .methods
        .iter()
        .filter(|m| m.is_native())
        .map(|m| NativeMethodInfo {
            class_name: class.name.to_string(),
            method_name: m.name.to_string(),
            descriptor: m.descriptor.to_string(),
        })
        .collect()
}

/// Convert a JVM internal class name + method to JNI short symbol name.
/// Example: `("java/lang/System", "arraycopy")` → `"Java_java_lang_System_arraycopy"`
pub fn jni_short_name(class_name: &str, method_name: &str) -> String {
    fn jni_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 2);
        for ch in s.chars() {
            match ch {
                '/' => out.push('_'),
                '_' => out.push_str("_1"),
                ';' => out.push_str("_2"),
                '[' => out.push_str("_3"),
                c if c.is_ascii() => out.push(c),
                c => {
                    out.push_str(&format!("_0{:04x}", c as u32));
                }
            }
        }
        out
    }
    format!("Java_{}_{}", jni_encode(class_name), jni_encode(method_name))
}

/// Convert a JVM internal class name + method + descriptor to JNI long symbol name.
pub fn jni_long_name(class_name: &str, method_name: &str, descriptor: &str) -> String {
    fn jni_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() * 2);
        for ch in s.chars() {
            match ch {
                '/' => out.push('_'),
                '_' => out.push_str("_1"),
                ';' => out.push_str("_2"),
                '[' => out.push_str("_3"),
                c if c.is_ascii() => out.push(c),
                c => {
                    out.push_str(&format!("_0{:04x}", c as u32));
                }
            }
        }
        out
    }
    let params = descriptor
        .strip_prefix('(')
        .and_then(|s| s.split(')').next())
        .unwrap_or("");
    format!(
        "Java_{}_{}__{}",
        jni_encode(class_name),
        jni_encode(method_name),
        jni_encode(params)
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use crate::config::VmConfig;

    fn test_shared() -> Arc<SharedVm> {
        Arc::new(SharedVm::new(VmConfig::default()))
    }

    // -----------------------------------------------------------------------
    // Java String helpers
    // -----------------------------------------------------------------------

    #[test]
    fn create_and_read_string() {
        let shared = test_shared();
        let obj = create_java_string(&shared, "Hello");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn create_empty_string() {
        let shared = test_shared();
        let obj = create_java_string(&shared, "");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn create_unicode_string() {
        let shared = test_shared();
        let obj = create_java_string(&shared, "\u{1F600} emoji");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("\u{1F600} emoji".to_string()));
    }

    #[test]
    fn string_pool_deduplicates() {
        let shared = test_shared();
        let obj1 = create_java_string(&shared, "same");
        let obj2 = create_java_string(&shared, "same");
        assert_eq!(obj1.as_ptr(), obj2.as_ptr());
    }

    #[test]
    fn string_pool_different_strings() {
        let shared = test_shared();
        let obj1 = create_java_string(&shared, "alpha");
        let obj2 = create_java_string(&shared, "beta");
        assert_ne!(obj1.as_ptr(), obj2.as_ptr());
    }

    #[test]
    fn read_string_non_string_object() {
        let shared = test_shared();
        // Create a plain object (not a string -- field 0 is not an array)
        let obj = shared.heap.alloc_object(ClassId::new(0), 2);
        shared.heap.set_field(obj, 0, Value::Int(0));
        let result = read_java_string(&shared.heap, obj);
        assert!(result.is_none());
    }

    #[test]
    fn read_string_null_value_field() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 2);
        shared.heap.set_field(obj, 0, Value::Object(None));
        let result = read_java_string(&shared.heap, obj);
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Compact string helpers
    // -----------------------------------------------------------------------

    #[test]
    fn compact_string_latin1_roundtrip() {
        let shared = test_shared();
        // Enable compact string mode
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        // Reset cached field count so it re-resolves (synthetic stub → 2 fields,
        // but we need 4 for compact layout). Manually set to 4.
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "Hello");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn compact_string_utf16_roundtrip() {
        let shared = test_shared();
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "\u{1F600} emoji");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("\u{1F600} emoji".to_string()));
    }

    #[test]
    fn compact_string_empty_roundtrip() {
        let shared = test_shared();
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn compact_string_latin1_boundary() {
        let shared = test_shared();
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        // Latin-1 boundary: U+00FF (ÿ) is the last Latin-1 char
        let obj = create_java_string(&shared, "café\u{00FF}");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("café\u{00FF}".to_string()));
    }

    #[test]
    fn compact_string_beyond_latin1() {
        let shared = test_shared();
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        // U+0100 (Ā) is beyond Latin-1 → must use UTF16 coder
        let obj = create_java_string(&shared, "Hello\u{0100}World");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("Hello\u{0100}World".to_string()));
    }

    #[test]
    fn compact_string_pool_deduplicates() {
        let shared = test_shared();
        shared.compact_strings.store(true, std::sync::atomic::Ordering::Relaxed);
        shared.cached_string_num_fields.store(4, std::sync::atomic::Ordering::Relaxed);

        let obj1 = create_java_string(&shared, "same");
        let obj2 = create_java_string(&shared, "same");
        assert_eq!(obj1.as_ptr(), obj2.as_ptr());
    }

    /// Wave 2 (S108) regression: a non-String object whose `field 0` holds an
    /// `Object[]` (Reference array) — e.g. Guava's `ImmutableList` or any
    /// collection backing — must NOT short-circuit `read_java_string` into
    /// returning `Some(garbage)`. Returning `Some(_)` would tell callers
    /// "this is already a String" and skip the proper `toString()` virtual
    /// dispatch, producing the `xs=㊘粹ƈ` mojibake the S107 GuavaTest hit.
    /// The fix returns `None` for any non-Char/Byte array element type so
    /// callers fall through to the toString() path.
    #[test]
    fn read_string_returns_none_for_reference_array_field() {
        let shared = test_shared();
        let obj = shared.heap.alloc_object(ClassId::new(0), 1);
        let elem_a = shared.heap.alloc_object(ClassId::new(0), 0);
        let elem_b = shared.heap.alloc_object(ClassId::new(0), 0);
        let elem_c = shared.heap.alloc_object(ClassId::new(0), 0);
        let arr = shared.heap.alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
        shared.heap.set_array_element(arr, 0, Value::Object(Some(elem_a))).unwrap();
        shared.heap.set_array_element(arr, 1, Value::Object(Some(elem_b))).unwrap();
        shared.heap.set_array_element(arr, 2, Value::Object(Some(elem_c))).unwrap();
        shared.heap.set_field(obj, 0, Value::Object(Some(arr)));
        // Pre-fix this returned Some(garbage); post-fix must return None.
        assert_eq!(read_java_string(&shared.heap, obj), None);
    }

    // -----------------------------------------------------------------------
    // Class mirror helpers
    // -----------------------------------------------------------------------

    #[test]
    fn class_mirror_stores_class_id_in_reverse_map() {
        let shared = test_shared();
        let class_id = ClassId::new(42);
        let mirror = get_or_create_class_mirror(&shared, class_id);
        // Field 0 still stores Int(class_id) for legacy compatibility.
        assert_eq!(shared.heap.get_field(mirror, 0), Value::Int(42));
        // But class_id is also recoverable via the reverse map.
        let recovered = shared.class_mirrors_reverse.read().get(&mirror).copied();
        assert_eq!(recovered, Some(class_id));
    }

    #[test]
    fn class_mirror_caching() {
        let shared = test_shared();
        let class_id = ClassId::new(7);
        let m1 = get_or_create_class_mirror(&shared, class_id);
        let m2 = get_or_create_class_mirror(&shared, class_id);
        assert_eq!(m1.as_ptr(), m2.as_ptr());
    }

    #[test]
    fn class_mirror_different_ids() {
        let shared = test_shared();
        let m1 = get_or_create_class_mirror(&shared, ClassId::new(1));
        let m2 = get_or_create_class_mirror(&shared, ClassId::new(2));
        assert_ne!(m1.as_ptr(), m2.as_ptr());
    }

    #[test]
    fn class_mirror_name_field() {
        let shared = test_shared();
        let class_id = ClassId::new(999); // non-existent class
        let mirror = get_or_create_class_mirror(&shared, class_id);
        // Field 1 should contain a name string
        let name_val = shared.heap.get_field(mirror, 1);
        match name_val {
            Value::Object(Some(name_ref)) => {
                let name = read_java_string(&shared.heap, name_ref);
                // For unknown class, falls back to "unknown_999"
                assert!(name.is_some());
                assert!(name.unwrap().contains("999"));
            }
            _ => panic!("Expected name string in field 1"),
        }
    }

    // -----------------------------------------------------------------------
    // Primitive mirror helpers
    // -----------------------------------------------------------------------

    #[test]
    fn primitive_mirror_int() {
        let shared = test_shared();
        let mirror = get_or_create_primitive_mirror(&shared, "int");
        // Field 0 = Int(-1) marker for primitive mirrors.
        assert_eq!(shared.heap.get_field(mirror, 0), Value::Int(-1));
        // Field 1 should be "int" string
        let name_val = shared.heap.get_field(mirror, 1);
        match name_val {
            Value::Object(Some(name_ref)) => {
                let name = read_java_string(&shared.heap, name_ref);
                assert_eq!(name, Some("int".to_string()));
            }
            _ => panic!("Expected name string in field 1"),
        }
    }

    #[test]
    fn primitive_mirror_caching() {
        let shared = test_shared();
        let m1 = get_or_create_primitive_mirror(&shared, "boolean");
        let m2 = get_or_create_primitive_mirror(&shared, "boolean");
        assert_eq!(m1.as_ptr(), m2.as_ptr());
    }

    #[test]
    fn primitive_mirror_different_types() {
        let shared = test_shared();
        let m_int = get_or_create_primitive_mirror(&shared, "int");
        let m_long = get_or_create_primitive_mirror(&shared, "long");
        assert_ne!(m_int.as_ptr(), m_long.as_ptr());
    }

    // -----------------------------------------------------------------------
    // Static field access
    // -----------------------------------------------------------------------

    #[test]
    fn get_static_default_value() {
        let shared = test_shared();
        let val = get_static_shared(&shared, ClassId::new(99), 0);
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn set_and_get_static() {
        let shared = test_shared();
        let class_id = ClassId::new(10);
        set_static_shared(&shared, class_id, 0, Value::Int(42));
        assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(42));
    }

    #[test]
    fn set_static_auto_resizes() {
        let shared = test_shared();
        let class_id = ClassId::new(11);
        set_static_shared(&shared, class_id, 5, Value::Long(100));
        assert_eq!(get_static_shared(&shared, class_id, 5), Value::Long(100));
        // Indices below should be default
        assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(0));
    }

    #[test]
    fn set_static_overwrite() {
        let shared = test_shared();
        let class_id = ClassId::new(12);
        set_static_shared(&shared, class_id, 0, Value::Int(1));
        set_static_shared(&shared, class_id, 0, Value::Int(2));
        assert_eq!(get_static_shared(&shared, class_id, 0), Value::Int(2));
    }

    #[test]
    fn set_static_multiple_classes() {
        let shared = test_shared();
        set_static_shared(&shared, ClassId::new(1), 0, Value::Int(10));
        set_static_shared(&shared, ClassId::new(2), 0, Value::Int(20));
        assert_eq!(get_static_shared(&shared, ClassId::new(1), 0), Value::Int(10));
        assert_eq!(get_static_shared(&shared, ClassId::new(2), 0), Value::Int(20));
    }
}
