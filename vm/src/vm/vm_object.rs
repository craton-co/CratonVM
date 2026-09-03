// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Object helpers: Java String interning, Class mirrors, static field access.

use crate::classloading::ClassId;
use crate::memory::heap::ArrayElementType;
use crate::memory::vm_heap::VmHeap;
use crate::types::{ObjectRef, Value};
use std::cell::Cell;

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

// JDK-ONLY-LAYOUT (anchor for every `java/lang/String` slot literal in this
// file): **safe on the declared JDK matrix, positional by construction.**
//
// The compact-string writers and `read_java_string_inner` address `String`
// fields as slot 0 = `value`, 1 = `coder`, 2 = `hash`, 3 = `hashIsZero`. That
// is not a synthetic invention: it is the REAL declaration order of
// `java.lang.String` on JDK 9+ (verified against the JDK 25 image — `private
// final byte[] value; private final byte coder; private int hash; private
// boolean hashIsZero;`, with `java/lang/Object` as superclass so there are no
// inherited slots ahead of them). The synthetic stub layout
// (`instance_fields(2)` in `classloading::class_manager::synthetic_stub_fields`)
// is the SUBSET `value`/`hash`, and is only reachable with `compact_strings`
// off, where the writers use the 2-slot branch.
//
// Consequences for `--jdk-only`:
//   * No conversion is required for correctness on JDK 17/21/25.
//   * It is nevertheless a *positional* dependency on a private JDK layout.
//     Pre-9 `String` is `char[] value; int hash;`, so slot 1 would be `hash`
//     and every write of `coder` would corrupt the cached hash — a silent
//     wrong-field write, not a type error. If the supported feature-version
//     matrix is ever widened downward, this must become a named lookup first.
//   * The conversion, if it is ever made, is a cached slot table resolved once
//     off the loaded `java/lang/String` — exactly the shape `ClassMirrorSlots`
//     / `resolve_class_mirror_slots` already use below for `java/lang/Class`.
//     It is deliberately NOT done here in wave 1: this is the hottest
//     allocation path in the VM (every `StringBuilder.toString`, `substring`,
//     `concat`, regex group and boxed number reaches it) and the existing
//     indices are demonstrably correct, so the change would add risk without
//     removing a defect. See `audits/jdk-only-object-layout-audit.md`.

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
    if let Some(&obj) = shared.mem.string_pool.read().get(text) {
        return obj;
    }

    // Slow path: create new string object
    let mut pool = shared.mem.string_pool.write();
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
    if let Some(&obj) = shared.mem.string_pool.read().get(text) {
        return Some(obj);
    }
    let mut pool = shared.mem.string_pool.write();
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

