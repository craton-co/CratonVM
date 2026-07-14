// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object helpers: Java String interning, Class mirrors, static field access.

use crate::classloading::ClassId;
use crate::memory::heap::ArrayElementType;
use crate::memory::vm_heap::VmHeap;
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

/// Fallible, pooled twin of [`create_java_string`]: returns `None` instead of
/// aborting the process when the heap is too full to allocate the `String`.
/// Used by exception materialization (`create_exception_object`) so a detail
/// message can be built when there is room, but a 100%-full heap surfaces a
/// catchable `OutOfMemoryError` (caller falls back to the pre-allocated
/// singleton) rather than the VM hard-aborting.
pub fn try_create_java_string(shared: &SharedVm, text: &str) -> Option<ObjectRef> {
    if let Some(&obj) = shared.string_pool.read().get(text) {
        return Some(obj);
    }
    let mut pool = shared.string_pool.write();
    if let Some(&obj) = pool.get(text) {
        return Some(obj);
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    let str_obj = try_alloc_java_string_object_from_units(shared, &units)?;
    pool.insert(text.to_string(), str_obj);
    Some(str_obj)
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

/// Create a Java String object directly from UTF-16 code `units`, **without**
/// pooling.
///
/// This is the constructor for string constants that contain **lone
/// surrogates** (U+D800..U+DFFF) — e.g. ANTLR's `_serializedATN` — which a Rust
/// `str` cannot represent. The constant pool carries the exact units in its
/// side table (`ConstantPool::get_utf8_wide`); routing them here reproduces the
/// original `char[]` byte-for-byte so `charAt`-based deserialisation
/// round-trips. Such constants are not interned (their content cannot be a
/// faithful Rust pool key, and string-literal identity is immaterial for them).
pub fn create_java_string_from_units(shared: &SharedVm, units: &[u16]) -> ObjectRef {
    alloc_java_string_object_from_units(shared, units)
}

/// Fallible twin of [`create_java_string_from_units`].
pub fn try_create_java_string_from_units(shared: &SharedVm, units: &[u16]) -> Option<ObjectRef> {
    try_alloc_java_string_object_from_units(shared, units)
}

/// Allocate and populate a fresh `java/lang/String` object for `text`.
/// Performs no pool lookup or insertion — callers decide pooling policy.
fn alloc_java_string_object(shared: &SharedVm, text: &str) -> ObjectRef {
    if shared
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
        && text.is_ascii()
    {
        return try_alloc_java_string_object_from_ascii(shared, text.as_bytes()).unwrap_or_else(
            || {
                eprintln!(
                    "FATAL: heap exhausted allocating java/lang/String ({} units)",
                    text.len()
                );
                std::process::abort();
            },
        );
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    alloc_java_string_object_from_units(shared, &units)
}

/// Allocate and populate a fresh `java/lang/String` from UTF-16 code `units`.
///
/// Shared core of [`alloc_java_string_object`] (which simply `encode_utf16`s a
/// Rust `&str`) and [`create_java_string_from_units`] (surrogate-bearing
/// constants). Performs no pool lookup or insertion. Aborts the process on heap
/// exhaustion — the long-standing contract for the ~all callers that cannot
/// recover. Catchable-OOM callers (exception materialization) must use the
/// fallible [`try_alloc_java_string_object_from_units`] instead.
fn alloc_java_string_object_from_units(shared: &SharedVm, units: &[u16]) -> ObjectRef {
    try_alloc_java_string_object_from_units(shared, units).unwrap_or_else(|| {
        eprintln!(
            "FATAL: heap exhausted allocating java/lang/String ({} units)",
            units.len()
        );
        std::process::abort();
    })
}

/// Resolve the loaded String class and its instance-field count. The field
/// count is cached; the ordinary fast path uses only a class-manager read lock.
fn java_string_allocation_layout(shared: &SharedVm) -> (ClassId, usize) {
    // Load java/lang/String class and resolve field count (cached after first call).
    // The field count is cached in an AtomicUsize to avoid lock contention:
    // once resolved, subsequent calls skip the class_manager *write* lock entirely.
    //
    // PERF (concurrency): the cached fast path MUST NOT take a write lock.
    // Every dynamically produced String (StringBuilder.toString, substring,
    // concat, ...) reaches here, so a write lock serialized all String
    // creation across threads — the single biggest hot-path bottleneck.
    // Once the field count is cached, the class is necessarily already loaded
    // (java/lang/String is a bootstrap class resolved long before any dynamic
    // String exists), so we resolve its ClassId through a *read* lock via the
    // zero-allocation `get_loaded_class_id` probe. Read locks do not serialize,
    // restoring full parallelism. The write lock is reserved for the genuine
    // cache-miss / first-resolution path below.
    let cached = shared
        .cached_string_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let resolved = if cached != 0 {
        // Fast path: field count already known. Resolve the class id under a
        // read lock only. If the probe misses (extremely unlikely — would mean
        // String isn't registered under any builtin loader), fall through to
        // the write-lock path to preserve correctness.
        shared
            .class_manager
            .read()
            .get_loaded_class_id("java/lang/String")
            .map(|id| (id, cached))
    } else {
        None
    };
    match resolved {
        Some(pair) => pair,
        None => {
            // Slow path: first resolution (or read-probe miss). Take the write
            // lock to load the class and compute + cache the field count.
            let mut cm = shared.class_manager.write();
            let id = cm.load_class("java/lang/String").unwrap_or(ClassId::new(0));
            let count = if cached != 0 {
                cached
            } else {
                let c = cm
                    .get_class(id)
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
                shared
                    .cached_string_num_fields
                    .store(c, std::sync::atomic::Ordering::Relaxed);
                c
            };
            (id, count)
        }
    }
}

/// Compact-String fast path for the overwhelmingly common ASCII dynamic
/// result (regex groups, numeric formatting, short substrings). It avoids the
/// temporary UTF-16 Vec and writes the byte[] payload with one bulk copy.
fn try_alloc_java_string_object_from_ascii(shared: &SharedVm, ascii: &[u8]) -> Option<ObjectRef> {
    debug_assert!(ascii.is_ascii());
    debug_assert!(shared
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed));
    let (string_class_id, field_count) = java_string_allocation_layout(shared);
    let str_obj = shared
        .heap
        .try_alloc_object_full(string_class_id, field_count)?;
    let byte_array =
        shared
            .heap
            .try_alloc_array_full(ClassId::new(0), ArrayElementType::Byte, ascii.len())?;
    if !ascii.is_empty() {
        match shared.heap.array_data_ptr(byte_array) {
            Some(base) => unsafe {
                cratonvm_gc::heap::cell_watch_check(
                    base as usize,
                    ascii.len(),
                    "try_alloc_java_string_object_from_ascii",
                    &byte_array.as_ptr(),
                );
                std::ptr::copy_nonoverlapping(ascii.as_ptr(), base, ascii.len());
            },
            None => {
                for (i, &byte) in ascii.iter().enumerate() {
                    if shared
                        .heap
                        .set_array_element(byte_array, i, Value::Int(byte as i32))
                        .is_err()
                    {
                        return None;
                    }
                }
            }
        }
    }
    shared
        .heap
        .set_field(str_obj, 0, Value::Object(Some(byte_array)));
    shared.heap.set_field(str_obj, 1, Value::Int(CODER_LATIN1));
    shared.heap.set_field(str_obj, 2, Value::Int(0));
    shared.heap.set_field(str_obj, 3, Value::Int(0));
    Some(str_obj)
}

/// Fallible twin of [`alloc_java_string_object_from_units`]: returns `None`
/// instead of aborting when the heap is too full to allocate the `String`
/// object or its backing array.
fn try_alloc_java_string_object_from_units(shared: &SharedVm, units: &[u16]) -> Option<ObjectRef> {
    let (string_class_id, field_count) = java_string_allocation_layout(shared);
    let str_obj = shared
        .heap
        .try_alloc_object_full(string_class_id, field_count)?;

    if populate_java_string_fields(shared, str_obj, units) {
        Some(str_obj)
    } else {
        None
    }
}

/// Populate an *already-allocated* `java/lang/String` object's `value`
/// (plus `coder`/`hash`/`hashIsZero` under compact strings) fields directly
/// from raw UTF-16 code `units`, using the identical Latin1-fits-in-a-byte
/// bulk scan + little-endian compact-string layout as
/// [`try_alloc_java_string_object_from_units`] (the sole source of truth for
/// that layout — see the comment there and `StringUTF16.isBigEndian()` in
/// `native-builtins/src/lang_string.rs` for the byte-order rationale).
///
/// Unlike `create_string`/`create_java_string_from_units`, this does **not**
/// allocate the `String` object itself and does **not** touch the intern
/// pool. It exists for native `<init>` overrides (`String(char[])`,
/// `String(char[], int, int)`) that intercept construction *after* the `new`
/// bytecode has already allocated `this` — a constructor native's contract is
/// to mutate `this`'s fields in place, not to return a different object.
///
/// Returns `false` only if the backing array allocation fails (heap
/// exhaustion); the caller should surface a catchable `OutOfMemoryError`
/// rather than aborting the process, since a huge user-supplied `char[]` is a
/// normal, recoverable-in-Java condition — unlike the VM's own internal
/// string-creation call sites that use the aborting `alloc_*` wrappers.
pub fn populate_java_string_fields(shared: &SharedVm, str_obj: ObjectRef, units: &[u16]) -> bool {
    let compact = shared
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed);

    if compact {
        // ---- JDK 9+ compact string layout ----
        // Field 0: byte[] value
        // Field 1: byte coder (0=LATIN1, 1=UTF16)
        // Field 2: int hash (0 = not yet computed)
        // Field 3: boolean hashIsZero (false)
        if units.iter().all(|&u| u <= 0xFF) {
            // LATIN1: one byte per char
            let byte_array = match shared.heap.try_alloc_array_full(
                ClassId::new(0),
                ArrayElementType::Byte,
                units.len(),
            ) {
                Some(a) => a,
                None => return false,
            };
            for (i, &u) in units.iter().enumerate() {
                let _ = shared
                    .heap
                    .set_array_element(byte_array, i, Value::Int((u & 0xFF) as i32));
            }
            shared
                .heap
                .set_field(str_obj, 0, Value::Object(Some(byte_array)));
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
            let byte_len = units.len() * 2;
            let byte_array = match shared.heap.try_alloc_array_full(
                ClassId::new(0),
                ArrayElementType::Byte,
                byte_len,
            ) {
                Some(a) => a,
                None => return false,
            };
            for (i, &unit) in units.iter().enumerate() {
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
            shared
                .heap
                .set_field(str_obj, 0, Value::Object(Some(byte_array)));
            // write_barrier fires automatically inside set_field
            shared.heap.set_field(str_obj, 1, Value::Int(CODER_UTF16));
        }
        shared.heap.set_field(str_obj, 2, Value::Int(0)); // hash
        shared.heap.set_field(str_obj, 3, Value::Int(0)); // hashIsZero
    } else {
        // ---- Legacy / synthetic layout ----
        // Field 0: char[] value (UTF-16)
        // Field 1: int hash
        let char_array = match shared.heap.try_alloc_array_full(
            ClassId::new(0),
            ArrayElementType::Char,
            units.len(),
        ) {
            Some(a) => a,
            None => return false,
        };
        for (i, &ch) in units.iter().enumerate() {
            let _ = shared
                .heap
                .set_array_element(char_array, i, Value::Int(ch as i32));
        }
        shared
            .heap
            .set_field(str_obj, 0, Value::Object(Some(char_array)));
        // write_barrier fires automatically inside set_field
        shared.heap.set_field(str_obj, 1, Value::Int(0));
    }

    true
}

/// Bulk-read a compact `byte[]` array payload into a `Vec<u8>`.
///
/// Mirrors [`VmHeap::read_char_array_bulk`] but for the 1-byte-per-element
/// `byte[]` storage used by JDK 9+ compact strings. `byte[]` elements are
/// stored contiguously immediately after the object header, one byte each
/// (the same layout exploited by `write_byte_array_from` /
/// `read_byte_array_into` in `vm_exec.rs`), so the common case is a single
/// `copy_nonoverlapping` instead of `len` boxed `get_array_element` calls
/// (each a virtual dispatch + `Value` box + element-type match).
///
/// `array_data_ptr` returns `None` for a G1 *humongous* array (payload split
/// across non-contiguous regions); in that case we fall back to the
/// region-safe per-element accessor, exactly as `read_char_array_bulk` does.
fn read_byte_array_bulk(heap: &VmHeap, obj: ObjectRef) -> Vec<u8> {
    let len = heap.array_length(obj);
    let mut out = vec![0u8; len];
    if len == 0 {
        return out;
    }
    match heap.array_data_ptr(obj) {
        // SAFETY: `obj` is a live `byte[]` (kind + element type checked by the
        // caller); its payload is `len` contiguous bytes starting at
        // `array_data_ptr` (1 byte per element). `out` was just allocated with
        // exactly `len` bytes. Heap arena (source) and the fresh `Vec`
        // (destination) cannot overlap. This is a synchronous read with no
        // intervening allocation, so the payload cannot move under the copy —
        // the same convention `read_char_array_bulk` / `read_byte_array_into`
        // already rely on.
        Some(base) => unsafe {
            std::ptr::copy_nonoverlapping(base, out.as_mut_ptr(), len);
        },
        // G1 humongous byte[]: region-safe per-element read. Byte slots decode
        // to `Value::Int(u8 as i32)`; mask back to the 8-bit value.
        None => {
            for (i, slot) in out.iter_mut().enumerate() {
                if let Ok(Value::Int(v)) = heap.get_array_element(obj, i) {
                    *slot = (v & 0xFF) as u8;
                }
            }
        }
    }
    out
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
            // Bulk-read the byte payload once, then decode from the contiguous
            // slice. Identical output to the previous per-element loops; only
            // the O(n) virtual-dispatch / Value-box overhead is removed.
            let bytes = read_byte_array_bulk(heap, value_array);
            if coder == CODER_LATIN1 {
                // LATIN1: 1 byte per char, code points U+0000..U+00FF map
                // directly via `b as char` (same as the old `(v & 0xFF) as u8`
                // push — the bytes are already masked to 8 bits).
                let mut s = String::with_capacity(bytes.len());
                for &b in &bytes {
                    s.push(b as char);
                }
                Some(s)
            } else {
                // UTF16: 2 bytes per code unit, little-endian (low byte at the
                // even index — matches StringUTF16.isBigEndian()==false and
                // create_java_string). `chunks_exact(2)` drops any trailing odd
                // byte, exactly as the old `len / 2` loop did.
                let utf16: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from(c[0]) | (u16::from(c[1]) << 8))
                    .collect();
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
fn read_java_string_inner(
    heap: &VmHeap,
    obj_ref: ObjectRef,
    _compact_override: bool,
) -> Option<String> {
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
            Value::Int(c)
                if (c == CODER_LATIN1 || c == CODER_UTF16) && heap.num_fields(obj_ref) >= 4 =>
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

/// Absolute heap-slot indices of the `java/lang/Class` instance fields that
/// the mirror populator writes through.
///
/// Resolved **by name** (and validated by descriptor) against the loaded
/// `java/lang/Class` rather than hardcoded to a particular JDK's field order:
/// the JDK's private layout of `java.lang.Class` is an implementation detail
/// that has reordered between releases, so baking in JDK-25 slot numbers
/// (name=1, modifiers=6, primitive=7, …) silently writes the wrong field —
/// or out of bounds — against any other layout. By-name resolution keeps the
/// mirror correct regardless of the loaded class's field order.
///
/// Each entry is `Some(absolute_index)` only when the field exists with a
/// matching descriptor; callers skip writes for `None` slots so a layout that
/// lacks a given field is simply left at its heap-zero default.
#[derive(Default, Clone, Copy)]
struct ClassMirrorSlots {
    /// `name : Ljava/lang/String;`
    name: Option<usize>,
    /// `modifiers : I` (HotSpot declares it `char` on some builds; both decode
    /// to an `int` slot, so we accept either descriptor).
    modifiers: Option<usize>,
    /// `primitive : Z`
    primitive: Option<usize>,
    /// `classRedefinedCount : I`
    class_redefined_count: Option<usize>,
    /// `reflectionData : Ljava/lang/ref/SoftReference;`
    reflection_data: Option<usize>,
}

/// Legacy fixed slot layout used for the **synthetic** `java/lang/Class` stub
/// (whose fields are unnamed `_fN` placeholders, so by-name resolution finds
/// nothing). These match the slots the VM's synthetic natives read directly:
/// name at slot 1, primitive at slot 7, classRedefinedCount at slot 12. Only
/// emitted when the allocated mirror actually has the slot.
const LEGACY_NAME_SLOT: usize = 1;
const LEGACY_PRIMITIVE_SLOT: usize = 7;
const LEGACY_CLASS_REDEFINED_COUNT_SLOT: usize = 12;

/// Resolve the [`ClassMirrorSlots`] for the loaded `java/lang/Class`.
///
/// `class` is the `java/lang/Class` definition whose instance-field layout we
/// mirror. For a real classfile we resolve each field **by name** (validated by
/// descriptor): `find_own_field` returns the **absolute** slot index (the same
/// index `VmHeap::set_field` takes), so the result is layout-independent and
/// survives JDK field reorderings.
///
/// A synthetic stub has only unnamed `_fN` placeholder fields (often zero of
/// them), so by-name resolution finds nothing; for it we fall back to the
/// historical fixed slot numbers the synthetic-mode natives expect, gated by
/// `mirror_field_count` — the number of slots the mirror is actually allocated
/// with (`CLASS_MIRROR_NUM_FIELDS` in synthetic mode, not the stub's own field
/// table) — so we never hand back an out-of-bounds index.
fn resolve_class_mirror_slots(
    class: &crate::classloading::Class,
    mirror_field_count: usize,
) -> ClassMirrorSlots {
    if class.is_synthetic_stub {
        let slot = |s: usize| (s < mirror_field_count).then_some(s);
        return ClassMirrorSlots {
            name: slot(LEGACY_NAME_SLOT),
            modifiers: None, // synthetic natives don't read a modifiers slot
            primitive: slot(LEGACY_PRIMITIVE_SLOT),
            class_redefined_count: slot(LEGACY_CLASS_REDEFINED_COUNT_SLOT),
            reflection_data: None,
        };
    }
    // Real classfile: resolve by name. Accept a field only when its descriptor
    // matches one of the expected forms — this rejects an unrelated same-named
    // field in a divergent layout instead of writing the wrong type into it.
    let resolve = |name: &str, descriptors: &[&str]| -> Option<usize> {
        class
            .find_own_field(name)
            .filter(|(_, f)| !f.is_static() && descriptors.contains(&&*f.descriptor))
            .map(|(idx, _)| idx)
    };
    ClassMirrorSlots {
        name: resolve("name", &["Ljava/lang/String;"]),
        // `modifiers` is `int` in current JDKs; accept `char` defensively.
        modifiers: resolve("modifiers", &["I", "C"]),
        primitive: resolve("primitive", &["Z"]),
        class_redefined_count: resolve("classRedefinedCount", &["I"]),
        reflection_data: resolve("reflectionData", &["Ljava/lang/ref/SoftReference;"]),
    }
}

/// Whether the `CRATONVM_DBG_TOARRAY` diagnostic is enabled.
///
/// Resolved once from the environment and cached for the process lifetime,
/// so the class-mirror hot path does not pay a per-call `std::env::var`
/// (String allocation + global env-mutex acquisition) on every lookup.
fn dbg_toarray_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("CRATONVM_DBG_TOARRAY").is_ok())
}

/// Get or create a java.lang.Class mirror object for the given ClassId.
///
/// Class mirrors are cached in `SharedVm.class_mirrors` to ensure identity:
/// `a.getClass() == a.getClass()` is always true.
///
/// Layout: the mirror is sized to the real `java/lang/Class` class
/// (`num_total_fields`, ≈19 in JDK 25; 2 for the synthetic stub).  We populate
/// the `name`, `modifiers` and `primitive` instance fields — and
/// `classRedefinedCount` / `reflectionData` when present — so JDK bytecode that
/// reads them directly via `getfield` sees correct values. The target slots are
/// resolved **by name** off the loaded `java/lang/Class` (see
/// [`resolve_class_mirror_slots`]) so the mirror stays correct regardless of the
/// JDK's private field order.
///
/// The class_id ↔ mirror mapping is maintained by `SharedVm.class_mirrors`
/// (forward) and `SharedVm.class_mirrors_reverse` (reverse), which lets
/// `mirror_class_id` recover the ClassId without encoding it in the mirror's
/// Java-visible fields.
pub fn get_or_create_class_mirror(shared: &SharedVm, class_id: ClassId) -> ObjectRef {
    // PERF: read the CRATONVM_DBG_TOARRAY flag once, not on every mirror
    // lookup. `std::env::var` allocates a String and touches a global env
    // mutex on every call; this runs on the class-mirror hot path. Cache the
    // resolved bool in a process-wide OnceLock (the env var is fixed for the
    // process lifetime).
    if dbg_toarray_enabled() {
        let nm = shared
            .class_manager
            .read()
            .get_class(class_id)
            .map(|c| c.name.to_string());
        eprintln!(
            "[DBG_TOARRAY] get_or_create_class_mirror cid={:?} name={:?}",
            class_id, nm
        );
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
    let cached = shared
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.class_manager.write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm
                .get_class(id)
                .map(|cls| {
                    if cls.is_synthetic_stub {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared
                .cached_class_mirror_num_fields
                .store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared.heap.alloc_object(class_class_id, mirror_field_count);

    // Populate the `java.lang.Class` mirror. The JDK lays out `Class`'s private
    // instance fields (`name`, `modifiers`, `primitive`, `classRedefinedCount`,
    // `reflectionData`, …) in an order that is an implementation detail and has
    // changed between releases, so rather than hardcoding JDK-25 slot numbers
    // we resolve each field's heap slot **by name** off the loaded
    // `java/lang/Class`. `set_field` writes the same default null/zero into
    // every other slot during allocation, so we only set the fields whose
    // value differs from that default — and only when the field is present in
    // the loaded layout (synthetic stubs resolve nothing).
    let (class_name, access_flags, slots) = {
        let cm = shared.class_manager.read();
        let name = cm
            .get_class(class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
        let flags = cm
            .get_class(class_id)
            .map(|c| c.access_flags.bits())
            .unwrap_or(0u16);
        // Resolve the mirror's writable slots off the `java/lang/Class`
        // definition (not the mirrored class), since they describe the
        // *mirror object's* layout.
        let slots = cm
            .get_class(class_class_id)
            .map(|c| resolve_class_mirror_slots(c, mirror_field_count))
            .unwrap_or_default();
        (name, flags, slots)
    };

    // Slot 0: store class_id as Int for legacy compatibility (mirror_class_id
    // fallback and internal VM code that reads field 0). This is a VM-internal
    // convention, not a JDK field, so it stays at a fixed slot. In real-JDK
    // mode it "occupies" the first instance slot (`cachedConstructor`), but
    // JDK bytecode that reads `cachedConstructor` gets an Int which it treats
    // as an invalid reference (effectively null) — safe because the field is
    // always read under an `if (cachedConstructor == null)` guard.
    shared
        .heap
        .set_field(mirror, 0, Value::Int(class_id.as_u32() as i32));

    // name → class name String.
    if let Some(idx) = slots.name {
        let name_obj = create_java_string(shared, &class_name);
        shared
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    // modifiers → access flags as int.
    if let Some(idx) = slots.modifiers {
        shared
            .heap
            .set_field(mirror, idx, Value::Int(access_flags as i32));
    }
    // primitive → false (0) for regular class mirrors.
    if let Some(idx) = slots.primitive {
        shared.heap.set_field(mirror, idx, Value::Int(0));
    }
    // reflectionData → null (SoftReference<ReflectionData>); already the heap
    // default, but set explicitly to document the contract.
    if let Some(idx) = slots.reflection_data {
        shared.heap.set_field(mirror, idx, Value::Object(None));
    }
    // classRedefinedCount → 0 (critical for Class.reflectionData(), which reads
    // it directly via bytecode).
    if let Some(idx) = slots.class_redefined_count {
        shared.heap.set_field(mirror, idx, Value::Int(0));
    }

    mirrors.insert(class_id, mirror);
    // Reverse map for mirror_class_id (`class_id_from_mirror`).
    shared
        .class_mirrors_reverse
        .write()
        .insert(mirror, class_id);

    // Class-mirror liveness pin (companion to HIB-CV-24's `loader_pin`, see
    // `cratonvm_types::mirror_pin`): if this class was defined by a
    // user-defined `ClassLoader`, record (loader_addr, mirror_addr) so the GC
    // marker keeps this mirror alive whenever that loader is independently
    // reachable — mirroring the `ClassLoader.classes` edge a real JDK gets
    // for free, which CratonVM's synthetic `ClassLoader` model doesn't
    // maintain. Without this, `roots.rs` step 6 (which stops unconditionally
    // rooting a user-defined class's mirror) would let a STILL-LOADED class's
    // mirror die simply because nothing happens to hold a fresh `Class<?>`
    // reference to it (e.g. Tomcat/Jasper's shared JSP base classes, whose
    // `Class<?>` is normally only touched transiently during annotation
    // scanning). Built-in-loader classes need no entry: their mirrors stay
    // unconditionally rooted directly.
    if let Some(loader) =
        cratonvm_native_builtins::classloader::defining_loader_for(class_id.as_u32())
    {
        if std::env::var_os("CRATONVM_DBG_MIRRORPIN").is_some() {
            let name = shared
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            eprintln!(
                "[DBG_MIRRORPIN] add_mirror_pin class={:?} cid={:?} mirror={:?} loader_addr={:#x}",
                name,
                class_id,
                mirror.as_ptr(),
                loader.as_ptr() as usize
            );
        }
        cratonvm_types::mirror_pin::add_mirror_pin(
            loader.as_ptr() as usize,
            mirror.as_ptr() as usize,
        );
    }

    mirror
}

/// Get or create a Class mirror for a primitive type ("int", "boolean", etc.).
///
/// Primitive mirrors use ClassId(0) and store Int(-1) in field 0 as a marker.
/// Field 1 stores the primitive name as a String.
pub fn get_or_create_primitive_mirror(shared: &SharedVm, prim_name: &str) -> ObjectRef {
    let prim_name = canonical_primitive_mirror_name(prim_name);

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
    let cached = shared
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.class_manager.write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm
                .get_class(id)
                .map(|cls| {
                    if cls.is_synthetic_stub {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared
                .cached_class_mirror_num_fields
                .store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared.heap.alloc_object(class_class_id, mirror_field_count);

    // Resolve the writable `java/lang/Class` mirror slots by name (see
    // `get_or_create_class_mirror` for why this is layout-independent).
    let slots = {
        let cm = shared.class_manager.read();
        cm.get_class(class_class_id)
            .map(|c| resolve_class_mirror_slots(c, mirror_field_count))
            .unwrap_or_default()
    };

    // Slot 0: Int(-1) marks this as a primitive Class mirror (legacy
    // VM-internal convention, not a JDK field — fixed slot).
    shared.heap.set_field(mirror, 0, Value::Int(-1));

    // name → primitive type name as String.
    if let Some(idx) = slots.name {
        let name_obj = create_java_string(shared, prim_name);
        shared
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    // primitive → true (1): this IS a primitive mirror.
    if let Some(idx) = slots.primitive {
        shared.heap.set_field(mirror, idx, Value::Int(1));
    }
    // classRedefinedCount → 0.
    if let Some(idx) = slots.class_redefined_count {
        shared.heap.set_field(mirror, idx, Value::Int(0));
    }

    mirrors.insert(prim_name.to_string(), mirror);
    mirror
}

fn canonical_primitive_mirror_name(name: &str) -> &str {
    match name {
        "I" | "int" => "int",
        "J" | "long" => "long",
        "F" | "float" => "float",
        "D" | "double" => "double",
        "Z" | "boolean" => "boolean",
        "B" | "byte" => "byte",
        "C" | "char" => "char",
        "S" | "short" => "short",
        "V" | "void" => "void",
        other => other,
    }
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
    // SATB pre-barrier for the overwritten static reference, centralized here
    // so EVERY caller is covered. Statics live in this Rust-side table, not
    // the heap, so the collectors' internal `set_field` pre-barrier never
    // sees them. The interpreter's putstatic and the JIT static helper fire
    // their own barrier before calling in (double-logging an old value only
    // re-grays — harmless), but reflection `Field.set`, `Unsafe`/`VarHandle`
    // static stores and `MethodHandle` REF_putStatic dispatch all reached
    // this function raw: during concurrent marking, a static holding the
    // last snapshot-visible path to an object could be overwritten with no
    // SATB log — final remark re-scans the (new) static and misses the old
    // referent → cleanup frees a live region (hidden-pointer SATB hole).
    // `satb_barrier` is a cheap no-op when no marking cycle is active.
    if let Value::Object(Some(_)) = fields[field_index] {
        shared.heap.satb_barrier(fields[field_index]);
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
    shared
        .compact_strings
        .store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!(
        "Pre-initialized String compact string statics (COMPACT_STRINGS=true, LATIN1=0, UTF16=1)"
    );
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
        let empty_array = shared
            .heap
            .alloc_array(class_id, ArrayElementType::Reference, 0);
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
fn wrapper_primitive_name(wrapper_name: &str) -> Option<&'static str> {
    match wrapper_name {
        "java/lang/Integer" => Some("int"),
        "java/lang/Long" => Some("long"),
        "java/lang/Boolean" => Some("boolean"),
        "java/lang/Byte" => Some("byte"),
        "java/lang/Short" => Some("short"),
        "java/lang/Character" => Some("char"),
        "java/lang/Float" => Some("float"),
        "java/lang/Double" => Some("double"),
        "java/lang/Void" => Some("void"),
        _ => None,
    }
}

fn wrapper_type_static_index(class: &crate::classloading::Class) -> Option<usize> {
    let mut static_idx = 0usize;
    for field in &class.fields {
        if field.is_static() {
            if &*field.name == "TYPE" {
                return Some(static_idx);
            }
            static_idx += 1;
        }
    }
    None
}

/// Pre-initialize one primitive-wrapper `TYPE` static when the class is already loaded.
///
/// Synthetic stubs created through `ensure_synthetic_class` can already be in
/// `ClassState::Initialized`, so they never execute the native wrapper
/// `<clinit>` that would normally populate `Integer.TYPE`, `Boolean.TYPE`, etc.
/// This helper repairs that already-initialized path and is idempotent.
pub fn pre_init_wrapper_type_field_for_class(shared: &SharedVm, class_id: ClassId) -> bool {
    let (type_idx, prim_name) = {
        let cm = shared.class_manager.read();
        let Some(cls) = cm.get_class(class_id) else {
            return false;
        };
        let Some(prim_name) = wrapper_primitive_name(&cls.name) else {
            return false;
        };
        let Some(type_idx) = wrapper_type_static_index(cls) else {
            return false;
        };
        (type_idx, prim_name)
    };

    if matches!(
        get_static_shared(shared, class_id, type_idx),
        Value::Object(Some(_))
    ) {
        return false;
    }

    let mirror = get_or_create_primitive_mirror(shared, prim_name);
    set_static_shared(shared, class_id, type_idx, Value::Object(Some(mirror)));
    true
}

pub fn pre_init_wrapper_type_fields(shared: &SharedVm) {
    let class_ids: Vec<ClassId> = {
        let cm = shared.class_manager.read();
        [
            "java/lang/Integer",
            "java/lang/Long",
            "java/lang/Boolean",
            "java/lang/Byte",
            "java/lang/Short",
            "java/lang/Character",
            "java/lang/Float",
            "java/lang/Double",
            "java/lang/Void",
        ]
        .iter()
        .filter_map(|wrapper_name| cm.get_loaded_class_id(wrapper_name))
        .collect()
    };

    let initialized = class_ids
        .into_iter()
        .filter(|&class_id| pre_init_wrapper_type_field_for_class(shared, class_id))
        .count();

    if initialized > 0 {
        tracing::info!(
            "Pre-initialized TYPE fields for {} wrapper classes",
            initialized
        );
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
        write!(
            f,
            "{}.{}{}",
            self.class_name, self.method_name, self.descriptor
        )
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
    format!(
        "Java_{}_{}",
        jni_encode(class_name),
        jni_encode(method_name)
    )
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
    use crate::config::VmConfig;
    use std::sync::Arc;

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
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Reset cached field count so it re-resolves (synthetic stub → 2 fields,
        // but we need 4 for compact layout). Manually set to 4.
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "Hello");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn compact_string_utf16_roundtrip() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "\u{1F600} emoji");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("\u{1F600} emoji".to_string()));
    }

    #[test]
    fn compact_string_empty_roundtrip() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn compact_string_latin1_boundary() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        // Latin-1 boundary: U+00FF (ÿ) is the last Latin-1 char
        let obj = create_java_string(&shared, "café\u{00FF}");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("café\u{00FF}".to_string()));
    }

    #[test]
    fn compact_string_beyond_latin1() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        // U+0100 (Ā) is beyond Latin-1 → must use UTF16 coder
        let obj = create_java_string(&shared, "Hello\u{0100}World");
        let result = read_java_string(&shared.heap, obj);
        assert_eq!(result, Some("Hello\u{0100}World".to_string()));
    }

    #[test]
    fn compact_string_pool_deduplicates() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj1 = create_java_string(&shared, "same");
        let obj2 = create_java_string(&shared, "same");
        assert_eq!(obj1.as_ptr(), obj2.as_ptr());
    }

    #[test]
    fn compact_ascii_uninterned_strings_are_fresh_and_roundtrip() {
        let shared = test_shared();
        shared
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj1 = create_java_string_uninterned(&shared, "5888890");
        let obj2 = create_java_string_uninterned(&shared, "5888890");
        assert_ne!(obj1.as_ptr(), obj2.as_ptr());
        assert_eq!(
            read_java_string(&shared.heap, obj1),
            Some("5888890".to_string())
        );
        assert_eq!(
            read_java_string(&shared.heap, obj2),
            Some("5888890".to_string())
        );
    }

    /// Build a `byte[]` payload directly and decode it via
    /// `decode_java_string_value_array`, exercising the bulk byte read
    /// (`read_byte_array_bulk`) added in the P1 perf fix. Pins exact output
    /// for LATIN1 (1 byte/char), UTF16 little-endian (2 bytes/char), the empty
    /// array, and an odd-length UTF16 buffer (the trailing odd byte must be
    /// dropped, matching the previous `len / 2` per-element loop).
    fn make_byte_array(shared: &SharedVm, bytes: &[u8]) -> ObjectRef {
        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            let _ = shared.heap.set_array_element(arr, i, Value::Int(b as i32));
        }
        arr
    }

    #[test]
    fn decode_value_array_byte_paths_bulk() {
        let shared = test_shared();

        // LATIN1: one byte per char, U+0000..U+00FF map directly.
        let latin1 = make_byte_array(&shared, &[b'H', b'i', 0xFF]);
        assert_eq!(
            decode_java_string_value_array(&shared.heap, latin1, CODER_LATIN1),
            Some("Hi\u{00FF}".to_string())
        );

        // UTF16 little-endian: low byte at even index. "Ā" = U+0100 → [0x00,0x01].
        let utf16 = make_byte_array(&shared, &[b'A', 0x00, 0x00, 0x01]);
        assert_eq!(
            decode_java_string_value_array(&shared.heap, utf16, CODER_UTF16),
            Some("A\u{0100}".to_string())
        );

        // Empty byte[] decodes to the empty string under either coder.
        let empty = make_byte_array(&shared, &[]);
        assert_eq!(
            decode_java_string_value_array(&shared.heap, empty, CODER_LATIN1),
            Some(String::new())
        );

        // Odd-length UTF16 buffer: the trailing odd byte is dropped, exactly
        // as the previous `num_units = len / 2` per-element loop did.
        let odd = make_byte_array(&shared, &[b'A', 0x00, 0x42]);
        assert_eq!(
            decode_java_string_value_array(&shared.heap, odd, CODER_UTF16),
            Some("A".to_string())
        );
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
        let arr = shared
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
        shared
            .heap
            .set_array_element(arr, 0, Value::Object(Some(elem_a)))
            .unwrap();
        shared
            .heap
            .set_array_element(arr, 1, Value::Object(Some(elem_b)))
            .unwrap();
        shared
            .heap
            .set_array_element(arr, 2, Value::Object(Some(elem_c)))
            .unwrap();
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

    /// Regression: the synthetic-stub `java/lang/Class` mirror is allocated
    /// with only `CLASS_MIRROR_NUM_FIELDS` (2) slots, yet the legacy fixed
    /// layout would place `primitive` at slot 7 and `classRedefinedCount` at
    /// slot 12. `resolve_class_mirror_slots` must gate those against the
    /// allocated field count so the populator never writes out of bounds —
    /// only slots 0 (class_id) and 1 (name) are touched on a 2-field mirror.
    #[test]
    fn class_mirror_synthetic_stub_respects_field_count() {
        let shared = test_shared();
        let mirror = get_or_create_class_mirror(&shared, ClassId::new(123));
        // The synthetic-stub mirror is sized to CLASS_MIRROR_NUM_FIELDS (2),
        // while the legacy fixed layout would place `primitive` at slot 7 and
        // `classRedefinedCount` at slot 12. Reaching this line at all proves
        // the slot gate kept those writes in bounds (an ungated write would
        // have panicked in `set_field`).
        let n = shared.heap.num_fields(mirror);
        assert!(n >= CLASS_MIRROR_NUM_FIELDS);
        // Slot 0 holds the class id; the legacy name slot (1) holds the name.
        assert_eq!(shared.heap.get_field(mirror, 0), Value::Int(123));
        assert!(matches!(
            shared.heap.get_field(mirror, LEGACY_NAME_SLOT),
            Value::Object(Some(_))
        ));
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
        assert_eq!(
            get_static_shared(&shared, ClassId::new(1), 0),
            Value::Int(10)
        );
        assert_eq!(
            get_static_shared(&shared, ClassId::new(2), 0),
            Value::Int(20)
        );
    }
}