/// Fallible twin of [`create_java_string_uninterned`]: `None` instead of
/// aborting the process when the heap cannot hold the `String`.
///
/// Callers that hold a `JvmThread` must go through
/// `interpreter::create_string_or_oom`, which GCs and retries around this and
/// finally raises a catchable `OutOfMemoryError` -- the same contract `new`
/// has. Before that existed, a `"a" + b` on a full heap called
/// [`create_java_string_uninterned`] and `std::process::abort()`ed the VM
/// ("FATAL: heap exhausted allocating java/lang/String"), where HotSpot throws
/// `OutOfMemoryError: Java heap space`.
pub fn try_create_java_string_uninterned(shared: &SharedVm, text: &str) -> Option<ObjectRef> {
    if shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
        && text.is_ascii()
    {
        return try_alloc_java_string_object_from_ascii(shared, text.as_bytes());
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    try_alloc_java_string_object_from_units(shared, &units)
}

/// Thread-aware, GC-safe dynamic String allocation for native helpers. The
/// ASCII path uses the ordinary object TLAB and a hit-only byte-array TLAB
/// allocation, retaining a fresh String object and backing array every call.
pub fn create_java_string_uninterned_gc_safe_threaded(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    text: &str,
) -> ObjectRef {
    if shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
        && text.is_ascii()
    {
        let (class_id, fields) = java_string_allocation_layout(shared);
        // The shape planner, not a bare legacy size -- see
        // `plan_tlab_object_shape`.
        let (object_size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_at(
            class_id,
            fields,
            crate::runtime::interpreter::tlab_site::STRING,
        );
        let str_obj = crate::runtime::interpreter::tlab_alloc_object(
            thread,
            shared,
            class_id,
            fields,
            object_size,
        )
        .or_else(|| shared.mem.heap.try_alloc_object_full(class_id, fields))
        .unwrap_or_else(|| alloc_java_string_object(shared, text));
        let byte_array =
            crate::runtime::interpreter::tlab_alloc_byte_array(thread, shared, text.len())
                .or_else(|| {
                    shared.mem.heap.try_alloc_array_full(
                        ClassId::new(0),
                        ArrayElementType::Byte,
                        text.len(),
                    )
                })
                .unwrap_or_else(|| return alloc_java_string_object(shared, text));
        if let Some(base) = shared.mem.heap.array_data_ptr(byte_array) {
            // SAFETY: the fresh byte array has exactly `text.len()` elements.
            unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), base, text.len()) };
        } else {
            for (index, byte) in text.bytes().enumerate() {
                let _ =
                    shared
                        .mem
                        .heap
                        .set_array_element(byte_array, index, Value::Int(byte as i32));
            }
        }
        shared
            .mem
            .heap
            .set_field(str_obj, 0, Value::Object(Some(byte_array)));
        shared
            .mem
            .heap
            .set_field(str_obj, 1, Value::Int(CODER_LATIN1));
        shared.mem.heap.set_field(str_obj, 2, Value::Int(0));
        shared.mem.heap.set_field(str_obj, 3, Value::Int(0));
        return str_obj;
    }
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
        .classes
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
        .classes
        .cached_string_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    // Every dynamic compact String reaches this function. After first
    // resolution, a class-manager read lock for every allocation becomes a
    // cross-thread hot-path bottleneck, even though bootstrap String's class
    // id and field shape are immutable for the VM lifetime. Keep the pair per
    // host thread and VM; the existing atomic field-count remains the
    // cross-thread first-resolution gate.
    thread_local! {
        static STRING_LAYOUT_CACHE: Cell<Option<(usize, u32, usize)>> = const { Cell::new(None) };
    }
    let vm_key = shared as *const SharedVm as usize;
    let resolved = if cached != 0 {
        STRING_LAYOUT_CACHE.with(|cache| match cache.get() {
            Some((cached_vm, class_raw, field_count))
                if cached_vm == vm_key && field_count == cached =>
            {
                Some((ClassId::new(class_raw), field_count))
            }
            _ => {
                let resolved = shared
                    .classes
                    .class_manager
                    .read()
                    .get_loaded_class_id("java/lang/String")
                    .map(|id| (id, cached));
                if let Some((id, field_count)) = resolved {
                    cache.set(Some((vm_key, id.as_u32(), field_count)));
                }
                resolved
            }
        })
    } else {
        None
    };
    match resolved {
        Some(pair) => pair,
        None => {
            // Slow path: first resolution (or read-probe miss). Take the write
            // lock to load the class and compute + cache the field count.
            let mut cm = shared.classes.class_manager_write();
            let id = cm.load_class("java/lang/String").unwrap_or(ClassId::new(0));
            let count = if cached != 0 {
                cached
            } else {
                let c = cm
                    .get_class(id)
                    .map(|cls| {
                        if cls.origin.is_compatibility_stub() {
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
                    .classes
                    .cached_string_num_fields
                    .store(c, std::sync::atomic::Ordering::Relaxed);
                c
            };
            STRING_LAYOUT_CACHE.with(|cache| {
                cache.set(Some((vm_key, id.as_u32(), count)));
            });
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
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed));
    let (string_class_id, field_count) = java_string_allocation_layout(shared);
    let str_obj = shared
        .mem
        .heap
        .try_alloc_object_full(string_class_id, field_count)?;
    let byte_array = shared.mem.heap.try_alloc_array_full(
        ClassId::new(0),
        ArrayElementType::Byte,
        ascii.len(),
    )?;
    if !ascii.is_empty() {
        match shared.mem.heap.array_data_ptr(byte_array) {
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
                        .mem
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
        .mem
        .heap
        .set_field(str_obj, 0, Value::Object(Some(byte_array)));
    shared
        .mem
        .heap
        .set_field(str_obj, 1, Value::Int(CODER_LATIN1));
    shared.mem.heap.set_field(str_obj, 2, Value::Int(0));
    shared.mem.heap.set_field(str_obj, 3, Value::Int(0));
    Some(str_obj)
}

/// Fallible twin of [`alloc_java_string_object_from_units`]: returns `None`
/// instead of aborting when the heap is too full to allocate the `String`
/// object or its backing array.
fn try_alloc_java_string_object_from_units(shared: &SharedVm, units: &[u16]) -> Option<ObjectRef> {
    let (string_class_id, field_count) = java_string_allocation_layout(shared);
    let str_obj = shared
        .mem
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
        .classes
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
            let byte_array = match shared.mem.heap.try_alloc_array_full(
                ClassId::new(0),
                ArrayElementType::Byte,
                units.len(),
            ) {
                Some(a) => a,
                None => return false,
            };
            // Bulk-write, matching `try_alloc_java_string_object_from_ascii`.
            // This path is no longer only used by `String(char[])`: string
            // CONCATENATION now builds its result as units (so an unpaired
            // surrogate survives -- see `execute_string_concat`), and concat is
            // the hottest allocation site in the VM. A per-element
            // `set_array_element` boxes a `Value` and re-checks bounds for every
            // byte, which would have turned the common all-Latin-1 concat from a
            // memcpy into an N-iteration loop.
            if !units.is_empty() {
                match shared.mem.heap.array_data_ptr(byte_array) {
                    Some(base) => unsafe {
                        cratonvm_gc::heap::cell_watch_check(
                            base as usize,
                            units.len(),
                            "populate_java_string_fields/latin1",
                            &byte_array.as_ptr(),
                        );
                        for (i, &u) in units.iter().enumerate() {
                            *base.add(i) = (u & 0xFF) as u8;
                        }
                    },
                    None => {
                        for (i, &u) in units.iter().enumerate() {
                            let _ = shared.mem.heap.set_array_element(
                                byte_array,
                                i,
                                Value::Int((u & 0xFF) as i32),
                            );
                        }
                    }
                }
            }
            shared
                .mem
                .heap
                .set_field(str_obj, 0, Value::Object(Some(byte_array)));
            // write_barrier fires automatically inside set_field
            shared
                .mem
                .heap
                .set_field(str_obj, 1, Value::Int(CODER_LATIN1));
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
            let byte_array = match shared.mem.heap.try_alloc_array_full(
                ClassId::new(0),
                ArrayElementType::Byte,
                byte_len,
            ) {
                Some(a) => a,
                None => return false,
            };
            // Bulk-write for the same reason as the LATIN1 branch above.
            // Little-endian: low byte at even index, high byte at odd index.
            if !units.is_empty() {
                match shared.mem.heap.array_data_ptr(byte_array) {
                    Some(base) => unsafe {
                        cratonvm_gc::heap::cell_watch_check(
                            base as usize,
                            byte_len,
                            "populate_java_string_fields/utf16",
                            &byte_array.as_ptr(),
                        );
                        for (i, &unit) in units.iter().enumerate() {
                            *base.add(i * 2) = (unit & 0xFF) as u8;
                            *base.add(i * 2 + 1) = (unit >> 8) as u8;
                        }
                    },
                    None => {
                        for (i, &unit) in units.iter().enumerate() {
                            let lo = (unit & 0xFF) as u8;
                            let hi = (unit >> 8) as u8;
                            let _ = shared.mem.heap.set_array_element(
                                byte_array,
                                i * 2,
                                Value::Int(lo as i32),
                            );
                            let _ = shared.mem.heap.set_array_element(
                                byte_array,
                                i * 2 + 1,
                                Value::Int(hi as i32),
                            );
                        }
                    }
                }
            }
            shared
                .mem
                .heap
                .set_field(str_obj, 0, Value::Object(Some(byte_array)));
            // write_barrier fires automatically inside set_field
            shared
                .mem
                .heap
                .set_field(str_obj, 1, Value::Int(CODER_UTF16));
        }
        shared.mem.heap.set_field(str_obj, 2, Value::Int(0)); // hash
        shared.mem.heap.set_field(str_obj, 3, Value::Int(0)); // hashIsZero
    } else {
        // ---- Legacy / synthetic layout ----
        // Field 0: char[] value (UTF-16)
        // Field 1: int hash
        let char_array = match shared.mem.heap.try_alloc_array_full(
            ClassId::new(0),
            ArrayElementType::Char,
            units.len(),
        ) {
            Some(a) => a,
            None => return false,
        };
        for (i, &ch) in units.iter().enumerate() {
            let _ = shared
                .mem
                .heap
                .set_array_element(char_array, i, Value::Int(ch as i32));
        }
        shared
            .mem
            .heap
            .set_field(str_obj, 0, Value::Object(Some(char_array)));
        // write_barrier fires automatically inside set_field
        shared.mem.heap.set_field(str_obj, 1, Value::Int(0));
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

/// Decode a String's `value` array to its UTF-16 code **units**, without going
/// through a Rust `String`.
///
/// Lossless twin of [`decode_java_string_value_array`]. That one ends in
/// `String::from_utf16_lossy`, which replaces an unpaired surrogate with
/// U+FFFD because a Rust `str` cannot represent one. Any caller that reads a
/// Java `String` only to build another Java `String` must use this instead —
/// the round trip through UTF-8 is where the surrogate dies.
pub fn decode_java_string_value_array_units(
    heap: &VmHeap,
    value_array: ObjectRef,
    coder: i32,
) -> Option<Vec<u16>> {
    if heap.kind_of(value_array) != crate::memory::heap::ObjectKind::Array {
        return None;
    }
    match heap.array_element_type(value_array)? {
        ArrayElementType::Char => Some(heap.read_char_array_bulk(value_array)),
        ArrayElementType::Byte => {
            let bytes = read_byte_array_bulk(heap, value_array);
            if coder == CODER_LATIN1 {
                Some(bytes.iter().map(|&b| u16::from(b)).collect())
            } else {
                // Little-endian pairs, matching `create_java_string` and
                // `StringUTF16.isBigEndian() == false`. `chunks_exact(2)`
                // drops a trailing odd byte exactly as the `str` decoder does.
                Some(
                    bytes
                        .chunks_exact(2)
                        .map(|c| u16::from(c[0]) | (u16::from(c[1]) << 8))
                        .collect(),
                )
            }
        }
        _ => None,
    }
}

/// Read a Java String object back to a Rust `String`.
///
/// Supports both compact string layout (JDK 9+: byte[] + coder) and legacy
/// layout (char[] value).
///
/// **Lossy for unpaired surrogates** — see [`read_java_string_units`].
pub fn read_java_string(heap: &VmHeap, obj_ref: ObjectRef) -> Option<String> {
    let (value_array, coder) = java_string_value_and_coder(heap, obj_ref)?;
    decode_java_string_value_array(heap, value_array, coder)
}

/// Read a Java String object back to its UTF-16 code **units**.
///
/// Same receiver guards as [`read_java_string`], but lossless: an unpaired
/// surrogate survives. Use this whenever the destination is another Java
/// `String` rather than Rust text.
///
/// # The write side already exists — do not add a second one
///
/// This reader's lossless twin is [`create_java_string_from_units`] (and its
/// fallible sibling [`try_create_java_string_from_units`]), one screen above.
/// Both bottom out in `populate_java_string_fields`, which is the ONE function
/// that decides a `String`'s coder and byte order, and which this decoder is
/// the exact inverse of — LATIN1 when every unit fits in a byte, otherwise
/// little-endian UTF-16 pairs.
///
/// That matters because the write side is easy to believe is missing. It is
/// reachable from native code without any new VM primitive:
///
/// ```text
///   NativeContext::init_string_from_units   (native-api/src/registry.rs)
///     -> VmNativeContext::init_string_from_units   (vm/src/vm/vm_exec.rs)
///       -> populate_java_string_fields             (this file)
/// ```
///
/// so a native that has already `new_object("java/lang/String")`'d gets the
/// same bytes this reader would decode. `native-builtins`' own
/// `lang_string::sb_string_from_units` is that pair packaged as one call and is
/// the writer every `String`-returning native in that file should use.
///
/// A `create_string_from_utf16` added to `NativeContext` would therefore be a
/// THIRD spelling of one concept, and "two encodings of one thing that
/// reconcile only at consumption" is precisely how the coder/endianness
/// agreement above gets broken. If a leaner path is ever wanted, route the
/// existing trait method at a direct
/// [`create_java_string_from_units`] — do not introduce a parallel one.
pub fn read_java_string_units(heap: &VmHeap, obj_ref: ObjectRef) -> Option<Vec<u16>> {
    let (value_array, coder) = java_string_value_and_coder(heap, obj_ref)?;
    decode_java_string_value_array_units(heap, value_array, coder)
}

/// The `(value, coder)` pair of a `java/lang/String`, behind every shape guard
/// that makes the positional slot read safe on a receiver that may not be a
/// String at all.
///
/// Factored out of `read_java_string` so the `str` and the code-unit readers
/// cannot drift: the guards below are subtle and were each added for a named
/// misidentification, and two copies of them would be two chances to weaken one.
fn java_string_value_and_coder(heap: &VmHeap, obj_ref: ObjectRef) -> Option<(ObjectRef, i32)> {
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
    //
    // That test is NOT an array screen, which is what it reads like. An array
    // MIRRORS ITS LENGTH into `num_slots`, so `num_fields(new byte[23])` is 23
    // and every `byte[]` / `char[]` / `Object[]` of length >= 2 walks straight
    // through it into the positional `get_field(obj, 0)` / `get_field(obj, 1)`
    // reads below — which stride packed element data as 16-byte `Value` cells
    // and, past the first couple of indices, leave the allocation. Same species
    // as the `String[]` producer closed by `is_plain_string`
    // (`corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`),
    // reached through a SHAPE probe instead of a class-name fast path.
    //
    // The kind test belongs here as well as at the accessor: this reader is
    // deliberately speculative, so the right answer for an array is "not a
    // String", not "ask the heap to refuse the read".
    if heap.kind_of(obj_ref) == crate::memory::heap::ObjectKind::Array {
        return None;
    }
    if heap.num_fields(obj_ref) < 2 {
        return None;
    }
    // JDK-ONLY-LAYOUT: safe — see the `String` slot anchor next to
    // `CODER_LATIN1` at the top of this file. Slots 0/1 are `value`/`coder` in
    // the real JDK 9+ declaration order. Note this reader is *deliberately*
    // speculative (it is called on receivers that may not be Strings at all),
    // so it must stay index-based: a named lookup would resolve `value` off
    // whatever class the receiver actually is and defeat the shape check. The
    // `coder ∈ {0,1}` and `num_fields >= 4` guards below are what make the
    // positional probe safe; do not weaken them when converting other sites.
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
        Some(ArrayElementType::Char) | Some(ArrayElementType::Byte) => Some((value_array, coder)),
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
/// map in `SharedVm.classes.class_mirrors_reverse`.  Returns `None` for primitive
/// mirrors and for non-mirror objects.
pub fn class_id_from_mirror(shared: &SharedVm, mirror: ObjectRef) -> Option<ClassId> {
    shared
        .classes
        .class_mirrors_reverse
        .read()
        .get(&mirror)
        .copied()
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
    /// `classLoader : Ljava/lang/ClassLoader;`
    ///
    /// This is deliberately populated for user-defined classes. Besides making
    /// the real-JDK field layout faithful, it is the heap reachability edge
    /// that keeps a loader alive while application code still holds its
    /// `Class<?>` mirror. The mirror cache itself does not root user loaders
    /// when unloading is enabled.
    class_loader: Option<usize>,
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
    if class.origin.is_compatibility_stub() {
        let slot = |s: usize| (s < mirror_field_count).then_some(s);
        return ClassMirrorSlots {
            name: slot(LEGACY_NAME_SLOT),
            modifiers: None, // synthetic natives don't read a modifiers slot
            primitive: slot(LEGACY_PRIMITIVE_SLOT),
            class_redefined_count: slot(LEGACY_CLASS_REDEFINED_COUNT_SLOT),
            reflection_data: None,
            class_loader: None,
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
        class_loader: resolve("classLoader", &["Ljava/lang/ClassLoader;"]),
    }
}

/// Whether the `CRATONVM_DBG_TOARRAY` diagnostic is enabled.
///
/// Resolved once from the environment and cached for the process lifetime,
/// so the class-mirror hot path does not pay a per-call `cratonvm_types::flags::runtime_var`
/// (String allocation + global env-mutex acquisition) on every lookup.
fn dbg_toarray_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| cratonvm_types::flags::runtime_var("CRATONVM_DBG_TOARRAY").is_ok())
}

/// Get or create a java.lang.Class mirror object for the given ClassId.
///
/// Class mirrors are cached in `SharedVm.classes.class_mirrors` to ensure identity:
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
/// The class_id ↔ mirror mapping is maintained by `SharedVm.classes.class_mirrors`
/// (forward) and `SharedVm.classes.class_mirrors_reverse` (reverse), which lets
/// `mirror_class_id` recover the ClassId without encoding it in the mirror's
/// Java-visible fields.
pub fn get_or_create_class_mirror(shared: &SharedVm, class_id: ClassId) -> ObjectRef {
    // PERF: read the CRATONVM_DBG_TOARRAY flag once, not on every mirror
    // lookup. `cratonvm_types::flags::runtime_var` allocates a String and touches a global env
    // mutex on every call; this runs on the class-mirror hot path. Cache the
    // resolved bool in a process-wide OnceLock (the env var is fixed for the
    // process lifetime).
    if dbg_toarray_enabled() {
        let nm = shared
            .classes
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
    if let Some(&mirror) = shared.classes.class_mirrors.read().get(&class_id) {
        return mirror;
    }

    // Slow path: create new mirror
    let mut mirrors = shared.classes.class_mirrors.write();
    // Double-check after acquiring write lock
    if let Some(&mirror) = mirrors.get(&class_id) {
        return mirror;
    }

    // Load java/lang/Class and resolve field count (cached after first call).
    let cached = shared
        .classes
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm
                .get_class(id)
                .map(|cls| {
                    if cls.origin.is_compatibility_stub() {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared
                .classes
                .cached_class_mirror_num_fields
                .store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared
        .mem
        .heap
        .alloc_object(class_class_id, mirror_field_count);

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
        let cm = shared.classes.class_manager.read();
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
    // convention, not a JDK field, so it stays at a fixed slot — but ONLY
    // where slot 0 is not a real reference field. See the gate below.
    //
    // JDK-ONLY-LAYOUT: was `safe` (adjudicated 2026-08-10 on the three checks
    // recorded below). **That verdict is WITHDRAWN as of 2026-08-12: it rested
    // on a claim that is true of the interpreter only.**
    //
    // The withdrawn argument was: in real-JDK mode this "occupies" the first
    // instance slot (`cachedConstructor`), but JDK bytecode that reads
    // `cachedConstructor` gets an Int it treats as an invalid reference
    // (effectively null) — safe because the field is only ever read under an
    // `if (cachedConstructor == null)` guard, in `Class.newInstance()`.
    //
    // Two things falsify it:
    //
    //   a. The value is no longer an Int in the slot. Since all four heaps
    //      converged on boxing (W7-84), a primitive stored into a declared
    //      REFERENCE slot is wrapped in an `AUTOBOX_CLASS_ID` object
    //      (`gc/src/autobox.rs::box_for_reference_slot`). The slot now holds a
    //      genuinely NON-NULL object reference. Readers that go through
    //      `Heap::get_field` un-box it back to `Int` and still see an invalid
    //      reference, so the null guard holds for them.
    //   b. The JIT does not go through `Heap::get_field`. Verified by reading,
    //      2026-08-12: `jit_getfield`'s compact-reference arm
    //      (`vm/src/jit/helpers.rs:5642-5660`) does its own raw
    //      `read_ref_slot` + `jit_decode_ref_word` — a heap-plausibility
    //      filter, not a class-id check — and never calls `crate::autobox`.
    //      The default inline path does not even reach that helper: it emits a
    //      bare `MOV RAX, [RAX + disp32]` against the slot
    //      (`jit/src/ir_lower.rs:2419-2421`,
    //      `jit/src/x64/bytecode_walk.rs:4034-4036`), behind null/alignment/
    //      region/`GC_FLAG_COMPACT` guards, none of which is a type check.
    //      `AUTOBOX_CLASS_ID` appears nowhere in `jit/`, `jit-api/`, or
    //      `vm/src/jit/`.
    //
    // So at the JIT tier `cachedConstructor != null` is TRUE, the fast path is
    // taken, and `Constructor.newInstance` is invoked on an object that is not
    // a `Constructor` — a wrong-type dispatch on a JDK method any application
    // can call, reachable only once the method tiers up. That is why the 2026-
    // 08-04/08-10 probes below came back clean: they ran interpreted, and the
    // interpreter is the one reader this overlay is safe for.
    //
    // FIX (2026-08-12): gate the write on whether slot 0 is a compact
    // REFERENCE field, the same idiom as the `System.out` fd tag in
    // `vm_init.rs`. In real-JDK + compact mode the write is skipped, so
    // `cachedConstructor` stays a true null and both tiers agree. In
    // synthetic-jdk mode `java/lang/Class` is a fabricated stub with no
    // registered compact layout at all, `class_layout` returns `None`, and the
    // legacy convention (plus every test asserting it) is untouched.
    //
    // Safety of skipping, established rather than asserted — see the reader
    // census at check 3 below, re-run 2026-08-12. `class_mirrors_reverse` is
    // populated a few lines down for every class mirror and answers every
    // runtime lookup; `mirror_class_id` consults it FIRST and only falls back
    // to slot 0. One reader was found that never asks the reverse map at all
    // (`ctx_annotation_values_equal`); it is nominated, not owned here.
    //
    // This is an *overlay*: a VM-internal value deliberately written on top of
    // a real JDK field, which is a different hazard from a mis-numbered slot.
    // Verified against JDK 25: instance field 0 of `java.lang.Class` is
    // `private volatile transient Constructor<T> cachedConstructor`, i.e. a
    // REFERENCE slot receiving an `Int`. The claim above ("treated as an
    // invalid reference, effectively null") is an assertion about this VM's
    // reference-vs-primitive decode, not about HotSpot, and it is exactly the
    // shape that produces `expected object reference, got int(N)` elsewhere in
    // the tree. `NativeContextImpl::set_field` also runs the value through
    // `set_field_as` with the DECLARED descriptor, so the stored tag depends on
    // that coercion — it is not obviously a stable Int.
    //
    // Evidence that would settle it (do not guess) — RUN 2026-08-04, results
    // inline. The verdict stays `unknown` but is **no longer ranked HIGH**: the
    // two checks that would have shown live harm both came back clean.
    //   1. Under `--real-jdk`, run a program that reaches
    //      `Class.getConstructor(...)` / `Class.getDeclaredConstructor(...)`
    //      twice on the same class (so the second call takes the
    //      `cachedConstructor != null` fast path) and check whether the
    //      `getfield cachedConstructor` succeeds, returns null, or raises.
    //      **CLEAN.** Three rounds of `getDeclaredConstructor()` on a nested
    //      class, interleaved with `String.class.getConstructor(String.class)`,
    //      all returned the right `Constructor` against a real JDK 21 image.
    //      Nothing raised, and no `expected object reference, got int(N)`.
    //   2. Run the same with `CRATONVM_DBG_OVERLAY` enabled — the overlay
    //      hunter in `vm_exec.rs` (`overlay_access_is_cross_type`) exists
    //      precisely to report a primitive written to a reference slot, and
    //      this write should appear in its output if the hazard is live.
    //      **CLEAN.** The hunter reported nothing on that run. Note the hunter
    //      was widened on 2026-08-05 (L4): it now also instruments READS and
    //      fires on any slot where CratonVM's fabricated model and the loaded
    //      image disagree, so this check is worth re-running — the 2026-08-04
    //      silence was over a strictly narrower detector.
    //   3. Confirm which readers still depend on slot 0: the reverse map
    //      (`class_mirrors_reverse` / `class_id_from_mirror`) is the primary
    //      path, and `mirror_class_id` in `native-builtins/src/lang_class.rs`
    //      only falls back to `get_field(mirror, 0)` when the reverse map
    //      misses. If the census shows zero fallback hits under a real JDK, the
    //      correct wave-2 fix is to DELETE the slot-0 write (and the slot-1
    //      name read in `mirror_class_name`) rather than relocate it.
    //      **ANSWERED 2026-08-10, and the question's premise was wrong.**
    //      `mirror_class_id` is NOT the only reader: grepping
    //      `get_field(mirror, 0)` finds eight in `native-builtins`. Two were
    //      reading it *wrong* — the slf4j and log4j `getLogger(Class)` shims
    //      expected a name String at slot 0, which no mirror this VM builds has
    //      ever held, so every such logger was named "unknown" — and both now
    //      go through `mirror_class_name`. `servlet.rs`'s ServiceLoader lookup
    //      took the ClassId straight from slot 0 behind an
    //      `object_num_fields == 0` guard and now goes through
    //      `mirror_class_id`. The rest already asked the reverse map first.
    //
    //      With that done, the fallback was measured on a broad real-JDK
    //      workload rather than a probe: five Spring Framework test classes
    //      (`AutowiredAnnotationBeanRegistrationAotContribution`,
    //      `BeanDefinitionMethodGenerator`,
    //      `CommonAnnotationBeanRegistrationAotContribution`,
    //      `ConfigurationClassPostProcessorAotContribution`,
    //      `InitDestroyMethodLifecycle`), 87 tests, all green, under
    //      `CRATONVM_DBG_OVERLAY=1`. **Zero fallback hits.** The reverse map
    //      answers every runtime lookup.
    //
    // What checks 1-3 settle, and what they do not. They establish that the
    // overlay is not destructive and not load-bearing at runtime — both still
    // hold, and both are why SKIPPING the write is safe. They do NOT establish
    // that keeping it is safe, because all three ran interpreted (see (b)
    // above). The 2026-08-10 conclusion "the write nevertheless stays, because
    // it is still load-bearing for `MockNativeContext`" was also wrong on its
    // own terms: `MockNativeContext` never calls this function. It builds its
    // own two-slot mirror inline (`native-builtins/src/test_utils.rs`
    // `get_class_mirror` / `primitive_class_mirror`) and its
    // `class_id_from_mirror` reads that. Nothing in the mock is coupled to
    // this store, in either mode.
    //
    // `class_layout` is only populated when compact reference fields are
    // enabled, so with that flag off this is a no-op and legacy behaviour
    // stands — the same caveat the `vm_init.rs` precedent carries.
    // REVERTED 2026-08-12, same day, by measurement.
    //
    // This store was gated on `!slot0_is_ref` to stop an `Int` landing in a
    // slot the real `java.lang.Class` declares as `Constructor cachedConstructor`
    // — every collector boxes it, and the JIT's field helpers read the compact
    // slot RAW without un-boxing, so a compiled `Class.newInstance()` sees a
    // non-null `AUTOBOX` wrapper and takes the cached path. That analysis is
    // sound and the JIT hazard is real (W7-84 §8; `AUTOBOX_CLASS_ID` appears
    // nowhere under `jit/`).
    //
    // But the gate's PREMISE was that `class_mirrors_reverse` answers every
    // runtime lookup and `mirror_class_id`'s slot-0 fallback is never used —
    // argued from a measured zero-hit count over 87 Spring tests. **That is
    // falsified.** With the gate in, `RJdkHello` fails at
    // `System.out instanceof PrintStream`; the pre-wave binary passes it 41/41.
    // Some type-check path resolves a mirror through the slot-0 fallback, and a
    // zero-hit count on one corpus did not license "never".
    //
    // A measured regression outweighs an unmeasured hazard, so the overlay is
    // restored and the JIT hazard stays OPEN with its analysis intact. The
    // durable fix is on the JIT side (un-box in `jit_getfield`'s compact-ref arm
    // and the two inline emitters, or refuse to inline reference loads); this
    // site is the wrong place to force it. Do not re-gate this without first
    // finding the reader that needs the fallback.
    //
    // G30 (2026-08-17) — TWO CORRECTIONS TO THE PARAGRAPHS ABOVE, both
    // MEASURED, and the second one is a trap:
    //
    //   1. This site is not "one of the" W7-84 sites, it is THE W7-84 site.
    //      MEASURED, `RJdkHello --jdk-only` with `CRATONVM_DBG_OVERLAY=1`:
    //      every one of the 12 `cratonvm::gc::guard` warnings a run emits is
    //      `class_id=ClassId(12) index=0`, and 12 log lines stand for at
    //      least 33 stores because the guard is rate-limited to
    //      `n < 8 || n.is_power_of_two()`. Corroborated across
    //      `RJdkNet`/`RJdkAsyncChannel`/`RSslNullSession`/`RJdkCollections`/
    //      `RCrypto`: 11-12 warnings each, same class, same slot. So a W7-84
    //      count is a census of THIS LINE and of nothing else — it is not a
    //      proxy for "primitives written at reference slots" anywhere in the
    //      tree, and G30-1 §1 records two records that read it that way.
    //
    //   2. The comment above says `NativeContextImpl::set_field` "also runs
    //      the value through `set_field_as` with the DECLARED descriptor, so
    //      the stored tag depends on that coercion — it is not obviously a
    //      stable Int". That is true of a NATIVE writing this slot and false
    //      of THIS write, and the difference decides whether the mirror
    //      works. The call below is the descriptor-LESS `set_field`, whose
    //      compact-reference arm goes to `gc::autobox::box_for_reference_slot`
    //      and BOXES: slot 0 ends up holding an `AUTOBOX_CLASS_ID` wrapper
    //      that `Heap::get_field` un-boxes back to `Int`, which is what
    //      `mirror_class_id`'s fallback needs. The descriptor-aware
    //      `set_field_as(.., b'L')` goes to
    //      `gc::heap::coerce_field_value_for_slot` instead, whose `b'L'` arm
    //      maps `Int(_)` to `Value::Object(None)` — it would store a NULL,
    //      the fallback would answer `None`, and `RJdkHello` would fail at
    //      `System.out instanceof PrintStream` exactly as the gate above
    //      already made it fail. **Do not "modernise" this to
    //      `set_field_as`.** `the_class_mirror_id_is_written_without_a_field_descriptor`
    //      is the tripwire.
    // 2026-09-01 — THIS STORE IS THE WHOLE OF THE W7-84 GUARD'S DEFAULT-RUN
    // OUTPUT, AND THAT IS NOW A MEASURED COST RATHER THAN A CURIOSITY.
    //
    // A stock `cratonvm Hello` — a one-line hello-world — printed 28 lines of
    // internal diagnostics to stderr where HotSpot prints zero, and about a
    // dozen of them are `cratonvm::gc::guard` warnings from this line and its
    // primitive-mirror twin: the boot mints ~33 mirrors, the guard's
    // `n < 8 || n.is_power_of_two()` sample turns that into occurrences
    // 0..=7, 8, 16, 32. Corroborated in G30 §1 above, which established that
    // EVERY such warning in a run is `class_id=ClassId(12) index=0`.
    //
    // So the guard spends its entire default-run budget reporting the VM to
    // itself, and a genuine third-party primitive-into-reference store — the
    // thing it exists to catch — arrives after the rate limiter has already
    // been spent on 33 copies of a known, deliberate overlay.
    //
    // The fix is NOT to quieten the guard and NOT to raise its threshold: it
    // stays fully armed, and it must, because it is the only detector that
    // sees this species at the store. The fix is to mark THIS producer as
    // expected, so the guard goes quiet for it alone.
    //
    // Three cheaper-looking answers were considered and are all unsound:
    //   * boxing by hand here (allocate the `AUTOBOX_CLASS_ID` wrapper and
    //     store a reference, so `needs_reference_box` is false) does not work:
    //     `autobox`'s read-side unbox is gated on a process-global
    //     `WRAPPER_CREATED` latch that only the boxing path arms, so
    //     `Heap::get_field` would hand `mirror_class_id`'s fallback the
    //     WRAPPER instead of the `Int` — the same failure the 2026-08-12 gate
    //     produced, by a different route;
    //   * `set_field_as` is refused by the G30 note directly above;
    //   * skipping the store is the 2026-08-12 gate, falsified by measurement.
    //
    // LANDED 2026-09-01. `gc::autobox::expect_primitive_into_reference` is a
    // thread-local, RAII, depth-counted scope; a boxing store made inside one
    // is counted into `autobox::expected_primitive_into_reference_count()` and
    // returns from the guard BEFORE `SEEN.fetch_add`, so it spends no
    // rate-limit budget either. That second half is the load-bearing one: a
    // suppression that still consumed the budget would push the first genuine
    // third-party store past occurrence 33, where the next printable
    // occurrence is 64, and it would never be printed at all.
    //
    // Three properties of the scope are deliberate and a future edit must keep
    // them:
    //   * THREAD-LOCAL, not a global flag. A global would also mute a
    //     concurrent third-party store on another thread while this populator
    //     runs, and the boot is exactly when other threads start.
    //   * SCOPED TO THE ONE STATEMENT. `drop(_expected)` is explicit rather
    //     than end-of-function: the `slots.name` write immediately below
    //     allocates a String and stores a reference, and the store this scope
    //     exists for allocates a wrapper, which can collect — nothing beyond
    //     the single `set_field` should inherit the expectation. What IS
    //     inside it is one `VmHeap::set_field`: the SATB pre-barrier, the
    //     backend `set_field`, and `autobox::box_for_reference_slot`, whose
    //     `observe_…` call happens BEFORE `alloc_wrapper`, so the count is
    //     taken before any collection can start.
    //   * NOT A `class_id == 12 && index == 0` FILTER. That would swallow a
    //     third-party store into this same slot, which is the single most
    //     interesting store the guard could ever see.
    let _expected = cratonvm_gc::autobox::expect_primitive_into_reference();
    shared
        .mem
        .heap
        .set_field(mirror, 0, Value::Int(class_id.as_u32() as i32));
    drop(_expected);

    // name → class name String.
    if let Some(idx) = slots.name {
        let name_obj = create_java_string(shared, &class_name);
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    // modifiers → access flags as int.
    if let Some(idx) = slots.modifiers {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Int(access_flags as i32));
    }
    // primitive → false (0) for regular class mirrors.
    if let Some(idx) = slots.primitive {
        shared.mem.heap.set_field(mirror, idx, Value::Int(0));
    }
    // reflectionData → null (SoftReference<ReflectionData>); already the heap
    // default, but set explicitly to document the contract.
    if let Some(idx) = slots.reflection_data {
        shared.mem.heap.set_field(mirror, idx, Value::Object(None));
    }
    // classRedefinedCount → 0 (critical for Class.reflectionData(), which reads
    // it directly via bytecode).
    if let Some(idx) = slots.class_redefined_count {
        shared.mem.heap.set_field(mirror, idx, Value::Int(0));
    }
    // A live Class mirror must keep its defining user loader live. Without
    // this real heap edge, the loader-unload pass can prune the side-table
    // entry while a framework (notably Hibernate's nested JUnit engine) still
    // retains the Class object. Subsequent `getClassLoader()` then falls back
    // to the app loader and same-named class selection crosses loaders.
    if let (Some(idx), Some(loader)) = (
        slots.class_loader,
        cratonvm_native_builtins::classloader::defining_loader_for(
            shared.vm_identity,
            class_id.as_u32(),
        ),
    ) {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(loader)));
    }

    mirrors.insert(class_id, mirror);
    // Reverse map for mirror_class_id (`class_id_from_mirror`).
    shared
        .classes
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
    if let Some(loader) = cratonvm_native_builtins::classloader::defining_loader_for(
        shared.vm_identity,
        class_id.as_u32(),
    ) {
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_MIRRORPIN").is_some() {
            let name = shared
                .classes
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
        // Arm the young marker's edge trace on THIS class's loader.
        // `CRATONVM_DBG_MARK_WHY_CLASS=<internal/class/Name>` — the address is
        // not knowable before the class is defined, which is why the watch is
        // armed here instead of parsed from a hex env var at startup.
        if let Ok(want) = cratonvm_types::flags::runtime_var("CRATONVM_DBG_MARK_WHY_CLASS") {
            let name = shared
                .classes
                .class_manager
                .read()
                .get_class(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            if name == want {
                let addr = loader.as_ptr() as usize;
                eprintln!("[MARKWHY] arming young-mark watch on {name} loader={addr:#x}");
                cratonvm_gc::heap::set_young_mark_watch(addr);
            }
        }
        cratonvm_types::mirror_pin::add_mirror_pin(
            shared.vm_identity,
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
    if let Some(&mirror) = shared.classes.primitive_mirrors.read().get(prim_name) {
        return mirror;
    }

    // Slow path
    let mut mirrors = shared.classes.primitive_mirrors.write();
    if let Some(&mirror) = mirrors.get(prim_name) {
        return mirror;
    }

    // Resolve the real field count for java/lang/Class (same logic as
    // get_or_create_class_mirror). This ensures primitive mirrors are
    // allocated with the correct number of fields when running against
    // real JDK classes. Falls back to CLASS_MIRROR_NUM_FIELDS for synthetic mode.
    let cached = shared
        .classes
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = {
        let mut cm = shared.classes.class_manager_write();
        let id = cm.load_class("java/lang/Class").unwrap_or(ClassId::new(0));
        let count = if cached != 0 {
            cached
        } else {
            let c = cm
                .get_class(id)
                .map(|cls| {
                    if cls.origin.is_compatibility_stub() {
                        CLASS_MIRROR_NUM_FIELDS
                    } else {
                        cls.num_total_fields
                    }
                })
                .unwrap_or(CLASS_MIRROR_NUM_FIELDS);
            let c = if c == 0 { CLASS_MIRROR_NUM_FIELDS } else { c };
            shared
                .classes
                .cached_class_mirror_num_fields
                .store(c, std::sync::atomic::Ordering::Relaxed);
            c
        };
        (id, count)
    };

    let mirror = shared
        .mem
        .heap
        .alloc_object(class_class_id, mirror_field_count);

    // Resolve the writable `java/lang/Class` mirror slots by name (see
    // `get_or_create_class_mirror` for why this is layout-independent).
    let slots = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(class_class_id)
            .map(|c| resolve_class_mirror_slots(c, mirror_field_count))
            .unwrap_or_default()
    };

    // Slot 0: Int(-1) marks this as a primitive Class mirror (legacy
    // VM-internal convention, not a JDK field — fixed slot).
    //
    // JDK-ONLY-LAYOUT: same overlay as the class-mirror populator above (slot 0
    // of a real `java.lang.Class` is `cachedConstructor`, a reference), and the
    // marker said to resolve the two together — so it carries the same
    // WITHDRAWN `safe` verdict and the same 2026-08-12 gate. Read the long
    // note in `get_or_create_class_mirror` for the JIT argument; in short, the
    // boxed wrapper is non-null and the JIT's field read does not un-box it.
    //
    // This site is the easier of the two to retire, precisely because a
    // primitive mirror has no legitimate `cachedConstructor` reader: `Int(-1)`
    // is a sentinel nothing but this VM asks for. Two further reasons skipping
    // it is safe, both checked 2026-08-12 rather than assumed:
    //   * Primitive mirrors are NOT in `class_mirrors_reverse` (this function
    //     never inserts; it registers by name in `primitive_mirrors`), so
    //     `mirror_class_id` returns `None` for them TODAY — `Int(-1)` fails its
    //     `v >= 0` guard. Skipping the write changes nothing for that reader.
    //   * "Is this a primitive mirror?" is not answered from slot 0 anywhere.
    //     `mirror_is_primitive` reads the by-name-resolved `primitive : Z` slot
    //     written a few lines below, falling back to the name string.
    // The `MockNativeContext` argument that previously kept this write does not
    // apply either: the mock builds its own mirrors and never calls this.
    // REVERTED 2026-08-12 alongside the class-mirror store above, for the same
    // reason and out of the same caution: the gate there was falsified by
    // measurement (`System.out instanceof PrintStream` began failing), and the
    // two stores are one convention. Reverting only the half that was proven to
    // regress, while leaving its twin gated, would leave the two mirror kinds
    // disagreeing about whether slot 0 is written at all — which is a worse
    // state than either consistent choice and exactly the kind of half-applied
    // repair this campaign kept finding.
    // Counted into the same W7-84 expected-overlay census as the class-mirror
    // store, by the same mechanism and for the same reason — read the
    // 2026-09-01 note at that site for the measurement, for the three unsound
    // alternatives (hand-boxing, `set_field_as`, skipping), and for why the
    // `drop` below is explicit rather than end-of-scope.
    //
    // The scope must not reach the `slots.name` write a few lines down: that
    // one allocates a String and stores a genuine reference, and a suppression
    // covering it would hide a real defect if the value ever stopped being one.
    let _expected = cratonvm_gc::autobox::expect_primitive_into_reference();
    shared.mem.heap.set_field(mirror, 0, Value::Int(-1));
    drop(_expected);

    // name → primitive type name as String.
    if let Some(idx) = slots.name {
        let name_obj = create_java_string(shared, prim_name);
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    // primitive → 1 ONLY when `prim_name` really is one of the nine primitive
    // type names. This function is not only the primitive-mirror factory: it
    // is also the VM's generic *stand-in* mirror factory, reached whenever a
    // native has a class NAME it could not resolve to a `ClassId` and still
    // has to hand Java code a `Class` object (`build_method_type_from_
    // descriptor`'s `class_id_by_name` miss, the array-mirror minting in
    // `lang_class.rs`, `getPrimitiveClass`-shaped shims, …). Writing `1`
    // unconditionally made every one of those stand-ins claim to be primitive.
    //
    // The damage is not cosmetic. `Class.isPrimitive()` is load-bearing inside
    // `java.lang.invoke`: `MethodTypeForm.canonicalize(t, ERASE)` erases a
    // reference parameter to `Object` only when `!t.isPrimitive()`, so a
    // stand-in that lies makes `findForm` believe an unerased `MethodType` is
    // already erased, hand it straight to `new MethodTypeForm(mt)`, and die in
    // `Wrapper.forPrimitiveType(<that class>)` with
    // `IllegalArgumentException: not primitive: <name>`. That is the exact
    // failure Groovy's `IndyInterface` fallback hit on every `invokedynamic`
    // call site whose receiver is a script class the by-name lookup cannot see
    // (`GroovyClassLoader$InnerLoader` defines it), and the reason the message
    // reads `not primitive: beans` rather than `not primitive: class beans` —
    // `Class.toString()` drops the `"class "` prefix precisely when
    // `isPrimitive()` is true, so the message is its own proof.
    // `int[].class.isPrimitive()` was wrong for the same reason.
    //
    // `mirror_is_primitive` still answers `true` for the nine real primitives
    // through its name fallback, so nothing that depends on a genuine
    // primitive mirror changes.
    let is_real_primitive = matches!(
        prim_name,
        "int" | "long" | "float" | "double" | "boolean" | "byte" | "char" | "short" | "void"
    );
    if let Some(idx) = slots.primitive {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Int(i32::from(is_real_primitive)));
    }
    // classRedefinedCount → 0.
    if let Some(idx) = slots.class_redefined_count {
        shared.mem.heap.set_field(mirror, idx, Value::Int(0));
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

/// Lock-free statics reads that found / did not find a published block.
///
/// A change that makes no measurable difference has two very different causes:
/// the fast path is never taken (an INERT lever), or it is taken and is not
/// actually cheaper. These separate them.
static STATICS_INDEX_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STATICS_INDEX_MISSES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn statics_index_hits() -> u64 {
    STATICS_INDEX_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn statics_index_misses() -> u64 {
    STATICS_INDEX_MISSES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Are the diagnostic hit/miss counters on? (`CRATONVM_DBG_GETSTATIC_PROF=1`)
#[inline]
fn statics_index_counters_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_GETSTATIC_PROF").is_some()
    })
}

/// Is the lock-free statics read path switched off? (`CRATONVM_NO_STATICS_INDEX=1`)
///
/// `pub` (re-exported as `crate::vm::statics_index_disabled`, like the
/// hit/miss counters beside it) because the JIT's compile-time static-slot
/// resolver honours the same switch: with the index off, compiled code must not
/// bake a direct load either, or the switch would silently stop being an A/B of
/// the lock-free path once methods tier up.
#[inline]
pub fn statics_index_disabled() -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OFF.get_or_init(|| {
        cratonvm_types::flags::runtime_var_os("CRATONVM_NO_STATICS_INDEX").is_some()
    })
}

/// Get a static field value from the shared state.
pub fn get_static_shared(shared: &SharedVm, class_id: ClassId, field_index: usize) -> Value {
    // Lock-free path first: `statics_index` mirrors the map and reaches the
    // same never-freed slot without an `RwLock` acquisition or a hash probe.
    // Those two were what remained of `getstatic`'s cost in compiled code once
    // the helper's other per-call work was removed.
    //
    // `CRATONVM_NO_STATICS_INDEX=1` routes every read back through the locked
    // map. That is both the escape hatch if the unsynchronized read ever proves
    // to matter, and the way to A/B this change inside ONE binary rather than
    // against a separately-built baseline.
    if !statics_index_disabled() {
        let hit = shared.classes.statics_index.get(class_id, field_index);
        // The counters are diagnostic only. Incrementing a process-wide atomic
        // on every static read would put a contended cache line in the exact
        // path this change exists to make cheap, so they are gated.
        if statics_index_counters_on() {
            if hit.is_some() {
                STATICS_INDEX_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                STATICS_INDEX_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if let Some(v) = hit {
            return v;
        }
    }
    shared
        .classes
        .statics
        .read()
        .get(&class_id)
        .and_then(|fields| fields.get(field_index))
        .copied()
        .unwrap_or(Value::Int(0))
}

/// Set a static field value in the shared state.
pub fn set_static_shared(shared: &SharedVm, class_id: ClassId, field_index: usize, value: Value) {
    let mut statics = shared.classes.statics.write();
    let mut republish = false;
    let fields = statics.entry(class_id).or_insert_with(|| {
        // Reading class_manager while holding statics write is fine because
        // class_manager read is non-exclusive, and no code path holds
        // class_manager write while trying to acquire statics.
        //
        // Type each slot from its static field's DESCRIPTOR. `StaticsBlock::new`
        // fills with `Value::Int(0)`, and a `J`/`D` static left at `Int(0)` is
        // read correctly by the interpreter (which widens) but as garbage by
        // JIT-compiled code, which takes the load width from the descriptor and
        // reads 8 bytes over a 4-byte payload. That is the loader/zip cluster's
        // defect arriving through a second door: `prepare_class` seeds typed
        // defaults, but this path runs when a static is written BEFORE its class
        // is prepared, and it used to undo that.
        let (static_descriptors, num_fields) = {
            let cm = shared.classes.class_manager.read();
            match cm.get_class(class_id) {
                Some(c) => (
                    c.fields
                        .iter()
                        .filter(|f| f.is_static())
                        .map(|f| f.descriptor.to_string())
                        .collect::<Vec<_>>(),
                    c.fields.len(),
                ),
                None => (Vec::new(), 0),
            }
        };
        republish = true;
        crate::vm::realms::class_realm::StaticsBlock::from_values(
            super::vm_util::typed_default_static_slots(&static_descriptors, num_fields),
        )
    });
    if field_index >= fields.len() {
        // Growth relocates the block (the old one stays mapped for any
        // in-flight lock-free reader), so the index must be re-pointed.
        fields.grow_to(field_index + 1);
        republish = true;
    }
    if republish {
        shared.classes.statics_index.publish(class_id, fields);
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
        shared.mem.heap.satb_barrier(fields[field_index]);
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
/// Also sets `SharedVm.classes.compact_strings` flag so `create_java_string` and
/// `read_java_string` use the correct layout.
pub fn pre_init_string_statics(shared: &SharedVm) {
    let cm = shared.classes.class_manager.read();
    let string_id = match cm.get_loaded_class_id("java/lang/String") {
        Some(id) => id,
        None => return, // String not loaded yet
    };
    let cls = match cm.get_class(string_id) {
        Some(c) => c,
        None => return,
    };

    // Only pre-init for real (non-synthetic) String class
    if cls.origin.is_compatibility_stub() {
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
        .classes
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
    let cm = shared.classes.class_manager.read();
    let class_id = match cm.get_loaded_class_id("java/lang/Class") {
        Some(id) => id,
        None => return,
    };
    let cls = match cm.get_class(class_id) {
        Some(c) => c,
        None => return,
    };
    if cls.origin.is_compatibility_stub() {
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
            .mem
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
        let cm = shared.classes.class_manager.read();
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
        let cm = shared.classes.class_manager.read();
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

    let cm = shared.classes.class_manager.read();
    // Iterate over all loaded classes in the ClassStore.
    //
    // `slot_count()`, not `len()`: `len()` is the LIVE class count, so after
    // any class unload the tombstone makes it smaller than the id upper bound
    // and this census silently stopped short of the highest-id classes — the
    // most recently loaded ones. `get` already skips tombstones below.
    for class_id_u32 in 0..cm.class_store.slot_count() as u32 {
        let class_id = ClassId::new(class_id_u32);
        let class = match cm.class_store.get(class_id) {
            Some(c) => c,
            None => continue,
        };
        // Only scan classes that HAVE a class file. A fabricated class's
        // NATIVE-flagged entries exist precisely because a native is registered
        // for them, so reporting them as "covered"/"missing" measures this
        // VM's own bookkeeping rather than the JDK surface this census is for.
        // Question (2), not (1) — see `Class::dispatch_lacks_class_file`.
        if class.dispatch_lacks_class_file() {
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
                .natives
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
// Test-only: every caller is a #[test] in `vm_init.rs`. Revealed when the
// region splitter stopped closing a test region early on a string brace.
#[cfg(test)]
pub fn scan_class_natives(shared: &SharedVm, class_name: &str) -> Vec<NativeMethodInfo> {
    let cm = shared.classes.class_manager.read();
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
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn create_empty_string() {
        let shared = test_shared();
        let obj = create_java_string(&shared, "");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn create_unicode_string() {
        let shared = test_shared();
        let obj = create_java_string(&shared, "\u{1F600} emoji");
        let result = read_java_string(&shared.mem.heap, obj);
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
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
        shared.mem.heap.set_field(obj, 0, Value::Int(0));
        let result = read_java_string(&shared.mem.heap, obj);
        assert!(result.is_none());
    }

    #[test]
    fn read_string_null_value_field() {
        let shared = test_shared();
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 2);
        shared.mem.heap.set_field(obj, 0, Value::Object(None));
        let result = read_java_string(&shared.mem.heap, obj);
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
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Reset cached field count so it re-resolves (synthetic stub → 2 fields,
        // but we need 4 for compact layout). Manually set to 4.
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "Hello");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some("Hello".to_string()));
    }

    #[test]
    fn compact_string_utf16_roundtrip() {
        let shared = test_shared();
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "\u{1F600} emoji");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some("\u{1F600} emoji".to_string()));
    }

    #[test]
    fn compact_string_empty_roundtrip() {
        let shared = test_shared();
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj = create_java_string(&shared, "");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some(String::new()));
    }

    #[test]
    fn compact_string_latin1_boundary() {
        let shared = test_shared();
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        // Latin-1 boundary: U+00FF (ÿ) is the last Latin-1 char
        let obj = create_java_string(&shared, "café\u{00FF}");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some("café\u{00FF}".to_string()));
    }

    #[test]
    fn compact_string_beyond_latin1() {
        let shared = test_shared();
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        // U+0100 (Ā) is beyond Latin-1 → must use UTF16 coder
        let obj = create_java_string(&shared, "Hello\u{0100}World");
        let result = read_java_string(&shared.mem.heap, obj);
        assert_eq!(result, Some("Hello\u{0100}World".to_string()));
    }

    #[test]
    fn compact_string_pool_deduplicates() {
        let shared = test_shared();
        shared
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
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
            .classes
            .compact_strings
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .classes
            .cached_string_num_fields
            .store(4, std::sync::atomic::Ordering::Relaxed);

        let obj1 = create_java_string_uninterned(&shared, "5888890");
        let obj2 = create_java_string_uninterned(&shared, "5888890");
        assert_ne!(obj1.as_ptr(), obj2.as_ptr());
        assert_eq!(
            read_java_string(&shared.mem.heap, obj1),
            Some("5888890".to_string())
        );
        assert_eq!(
            read_java_string(&shared.mem.heap, obj2),
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
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Byte, bytes.len());
        for (i, &b) in bytes.iter().enumerate() {
            let _ = shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Int(b as i32));
        }
        arr
    }

    #[test]
    fn decode_value_array_byte_paths_bulk() {
        let shared = test_shared();

        // LATIN1: one byte per char, U+0000..U+00FF map directly.
        let latin1 = make_byte_array(&shared, &[b'H', b'i', 0xFF]);
        assert_eq!(
            decode_java_string_value_array(&shared.mem.heap, latin1, CODER_LATIN1),
            Some("Hi\u{00FF}".to_string())
        );

        // UTF16 little-endian: low byte at even index. "Ā" = U+0100 → [0x00,0x01].
        let utf16 = make_byte_array(&shared, &[b'A', 0x00, 0x00, 0x01]);
        assert_eq!(
            decode_java_string_value_array(&shared.mem.heap, utf16, CODER_UTF16),
            Some("A\u{0100}".to_string())
        );

        // Empty byte[] decodes to the empty string under either coder.
        let empty = make_byte_array(&shared, &[]);
        assert_eq!(
            decode_java_string_value_array(&shared.mem.heap, empty, CODER_LATIN1),
            Some(String::new())
        );

        // Odd-length UTF16 buffer: the trailing odd byte is dropped, exactly
        // as the previous `num_units = len / 2` per-element loop did.
        let odd = make_byte_array(&shared, &[b'A', 0x00, 0x42]);
        assert_eq!(
            decode_java_string_value_array(&shared.mem.heap, odd, CODER_UTF16),
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
        let obj = shared.mem.heap.alloc_object(ClassId::new(0), 1);
        let elem_a = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let elem_b = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let elem_c = shared.mem.heap.alloc_object(ClassId::new(0), 0);
        let arr = shared
            .mem
            .heap
            .alloc_array(ClassId::new(0), ArrayElementType::Reference, 3);
        shared
            .mem
            .heap
            .set_array_element(arr, 0, Value::Object(Some(elem_a)))
            .unwrap();
        shared
            .mem
            .heap
            .set_array_element(arr, 1, Value::Object(Some(elem_b)))
            .unwrap();
        shared
            .mem
            .heap
            .set_array_element(arr, 2, Value::Object(Some(elem_c)))
            .unwrap();
        shared.mem.heap.set_field(obj, 0, Value::Object(Some(arr)));
        // Pre-fix this returned Some(garbage); post-fix must return None.
        assert_eq!(read_java_string(&shared.mem.heap, obj), None);
    }

    // -----------------------------------------------------------------------
    // Class mirror helpers
    // -----------------------------------------------------------------------

    /// NOTE (2026-08-12): the slot-0 assertion below is mode-dependent by
    /// design. `get_or_create_class_mirror` now skips the slot-0 write when
    /// `java/lang/Class` slot 0 is a compact REFERENCE field (real-JDK mode);
    /// see the long note there. These tests run under `test_shared()`, i.e.
    /// synthetic-jdk mode, where `java/lang/Class` is a fabricated
    /// compatibility stub. `build_compact_layout_ordered` returns `None` for a
    /// class whose slots are entirely padding, so no compact layout is ever
    /// registered for it, `class_layout` answers `None`, the gate evaluates
    /// `false`, and the write still happens. The reverse-map half of this test
    /// is the part that holds in BOTH modes — and it is the half the runtime
    /// actually depends on.
    #[test]
    fn class_mirror_stores_class_id_in_reverse_map() {
        let shared = test_shared();
        let class_id = ClassId::new(42);
        let mirror = get_or_create_class_mirror(&shared, class_id);
        // Field 0 still stores Int(class_id) for legacy compatibility.
        assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(42));
        // But class_id is also recoverable via the reverse map.
        let recovered = shared
            .classes
            .class_mirrors_reverse
            .read()
            .get(&mirror)
            .copied();
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
        let name_val = shared.mem.heap.get_field(mirror, 1);
        match name_val {
            Value::Object(Some(name_ref)) => {
                let name = read_java_string(&shared.mem.heap, name_ref);
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
        let n = shared.mem.heap.num_fields(mirror);
        assert!(n >= CLASS_MIRROR_NUM_FIELDS);
        // Slot 0 holds the class id; the legacy name slot (1) holds the name.
        assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(123));
        assert!(matches!(
            shared.mem.heap.get_field(mirror, LEGACY_NAME_SLOT),
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
        assert_eq!(shared.mem.heap.get_field(mirror, 0), Value::Int(-1));
        // Field 1 should be "int" string
        let name_val = shared.mem.heap.get_field(mirror, 1);
        match name_val {
            Value::Object(Some(name_ref)) => {
                let name = read_java_string(&shared.mem.heap, name_ref);
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

    // -----------------------------------------------------------------------
    // G30 — the class-mirror overlay and the coercion it must never meet
    // -----------------------------------------------------------------------

    /// The two mirror populators write their VM-internal tag into slot 0 of an
    /// object stamped `java/lang/Class`, whose real JDK 25 slot 0 is
    /// `Constructor<T> cachedConstructor` — a REFERENCE. Which store they use
    /// decides what ends up there, and the two answers are not close:
    ///
    /// | store | path | slot 0 afterwards |
    /// |---|---|---|
    /// | `set_field` (descriptor-less) | `gc::autobox::box_for_reference_slot` | an `AUTOBOX_CLASS_ID` wrapper that `get_field` un-boxes back to `Int` |
    /// | `set_field_as(.., b'L')` | `gc::heap::coerce_field_value_for_slot` | **`Value::Object(None)`** — the tag is gone |
    ///
    /// `mirror_class_id` falls back to slot 0 when `class_mirrors_reverse`
    /// misses, and that fallback is load-bearing: gating this write off was
    /// measured to fail `RJdkHello` at `System.out instanceof PrintStream`
    /// (see the long note at the write site). Nulling it would do the same
    /// thing by a different route, and would do it silently, because the
    /// coercion does not refuse — it answers `null` and returns.
    ///
    /// This has to scan source. The defect it guards against is a change that
    /// compiles, runs, and produces a mirror that is wrong only where the
    /// reverse map happens to miss, so there is nothing to observe from a
    /// unit test that does not know which lookups will miss.
    #[test]
    fn the_class_mirror_id_is_written_without_a_field_descriptor() {
        // Scan only what is ABOVE this test module. Every needle below also
        // appears verbatim in this test's own assertions, so scanning the
        // whole file would find each one inside itself and the test would
        // pass whatever the real code said.
        let src = include_str!("vm_object.rs")
            .split("mod tests {")
            .next()
            .expect("split always yields a first element");
        for (marker, what) in [
            (
                "set_field(mirror, 0, Value::Int(class_id.as_u32() as i32))",
                "get_or_create_class_mirror",
            ),
            (
                "set_field(mirror, 0, Value::Int(-1))",
                "get_or_create_primitive_mirror",
            ),
        ] {
            assert!(
                src.contains(marker),
                "{what} must still write the mirror's ClassId tag into slot 0                  through the DESCRIPTOR-LESS `set_field`. If this fired                  because the call was switched to `set_field_as`, revert it:                  the `b'L'` arm of `gc::heap::coerce_field_value_for_slot`                  maps `Int(_)` to `Object(None)`, so the tag would be replaced                  by null and `mirror_class_id`'s fallback would answer None.                  If it fired because the write was deleted, see the measured                  `RJdkHello` regression recorded at the write site.",
            );
        }
        assert!(
            !src.contains("set_field_as(mirror, 0"),
            "slot 0 of a class mirror must never be written through the              descriptor-aware setter — see the table on this test",
        );
    }
}
