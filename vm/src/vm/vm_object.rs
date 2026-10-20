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
//     removing a defect. See `jdk-only-object-layout-audit.md`.

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
///
/// **Aborts the process on a full heap.** A per-operation caller that holds a
/// `JvmThread` uses `interpreter::intern_string_literal_or_oom` instead (the
/// same pooled String under `new`'s contract). The production callers left at
/// gc-common w9-c are start-up and embedding paths (system properties, the
/// launcher's `main` arguments, `libcratonvm`), `vm_util`'s one-shot
/// post-`<clinit>` enum fixups, the non-unwinding arm of
/// `NativeContextImpl`'s thread plumbing, a proxy `Method`'s descriptor slot
/// (a pool hit by construction: its caller has just interned the same text
/// fallibly), and the JIT's two `ldc` String helpers, which belong to the JIT
/// round (`common-w8v-constant-resolution-strings-abort-on-a-full-heap`).
pub fn create_java_string(shared: &SharedVm, text: &str) -> ObjectRef {
    create_java_string_reporting(shared, text).0
}

/// [`create_java_string`], also saying whether this call ALLOCATED.
///
/// An intern hit returns an existing object and allocates nothing, so a caller
/// that charges an allocation counter (`NativeContextImpl::create_string`)
/// needs to tell the two apart. Reported from inside rather than by probing the
/// pool a second time from outside: the probe would double the read-lock
/// acquisition on the hit path, which is the hot one.
pub fn create_java_string_reporting(shared: &SharedVm, text: &str) -> (ObjectRef, bool) {
    // Fast path: check pool. gci i21/sw: every hit answers through the
    // weak G1 table's keep-alive (`string_table::keep_alive_on_hit`; one
    // relaxed load off that switch).
    if let Some(&obj) = shared.mem.string_pool.read().get(text) {
        return (crate::memory::string_table::keep_alive_on_hit(shared, obj), false);
    }

    // gci i25/st2 (`CRATONVM_GC_STRING_TABLE_FUNNEL`, opt-in): allocate with no
    // pool lock held, then the one insert funnel (`string_table` module doc,
    // "The insert funnel"). The allocation below never fails (it aborts), so
    // the funnel always answers.
    if crate::memory::string_table::funnel_engaged(shared) {
        return crate::memory::string_table::intern_miss_via_funnel(shared, text, || {
            Some(alloc_java_string_object(shared, text))
        })
        .unwrap_or_else(|| fatal_string_heap_exhausted(text.len()));
    }

    // Slow path: create new string object
    let mut pool = shared.mem.string_pool.write();
    // Double-check after acquiring write lock
    if let Some(&obj) = pool.get(text) {
        return (crate::memory::string_table::keep_alive_on_hit(shared, obj), false);
    }

    let str_obj = alloc_java_string_object(shared, text);
    pool.insert(text.to_string(), str_obj);
    // gcj j1/st: the opt-in insert grace (one relaxed load off it).
    crate::memory::string_table::note_pooled_by_constructor(shared, str_obj);
    (str_obj, true)
}

/// Fallible, pooled twin of [`create_java_string`]: returns `None` instead of
/// aborting the process when the heap is too full to allocate the `String`.
/// Used by exception materialization (`create_exception_object`) so a detail
/// message can be built when there is room, but a 100%-full heap surfaces a
/// catchable `OutOfMemoryError` (caller falls back to the pre-allocated
/// singleton) rather than the VM hard-aborting.
pub fn try_create_java_string(shared: &SharedVm, text: &str) -> Option<ObjectRef> {
    // gci i21/sw: hits through `string_table::keep_alive_on_hit`.
    if let Some(&obj) = shared.mem.string_pool.read().get(text) {
        return Some(crate::memory::string_table::keep_alive_on_hit(shared, obj));
    }
    // gci i25/st2: allocate outside the pool lock, then the insert funnel.
    // gcj j1/st: through `try_create_java_string_uninterned` (the compact
    // ASCII fast path: no temporary UTF-16 `Vec`), which builds the identical
    // String -- the same object shape `try_create_java_string_reporting` and
    // `create_java_string_reporting` already pool.
    if crate::memory::string_table::funnel_engaged(shared) {
        return crate::memory::string_table::intern_miss_via_funnel(shared, text, || {
            try_create_java_string_uninterned(shared, text)
        })
        .map(|(obj, _)| obj);
    }
    let mut pool = shared.mem.string_pool.write();
    if let Some(&obj) = pool.get(text) {
        return Some(crate::memory::string_table::keep_alive_on_hit(shared, obj));
    }
    let str_obj = try_create_java_string_uninterned(shared, text)?;
    pool.insert(text.to_string(), str_obj);
    // gcj j1/st: the opt-in insert grace (one relaxed load off it).
    crate::memory::string_table::note_pooled_by_constructor(shared, str_obj);
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
/// Callers that hold a `JvmThread` must go through the interpreter's ladders
/// (`create_string_from_units_or_oom` for a fresh String,
/// `intern_string_literal_or_oom` for a pooled one; the `&str` twin
/// `create_string_or_oom` is test-only), which GC and retry around the
/// fallible constructors and finally raise a catchable `OutOfMemoryError` --
/// the same contract `new` has. Before that existed, a `"a" + b` on a full heap called
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

/// Fallible twin of [`create_java_string_reporting`]: pooled, reports whether
/// this call allocated, and answers `None` instead of aborting the process when
/// the heap cannot hold the `String`. The object it builds is laid out exactly
/// as [`create_java_string_reporting`]'s (same compact/ASCII branch, through
/// [`try_create_java_string_uninterned`]).
///
/// gen r4w3/hunter (2026-09-23): the unwinding `NativeContext::create_string`
/// uses it so a native building a String on a full heap raises a catchable
/// `OutOfMemoryError` (`native_oom::raise`) like `heap_alloc_object` and
/// `new_array` already do, instead of `std::process::abort()`.
pub fn try_create_java_string_reporting(
    shared: &SharedVm,
    text: &str,
) -> Option<(ObjectRef, bool)> {
    // gci i21/sw: hits through `string_table::keep_alive_on_hit`.
    if let Some(&obj) = shared.mem.string_pool.read().get(text) {
        return Some((crate::memory::string_table::keep_alive_on_hit(shared, obj), false));
    }
    // gci i25/st2: allocate outside the pool lock, then the insert funnel.
    if crate::memory::string_table::funnel_engaged(shared) {
        return crate::memory::string_table::intern_miss_via_funnel(shared, text, || {
            try_create_java_string_uninterned(shared, text)
        });
    }
    let mut pool = shared.mem.string_pool.write();
    if let Some(&obj) = pool.get(text) {
        return Some((crate::memory::string_table::keep_alive_on_hit(shared, obj), false));
    }
    let str_obj = try_create_java_string_uninterned(shared, text)?;
    pool.insert(text.to_string(), str_obj);
    // gcj j1/st: the opt-in insert grace (one relaxed load off it).
    crate::memory::string_table::note_pooled_by_constructor(shared, str_obj);
    Some((str_obj, true))
}

/// Thread-aware, GC-safe dynamic String allocation for native helpers. The
/// ASCII path uses the ordinary object TLAB and a hit-only byte-array TLAB
/// allocation, retaining a fresh String object and backing array every call.
/// (The infallible wrapper, which aborted on a full heap, lost its last
/// production caller in gc-common w5-c and was removed at the w5 merge.)
///
/// Fallible:
/// `None` when the heap cannot hold the String or its backing array. Performs
/// no collection (the TLAB arm only bumps or refills young space).
///
/// gc-common w5-c. The infallible form used to fall back PER HALF, and the
/// backing-array half was `.unwrap_or_else(|| return alloc_java_string_object(..))`:
/// the `return` leaves the CLOSURE, not the function, so on a byte-array
/// refusal the "backing array" became a whole `java/lang/String` object, and
/// the new String's `value` field was set to another String -- a type-confused
/// String on a nearly full heap, read by every `String` accessor as a
/// `byte[]`. Both halves now fail as one.
pub fn try_create_java_string_uninterned_gc_safe_threaded(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    text: &str,
) -> Option<ObjectRef> {
    try_create_java_string_uninterned_gc_safe_threaded_with_tlab_status(shared, thread, text)
        .map(|(obj, _)| obj)
}

/// Like [`try_create_java_string_uninterned_gc_safe_threaded`], also returning
/// `true` when both the `String` object and its backing `byte[]` were bumped
/// inside `thread.tlab` (so `thread.tlab.thread_allocated_bytes()` already
/// covers 100% of the allocation and `note_native_string_alloc_minus_tlab` does
/// not need to re-read the heap headers).
pub fn try_create_java_string_uninterned_gc_safe_threaded_with_tlab_status(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    text: &str,
) -> Option<(ObjectRef, bool)> {
    if shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
        && text.is_ascii()
    {
        // ASCII is Latin-1, byte for byte: the shared TLAB constructor below
        // (factored out of this body for the compiled concat bridge's
        // Latin-1 fast path, JIT round 19 lane strgap, STRGAP19-1) builds the
        // identical String.
        let (class_id, fields) = java_string_allocation_layout(shared);
        return alloc_latin1_string_threaded_with_tlab_status(
            shared,
            thread,
            class_id,
            fields,
            text.as_bytes(),
        );
    }
    try_create_java_string_uninterned(shared, text).map(|s| (s, false))
}

/// A fresh, uninterned compact `java/lang/String` whose payload is the
/// Latin-1 `bytes` (`coder` LATIN1, `hash` 0, `hashIsZero` false), allocated
/// the way [`try_create_java_string_uninterned_gc_safe_threaded`] allocates an
/// ASCII one: the String from the thread's object TLAB (refill allowed), the
/// `byte[]` hit-only from the TLAB, each falling back to the heap's no-GC
/// allocator.
///
/// `None` when compact strings are off (the caller must build a UTF-16
/// String instead) or when the heap cannot hold either object right now.
/// Performs no collection, so the caller may hold raw references across it
/// only to objects it does not read afterwards; the compiled concat bridge
/// reads every operand into `bytes` BEFORE calling this.
///
/// JIT round 19, lane strgap (STRGAP19-1): factored out of the ASCII
/// constructor so the bridge's exact-size Latin-1 result needs no UTF-16
/// detour and no Rust heap `Vec`.
pub fn try_create_java_string_latin1_threaded(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    bytes: &[u8],
) -> Option<ObjectRef> {
    if !shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return None;
    }
    let (class_id, fields) = java_string_allocation_layout(shared);
    // The four slots written below are JDK 9+'s `value`, `coder`, `hash`,
    // `hashIsZero`. A smaller layout (the synthetic stub's `value` + `hash`)
    // with compact strings armed is not a configuration a real image
    // produces; refuse it rather than write past the object.
    if fields < 4 {
        return None;
    }
    alloc_latin1_string_threaded(shared, thread, class_id, fields, bytes)
}

/// The body [`try_create_java_string_uninterned_gc_safe_threaded`]'s ASCII arm
/// has always run (factored out unchanged, STRGAP19-1): allocate a compact
/// `String` of `class_id` / `fields` and its LATIN1 `byte[]` holding `bytes`.
fn alloc_latin1_string_threaded(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    class_id: ClassId,
    fields: usize,
    bytes: &[u8],
) -> Option<ObjectRef> {
    alloc_latin1_string_threaded_with_tlab_status(shared, thread, class_id, fields, bytes)
        .map(|(obj, _)| obj)
}

fn alloc_latin1_string_threaded_with_tlab_status(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    class_id: ClassId,
    fields: usize,
    bytes: &[u8],
) -> Option<(ObjectRef, bool)> {
    alloc_compact_string_threaded_with_tlab_status(
        shared,
        thread,
        class_id,
        fields,
        bytes,
        CODER_LATIN1,
    )
}

/// JIT round 21 wave 5, lane concat (CONCAT21-2,
/// `CRATONVM_JIT_CONCAT_UTF16_FAST`, opt-in): [`try_create_java_string_latin1_threaded`]
/// for a UTF16 result. `units` are stored as little-endian pairs (the VM's
/// own reader's order, `StringUTF16.isBigEndian() == false`), `coder` UTF16,
/// `hash` 0, `hashIsZero` false -- the String the general renderer's
/// `create_string_from_units_or_oom` builds for a result with a unit above
/// `0xFF`. Same contract: no collection, `None` with compact strings off, a
/// layout under four fields, or a heap that cannot hold either object now.
pub fn try_create_java_string_utf16_threaded(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    units: &[u16],
) -> Option<ObjectRef> {
    if !shared
        .classes
        .compact_strings
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return None;
    }
    let (class_id, fields) = java_string_allocation_layout(shared);
    if fields < 4 {
        return None;
    }
    let mut payload: smallvec::SmallVec<[u8; 256]> = smallvec::SmallVec::with_capacity(units.len() * 2);
    for &u in units {
        payload.extend_from_slice(&u.to_le_bytes());
    }
    alloc_compact_string_threaded_with_tlab_status(
        shared,
        thread,
        class_id,
        fields,
        &payload,
        CODER_UTF16,
    )
    .map(|(obj, _)| obj)
}

/// The shared body of the two compact constructors above: a String of
/// `class_id` / `fields` over a fresh `byte[]` holding `bytes`, with `coder`.
fn alloc_compact_string_threaded_with_tlab_status(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    class_id: ClassId,
    fields: usize,
    bytes: &[u8],
    coder: i32,
) -> Option<(ObjectRef, bool)> {
    // The shape planner, not a bare legacy size -- see
    // `plan_tlab_object_shape`.
    let (object_size, _, _) = crate::runtime::interpreter::plan_tlab_object_shape_for(
        shared,
        class_id,
        fields,
        crate::runtime::interpreter::tlab_site::STRING,
    );
    let (str_obj, str_in_tlab) = match crate::runtime::interpreter::tlab_alloc_object(
        thread,
        shared,
        class_id,
        fields,
        object_size,
    ) {
        Some(obj) => (obj, true),
        None => (
            shared.mem.heap.try_alloc_object_full(class_id, fields)?,
            false,
        ),
    };
    // A refusal here leaves `str_obj` as unreachable garbage with no field
    // written; nothing refers to it, so the next collection reclaims it.
    let (byte_array, arr_in_tlab) =
        match crate::runtime::interpreter::tlab_alloc_byte_array(thread, shared, bytes.len()) {
            Some(arr) => (arr, true),
            None => (
                shared.mem.heap.try_alloc_array_full(
                    ClassId::new(0),
                    ArrayElementType::Byte,
                    bytes.len(),
                )?,
                false,
            ),
        };
    if let Some(base) = shared.mem.heap.array_data_ptr(byte_array) {
        // SAFETY: the fresh byte array has exactly `bytes.len()` elements.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len()) };
    } else {
        for (index, &byte) in bytes.iter().enumerate() {
            // A Java `byte` is signed: the element of a Latin-1 unit above
            // 0x7F is its two's-complement value (same low byte).
            let _ =
                shared
                    .mem
                    .heap
                    .set_array_element(byte_array, index, Value::Int(byte as i8 as i32));
        }
    }
    shared
        .mem
        .heap
        .set_field(str_obj, 0, Value::Object(Some(byte_array)));
    // The reference publication above keeps its ordinary barrier. Resolve the
    // three primitive defaults once; alternate/legacy String layouts retain
    // the same individual stores. This constructor cannot collect.
    if shared
        .mem
        .heap
        .try_set_compact_primitive_fields(
            str_obj,
            &[(1, b'B'), (2, b'I'), (3, b'Z')],
            &[Value::Int(coder), Value::Int(0), Value::Int(0)],
        )
        .is_none()
    {
        shared.mem.heap.set_field(str_obj, 1, Value::Int(coder));
        shared.mem.heap.set_field(str_obj, 2, Value::Int(0));
        shared.mem.heap.set_field(str_obj, 3, Value::Int(0));
    }
    Some((str_obj, str_in_tlab && arr_in_tlab))
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
///
/// **Aborts the process on a full heap.** Since gc-common w9-c the constant
/// paths (`ldc`, a condy's String argument) no longer call it: they intern
/// through `constants.rs::intern_wide_string_literal_or_oom`, which collects
/// and throws `OutOfMemoryError`. The one production caller left is the
/// non-unwinding arm of `NativeContextImpl::create_string_from_units`.
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
        return try_alloc_java_string_object_from_ascii(shared, text.as_bytes())
            .unwrap_or_else(|| fatal_string_heap_exhausted(text.len()));
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    alloc_java_string_object_from_units(shared, &units)
}

/// Allocate and populate a fresh `java/lang/String` from UTF-16 code `units`.
///
/// Shared core of [`alloc_java_string_object`] (which simply `encode_utf16`s a
/// Rust `&str`) and [`create_java_string_from_units`] (surrogate-bearing
/// constants). Performs no pool lookup or insertion. Exhaustion raises the
/// native OOM channel under a catch scope, otherwise a typed initialization
/// panic. Callers needing a refusal value use
/// [`try_alloc_java_string_object_from_units`] instead.
fn alloc_java_string_object_from_units(shared: &SharedVm, units: &[u16]) -> ObjectRef {
    try_alloc_java_string_object_from_units(shared, units)
        .unwrap_or_else(|| fatal_string_heap_exhausted(units.len()))
}

/// The legacy String constructors' exhaustion channel. A native scope catches
/// the OOM; an explicit host/initialization call gets a typed startup failure.
#[cold]
fn fatal_string_heap_exhausted(units: usize) -> ! {
    if crate::runtime::native_oom::unwind_ok() {
        crate::runtime::native_oom::raise(units, "java/lang/String");
    }
    if cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_OOM_BT") {
        eprintln!("{}", std::backtrace::Backtrace::force_capture());
    }
    // Boot and explicit legacy host APIs have no Java frame. The launcher
    // already renders this typed startup refusal with status 1.
    std::panic::panic_any(crate::error::VmInitializationError {
        detail: "java.lang.OutOfMemoryError: Java heap space".into(),
    });
}

thread_local! {
    static STRING_LAYOUT_CACHE: Cell<Option<(usize, u32, usize)>> = const { Cell::new(None) };
}

/// The already-resolved layout only: never takes a class-manager lock.
pub(super) fn cached_java_string_class_id(shared: &SharedVm) -> Option<ClassId> {
    let fields = shared
        .classes
        .cached_string_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    STRING_LAYOUT_CACHE.with(|cache| {
        cache
            .get()
            .filter(|(vm, _, count)| *vm == shared.vm_identity && *count == fields)
            .map(|(_, id, _)| ClassId::new(id))
    })
}

/// Resolve the loaded String class and its instance-field count. The field
/// count is cached; the ordinary fast path uses only a class-manager read lock.
pub(super) fn java_string_allocation_layout(shared: &SharedVm) -> (ClassId, usize) {
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
    // Keyed by `vm_identity`, never the `SharedVm` address (wave 16): a VM
    // dropped and another created at the recycled address on the same thread
    // matched the old row whenever the field counts agreed, and its Strings
    // were allocated -- and `is_java_lang_string` answered -- with the dead
    // VM's `String` class id. `vm_identity` is never reused.
    let vm_key = shared.vm_identity;
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
///
/// The allocation is the NON-collecting `try_alloc_array_full`, so `false`
/// also means only "the young space is full right now". This function keeps
/// that contract (its callers may hold raw references); the collection is
/// the caller's decision. Every native `<init>` that reaches it through
/// `NativeContext::init_string_from_units` goes through
/// `native-builtins`' `lang_string::init_string_from_units_or_oom`: one
/// `reclaim_before_alloc_retry`, one retry with `this` pinned, then the
/// `OutOfMemoryError` (gc-common w20-b,
/// `common-w19g-init-string-from-units-never-collects`). Its fresh-String
/// caller here, [`try_alloc_java_string_object_from_units`], is retried where
/// the interpreter reaches it (`gc_and_alloc::create_string_from_units_or_oom`,
/// the `collect_and_retry` ladder); JNI's `NewString` deliberately does not
/// collect (raw local refs, `common-w2c-jni-local-refs-are-raw-addresses`).
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
                if bytes.is_ascii() {
                    // SAFETY: `bytes.is_ascii()` verified every byte is < 0x80
                    // (valid UTF-8), so the bulk-read `Vec<u8>` becomes a
                    // `String` in place with zero extra allocation or copying.
                    return Some(unsafe { String::from_utf8_unchecked(bytes) });
                }
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
            let mut units = Vec::with_capacity(if coder == CODER_LATIN1 {
                bytes.len()
            } else {
                bytes.len() / 2
            });
            push_string_payload_units(&bytes, coder, &mut units);
            Some(units)
        }
        _ => None,
    }
}

/// The code units of a String's `byte[]` payload under `coder`, appended to
/// `out`. The one decoder behind [`decode_java_string_value_array_units`] and
/// [`append_java_string_units`].
fn push_string_payload_units(bytes: &[u8], coder: i32, out: &mut Vec<u16>) {
    if coder == CODER_LATIN1 {
        out.extend(bytes.iter().map(|&b| u16::from(b)));
    } else {
        // Little-endian pairs, matching `create_java_string` and
        // `StringUTF16.isBigEndian() == false`. `chunks_exact(2)` drops a
        // trailing odd byte exactly as the `str` decoder does.
        out.extend(
            bytes
                .chunks_exact(2)
                .map(|c| u16::from(c[0]) | (u16::from(c[1]) << 8)),
        );
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

/// The number of UTF-16 code units of a `java/lang/String` (`String.length()`),
/// in O(1): the value array's length, halved for a UTF16-coded `byte[]`.
///
/// gci i14/jh (F12 of
/// `gci-i13jc-jni-critical-weak-globals-local-frames-review-findings-FIXED-20261016`):
/// JNI `GetStringLength` copied every unit ([`read_java_string_units`]) to
/// count them; HotSpot reads the length. Same receiver guards as
/// [`read_java_string_units`] (the shared [`java_string_value_and_coder`]) and
/// always its `len()`: a trailing odd byte of a UTF16 payload is dropped, as
/// `push_string_payload_units` drops it. Lock-free (no class-manager lookup),
/// so a JNI leaf may call it.
pub fn java_string_unit_count(heap: &VmHeap, obj_ref: ObjectRef) -> Option<usize> {
    let (value_array, coder) = java_string_value_and_coder(heap, obj_ref)?;
    let len = heap.array_length(value_array);
    match heap.array_element_type(value_array)? {
        ArrayElementType::Char => Some(len),
        ArrayElementType::Byte if coder == CODER_LATIN1 => Some(len),
        ArrayElementType::Byte => Some(len / 2),
        _ => None,
    }
}

/// Whether `obj_ref` is a `java.lang.String` of this VM — by its CLASS, not by
/// its shape.
///
/// [`read_java_string`] / [`read_java_string_units`] are speculative by
/// design (see [`java_string_value_and_coder`]): they accept any object whose
/// first slots look like `value` / `coder`. A `StringBuilder` passes every
/// one of those guards (`AbstractStringBuilder` declares `byte[] value;
/// byte coder; boolean maybeLatin1; int count`), and the reader then decodes
/// its whole buffer, unused capacity included. A caller that renders an
/// ARBITRARY object as `String.valueOf` would asks this first and runs
/// `toString()` otherwise (interpreter round i1 wave 16, lane L1).
///
/// The same question as `vm_exec::is_real_java_string` (the `NativeContext`
/// string readers' guard), answered by class id instead of by name: `String`
/// is a bootstrap class no other loader may define, and its id is the one the
/// String allocation layout already caches per thread, so this takes no lock
/// on the hot path (the concat operand loop).
pub fn is_java_lang_string(shared: &SharedVm, obj_ref: ObjectRef) -> bool {
    shared.mem.heap.kind_of(obj_ref) == crate::memory::heap::ObjectKind::Object
        && shared.mem.heap.class_id_of(obj_ref) == java_string_allocation_layout(shared).0
}

/// [`read_java_string_units`], appended to `out` instead of returned: a
/// `byte[]` payload is decoded in place, with no intermediate `Vec` for the
/// bytes or for the units (the string-concat `invokedynamic` copies every
/// `String` operand this way, interpreter round i1 wave 16, lane L1).
/// `false`, with `out` untouched, wherever the reader answers `None`.
/// Speculative by shape exactly like the reader: for an arbitrary object ask
/// [`is_java_lang_string`] first.
pub fn append_java_string_units(heap: &VmHeap, obj_ref: ObjectRef, out: &mut Vec<u16>) -> bool {
    let Some((value_array, coder)) = java_string_value_and_coder(heap, obj_ref) else {
        return false;
    };
    match heap.array_element_type(value_array) {
        Some(ArrayElementType::Char) => {
            out.extend_from_slice(&heap.read_char_array_bulk(value_array));
            true
        }
        Some(ArrayElementType::Byte) => {
            let len = heap.array_length(value_array);
            match heap.array_data_ptr(value_array) {
                Some(base) if len > 0 => {
                    // SAFETY: `value_array` is a live `byte[]` (kind and
                    // element type checked by `java_string_value_and_coder`);
                    // its payload is `len` contiguous bytes at
                    // `array_data_ptr`, the region `read_byte_array_bulk`
                    // copies. Nothing between here and the decode allocates,
                    // so the payload cannot move under the read, and a
                    // String's value array is never written after
                    // construction.
                    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
                    push_string_payload_units(bytes, coder, out);
                }
                _ => {
                    let bytes = read_byte_array_bulk(heap, value_array);
                    push_string_payload_units(&bytes, coder, out);
                }
            }
            true
        }
        _ => false,
    }
}

/// Append the payload of a LATIN1-coded `java/lang/String` to `out`, byte
/// for byte. `false`, with `out` untouched, for anything else: a UTF16-coded
/// String, a non-compact (`char[]`) layout, or a receiver the shape reader
/// does not accept. Speculative by shape exactly like
/// [`append_java_string_units`]: for an arbitrary object ask
/// [`is_java_lang_string`] first.
///
/// JIT round 19, lane strgap (STRGAP19-1): the compiled concat bridge's
/// Latin-1 fast path copies `String` operands with it.
pub fn append_java_string_latin1(
    heap: &VmHeap,
    obj_ref: ObjectRef,
    out: &mut smallvec::SmallVec<[u8; 128]>,
) -> bool {
    let Some((value_array, coder)) = java_string_value_and_coder(heap, obj_ref) else {
        return false;
    };
    if coder != CODER_LATIN1 || heap.array_element_type(value_array) != Some(ArrayElementType::Byte)
    {
        return false;
    }
    let len = heap.array_length(value_array);
    if len == 0 {
        return true;
    }
    match heap.array_data_ptr(value_array) {
        Some(base) => {
            // SAFETY: as in `append_java_string_units`: a live `byte[]` of
            // `len` contiguous bytes, nothing allocates during the copy, and a
            // String's value array is never written after construction.
            let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
            out.extend_from_slice(bytes);
        }
        None => out.extend_from_slice(&read_byte_array_bulk(heap, value_array)),
    }
    true
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
    /// `componentType : Ljava/lang/Class;` -- HotSpot writes it for every array
    /// class (JDK 25's `isArray`, `getComponentType`, `descriptorString` and
    /// `arrayType` read it directly).
    component_type: Option<usize>,
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
    /// `module : Ljava/lang/Module;` (SBR2 PUREMOD): the module the class
    /// belongs to, which HotSpot's VM stores at definition (and fixes up for the
    /// mirrors that exist before the module system does). The JDK's own
    /// `Class.getModule()` is `return module;`, so with no native in front of it
    /// (`CRATONVM_PURE_BYTECODE`, or the row retired) the field must be right.
    /// `--jdk-only` only; see [`class_module_object`].
    module: Option<usize>,
}

/// `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO` (interpreter round i3 wave 80, lane L5;
/// `VmConfig::class_mirror_layout_memo`, held in
/// `ClassRealm::class_mirror_layout`): everything a new mirror needs to know
/// about the mirror OBJECT's layout, which depends only on the loaded
/// `java/lang/Class`, never on the class being mirrored. The slow path of
/// [`class_mirror_impl`] computes each of these per mirror -- a class-manager
/// WRITE lock for `load_class("java/lang/Class")`, eight by-name field walks
/// ([`resolve_class_mirror_slots`]), a read lock for [`mirror_slot0_overlay`]
/// and, per user-loader mirror, two environment reads. Filled by
/// [`remember_class_mirror_layout`] only from a REAL `java/lang/Class` whose
/// field count is the cached mirror field count, so a hit answers exactly what
/// the slow path would.
pub(crate) struct ClassMirrorLayout {
    class_class_id: ClassId,
    mirror_field_count: usize,
    slots: ClassMirrorSlots,
    overlay0: bool,
    /// `CRATONVM_DBG_MIRRORPIN` is set.
    dbg_mirrorpin: bool,
    /// `CRATONVM_DBG_MARK_WHY_CLASS`'s class name, when set.
    dbg_mark_why_class: Option<Box<str>>,
}

/// The memoized mirror layout, when the switch is on and it has been filled.
#[inline]
fn class_mirror_layout(shared: &SharedVm) -> Option<&ClassMirrorLayout> {
    if !shared.config.class_mirror_layout_memo {
        return None;
    }
    shared.classes.class_mirror_layout.get()
}

/// Fill the memo from what the slow path just computed, if the switch is on,
/// the memo is empty, and `class_class_id` is a real (not compatibility-stub)
/// `java/lang/Class` whose own field count is `mirror_field_count` (a count
/// cached while `Class` was still a stub never fills it: those mirrors keep
/// the slow path). Takes the class-manager READ lock; callers may hold
/// `class_mirrors` / `primitive_mirrors` (the slow path already takes the
/// class manager under them).
#[cold]
#[inline(never)]
fn remember_class_mirror_layout(
    shared: &SharedVm,
    class_class_id: ClassId,
    mirror_field_count: usize,
    slots: ClassMirrorSlots,
    overlay0: bool,
) {
    if !shared.config.class_mirror_layout_memo || shared.classes.class_mirror_layout.get().is_some()
    {
        return;
    }
    let real = shared
        .classes
        .class_manager
        .read()
        .get_class(class_class_id)
        .is_some_and(|c| {
            &*c.name == "java/lang/Class"
                && !c.origin.is_compatibility_stub()
                && c.num_total_fields == mirror_field_count
        });
    if !real {
        return;
    }
    let _ = shared.classes.class_mirror_layout.set(ClassMirrorLayout {
        class_class_id,
        mirror_field_count,
        slots,
        overlay0,
        dbg_mirrorpin: cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIRRORPIN"),
        dbg_mark_why_class: cratonvm_types::flags::runtime_var("CRATONVM_DBG_MARK_WHY_CLASS")
            .ok()
            .map(String::into_boxed_str),
    });
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
            component_type: None,
            class_redefined_count: slot(LEGACY_CLASS_REDEFINED_COUNT_SLOT),
            reflection_data: None,
            class_loader: None,
            module: None,
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
        component_type: resolve("componentType", &["Ljava/lang/Class;"]),
        class_redefined_count: resolve("classRedefinedCount", &["I"]),
        reflection_data: resolve("reflectionData", &["Ljava/lang/ref/SoftReference;"]),
        class_loader: resolve("classLoader", &["Ljava/lang/ClassLoader;"]),
        module: resolve("module", &["Ljava/lang/Module;"]),
    }
}

/// The `java.lang.Module` object `class_id`'s `Class.module` names, when it is
/// already known without running Java (SBR2 PUREMOD; `--jdk-only` only).
///
/// * a class of a named module: the canonical module object the boot layer (or
///   the first `Class.getModule()` native) cached under the module's name;
/// * a class of an unnamed module: its defining loader's own `unnamedModule`
///   field (the JDK's `ClassLoader` constructor makes it), for a user-defined
///   loader and for the adopted built-in ones.
///
/// `None` while the answer is not there yet (the boot layer is built after the
/// first mirrors); [`fixup_class_module_fields`] revisits those.
pub(crate) fn class_module_object(shared: &SharedVm, class_id: ClassId) -> Option<ObjectRef> {
    if !crate::vm::dispatch_policy(shared).is_jdk_only() {
        return None;
    }
    let (facts, cname) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id)?;
        let name = class.module_name.clone();
        let cpo = name
            .as_deref()
            .is_some_and(|n| cm.module_registry.is_class_path_only(n))
            || module_label_is_not_membership(shared, class);
        (
            ClassModuleFacts {
                module_name: name,
                class_path_only: cpo,
                loader_tag: cm.get_loader_id(class_id).map(|id| id.to_native_id()),
            },
            class.name.to_string(),
        )
    };
    class_module_object_from(shared, class_id, facts, &cname, None)
}

/// Is `class`'s module label NOT its module membership? A class a user-defined
/// loader defined in a package of a platform module (`jdk/internal/reflect/X`:
/// only `java.*` is prohibited) is labelled with that module by its package
/// name at define, but a platform module is defined to the boot or platform
/// loader, so on HotSpot such a class is in its own loader's UNNAMED module
/// (`Class.getModule() == loader.getUnnamedModule()`), as linkage already
/// treats it (`access_control::member_module_of`). `CRATONVM_SPOOFED_PACKAGE_UNNAMED_MODULE`
/// (`VmConfig::spoofed_package_unnamed_module`, default ON, `=0` off; HotSpot
/// parity; interpreter round i6 wave 118, lane L5, `i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`;
/// probe `L5W118SpoofedPackageModule`).
fn module_label_is_not_membership(shared: &SharedVm, class: &cratonvm_classloading::Class) -> bool {
    shared.config.spoofed_package_unnamed_module
        && matches!(class.loader_id, cratonvm_classloading::ClassLoaderId::UserDefined(_))
        && class
            .module_name
            .as_deref()
            .is_some_and(cratonvm_classloading::module::is_platform_module_name)
}

/// What [`class_module_object`] reads from the class row: the class's module
/// name, whether that module is a class-path-only one, and its loader tag.
/// `class_mirror_impl` gathers them under the read guard that reads the name
/// (`CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`).
struct ClassModuleFacts {
    module_name: Option<String>,
    class_path_only: bool,
    loader_tag: Option<u32>,
}

/// [`class_module_object`] after its class-manager read. `own_loader`, when
/// `Some`, is `defining_loader_for(class_id)` already asked by the caller
/// (`CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`); `None` asks the table here.
/// `--jdk-only` callers only (the caller has checked the mode).
fn class_module_object_from(
    shared: &SharedVm,
    class_id: ClassId,
    facts: ClassModuleFacts,
    cname: &str,
    own_loader: Option<Option<ObjectRef>>,
) -> Option<ObjectRef> {
    let ClassModuleFacts {
        module_name,
        class_path_only,
        loader_tag,
    } = facts;
    if let Some(name) = module_name.filter(|_| !class_path_only) {
        return shared
            .classes
            .module_mirrors
            .read()
            .get(name.as_str())
            .copied();
    }
    // Arrays live in their element class's module; primitives in java.base.
    if let Some(elem) = cname
        .trim_start_matches('[')
        .strip_prefix('L')
        .and_then(|e| e.strip_suffix(';'))
    {
        if cname.starts_with('[') {
            let eid = shared
                .classes
                .class_manager
                .read()
                .get_loaded_class_id(elem);
            return eid.and_then(|e| class_module_object(shared, e));
        }
    }
    let is_primitive = matches!(
        cname.trim_start_matches('['),
        "int"
            | "long"
            | "short"
            | "byte"
            | "char"
            | "boolean"
            | "float"
            | "double"
            | "void"
            | "I"
            | "J"
            | "S"
            | "B"
            | "C"
            | "Z"
            | "F"
            | "D"
    );
    if is_primitive {
        return shared
            .classes
            .module_mirrors
            .read()
            .get("java.base")
            .copied();
    }
    let own_loader = match own_loader {
        Some(asked) => asked,
        None => cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            class_id.as_u32(),
        ),
    };
    let loader = own_loader.or_else(|| {
        loader_tag.and_then(|t| {
            cratonvm_native_builtins::classloader::adopted_builtin_loader_for_tag(
                &shared.loader_identity,
                t,
                cname,
            )
        })
    })?;
    let (loader, ok) = shared.mem.heap.load_and_forward_checked(loader);
    if !ok {
        return None;
    }
    let lcid = shared.mem.heap.class_id_of_validated(loader);
    let slot =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, lcid, "unnamedModule")?;
    match shared.mem.heap.get_field(loader, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// Fill `Class.module` of every class that already has a mirror (HotSpot's
/// `java_lang_Class::fixup_module_field` pass, run once the module system's
/// objects exist). `--jdk-only` only; a mirror whose module is not known yet
/// keeps `null`. Returns the number of mirrors it set.
pub fn fixup_class_module_fields(shared: &SharedVm) -> usize {
    if !crate::vm::dispatch_policy(shared).is_jdk_only() {
        return 0;
    }
    let class_cid = shared
        .classes
        .class_manager
        .read()
        .get_loaded_class_id("java/lang/Class");
    let Some(class_cid) = class_cid else { return 0 };
    let Some(slot) =
        crate::vm::vm_exec::resolve_field_slot_by_name_cached(shared, class_cid, "module")
    else {
        return 0;
    };
    let ids: Vec<(ClassId, ObjectRef)> = shared
        .classes
        .class_mirrors
        .read()
        .iter()
        .map(|(c, m)| (*c, *m))
        .collect();
    let mut set = 0;
    for (cid, mirror) in ids {
        if let Value::Object(Some(_)) = shared.mem.heap.get_field(mirror, slot) {
            continue;
        }
        if let Some(m) = class_module_object(shared, cid) {
            shared
                .mem
                .heap
                .set_field(mirror, slot, Value::Object(Some(m)));
            set += 1;
        }
    }
    let java_base = shared
        .classes
        .module_mirrors
        .read()
        .get("java.base")
        .copied();
    if let Some(m) = java_base {
        let prims: Vec<ObjectRef> = shared
            .classes
            .primitive_mirrors
            .read()
            .values()
            .copied()
            .collect();
        for mirror in prims {
            if !matches!(
                shared.mem.heap.get_field(mirror, slot),
                Value::Object(Some(_))
            ) {
                shared
                    .mem
                    .heap
                    .set_field(mirror, slot, Value::Object(Some(m)));
                set += 1;
            }
        }
    }
    set
}

/// A JVMTI redefinition of `class_id` succeeded: make the reflection caches
/// keyed on it start over, as HotSpot's
/// `VM_RedefineClasses::increment_class_counter` does (interpreter round i1
/// wave 27, lane L3; `L3/L3W26RedefinedReflection`).
///
/// * `java.lang.Class.classRedefinedCount` is advanced on the mirror of the
///   class and of every loaded subclass. `Class.reflectionData()` and
///   `Class.annotationData()` compare it against the count their cached
///   `ReflectionData` / `AnnotationData` was built for and rebuild when it
///   moved; it used to stay 0, so both served the previous version's answers
///   (the declared `Method` / `Field` roots with their annotation bytes and
///   generic signatures, the class annotations). A class without a mirror yet
///   has cached nothing and is skipped.
/// * The annotation natives' proxy cache (`lang_class`'s
///   `ANNOTATION_PROXY_CACHE`, keyed by holder class, and not by the count)
///   drops the rows of those classes, so `getAnnotation` rebuilds from the
///   redefined attributes.
///
/// Called with no class-manager lock held.
pub(crate) fn note_class_redefined_for_reflection(shared: &SharedVm, class_id: ClassId) {
    let (classes, count_slot) = {
        let cm = shared.classes.class_manager.read();
        let mut classes = cm.class_store.descendants_of(class_id);
        classes.push(class_id);
        let mirror_field_count = shared
            .classes
            .cached_class_mirror_num_fields
            .load(std::sync::atomic::Ordering::Relaxed);
        let count_slot = cm
            .find_bootstrap_class_by_name("java/lang/Class")
            .and_then(|id| cm.get_class(id))
            .and_then(|c| resolve_class_mirror_slots(c, mirror_field_count).class_redefined_count);
        (classes, count_slot)
    };
    if let Some(slot) = count_slot {
        let mirrors = shared.classes.class_mirrors.read();
        for cid in &classes {
            if let Some(&mirror) = mirrors.get(cid) {
                let next = match shared.mem.heap.get_field(mirror, slot) {
                    Value::Int(count) => count.wrapping_add(1),
                    _ => 1,
                };
                shared.mem.heap.set_field(mirror, slot, Value::Int(next));
            }
        }
    }
    let redefined: rustc_hash::FxHashSet<u32> = classes.iter().map(|c| c.as_u32()).collect();
    cratonvm_native_builtins::lang_class::forget_redefined_annotation_proxies(
        shared.vm_identity,
        &redefined,
    );
}

/// `ACC_PUBLIC | ACC_FINAL | ACC_ABSTRACT`: HotSpot's modifiers of a
/// primitive's mirror (`int.class`, `void.class`).
const PRIMITIVE_MIRROR_MODIFIERS: i32 = 0x0411;

/// `ACC_FINAL | ACC_SYNTHETIC`: HotSpot's modifiers of a lambda proxy's
/// hidden class (`InnerClassLambdaMetafactory` spins it with these flags).
const LAMBDA_PROXY_MODIFIERS: i32 = 0x1010;

/// The value HotSpot stores in a class mirror's `modifiers` field
/// (`Klass::modifier_flags`, which JDK 25's `Class.getModifiers()` returns
/// and `Class.isInterface()` tests), for the class `class_id` of `cm`:
///
/// * an array: `ACC_FINAL | ACC_ABSTRACT` plus the leaf element's
///   `PUBLIC` / `PRIVATE` / `PROTECTED` bits (a primitive leaf is public);
/// * a member class: its own `InnerClasses` entry's flags (where `private`,
///   `protected` and `static` live), else the class file's `access_flags`;
/// * never `ACC_SUPER`.
///
/// A lambda proxy (`final synthetic`, its class file's flags on HotSpot) is
/// not in the class store; [`class_mirror_impl`] answers it before this.
/// The same rule as `lang_class::native_class_get_modifiers`, the Bridge that
/// answered `getModifiers()` while the field held the raw `access_flags`
/// (interpreter round i1 wave 52, lane L3;
/// `docs/internal/fixed-bugs/interpreter-L3-proposal-the-class-mirror-carries-hotspots-modifiers-FIXED-20261016.md`,
/// probe `tools/probes/interp/L3/L3W52ClassMirrorModifiers.java`).
fn hotspot_class_modifiers(
    store: &crate::classloading::ClassStore,
    class_id: ClassId,
    is_lambda_proxy: impl Fn(ClassId) -> bool,
) -> i32 {
    const ACC_SUPER: u16 = 0x0020;
    const ACCESS_BITS: i32 = 0x0007;
    const ACC_FINAL_ABSTRACT: i32 = 0x0410;
    let own = |id: ClassId| -> i32 {
        let Some(class) = store.get(id) else {
            return 0;
        };
        let flags = class
            .inner_classes
            .iter()
            .find(|ic| ic.inner_class == *class.name)
            .map_or(class.access_flags.bits(), |ic| ic.access_flags);
        i32::from(flags & !ACC_SUPER)
    };
    let Some(class) = store.get(class_id) else {
        return 0;
    };
    if !class.name.starts_with('[') {
        return own(class_id);
    }
    if !class.name.trim_start_matches('[').starts_with('L') {
        return ACC_FINAL_ABSTRACT | 0x0001;
    }
    let mut leaf = class_id;
    for _ in 0..256 {
        match store.get(leaf).and_then(|c| c.array_info.as_ref()) {
            Some(info) => leaf = info.component_class_id,
            None => break,
        }
    }
    let leaf_is_element = store.get(leaf).is_some_and(|c| !c.name.starts_with('['));
    let access = if leaf_is_element {
        own(leaf) & ACCESS_BITS
    } else if leaf != class_id && is_lambda_proxy(leaf) {
        // A lambda proxy leaf is not in the class store; its flags are
        // `final synthetic` (no access bit), so `Lambda[]` is 0x410 on
        // HotSpot, not 0x411 (interpreter round i5 build w102d,
        // `L3W102ClassNameShapes` row `lambda[]`).
        LAMBDA_PROXY_MODIFIERS & ACCESS_BITS
    } else {
        0x0001
    };
    ACC_FINAL_ABSTRACT | access
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

/// Allocate the heap object of a `java.lang.Class` mirror (class or
/// primitive).
///
/// Fallible first, so the collector's reserve is touched only when it must
/// be: on G1 the fallible entry stops at the regions held back for callers
/// that cannot report failure, and this is one of them -- the mirror
/// constructors return a bare `ObjectRef`, and they allocate under the
/// `class_mirrors` write guard, where no collection may start. Only when the
/// ordinary heap is exhausted does the infallible entry take the reserve (and
/// abort if even that is gone). gc-common w3-c,
/// `common-f-g1-fatal-abort-from-infallible-alloc-object`: `GcOverheadProbe`'s
/// `catch (OutOfMemoryError)` block mints mirrors for the string-concat
/// bootstrap on a heap that has just thrown. The complete fix is a
/// `Result`-returning mirror constructor, so class loading raises
/// `OutOfMemoryError` as HotSpot's `java_lang_Class::create_mirror` does; that
/// touches every caller.
///
/// Every backend. It only matters on G1 (Generational's and ZGC's fallible
/// entries are their infallible ones minus the abort), but since gc-common
/// w4-c ZGC's fallible entry plans the same compact body its infallible one
/// does (`common-w2d-g1-zgc-fallible-object-path-never-plans-compact`), so the
/// w3-c `is_g1()` guard that kept ZGC mirrors from turning legacy-shaped is
/// no longer needed: one rule, one shape per class.
fn alloc_class_mirror_object(
    shared: &SharedVm,
    class_class_id: ClassId,
    mirror_field_count: usize,
) -> ObjectRef {
    let heap = &shared.mem.heap;
    if let Some(mirror) = heap.try_alloc_object_full(class_class_id, mirror_field_count) {
        return mirror;
    }
    heap.alloc_object(class_class_id, mirror_field_count)
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
    // gen r5w1/oom5 (review6 page item 3): inside a native callback (or a
    // `native_oom::catch_alloc_oom` scope) heap exhaustion has a catchable
    // channel, exactly as `NativeContextImpl::heap_alloc_object` uses it: the
    // fallible mirror, then the `NativeAllocOom` unwind, which the native
    // funnel (or the enclosing `execute`) turns into `OutOfMemoryError`. This
    // is every native first use of a mirror -- `Object.getClass`,
    // `Class.forName`, reflection's parameter and member types, the
    // `NativeContext::get_class_mirror` door -- which otherwise ended the
    // process with `FATAL: ... young gen exhausted`. The mirror cache is
    // untouched when it declines, and no lock is held when it unwinds.
    // Outside such a scope (boot, JIT helpers) nothing changes. Until gcg
    // g3/o G1 kept its old answer (the infallible arm may take the collector's
    // reserve, which G1's fallible entry stops short of), so only Generational
    // and ZGC -- whose infallible arm is the fallible one plus an abort --
    // changed, and only where they aborted.
    //
    // gcg g3/o (`gcg-g1o-reading-a-caught-oome-meets-non-collecting-doors`,
    // door 2): G1 now takes the fallible arm too. On its refusal G1 still
    // falls back to the infallible arm -- which may take the reserve -- but
    // only while a Free region is left for it to claim; with none, that arm
    // could only abort the process (vg1 `o1_jni_g1`, rc 134, `FATAL: G1: out
    // of heap space` at `e.getClass()` on a caught OOME), so the refusal
    // unwinds to the catchable `OutOfMemoryError` instead.
    // `CRATONVM_GC_G1_NATIVE_MIRROR_OOME=0` restores the old G1 answer.
    if crate::runtime::native_oom::unwind_ok() {
        if let Some(mirror) = class_mirror_impl(shared, class_id, true, None) {
            return mirror;
        }
        match native_mirror_refusal(
            shared.mem.heap.is_g1(),
            g1_free_regions(&shared.mem.heap),
            shared.mem.heap.is_g1() && g1_native_mirror_oome_enabled(),
        ) {
            NativeMirrorRefusal::Raise => crate::runtime::native_oom::raise(1, "class mirror"),
            NativeMirrorRefusal::Infallible => {}
        }
    }
    match class_mirror_impl(shared, class_id, false, None) {
        Some(mirror) => mirror,
        // Unreachable: with `fallible == false` every allocation below is the
        // panicking one, so the impl never declines. Kept total without a
        // panic by allocating a bare mirror the way the impl would have.
        None => alloc_class_mirror_object(shared, ClassId::new(0), CLASS_MIRROR_NUM_FIELDS),
    }
}

/// What [`get_or_create_class_mirror`] does when its fallible arm declined
/// inside an `unwind_ok()` scope. gcg g3/o.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NativeMirrorRefusal {
    /// Unwind to the catchable `OutOfMemoryError`.
    Raise,
    /// Fall through to the infallible arm (G1's reserve).
    Infallible,
}

/// The pure rule behind [`NativeMirrorRefusal`].
///
/// * Generational and ZGC: their infallible arm is the fallible one plus an
///   abort, so a refusal always raises (gen r5w1/oom5).
/// * G1 with the switch off: the infallible arm, as before gcg g3/o.
/// * G1 with the switch on (the default): the infallible arm only while a
///   Free region is left (G1's fallible entry stops short of the last
///   `tlab_reserve_regions`, the infallible one may claim them); with none,
///   the infallible arm can only abort, so raise.
fn native_mirror_refusal(
    is_g1: bool,
    g1_free_regions: usize,
    g1_switch_on: bool,
) -> NativeMirrorRefusal {
    if !is_g1 {
        return NativeMirrorRefusal::Raise;
    }
    if g1_switch_on && g1_free_regions == 0 {
        NativeMirrorRefusal::Raise
    } else {
        NativeMirrorRefusal::Infallible
    }
}

/// Free regions of a G1 heap (`0` on every other backend). One relaxed load.
fn g1_free_regions(heap: &VmHeap) -> usize {
    match heap {
        VmHeap::G1(g1) => g1.free_region_count(),
        _ => 0,
    }
}

/// gcg g3/o: `CRATONVM_GC_G1_NATIVE_MIRROR_OOME` (default ON;
/// `runtime_flag_default_on`, `=0` restores the infallible G1 arm). Read only
/// on a refused mirror allocation inside a native, never on the cached path.
fn g1_native_mirror_oome_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GC_G1_NATIVE_MIRROR_OOME")
}

/// [`get_or_create_class_mirror`] that DECLINES instead of aborting when the
/// heap cannot hold the mirror (or its name String) without a collection.
/// `None` leaves the mirror cache untouched; callers retry through
/// `gc_and_alloc::collect_and_retry` and throw `OutOfMemoryError` when that
/// fails (gen r4w5: `ldc` of a Class constant during class initialisation on
/// a full heap aborted the VM with `FATAL: OutOfMemoryError: young gen
/// exhausted` in `GenR4W5ThreadsOomProbe`).
///
/// The slot-0 class-id store boxes through `VmHeap::try_set_field` on this
/// path, so its wrapper allocation declines too (gen r5w1/oom5). The loader
/// edge store is a reference store and allocates nothing.
pub fn try_get_or_create_class_mirror(shared: &SharedVm, class_id: ClassId) -> Option<ObjectRef> {
    class_mirror_impl(shared, class_id, true, None)
}

/// zs s7/nd6 (`zgc-zs2nd2-proposal-mirror-inside-the-define-20261014.md`,
/// "Variant: a reserved mirror"): the mirror OBJECT of a class that is about
/// to be defined, allocated through the fallible door BEFORE the define, so a
/// heap that cannot hold it fails the define before anything is published
/// (HotSpot creates the mirror inside `define_instance_class`; an
/// `OutOfMemoryError` there leaves nothing defined). With `pooled_name` (the
/// class's internal name, a named non-hidden define) the name String the
/// mirror will carry is interned now too and parked in the mirror's `name`
/// slot, so the adoption's own name lookup is a pool hit. `None` = refused
/// (nothing kept: the object is garbage). [`adopt_reserved_class_mirror`]
/// makes it the class's mirror after the define.
pub(crate) fn try_reserve_class_mirror(
    shared: &SharedVm,
    pooled_name: Option<&str>,
) -> Option<ObjectRef> {
    try_reserve_class_mirror_parking(shared, pooled_name, mirror_name_kept_null(shared))
}

/// Whether a class mirror's `name` field stays null: the opt-in
/// pure-bytecode lever under `--jdk-only`, so the JDK's own `Class.getName()`
/// (`initClassName()`) answers the binary name (see [`class_mirror_impl`]).
fn mirror_name_kept_null(shared: &SharedVm) -> bool {
    cratonvm_native_api::unretire::pure_bytecode()
        && crate::vm::dispatch_policy(shared).is_jdk_only()
}

/// [`try_reserve_class_mirror`] with the lever's answer passed in (a test
/// models it without the process-wide `CRATONVM_PURE_BYTECODE` latch). zs
/// s8/nd7 (rv5): `keep_name_null` parks NO name -- the reservation used to
/// write the internal name the mint deliberately leaves null, so
/// `Class.getName()` of a `defineClass1/2` class answered `pkg/Foo`.
pub(crate) fn try_reserve_class_mirror_parking(
    shared: &SharedVm,
    pooled_name: Option<&str>,
    keep_name_null: bool,
) -> Option<ObjectRef> {
    let pooled_name = pooled_name.filter(|_| !keep_name_null);
    let layout = class_mirror_layout(shared);
    let (class_class_id, mirror_field_count) = class_mirror_object_shape(shared, layout);
    let mirror = shared
        .mem
        .heap
        .try_alloc_object_full(class_class_id, mirror_field_count)?;
    let name_slot = match layout {
        Some(l) => l.slots.name,
        None => shared
            .classes
            .class_manager
            .read()
            .get_class(class_class_id)
            .and_then(|c| resolve_class_mirror_slots(c, mirror_field_count).name),
    };
    if let (Some(name), Some(idx)) = (pooled_name.filter(|n| !n.is_empty()), name_slot) {
        // `try_create_java_string` does not collect: `mirror` stays put.
        // zs s8/nd7 (`zgc-zs7nd6-native-door-residuals-FIXED-20261019.md` §2): the
        // name is NOT pooled yet -- an already-pooled String is parked as is,
        // else an unpooled one, which the adoption interns
        // ([`adopted_mirror_name`]); a define that fails leaves garbage, not
        // a pool entry. `=0`: nd6's pooled name.
        let name_obj = if define_mirror_name_pooled_at_adoption() {
            let pooled = shared.mem.string_pool.read().get(name).copied();
            match pooled {
                Some(obj) => obj,
                None => try_create_java_string_uninterned(shared, name)?,
            }
        } else {
            try_create_java_string(shared, name)?
        };
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    Some(mirror)
}

/// zt t1/nd: the three outcomes the define-mirror census counts
/// ([`crate::vm::realms::heap_realm::AllocLadderState::define_mirror_census`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefineMirrorCensus {
    /// A reservation was served (the mirror object is held across a define).
    Reserved = 0,
    /// The fallible door refused a reservation (before or after the reclaim
    /// ladder's collection): a refusal after the ladder is the define's
    /// `OutOfMemoryError` with nothing defined.
    Refused = 1,
    /// A reserved object became the defined class's mirror.
    Adopted = 2,
}

/// zt t1/nd: count one define-mirror outcome. On a refusal, with
/// `CRATONVM_DBG_ZGC_DEFINE_MIRROR=1`, print the cumulative
/// [`zgc_define_mirror_line`] on stderr (refusals are rare: the flag is read
/// only there), so a probe row can prove the reservation path refused without
/// a shutdown hook.
pub(crate) fn note_define_mirror(shared: &SharedVm, outcome: DefineMirrorCensus) {
    shared.mem.alloc_ladder.define_mirror_census[outcome as usize]
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if outcome == DefineMirrorCensus::Refused
        && cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_ZGC_DEFINE_MIRROR")
    {
        if let Some(line) = zgc_define_mirror_line(shared) {
            eprintln!("{line}");
        }
    }
}

/// zt t1/nd: `[GC] zgc-define-mirror: reserved=<n> refused=<n> adopted=<n>`,
/// the define-mirror reservation's census (ZGC only; `None` elsewhere, where
/// nothing reserves). `refused >= 1` on a run proves a define met the
/// reservation's refusal (the positive control of the `Zt1Nd*DefineRetry`
/// rows); `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED=0` keeps all three at `0`.
pub fn zgc_define_mirror_line(shared: &SharedVm) -> Option<String> {
    if !heap_is_zgc(shared) {
        return None;
    }
    let c = &shared.mem.alloc_ladder.define_mirror_census;
    let read = |o: DefineMirrorCensus| c[o as usize].load(std::sync::atomic::Ordering::Relaxed);
    Some(format!(
        "[GC] zgc-define-mirror: reserved={} refused={} adopted={}",
        read(DefineMirrorCensus::Reserved),
        read(DefineMirrorCensus::Refused),
        read(DefineMirrorCensus::Adopted),
    ))
}

/// zs s8/nd7: whether a reserved mirror parks its name unpooled until the
/// adoption (`CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION`, default on;
/// `=0` pools it at the reservation, as nd6 did). Read per define (not
/// cached), so a test's thread override reaches it.
fn define_mirror_name_pooled_at_adoption() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_ZGC_DEFINE_MIRROR_NAME_POOLED_AT_ADOPTION",
    )
}

/// zs s8/nd7: the pooled name String an adopted mirror carries: the String
/// [`try_reserve_class_mirror`] parked in the reserved object's `name` slot,
/// interned now that the class is defined (an entry another thread pooled
/// meanwhile wins, so `intern()` identity holds). `None` when nothing
/// matching is parked; the caller pools the name as before. Allocates
/// nothing.
fn adopted_mirror_name(
    shared: &SharedVm,
    reserved: ObjectRef,
    idx: usize,
    class_name: &str,
) -> Option<ObjectRef> {
    let Value::Object(Some(parked)) = shared.mem.heap.get_field(reserved, idx) else {
        return None;
    };
    if read_java_string(&shared.mem.heap, parked).as_deref() != Some(class_name) {
        return None;
    }
    // gcj j2/rf2 (the j1-st report's funnel bypass): a miss is pooled through
    // the one insert (`string_table::pool_insert_if_absent`), so the funnel's
    // young list keeps its superset if a Generational caller ever reaches
    // here (today only ZGC does: `try_reserve_class_mirror` answers
    // `Unsupported` elsewhere, and there the insert is the plain one it was).
    // A hit answers the pooled String, as `entry().or_insert` did.
    let pooled = shared.mem.string_pool.read().get(class_name).copied();
    if let Some(existing) = pooled {
        return Some(existing);
    }
    Some(crate::memory::string_table::pool_insert_if_absent(shared, class_name, parked).0)
}

/// zs s9/nd8: the String parked in a reserved mirror's `name` slot when it
/// reads `class_name`, NOT interned (a hidden class's name is unpooled on
/// ZGC, [`hidden_mirror_name_unpooled`]). `None`: nothing matching parked.
/// Allocates nothing.
fn parked_unpooled_mirror_name(
    shared: &SharedVm,
    reserved: ObjectRef,
    idx: usize,
    class_name: &str,
) -> Option<ObjectRef> {
    let Value::Object(Some(parked)) = shared.mem.heap.get_field(reserved, idx) else {
        return None;
    };
    (read_java_string(&shared.mem.heap, parked).as_deref() == Some(class_name)).then_some(parked)
}

/// zs s9/nd8: `CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED` (default on;
/// `=0`: the adoption allocates a hidden class's name after the define, as
/// before). Read per adoption, so a test's thread override reaches it.
fn define_mirror_hidden_name_parked() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_DEFINE_MIRROR_HIDDEN_NAME_PARKED")
}

/// zs s9/nd8: test-only count of adoptions that allocated the mirror's name
/// String AFTER the define (no matching parked name).
#[cfg(test)]
pub(crate) mod zs9nd8_adoption_name_allocs {
    use std::cell::Cell;
    thread_local! {
        static N: Cell<u32> = const { Cell::new(0) };
    }
    pub(crate) fn note() {
        N.with(|n| n.set(n.get() + 1));
    }
    /// The count since the last call (and reset).
    pub(crate) fn take() -> u32 {
        N.with(|n| n.replace(0))
    }
}

/// zs s7/nd6: make `reserved` (from [`try_reserve_class_mirror`], read back
/// from its pin by the caller) the mirror of the just-defined `class_id`:
/// [`class_mirror_impl`]'s fallible arm with the object already in hand. An
/// existing mirror wins (the reservation is then garbage); `None` when the
/// fallible arm declined a later allocation (the caller mints as before) or
/// the object is not a mirror-shaped `java/lang/Class` instance any more.
pub(crate) fn adopt_reserved_class_mirror(
    shared: &SharedVm,
    class_id: ClassId,
    reserved: ObjectRef,
) -> Option<ObjectRef> {
    class_mirror_impl(shared, class_id, true, Some(reserved))
}

/// The class id and slot count of a mirror OBJECT (`java/lang/Class`): the
/// memoized layout's, else resolved and cached as [`class_mirror_impl`]'s
/// slow path does. Takes the class-manager write lock on the unmemoized path.
fn class_mirror_object_shape(
    shared: &SharedVm,
    layout: Option<&ClassMirrorLayout>,
) -> (ClassId, usize) {
    if let Some(l) = layout {
        return (l.class_class_id, l.mirror_field_count);
    }
    let cached = shared
        .classes
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
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
}

/// The accessor half of a JPMS export refusal written by the class manager
/// (`classloading/src/access_control.rs`: `class A (in unnamed module) cannot
/// access class T (in module M) because module M does not export p to unnamed
/// module`), which cannot read the accessor's `Module` object.
const UNNAMED_ACCESSOR: &str = "(in unnamed module) cannot access class ";
const EXPORTED_TO_UNNAMED: &str = " to unnamed module";

/// HotSpot's spelling of that refusal (`Reflection::verify_class_access_msg`
/// names an unnamed module `unnamed module @0x<identity hash>`), given the
/// accessor's unnamed `module` object. Any other message is returned
/// unchanged. The one copy of the rewrite (interpreter round i5 wave 100, lane
/// L4): the define-time refusal (`vm_exec.rs`
/// `define_refusal_with_unnamed_module_hash`), the constant-pool resolution
/// refusal (`constants.rs` `raise_class_access_denial`), the method-handle
/// site link (`invoke.rs` `mh_site_link_access_refusal`) and the doors' copy
/// (`vm_exec.rs` `class_access_denial_for_name`) all come here.
pub(crate) fn unnamed_module_access_text(
    shared: &SharedVm,
    module: ObjectRef,
    message: String,
) -> String {
    if !message.contains(UNNAMED_ACCESSOR) || !message.ends_with(EXPORTED_TO_UNNAMED) {
        return message;
    }
    let heap = &shared.mem.heap;
    let heap_answer = heap.identity_hash_code(module);
    let hash = shared
        .threads
        .monitors
        .java_identity_hash(module, heap_answer, || heap.next_identity_hash()) as u32;
    let with_hash = format!("unnamed module @0x{hash:x}");
    let accessor = format!("(in {with_hash}) cannot access class ");
    let mut text = message.replacen(UNNAMED_ACCESSOR, &accessor, 1);
    text.truncate(text.len() - "unnamed module".len());
    text.push_str(&with_hash);
    text
}

/// [`unnamed_module_access_text`] for a refusal whose accessor is the loaded
/// class `accessor`: its module is its mirror's `module` field, else its
/// loader's `unnamedModule`. A module that cannot be read leaves the message
/// as it is. May create the accessor's mirror: call it with no class-manager
/// guard held.
#[cold]
pub(crate) fn access_denial_with_unnamed_module_hash(
    shared: &SharedVm,
    accessor: ClassId,
    message: String,
) -> String {
    if !message.contains(UNNAMED_ACCESSOR) || !message.ends_with(EXPORTED_TO_UNNAMED) {
        return message;
    }
    let heap = &shared.mem.heap;
    let field = |obj: ObjectRef, name: &str| -> Option<ObjectRef> {
        let class_id = heap.class_id_of(obj);
        let slot = super::vm_exec::resolve_field_slot_by_name_cached(shared, class_id, name)?;
        match heap.get_field(obj, slot) {
            Value::Object(Some(o)) => Some(o),
            _ => None,
        }
    };
    let Some(mirror) = try_get_or_create_class_mirror(shared, accessor) else {
        return message;
    };
    // `--jdk-only` fills `Class.module` (`class_module_object`). `--compatible`
    // leaves it null and answers `Class.getModule()` natively (w101c hand-back:
    // the hash was missing there): an application- or platform-loader class
    // gets the ONE canonical unnamed module that native publishes in
    // `module_mirrors[""]` (`lang_class::canonical_unnamed_module`, the JDK
    // application loader's own once adopted); a user loader's class, its
    // loader's `unnamedModule` (`lang_class::unnamed_module_for_loader`), the
    // loader read off the mirror or, for a built-in tag, the VM's loader object.
    let loader_id = shared.classes.class_manager.read().get_loader_id(accessor);
    let builtin = matches!(
        loader_id,
        Some(cratonvm_types::ClassLoaderId::Application | cratonvm_types::ClassLoaderId::Extension)
    );
    let canonical = || shared.classes.module_mirrors.read().get("").copied();
    let loader_object = || {
        field(mirror, "classLoader").or_else(|| {
            let tag = loader_id?.to_native_id();
            cratonvm_native_builtins::classloader::adopted_builtin_loader_for_tag(
                &shared.loader_identity,
                tag,
                "",
            )
        })
    };
    let module = field(mirror, "module")
        .or_else(|| if builtin { canonical() } else { None })
        .or_else(|| field(loader_object()?, "unnamedModule"));
    match module {
        Some(module) => unnamed_module_access_text(shared, module, message),
        None => message,
    }
}

/// [`access_denial_with_unnamed_module_hash`] for a caller that holds the
/// running thread: when the module cannot be read without Java (`--compatible`
/// before anything asked `Class.getModule()`: the canonical unnamed module is
/// published only by that native, `lang_class::canonical_unnamed_module`), the
/// accessor's `Class.getModule()` is CALLED, so the module named is the very
/// object `getModule()` answers (and is published per VM by the native, as on
/// its first ordinary call). Any failure of that call leaves the message as the
/// shared-only helper answered it; no exception escapes. Interpreter round i5
/// wave 101 hand-back (lane L4). Call it with no class-manager guard held: it
/// may run the native (allocation, a collection point).
#[cold]
pub(crate) fn access_denial_with_unnamed_module_hash_on(
    shared: &SharedVm,
    thread: &mut crate::threading::jvm_thread::JvmThread,
    accessor: ClassId,
    message: String,
) -> String {
    let message = access_denial_with_unnamed_module_hash(shared, accessor, message);
    if !message.contains(UNNAMED_ACCESSOR) || !message.ends_with(EXPORTED_TO_UNNAMED) {
        // Rewritten already, or not this refusal.
        return message;
    }
    let Some(mirror) = try_get_or_create_class_mirror(shared, accessor) else {
        return message;
    };
    let mut ctx = super::NativeContextImpl { shared, thread };
    let answer = cratonvm_native_api::NativeInvokeAccess::invoke_virtual(
        &mut ctx,
        mirror,
        "getModule",
        "()Ljava/lang/Module;",
        &[],
    );
    match answer {
        Ok(Some(Value::Object(Some(module)))) => {
            unnamed_module_access_text(shared, module, message)
        }
        _ => message,
    }
}

/// The component type of an array class, as its mirror is keyed.
enum MirrorComponent {
    /// A reference (or array) component: the component class's id.
    Class(ClassId),
    /// A primitive component of a `[I`-shaped array: the canonical mirror name.
    Primitive(&'static str),
}

/// The component of `class_id` when it is an array class. Primitive-component
/// arrays carry no `ArrayInfo` (a primitive has no `ClassId`), so those are
/// recognised by their one-letter descriptor.
fn array_mirror_component(shared: &SharedVm, class_id: ClassId) -> Option<MirrorComponent> {
    let cm = shared.classes.class_manager.read();
    let class = cm.get_class(class_id)?;
    if let Some(info) = class.array_info.as_ref() {
        return Some(MirrorComponent::Class(info.component_class_id));
    }
    let name: &str = &class.name;
    let prim = match name {
        "[I" => "int",
        "[J" => "long",
        "[F" => "float",
        "[D" => "double",
        "[Z" => "boolean",
        "[B" => "byte",
        "[C" => "char",
        "[S" => "short",
        _ => return None,
    };
    Some(MirrorComponent::Primitive(prim))
}

/// Whether a class mirror's slot 0 carries the VM's class-id overlay.
///
/// SBR2-CLASSREFL: slot 0 of the real `java.lang.Class` is
/// `cachedConstructor`. With the real class the overlay is off in both
/// execution policies, so the JDK's own `Class.newInstance` sees a null there.
/// Synthetic-stub `Class` keeps it. The environment switch
/// `CRATONVM_CLASS_MIRROR_SLOT0_OVERLAY=1` forces the overlay on; it is read
/// once per VM (`VmConfig::class_mirror_slot0_overlay`), not into a process
/// `OnceLock` as before interpreter round i4 wave 95 (lane L5).
fn mirror_slot0_overlay(shared: &SharedVm, class_class_id: ClassId) -> bool {
    if shared.config.class_mirror_slot0_overlay {
        return true;
    }
    shared
        .classes
        .class_manager
        .read()
        .get_class(class_class_id)
        .map(|c| c.origin.is_compatibility_stub())
        .unwrap_or(true)
}

/// gci i12/ul (F2b): the innermost REFERENCE element class of the array class
/// `class_id`, by identity: follow `ArrayInfo::component_class_id` until a
/// class that is not an array. `class_of(id)` answers `(is_array, component)`
/// for a class in the store, `None` for an id the store does not hold.
///
/// `None` when `class_id` is not an array, when the chain ends on a primitive
/// array (`[I` has no component), when a link is missing from the store (an
/// array minted before `array_info`, a lambda-proxy component, an unloaded
/// element), or after 256 links (the JVMS caps dimensions at 255). The caller
/// then falls back to the name lookup it used before. Pure.
fn innermost_array_element(
    class_id: ClassId,
    class_of: &dyn Fn(ClassId) -> Option<(bool, Option<ClassId>)>,
) -> Option<ClassId> {
    let mut current = class_id;
    for _ in 0..=256 {
        let (is_array, component) = class_of(current)?;
        if !is_array {
            return (current != class_id).then_some(current);
        }
        current = component?;
    }
    None
}

/// gci i12/ul F2: if `class_id` is an array class of a USER namespace, hand
/// `classloader::register_array_class_loader` its innermost element, which
/// gives the array the element's defining-loader row (behind
/// `CRATONVM_ARRAY_CLASS_LOADER_ROWS`, default ON since 2026-10-17). Returns
/// whether a row was added. One class-manager read; nothing else held is taken. Called when an
/// array's mirror is minted and by the root scan's drain of
/// `ClassManager::take_pending_user_arrays`.
pub fn register_user_array_loader_row(shared: &SharedVm, class_id: ClassId) -> bool {
    if !cratonvm_native_builtins::classloader::array_class_loader_rows_enabled() {
        return false;
    }
    let element = user_array_element(&shared.classes.class_manager.read(), class_id);
    match element {
        Some(element) => cratonvm_native_builtins::classloader::register_array_class_loader(
            &shared.loader_identity,
            class_id.as_u32(),
            element.as_u32(),
        ),
        None => false,
    }
}

/// gci i12/ul F2: the innermost element of `class_id` when it is an array
/// class of a USER namespace ([`innermost_array_element`]), else `None`.
pub fn user_array_element(
    cm: &crate::classloading::ClassManager,
    class_id: ClassId,
) -> Option<ClassId> {
    let is_user_array = cm.get_class(class_id).is_some_and(|c| {
        c.name.starts_with('[')
            && matches!(
                c.loader_id,
                crate::classloading::ClassLoaderId::UserDefined(_)
            )
    });
    if !is_user_array {
        return None;
    }
    let class_of = |id: ClassId| {
        cm.get_class(id).map(|c| {
            (
                c.name.starts_with('['),
                c.array_info.as_ref().map(|info| info.component_class_id),
            )
        })
    };
    innermost_array_element(class_id, &class_of)
}

/// `reserved` (zs s7/nd6): a mirror object allocated before the define
/// ([`try_reserve_class_mirror`]), used instead of a new allocation; only with
/// `fallible`.
fn class_mirror_impl(
    shared: &SharedVm,
    class_id: ClassId,
    fallible: bool,
    reserved: Option<ObjectRef>,
) -> Option<ObjectRef> {
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
        return Some(mirror);
    }
    // `CRATONVM_DBG_DEFINE_CENSUS` (interpreter round i4 wave 95, lane L5):
    // a fresh mirror made inside a charged define's `mirror-create`
    // (`define_census::charged_scope`) is split into `mirror-alloc`,
    // `mirror-fields`, `mirror-loader-module` and `mirror-publish`. Guards for
    // the first two, which have early returns.
    use cratonvm_classloading::define_census::{
        charged_scope, phase_end, phase_guard, phase_start_if, DefinePhase,
    };
    let census = charged_scope();
    let census_alloc = phase_guard(DefinePhase::MirrorAlloc, phase_start_if(census));

    // An array class's mirror carries its component type's mirror in the real
    // `Class.componentType` field, as HotSpot's `java_lang_Class::create_mirror`
    // writes it. The component's mirror is minted HERE, before the write guard
    // below is taken: minting it under the guard would re-enter this function
    // and deadlock on `class_mirrors`. It is read back out of the cache under
    // the guard, so a collection between the two cannot leave a stale address
    // in the field.
    let component = array_mirror_component(shared, class_id);
    match &component {
        Some(MirrorComponent::Class(component_id)) => {
            if fallible {
                try_get_or_create_class_mirror(shared, *component_id)?;
            } else {
                get_or_create_class_mirror(shared, *component_id);
            }
        }
        Some(MirrorComponent::Primitive(prim)) => {
            if fallible {
                try_get_or_create_primitive_mirror(shared, prim)?;
            } else {
                get_or_create_primitive_mirror(shared, prim);
            }
        }
        None => {}
    }
    // A lambda proxy is not in the class store: HotSpot's modifiers of its
    // hidden class are `ACC_FINAL | ACC_SYNTHETIC`. Asked before the mirror
    // lock, so the proxy table's lock is never taken under it. The table is
    // written right after the proxy's id is allocated, before any instance
    // (and so any `getClass()`) exists.
    let lambda_proxy = shared.classes.is_lambda_proxy_class(class_id);

    // Slow path: create new mirror
    let mut mirrors = shared.classes.class_mirrors.write();
    // Double-check after acquiring write lock
    if let Some(&mirror) = mirrors.get(&class_id) {
        return Some(mirror);
    }

    // `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO`: the mirror object's layout without
    // the write lock, the by-name slot walk or the overlay read lock below.
    let layout = class_mirror_layout(shared);

    // Load java/lang/Class and resolve field count (cached after first call).
    let (class_class_id, mirror_field_count) = class_mirror_object_shape(shared, layout);

    // zs s7/nd6: a reserved mirror of the right shape stands for the
    // allocation (fallible arm only); a stale shape is refused, and the
    // caller mints as before.
    let reserved = match reserved {
        Some(r) if fallible => {
            if shared.mem.heap.class_id_of(r) != class_class_id
                || shared.mem.heap.num_fields(r) < mirror_field_count
            {
                return None;
            }
            Some(r)
        }
        _ => None,
    };
    let mirror = if let Some(r) = reserved {
        r
    } else if fallible {
        // Nothing is inserted before this point, so declining here leaves the
        // cache exactly as it was; the write lock drops with the return.
        shared
            .mem
            .heap
            .try_alloc_object_full(class_class_id, mirror_field_count)?
    } else {
        alloc_class_mirror_object(shared, class_class_id, mirror_field_count)
    };

    // Populate the `java.lang.Class` mirror. The JDK lays out `Class`'s private
    // instance fields (`name`, `modifiers`, `primitive`, `classRedefinedCount`,
    // `reflectionData`, …) in an order that is an implementation detail and has
    // changed between releases, so rather than hardcoding JDK-25 slot numbers
    // we resolve each field's heap slot **by name** off the loaded
    // `java/lang/Class`. `set_field` writes the same default null/zero into
    // every other slot during allocation, so we only set the fields whose
    // value differs from that default — and only when the field is present in
    // the loaded layout (synthetic stubs resolve nothing).
    // `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` (interpreter round i6 wave
    // 115, lane L5): `class_module_object`'s facts are read under this guard
    // too, and the defining-loader table is asked once below.
    let one_lookup = shared.config.class_mirror_one_loader_lookup;
    let mut module_facts: Option<ClassModuleFacts> = None;
    let (class_name, hidden, modifiers, slots) = {
        let cm = shared.classes.class_manager.read();
        let class_row = cm.get_class(class_id);
        if one_lookup && crate::vm::dispatch_policy(shared).is_jdk_only() {
            module_facts = class_row.map(|class| {
                let module_name = class.module_name.clone();
                let class_path_only = module_name
                    .as_deref()
                    .is_some_and(|n| cm.module_registry.is_class_path_only(n))
                    || module_label_is_not_membership(shared, class);
                ClassModuleFacts {
                    module_name,
                    class_path_only,
                    loader_tag: cm.get_loader_id(class_id).map(|id| id.to_native_id()),
                }
            });
        }
        let name = class_row
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
        let hidden = cm.get_class(class_id).is_some_and(|c| c.is_hidden());
        // HotSpot's `modifier_flags`, not the raw `access_flags` (interpreter
        // round i1 wave 52, lane L3; [`hotspot_class_modifiers`]).
        let flags = if lambda_proxy {
            LAMBDA_PROXY_MODIFIERS
        } else {
            hotspot_class_modifiers(&cm.class_store, class_id, |id| {
                shared.classes.is_lambda_proxy_class(id)
            })
        };
        // Resolve the mirror's writable slots off the `java/lang/Class`
        // definition (not the mirrored class), since they describe the
        // *mirror object's* layout.
        let slots = match layout {
            Some(l) => l.slots,
            None => cm
                .get_class(class_class_id)
                .map(|c| resolve_class_mirror_slots(c, mirror_field_count))
                .unwrap_or_default(),
        };
        (name, hidden, flags, slots)
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
    //
    // gen r5w1/oom5 (review6 page item 2): on the FALLIBLE arm the same
    // descriptor-less store goes through `VmHeap::try_set_field`, which boxes
    // with a non-aborting wrapper allocation. On a heap that cannot hold the
    // 24-byte wrapper it stores null and answers `false`; the half-built
    // mirror is then dropped unpublished (nothing is cached before this
    // point), exactly like the two declines around it. The infallible arm is
    // unchanged.
    //
    // SBR2-CLASSREFL (2026-09-30): under `--jdk-only` with a real
    // `java.lang.Class` the store is skipped (see `mirror_slot0_overlay`), so
    // `cachedConstructor` is a true null and the real `Class.newInstance` body
    // can run. `CRATONVM_CLASS_MIRROR_SLOT0_OVERLAY=1` restores the overlay.
    let overlay0 = match layout {
        Some(l) => l.overlay0,
        None => {
            let overlay0 = mirror_slot0_overlay(shared, class_class_id);
            remember_class_mirror_layout(
                shared,
                class_class_id,
                mirror_field_count,
                slots,
                overlay0,
            );
            overlay0
        }
    };
    let _expected = cratonvm_gc::autobox::expect_primitive_into_reference();
    let slot0_stored = if !overlay0 {
        true
    } else if fallible {
        shared
            .mem
            .heap
            .try_set_field(mirror, 0, Value::Int(class_id.as_u32() as i32))
    } else {
        shared
            .mem
            .heap
            .set_field(mirror, 0, Value::Int(class_id.as_u32() as i32));
        true
    };
    drop(_expected);
    if !slot0_stored {
        return None;
    }
    drop(census_alloc);
    let census_fields = phase_guard(DefinePhase::MirrorFields, phase_start_if(census));

    // name → class name String. Under the opt-in pure-bytecode lever the field
    // stays null so the JDK's own `Class.getName()` (`name != null ? name :
    // initClassName()`) answers the binary form instead of this overlay's
    // internal one; every VM reader of a class mirror's name falls back to the
    // class id.
    let keep_name_null = mirror_name_kept_null(shared);
    // zs s8/nd7 (rv5): a reserved mirror parked no name under the lever
    // ([`try_reserve_class_mirror`]); null the slot anyway, so an adopted
    // object never carries one the lever means to hide.
    if let (true, Some(idx), Some(_)) = (keep_name_null, slots.name, reserved) {
        shared.mem.heap.set_field(mirror, idx, Value::Object(None));
    }
    if let Some(idx) = slots.name.filter(|_| !keep_name_null) {
        // zs s4/nd4: a HIDDEN class's name is not pooled on ZGC
        // ([`hidden_mirror_name_unpooled`]); the String is stored into the
        // mirror right below, with nothing allocated in between.
        // zt t7/cl: nor is the name of a class a loader OBJECT defined (the
        // classes that can unload; [`mirror_name_unpooled`]).
        let unpooled = mirror_name_unpooled(shared, class_id, hidden);
        // zs s8/nd7: a reserved mirror's parked name, interned now.
        let adopted = match reserved {
            Some(r) if !unpooled => adopted_mirror_name(shared, r, idx, &class_name),
            // zs s9/nd8 (`zgc-zs8nd7-define-mirror-residuals-FIXED-20261019.md` §1):
            // a hidden class's parked (unpooled) name, reused as is.
            // zt t7/cl: a non-hidden class's parked name likewise (it is the
            // pool's own String only when the pool already held the text).
            Some(r) if !hidden || define_mirror_hidden_name_parked() => {
                parked_unpooled_mirror_name(shared, r, idx, &class_name)
            }
            _ => None,
        };
        #[cfg(test)]
        if reserved.is_some() && adopted.is_none() {
            zs9nd8_adoption_name_allocs::note();
        }
        let name_obj = if let Some(obj) = adopted {
            obj
        } else if fallible {
            // Declining here drops the half-built mirror before it is
            // published: it is unreachable garbage, never a cached mirror.
            if unpooled {
                try_create_java_string_uninterned(shared, &class_name)?
            } else {
                try_create_java_string(shared, &class_name)?
            }
        } else if unpooled {
            create_java_string_uninterned(shared, &class_name)
        } else {
            create_java_string(shared, &class_name)
        };
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(name_obj)));
    }
    // componentType → the element type's mirror (arrays only). Read from the
    // caches, not from a value held across the lock: see the note where the
    // component is minted.
    if let (Some(idx), Some(component)) = (slots.component_type, component.as_ref()) {
        let component_mirror = match component {
            MirrorComponent::Class(component_id) => mirrors.get(component_id).copied(),
            MirrorComponent::Primitive(prim) => {
                shared.classes.primitive_mirrors.read().get(*prim).copied()
            }
        };
        if let Some(component_mirror) = component_mirror {
            shared
                .mem
                .heap
                .set_field(mirror, idx, Value::Object(Some(component_mirror)));
        }
    }
    // modifiers → HotSpot's `modifier_flags` (what JDK 25's
    // `Class.getModifiers()` / `isInterface()` read).
    if let Some(idx) = slots.modifiers {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Int(modifiers));
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
    drop(census_fields);
    let loader_module_clock = phase_start_if(census);
    // A live Class mirror must keep its defining user loader live. Without
    // this real heap edge, the loader-unload pass can prune the side-table
    // entry while a framework (notably Hibernate's nested JUnit engine) still
    // retains the Class object. Subsequent `getClassLoader()` then falls back
    // to the app loader and same-named class selection crosses loaders.
    // An array class has no defining loader of its own: HotSpot's
    // `Class.classLoader` of `X[]` is the loader of `X`'s innermost element
    // (`null` for a primitive element). The JDK's `Class.getClassLoader()` is
    // `return classLoader;`, so under the pure-bytecode lever the field must say
    // so; resolve the loader lookups below against that element class.
    // A lambda proxy is defined in its host class's loader (HotSpot: the
    // hidden class's `Class.classLoader` is the host's). The proxy has no
    // loader of its own in this VM, so read the host's. SBR3 `refmisc`:
    // the JDK's `Class.getPackage` reads this field directly.
    let lambda_host = crate::runtime::interpreter::lambda_proxy_host(shared, class_id);
    let loader_cid = if one_lookup && lambda_host.is_none() && !class_name.starts_with('[') {
        // `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`: what the block below
        // answers for a class that is neither a lambda proxy nor an array
        // (its own id), without its read guard and name copy.
        Some(class_id)
    } else {
        let cm = shared.classes.class_manager.read();
        let cname = cm.get_class(class_id).map(|c| c.name.to_string());
        match (lambda_host, cname) {
            (Some((Some(host), _)), _) => Some(host),
            (Some((None, owner)), _) => cm.get_loaded_class_id(&owner),
            // gci i12/ul (F2b of
            // `gci-i12ul-class-unloading-and-side-tables-on-g1-review-findings-RETIRED-UNFIXED-20261018`):
            // the element class BY IDENTITY, through the array's own
            // `array_info` chain. The element's NAME is ambiguous as soon as two
            // loaders define it (a redeploy's old and new loader, or a
            // child-first loader over an application class): the loader-blind
            // `get_loaded_class_id` then answered `None` (field left null:
            // `X[].class.getClassLoader()` read `null`) or the application
            // loader's copy (the wrong loader). The name lookup stays as the
            // fallback for an array minted without `array_info`.
            (None, Some(n)) if n.starts_with('[') => {
                let class_of = |id: ClassId| {
                    cm.get_class(id).map(|c| {
                        (
                            c.name.starts_with('['),
                            c.array_info.as_ref().map(|info| info.component_class_id),
                        )
                    })
                };
                innermost_array_element(class_id, &class_of).or_else(|| {
                    n.trim_start_matches('[')
                        .strip_prefix('L')
                        .and_then(|e| e.strip_suffix(';'))
                        .and_then(|e| cm.get_loaded_class_id(e))
                })
            }
            _ => Some(class_id),
        }
    };
    // `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`: the class's own row, asked
    // once for `Class.classLoader`, `Class.module` and the mirror pin below.
    // `Some(row)` only when the switch is on and the class is its own loader
    // class (not a lambda proxy, not an array).
    let own_loader: Option<Option<ObjectRef>> = (one_lookup && loader_cid == Some(class_id))
        .then(|| {
            cratonvm_native_builtins::classloader::defining_loader_for(
                &shared.loader_identity,
                class_id.as_u32(),
            )
        });
    if let (Some(idx), Some(loader)) = (
        slots.class_loader,
        match own_loader {
            Some(row) => row,
            None => loader_cid.and_then(|cid| {
                cratonvm_native_builtins::classloader::defining_loader_for(
                    &shared.loader_identity,
                    cid.as_u32(),
                )
            }),
        },
    ) {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Object(Some(loader)));
    } else if let Some(idx) = slots.class_loader {
        // SBR2-LOADERS: with the JDK's own built-in loaders adopted, a class the
        // platform / application loader defined names it in `Class.classLoader`,
        // as HotSpot does, so the JDK's `getClassLoader0` reads the right thing.
        let (tag, cname, image_module) = {
            let cm = shared.classes.class_manager.read();
            match loader_cid {
                Some(cid) => {
                    let class = cm.get_class(cid);
                    // gci i4/jd: the named image module of a boot-tagged class
                    // (a modular jar on the class path is not one).
                    let image_module = class
                        .and_then(|c| c.module_name.clone())
                        .filter(|m| !cm.module_registry.is_class_path_only(m));
                    (
                        cm.get_loader_id(cid).map(|id| id.to_native_id()),
                        class.map(|c| c.name.to_string()),
                        image_module,
                    )
                }
                None => (None, None, None),
            }
        };
        let app_module_loaders = shared.natives.native_methods.image_app_module_loaders();
        if let Some(loader) = tag.zip(cname).and_then(|(t, n)| {
            cratonvm_native_builtins::classloader::adopted_builtin_loader_for_tag(
                &shared.loader_identity,
                t,
                &n,
            )
            .or_else(|| {
                // gci i4/jd: an application-loader image module's class
                // (`jdk.compiler`) names the app loader from creation, as the
                // platform modules' classes name theirs just above, so the
                // JDK's `return classLoader;` agrees with the native.
                let module = image_module.as_deref().filter(|_| t == 0)?;
                cratonvm_native_builtins::classloader::adopted_app_loader_for_image_module(
                    &shared.loader_identity,
                    module,
                    &n,
                    app_module_loaders,
                )
            })
        }) {
            shared
                .mem
                .heap
                .set_field(mirror, idx, Value::Object(Some(loader)));
        }
    }

    // SBR2 PUREMOD: `Class.module` (`--jdk-only`); a class whose module object
    // does not exist yet is revisited by `fixup_class_module_fields`.
    if let Some(idx) = slots.module {
        // `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`: the facts read above and
        // the row asked above (`module_facts` is `Some` only under the switch
        // and `--jdk-only`, the mode `class_module_object` answers in).
        let module = match module_facts.take() {
            Some(facts) => {
                class_module_object_from(shared, class_id, facts, &class_name, own_loader)
            }
            None => class_module_object(shared, class_id),
        };
        if let Some(m) = module {
            shared
                .mem
                .heap
                .set_field(mirror, idx, Value::Object(Some(m)));
        }
    }

    phase_end(DefinePhase::MirrorLoaderModule, loader_module_clock);
    let publish_clock = phase_start_if(census);
    mirrors.insert(class_id, mirror);
    // Reverse map for mirror_class_id (`class_id_from_mirror`).
    shared
        .classes
        .class_mirrors_reverse
        .write()
        .insert(mirror, class_id);

    // gci i12/ul F2: an array class of a user namespace has no defining-loader
    // row of its own, so the `mirror_pin` row below was never recorded for its
    // mirror, the root scan rooted the mirror on every scan, and through its
    // `componentType` it kept the element's loader alive for the VM's life.
    // Give it the element's row first (`CRATONVM_ARRAY_CLASS_LOADER_ROWS`,
    // default ON since 2026-10-17; a no-op with `=0`), so the block below pins the mirror to
    // the loader instead. The root scan registers arrays that never get a
    // mirror (`roots::register_pending_user_array_rows`).
    register_user_array_loader_row(shared, class_id);
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
    // `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP`: the row asked above. Between
    // that ask and here nothing allocates (the `mirror` reference is held
    // across the same span), and `register_user_array_loader_row` only adds a
    // row for an ARRAY class, which never has `own_loader`.
    let pin_loader = match own_loader {
        Some(row) => row,
        None => cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            class_id.as_u32(),
        ),
    };
    if let Some(loader) = pin_loader {
        let dbg_mirrorpin = match layout {
            Some(l) => l.dbg_mirrorpin,
            None => cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_MIRRORPIN"),
        };
        if dbg_mirrorpin {
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
        let mark_why = match layout {
            Some(l) => l.dbg_mark_why_class.as_deref().map(str::to_owned),
            None => cratonvm_types::flags::runtime_var("CRATONVM_DBG_MARK_WHY_CLASS").ok(),
        };
        if let Some(want) = mark_why {
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
    phase_end(DefinePhase::MirrorPublish, publish_clock);

    Some(mirror)
}

/// zr z4/ab: is this VM's heap the ZGC backend? The conversions below change
/// ZGC only: Generational's and G1's infallible arms spill / take a reserve,
/// so their answer stays the infallible entry's.
#[inline]
pub(crate) fn heap_is_zgc(shared: &SharedVm) -> bool {
    matches!(shared.mem.heap, VmHeap::Zgc(_))
}

/// zs s4/nd4 (`Zs2Nd2FfmMints define`, host `door_oome` ~6400 of 20000 where
/// HotSpot has 0): every class mirror's `name` String went into the interned
/// pool, and on ZGC that pool is a STRONG root (`CRATONVM_ZGC_WEAK_STRING_TABLE`
/// is opt-in). A non-strong hidden class unloads with its mirror, but its name
/// (`Foo$Tiny/0x...`, about 88 bytes) stayed pooled for the life of the VM:
/// measured locally, the live set after `System.gc()` grew by ~84 bytes per
/// `Lookup.defineHiddenClass`, so the 1 MiB the probe leaves free was gone
/// after ~12 000 defines and every later allocation threw. No native door
/// refused: the failing allocation was the probe's own `new byte[512]`.
/// HotSpot's `StringTable` is weak, so there the name dies with the mirror.
/// A hidden class's name cannot equal a String the program interned before
/// the define (its suffix is minted by the define), so leaving it out of the
/// pool is visible only to a later `intern()` of it, which pools it then.
/// ZGC heap only (round scope); `CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED=0` restores
/// the pooled name. Cached.
#[inline]
pub(crate) fn hidden_mirror_name_unpooled(shared: &SharedVm) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    heap_is_zgc(shared)
        && *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED")
        })
}

/// zt t7/cl (`zgc-zs4nd4-unloaded-class-names-stay-in-the-strong-string-pool-20261015.md`
/// residual 1): `CRATONVM_ZGC_CLASS_NAME_UNPOOLED` (default ON, `=0` kill;
/// read once per process): on ZGC, the mirror `name` String of a class that a
/// loader OBJECT defined (it has a defining-loader row, which every define
/// door records before the mirror is minted) is not added to the
/// interned-string pool, a strong root on ZGC. Such a class unloads with its
/// loader and now takes its name with it.
///
/// Not Java-visible. The slot holds this VM's INTERNAL name (`pkg/Foo`), an
/// overlay HotSpot does not have: HotSpot's `Class.name` is null until the
/// first `getName()`, which stores the interned binary name. Java reaches
/// identity only through `Class.getName()`, whose native interns its own
/// answer (`lang_class::native_class_get_name`: every arm `intern_string`s),
/// so `getName() == "Foo"` keeps HotSpot's answer either way; core
/// reflection filters `Class`'s fields. What changes is that an unrelated
/// pooled String with the same text (an `ldc "pkg/Foo"`, a `String.intern()`)
/// is no longer this slot's object, which nothing reads by identity (the
/// `getName()` memo compares the slot with itself).
pub(crate) fn loader_class_name_unpooled(shared: &SharedVm) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    heap_is_zgc(shared)
        && *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_CLASS_NAME_UNPOOLED")
        })
}

/// Whether a new mirror's `name` String stays out of the interned-string
/// pool: a hidden class's under [`hidden_mirror_name_unpooled`], a class with
/// a defining-loader row under [`loader_class_name_unpooled`] (zt t7/cl).
/// Asked under the `class_mirrors` write lock, as `class_mirror_impl` already
/// asks `defining_loader_for` there.
fn mirror_name_unpooled(shared: &SharedVm, class_id: ClassId, hidden: bool) -> bool {
    if hidden {
        return hidden_mirror_name_unpooled(shared);
    }
    loader_class_name_unpooled(shared)
        && cratonvm_native_builtins::classloader::defining_loader_for(
            &shared.loader_identity,
            class_id.as_u32(),
        )
        .is_some()
}

/// zr z4/ab: take the converted (fallible) path at the VM-internal
/// allocation sites z4/ab changed -- [`heap_is_zgc`] and
/// `CRATONVM_ZGC_INTERNAL_ALLOC_OOME` controls ZGC's native
/// `try_get_class_mirror` door (default ON); `=0` selects its legacy mirror
/// policy. VM preseeds, fixups, streams and launcher allocations are fallible
/// on every collector independently of this native-door switch.
pub(crate) fn zgc_internal_alloc_converted(shared: &SharedVm) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    heap_is_zgc(shared)
        && *ON.get_or_init(|| {
            cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_INTERNAL_ALLOC_OOME")
        })
}

/// Collection-free mirror creation; publish only after mirror and name fit.
pub fn try_get_or_create_primitive_mirror(shared: &SharedVm, prim_name: &str) -> Option<ObjectRef> {
    primitive_mirror_impl(shared, prim_name, true)
}

/// The detail line HotSpot prints under `Error occurred during initialization
/// of VM` when the heap cannot hold a start-up object
/// (`vm_exit_during_initialization` with the pending `OutOfMemoryError`,
/// rendered as `java.lang.OutOfMemoryError: Java heap space`).
pub(crate) const BOOT_OOME_DETAIL: &str = "java.lang.OutOfMemoryError: Java heap space";

/// zs s2/nd2 (`zgc-zr4ab-infallible-residuals-20261012.md` item 2): should a
/// refused start-up object end the boot as HotSpot ends it -- the two init
/// lines on stdout and exit status 1 -- instead of `abort()` (134)? Only
/// while the VM is still initializing (`get_init_level() < 4`: `main` has not been
/// reached), where HotSpot fails VM initialisation in the same place; a
/// later caller of a legacy (no failure channel) host API keeps the abort.
/// `CRATONVM_ZGC_BOOT_OOME_INIT_ERROR=0` restores the abort.
///
/// zs s7/nd6 (§8 of `zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md`): "still
/// initializing" is [`SharedVm::boot_finished`], which an embedder that never
/// raises the level to 4 latches with `SharedVm::mark_boot_finished`
/// (`cratonvm_embed::finish_boot`, `preallocate_out_of_memory_error`).
pub(crate) fn boot_oome_is_init_error(shared: &SharedVm) -> bool {
    !shared.boot_finished()
        && cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_BOOT_OOME_INIT_ERROR")
}

/// A legacy host API with no failure channel could not allocate `what`:
/// while booting ([`boot_oome_is_init_error`]) the typed startup refusal the
/// launcher renders as HotSpot's two init lines with status 1; after it (or
/// with `CRATONVM_ZGC_BOOT_OOME_INIT_ERROR=0`) the abort, with a line naming
/// the object and the heap setting.
#[cold]
#[inline(never)]
pub(crate) fn legacy_alloc_refused(shared: &SharedVm, what: &str) -> ! {
    if boot_oome_is_init_error(shared) {
        tracing::warn!(
            what,
            "a start-up object could not be allocated: VM init fails"
        );
        std::panic::panic_any(crate::error::VmInitializationError {
            detail: BOOT_OOME_DETAIL.to_string(),
        });
    }
    eprintln!(
        "Error occurred during initialization of VM\n\
         java.lang.OutOfMemoryError: Java heap space (allocating {what}; this VM-internal \
         object has no failure channel -- raise -Xmx)"
    );
    std::process::abort()
}

/// Legacy collection-free factory. A native catch scope converts exhaustion
/// into OOME; a host without that scope accepts the fatal allocation contract.
/// Java constant resolution uses `primitive_mirror_or_oom` instead.
pub fn get_or_create_primitive_mirror(shared: &SharedVm, prim_name: &str) -> ObjectRef {
    if crate::runtime::native_oom::unwind_ok() {
        return try_get_or_create_primitive_mirror(shared, prim_name)
            .unwrap_or_else(|| crate::runtime::native_oom::raise(1, "primitive mirror"));
    }
    primitive_mirror_impl(shared, prim_name, false).expect("infallible primitive mirror")
}

fn primitive_mirror_impl(shared: &SharedVm, prim_name: &str, fallible: bool) -> Option<ObjectRef> {
    let prim_name = canonical_primitive_mirror_name(prim_name);

    // Fast path: check cache
    if let Some(&mirror) = shared.classes.primitive_mirrors.read().get(prim_name) {
        return Some(mirror);
    }

    // Slow path
    let mut mirrors = shared.classes.primitive_mirrors.write();
    if let Some(&mirror) = mirrors.get(prim_name) {
        return Some(mirror);
    }

    // Resolve the real field count for java/lang/Class (same logic as
    // get_or_create_class_mirror). This ensures primitive mirrors are
    // allocated with the correct number of fields when running against
    // real JDK classes. Falls back to CLASS_MIRROR_NUM_FIELDS for synthetic mode.
    // `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO`: the memoized layout, as in
    // `class_mirror_impl`.
    let layout = class_mirror_layout(shared);
    let cached = shared
        .classes
        .cached_class_mirror_num_fields
        .load(std::sync::atomic::Ordering::Relaxed);
    let (class_class_id, mirror_field_count) = if let Some(l) = layout {
        (l.class_class_id, l.mirror_field_count)
    } else {
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

    let mirror = if fallible {
        shared
            .mem
            .heap
            .try_alloc_object_full(class_class_id, mirror_field_count)?
    } else {
        alloc_class_mirror_object(shared, class_class_id, mirror_field_count)
    };

    // Resolve the writable `java/lang/Class` mirror slots by name (see
    // `get_or_create_class_mirror` for why this is layout-independent).
    let slots = match layout {
        Some(l) => l.slots,
        None => {
            let cm = shared.classes.class_manager.read();
            cm.get_class(class_class_id)
                .map(|c| resolve_class_mirror_slots(c, mirror_field_count))
                .unwrap_or_default()
        }
    };
    let overlay0 = match layout {
        Some(l) => l.overlay0,
        None => mirror_slot0_overlay(shared, class_class_id),
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
    if overlay0 {
        shared.mem.heap.set_field(mirror, 0, Value::Int(-1));
    }
    drop(_expected);

    // name → primitive type name as String.
    if let Some(idx) = slots.name {
        let name_obj = if fallible {
            try_create_java_string(shared, prim_name)?
        } else {
            create_java_string(shared, prim_name)
        };
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
    // HotSpot's `modifiers` of a primitive mirror (interpreter round i1 wave
    // 52, lane L3; [`hotspot_class_modifiers`]); it was left 0.
    if let (Some(idx), true) = (slots.modifiers, is_real_primitive) {
        shared
            .mem
            .heap
            .set_field(mirror, idx, Value::Int(PRIMITIVE_MIRROR_MODIFIERS));
    }
    // classRedefinedCount → 0.
    if let Some(idx) = slots.class_redefined_count {
        shared.mem.heap.set_field(mirror, idx, Value::Int(0));
    }

    if is_real_primitive && crate::vm::dispatch_policy(shared).is_jdk_only() {
        if let (Some(idx), Some(m)) = (
            slots.module,
            shared
                .classes
                .module_mirrors
                .read()
                .get("java.base")
                .copied(),
        ) {
            shared
                .mem
                .heap
                .set_field(mirror, idx, Value::Object(Some(m)));
        }
    }

    mirrors.insert(prim_name.to_string(), mirror);
    Some(mirror)
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

/// Index into [`interp_static_site_counts`]: a `getstatic` answered by the
/// interpreter's quickened site table (`interpreter::field_fast`).
pub const INTERP_STATIC_SITE_GET_HIT: usize = 0;
/// A `putstatic` answered by the quickened site table.
pub const INTERP_STATIC_SITE_PUT_HIT: usize = 1;
/// A site written to the table by either slow handler.
pub const INTERP_STATIC_SITE_FILL: usize = 2;

/// Engagement of the interpreter's quickened static sites, under the same
/// `CRATONVM_DBG_GETSTATIC_PROF=1` gate as the index counters above, so a flat
/// A/B can be told apart from an arm that never fired.
static INTERP_STATIC_SITE_COUNTS: [std::sync::atomic::AtomicU64; 3] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Count one quickened-static-site event (`INTERP_STATIC_SITE_*`); one cached
/// gate load when the instrument is off.
#[inline]
pub fn note_interp_static_site(which: usize) {
    if statics_index_counters_on() {
        if let Some(c) = INTERP_STATIC_SITE_COUNTS.get(which) {
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// `[get_hit, put_hit, fill]` for the quickened static sites.
pub fn interp_static_site_counts() -> [u64; 3] {
    let g = |i: usize| INTERP_STATIC_SITE_COUNTS[i].load(std::sync::atomic::Ordering::Relaxed);
    [g(0), g(1), g(2)]
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
///
/// **Steady state is lock-free** (2026-09-23): once the class's block is
/// published and `field_index` is inside it, the store goes straight to the
/// cell through [`StaticsIndex::cell_ptr`](crate::vm::realms::class_realm::StaticsIndex::cell_ptr).
/// Every interpreted `putstatic` and every JIT `putstatic` helper used to take
/// the process-wide `statics` WRITE lock here, so two threads storing to ANY
/// statics serialized on one lock. The locked path below remains for creating
/// a block (a store before `prepare_class_shared`), growing one, and
/// `CRATONVM_NO_STATICS_INDEX=1` (which turns off both lock-free halves).
///
/// What the lock used to provide, and what replaces it:
///
/// * **SATB pre-barrier** on the overwritten reference: still done, on the
///   value read from the cell. Two racing writers may both log the value they
///   read rather than each other's; that is sound because a marking cycle
///   starts at a pause, which cannot fall inside this call, so a value written
///   during marking was not in the snapshot through this static.
/// * **Ordering**: a `Release` fence before the store keeps what the unlock
///   gave a racy publication (`static Foo f = new Foo()` read by another
///   thread). Volatile statics are fenced by the callers, as before.
/// * **Exclusion from republication**: not provided. A store racing a GROW of
///   the same class's block (a write past the declared field count, which no
///   resolved `putstatic` makes) or a re-`prepare_class_shared` can land in
///   the abandoned block and be lost — exactly as compiled code's baked-address
///   accesses already could, and a store racing a re-prepare was lost under
///   the lock too (the prepare replaces the block).
pub fn set_static_shared(shared: &SharedVm, class_id: ClassId, field_index: usize, value: Value) {
    if !statics_index_disabled() {
        if let Some(cell) = shared.classes.statics_index.cell_ptr(class_id, field_index) {
            // SAFETY: `cell_ptr` bounds-checked `field_index` against the
            // published block, which is leaked, so the cell is valid and
            // 8-aligned. The old-value read is the torn-read-safe two-word
            // form `StaticsIndex::get` uses; the store is a single volatile
            // 16-byte write, which never splits an 8-byte-aligned payload word
            // (see `StaticsIndex::get`).
            let words = cell as *const u64;
            let old = unsafe {
                let lo = words.read_volatile();
                let hi = words.add(1).read_volatile();
                std::mem::transmute::<[u64; 2], Value>([lo, hi])
            };
            if let Value::Object(Some(_)) = old {
                shared.mem.heap.satb_barrier(old);
            }
            std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            // SAFETY: as above — a valid, aligned cell of a leaked block.
            unsafe { cell.write_volatile(value) };
            return;
        }
    }
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
    // sees them. The interpreter's putstatic and the JIT static helper used to
    // fire their own barrier before calling in; since 2026-09-23 they rely on
    // this one. Reflection `Field.set`, `Unsafe`/`VarHandle`
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
pub fn pre_init_class_statics(
    shared: &SharedVm,
) -> Result<(), crate::runtime::native_oom::NativeAllocOom> {
    let cm = shared.classes.class_manager.read();
    let class_id = match cm.get_loaded_class_id("java/lang/Class") {
        Some(id) => id,
        None => return Ok(()),
    };
    let cls = match cm.get_class(class_id) {
        Some(c) => c,
        None => return Ok(()),
    };
    if cls.origin.is_compatibility_stub() {
        return Ok(());
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
            .try_alloc_array_full(class_id, ArrayElementType::Reference, 0)
            .ok_or(crate::runtime::native_oom::NativeAllocOom {
                length: 0,
                what: "java.lang.Class.EMPTY_CLASS_ARRAY",
            })?;
        set_static_shared(shared, class_id, idx, Value::Object(Some(empty_array)));
    }

    tracing::info!("Pre-initialized Class statics (EMPTY_CLASS_ARRAY)");
    Ok(())
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

    /// gc-common w5-c: the fallible GC-safe constructor (which the unwinding
    /// `NativeContext::create_string_uninterned_gc_safe` now collects and
    /// retries around) builds the same String as the infallible form it backs,
    /// and neither interns.
    #[test]
    fn gc_safe_threaded_string_is_fallible_and_unpooled() {
        use crate::threading::jvm_thread::{JvmThread, ThreadId};
        let shared = test_shared();
        let mut thread = JvmThread::new(ThreadId(0), "w5c-gc-safe-string");
        let a = try_create_java_string_uninterned_gc_safe_threaded(&shared, &mut thread, "w5c")
            .expect("an empty heap holds a String");
        let b = try_create_java_string_uninterned_gc_safe_threaded(&shared, &mut thread, "w5c")
            .expect("an empty heap holds a String");
        assert_eq!(
            read_java_string(&shared.mem.heap, a).as_deref(),
            Some("w5c")
        );
        assert_eq!(
            read_java_string(&shared.mem.heap, b).as_deref(),
            Some("w5c")
        );
        assert_ne!(a.as_ptr(), b.as_ptr());
    }

    /// gen r4w3/hunter: the fallible pooled constructor the unwinding
    /// `NativeContext::create_string` uses builds the same string, reports a
    /// miss as an allocation, and shares the pool with the infallible one.
    #[test]
    fn try_create_java_string_reporting_is_pooled_and_reports_allocation() {
        let shared = test_shared();
        let (a, allocated) =
            try_create_java_string_reporting(&shared, "r4w3-pooled").expect("room on a fresh heap");
        assert!(allocated, "a pool miss allocates");
        assert_eq!(
            read_java_string(&shared.mem.heap, a),
            Some("r4w3-pooled".to_string())
        );
        let (b, allocated) =
            try_create_java_string_reporting(&shared, "r4w3-pooled").expect("pool hit");
        assert!(!allocated, "a pool hit allocates nothing");
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(
            create_java_string(&shared, "r4w3-pooled").as_ptr(),
            a.as_ptr(),
            "one pool for both constructors"
        );
        let (u, allocated) =
            try_create_java_string_reporting(&shared, "\u{1F600} r4w3").expect("room");
        assert!(allocated);
        assert_eq!(
            read_java_string(&shared.mem.heap, u),
            Some("\u{1F600} r4w3".to_string())
        );
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
    fn real_class_mirrors_preserve_the_constructor_slot_in_both_modes() {
        use cratonvm_types::compat::CompatibilityMode;
        for mode in [CompatibilityMode::JdkOnly, CompatibilityMode::Compatible] {
            let mut config = VmConfig::default();
            config.compatibility_mode = mode;
            config.use_synthetic_jdk = false;
            let shared = Arc::new(SharedVm::new(config));
            let class_id = {
                let mut cm = shared.classes.class_manager.write();
                let id = cm.get_loaded_class_id("java/lang/Class").unwrap();
                // The synthetic fixture supplies the allocation shape; marking
                // its origin as real exercises the same policy as a JDK image.
                cm.get_class_mut(id).unwrap().origin =
                    cratonvm_classloading::ClassOrigin::ApplicationClassPath {
                        source: Arc::from("real-mirror-regression"),
                    };
                id
            };
            assert!(!mirror_slot0_overlay(&shared, class_id), "{mode:?}");
            let mirror = get_or_create_class_mirror(&shared, ClassId::new(123));
            let untouched =
                alloc_class_mirror_object(&shared, class_id, shared.mem.heap.num_fields(mirror));
            let initial_slot = shared.mem.heap.get_field(untouched, 0);
            assert_eq!(
                shared.mem.heap.get_field(mirror, 0),
                initial_slot,
                "{mode:?}"
            );
            assert_eq!(
                class_id_from_mirror(&shared, mirror),
                Some(ClassId::new(123))
            );
        }
    }

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

    /// Once a class's block is published, a store must not need the `statics`
    /// lock: a reader holding it (standing in for every other thread's
    /// `putstatic`) must not stall this one. Run on a second thread with a
    /// timeout so a regression fails instead of deadlocking the test binary.
    #[test]
    fn set_static_steady_state_store_takes_no_statics_lock() {
        let shared = test_shared();
        let class_id = ClassId::new(13);
        // First store creates and publishes the block (locked path).
        set_static_shared(&shared, class_id, 1, Value::Long(7));
        if statics_index_disabled() {
            return; // the kill switch routes every store through the lock
        }
        let guard = shared.classes.statics.read();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared_ref: &SharedVm = &shared;
        let stored = std::thread::scope(|s| {
            s.spawn(move || {
                set_static_shared(shared_ref, class_id, 1, Value::Long(8));
                let _ = tx.send(());
            });
            let stored = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
            // Release the lock before the scope joins, whichever way it went.
            drop(guard);
            stored
        });
        assert!(
            stored,
            "a store to a published static waited on the statics lock"
        );
        assert_eq!(get_static_shared(&shared, class_id, 1), Value::Long(8));
        // The map's block is the published one, so the locked reader agrees.
        assert_eq!(
            shared
                .classes
                .statics
                .read()
                .get(&class_id)
                .and_then(|f| f.get(1))
                .copied(),
            Some(Value::Long(8))
        );
        // Past the published length still grows through the locked path.
        set_static_shared(&shared, class_id, 64, Value::Int(3));
        assert_eq!(get_static_shared(&shared, class_id, 64), Value::Int(3));
        assert_eq!(get_static_shared(&shared, class_id, 1), Value::Long(8));
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
                "{what} must still write the mirror's ClassId tag into slot 0 \
                 through the DESCRIPTOR-LESS `set_field`. If this fired \
                 because the call was switched to `set_field_as`, revert it: \
                 the `b'L'` arm of `gc::heap::coerce_field_value_for_slot` \
                 maps `Int(_)` to `Object(None)`, so the tag would be replaced \
                 by null and `mirror_class_id`'s fallback would answer None. \
                 If it fired because the write was deleted, see the measured \
                 `RJdkHello` regression recorded at the write site.",
            );
        }
        assert!(
            !src.contains("set_field_as(mirror, 0"),
            "slot 0 of a class mirror must never be written through the \
             descriptor-aware setter — see the table on this test",
        );
    }

    /// Interpreter round i3 wave 80, lane L5: `CRATONVM_CLASS_MIRROR_LAYOUT_MEMO`
    /// mints the same mirrors as the slow path, and fills its memo only from a
    /// real (not compatibility-stub) `java/lang/Class`. The switch is set in
    /// each VM's config explicitly; no default is pinned.
    #[test]
    fn i80_the_mirror_layout_memo_mints_the_slow_paths_mirrors() {
        let make_vm = |memo: bool| {
            let mut config = VmConfig::default();
            config.class_mirror_layout_memo = memo;
            Arc::new(SharedVm::new(config))
        };
        let shape = |shared: &SharedVm, mirror: ObjectRef| -> Vec<String> {
            (0..shared.mem.heap.num_fields(mirror))
                .map(|i| match shared.mem.heap.get_field(mirror, i) {
                    // Which object differs per VM; that a slot holds one does not.
                    Value::Object(Some(_)) => "obj".to_string(),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        let off = make_vm(false);
        let on = make_vm(true);
        for cid in [1u32, 2, 3, 1] {
            let a = get_or_create_class_mirror(&off, ClassId::new(cid));
            let b = get_or_create_class_mirror(&on, ClassId::new(cid));
            assert_eq!(shape(&*off, a), shape(&*on, b), "mirror of class {cid}");
        }
        let pa = get_or_create_primitive_mirror(&off, "int");
        let pb = get_or_create_primitive_mirror(&on, "int");
        assert_eq!(shape(&*off, pa), shape(&*on, pb), "int.class");
        assert!(
            off.classes.class_mirror_layout.get().is_none(),
            "the switch off never fills the memo"
        );
        if let Some(layout) = on.classes.class_mirror_layout.get() {
            let cm = on.classes.class_manager.read();
            let class = cm
                .get_class(layout.class_class_id)
                .expect("the memo names a loaded class");
            assert_eq!(&*class.name, "java/lang/Class");
            assert!(
                !class.origin.is_compatibility_stub(),
                "a stub `Class` never fills the memo"
            );
            assert_eq!(class.num_total_fields, layout.mirror_field_count);
        }
    }

    /// Interpreter round i6 wave 115, lane L5:
    /// `CRATONVM_CLASS_MIRROR_ONE_LOADER_LOOKUP` mints the mirrors the three
    /// lookups mint (the same slots set, both execution policies).
    ///
    /// Wave 116: a slot is compared by what it holds -- null / zero, or an
    /// object -- not by the `Value` variant it reads back as. Two VMs of one
    /// process need not allocate `java/lang/Class` the same way: the real-JDK
    /// pair (`jdk_only`) read one mirror as a compact object (an unwritten
    /// primitive slot reads `Int(0)`) and the other as a legacy one (an
    /// unwritten slot reads `Object(None)`). That choice is the heap's
    /// (`plan_object_alloc` asks the layout registry) and is made at the
    /// allocation, before any code the switch gates runs; the switch only
    /// writes `classLoader` / `module` (references) and the mirror pin.
    #[test]
    fn i115_one_loader_lookup_mints_the_same_mirrors() {
        for jdk_only in [false, true] {
            let make_vm = |one: bool| {
                let mut config = VmConfig::default();
                config.class_mirror_one_loader_lookup = one;
                if jdk_only {
                    config.compatibility_mode =
                        cratonvm_types::compat::CompatibilityMode::JdkOnly;
                    config.use_synthetic_jdk = false;
                }
                Arc::new(SharedVm::new(config))
            };
            let shape = |shared: &SharedVm, mirror: ObjectRef| -> Vec<String> {
                (0..shared.mem.heap.num_fields(mirror))
                    .map(|i| match shared.mem.heap.get_field(mirror, i) {
                        Value::Object(Some(_)) => "obj".to_string(),
                        Value::Object(None)
                        | Value::Int(0)
                        | Value::Long(0) => "zero".to_string(),
                        Value::Float(f) if f == 0.0 => "zero".to_string(),
                        Value::Double(d) if d == 0.0 => "zero".to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect()
            };
            let off = make_vm(false);
            let on = make_vm(true);
            for cid in [1u32, 2, 3, 1] {
                let a = get_or_create_class_mirror(&off, ClassId::new(cid));
                let b = get_or_create_class_mirror(&on, ClassId::new(cid));
                assert_eq!(
                    shape(&*off, a),
                    shape(&*on, b),
                    "class {cid}, jdk_only {jdk_only}"
                );
            }
        }
    }
}

#[cfg(test)]
mod gcg_g3o_native_mirror_tests {
    use super::{native_mirror_refusal, NativeMirrorRefusal};

    /// gcg g3/o (door 2): inside a native, a refused mirror on Generational
    /// and ZGC always raises; on G1 it raises only once no Free region is
    /// left for the infallible arm (which could then only abort), and the
    /// switch off restores the infallible G1 arm.
    #[test]
    fn gcg_g3o_a_refused_native_mirror_raises_where_the_infallible_arm_would_abort() {
        use NativeMirrorRefusal::{Infallible, Raise};
        // Generational / ZGC: the switch and the region count play no part.
        assert_eq!(native_mirror_refusal(false, 0, false), Raise);
        assert_eq!(native_mirror_refusal(false, 7, true), Raise);
        // G1, switch on: the reserve while a Free region is left, else raise.
        assert_eq!(native_mirror_refusal(true, 3, true), Infallible);
        assert_eq!(native_mirror_refusal(true, 0, true), Raise);
        // G1, switch off: the pre-g3 answer, whatever is left.
        assert_eq!(native_mirror_refusal(true, 0, false), Infallible);
        assert_eq!(native_mirror_refusal(true, 3, false), Infallible);
    }

    /// The switch reads default ON and `=0` turns it off.
    #[test]
    fn gcg_g3o_the_g1_native_mirror_switch_is_default_on() {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_G1_NATIVE_MIRROR_OOME", None)],
            || assert!(super::g1_native_mirror_oome_enabled()),
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_GC_G1_NATIVE_MIRROR_OOME", Some("0"))],
            || assert!(!super::g1_native_mirror_oome_enabled()),
        );
    }
}

#[cfg(test)]
mod i52_l3_mirror_modifiers_tests {
    use super::hotspot_class_modifiers;
    use crate::classloading::{Class, ClassId, ClassLoaderId, ClassState, ClassStore};
    use cratonvm_classloading::{ArrayInfo, InnerClassEntry};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
    use std::sync::Arc;

    fn add(
        store: &mut ClassStore,
        name: &str,
        access_flags: ClassAccessFlags,
        inner_classes: Vec<InnerClassEntry>,
        array_info: Option<ArrayInfo>,
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Bootstrap,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
            access_flags,
            superclass: None,
            interfaces: Vec::new(),
            fields: Vec::new(),
            methods: Vec::new(),
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: Vec::new(),
            signature: None,
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes,
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: cratonvm_classloading::ClassOrigin::default(),
            has_finalizer: false,
            code_source: None,
            array_info,
            init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        })
    }

    fn array_of(component: ClassId, dimension: u8, leaf: &str) -> Option<ArrayInfo> {
        Some(ArrayInfo {
            component_class_id: component,
            array_dimension: dimension,
            leaf_component_name: Arc::from(leaf),
        })
    }

    /// HotSpot's `modifier_flags` (`L3W52ClassMirrorModifiers`): no
    /// `ACC_SUPER`; a member class's InnerClasses flags; an array's
    /// `FINAL | ABSTRACT` plus its leaf element's access bits.
    #[test]
    fn the_mirror_carries_hotspots_modifiers() {
        let mut store = ClassStore::new();
        let object = add(
            &mut store,
            "java/lang/Object",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            Vec::new(),
            None,
        );
        // `private static class P` inside `O`: the class file says
        // package-private `SUPER`, its InnerClasses entry `PRIVATE | STATIC`.
        let nested = add(
            &mut store,
            "O$P",
            ClassAccessFlags::SUPER,
            vec![InnerClassEntry {
                inner_class: "O$P".into(),
                outer_class: "O".into(),
                inner_name: "P".into(),
                access_flags: 0x000a,
            }],
            None,
        );
        let nested_array = add(
            &mut store,
            "[LO$P;",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
            Vec::new(),
            array_of(nested, 1, "O$P"),
        );
        let nested_array2 = add(
            &mut store,
            "[[LO$P;",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
            Vec::new(),
            array_of(nested_array, 2, "O$P"),
        );
        let int_array2 = add(
            &mut store,
            "[[I",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
            Vec::new(),
            None,
        );
        assert_eq!(hotspot_class_modifiers(&store, object, |_| false), 0x0001);
        assert_eq!(hotspot_class_modifiers(&store, nested, |_| false), 0x000a);
        assert_eq!(hotspot_class_modifiers(&store, nested_array, |_| false), 0x0412);
        assert_eq!(hotspot_class_modifiers(&store, nested_array2, |_| false), 0x0412);
        assert_eq!(hotspot_class_modifiers(&store, int_array2, |_| false), 0x0411);
        // Not in the store (a lambda proxy is answered before this): 0.
        assert_eq!(
            hotspot_class_modifiers(&store, ClassId::new(0x8000_0001), |_| false),
            0
        );
        // An array of a lambda proxy (the leaf is not in the store): no
        // access bit, `final synthetic` on HotSpot, so 0x410; an unknown
        // leaf that is not a proxy keeps the old public default.
        let proxy = ClassId::new(0x8000_0002);
        let proxy_array = add(
            &mut store,
            "[LO$$Lambda/0x01;",
            ClassAccessFlags::PUBLIC | ClassAccessFlags::FINAL,
            Vec::new(),
            array_of(proxy, 1, "O$$Lambda/0x01"),
        );
        assert_eq!(
            hotspot_class_modifiers(&store, proxy_array, |id| id == proxy),
            0x0410
        );
        assert_eq!(hotspot_class_modifiers(&store, proxy_array, |_| false), 0x0411);
    }
}

/// gci i12/ul (F2b): the array mirror's `classLoader` element, by identity.
#[cfg(test)]
mod gci_i12_ul_array_element_tests {
    use super::innermost_array_element;
    use crate::classloading::ClassId;

    /// A fake store: `(id, is_array, component)`.
    fn store(
        rows: &[(u32, bool, Option<u32>)],
    ) -> impl Fn(ClassId) -> Option<(bool, Option<ClassId>)> + '_ {
        move |id: ClassId| {
            rows.iter()
                .find(|(i, _, _)| *i == id.as_u32())
                .map(|&(_, is_array, component)| (is_array, component.map(ClassId::new)))
        }
    }

    #[test]
    fn gci_i12_ul_array_element_follows_array_info_not_the_name() {
        // Two loaders' `p/X` (ids 10 and 20) and each one's `[Lp/X;` (11, 21),
        // and `[[Lp/X;` over 21 (22): each array names ITS element.
        let rows = [
            (10, false, None),
            (11, true, Some(10)),
            (20, false, None),
            (21, true, Some(20)),
            (22, true, Some(21)),
        ];
        let class_of = store(&rows);
        assert_eq!(innermost_array_element(ClassId::new(11), &class_of), Some(ClassId::new(10)));
        assert_eq!(innermost_array_element(ClassId::new(21), &class_of), Some(ClassId::new(20)));
        assert_eq!(innermost_array_element(ClassId::new(22), &class_of), Some(ClassId::new(20)));
    }

    #[test]
    fn gci_i12_ul_array_element_declines_what_it_cannot_prove() {
        // `[I` (30) has no component; `[[I` (31) ends on it; a non-array (10);
        // an array whose element left the store (40 over 41); an id the store
        // does not hold (50). Each answers `None` (the caller's name fallback).
        let rows = [
            (10, false, None),
            (30, true, None),
            (31, true, Some(30)),
            (40, true, Some(41)),
        ];
        let class_of = store(&rows);
        assert_eq!(innermost_array_element(ClassId::new(30), &class_of), None);
        assert_eq!(innermost_array_element(ClassId::new(31), &class_of), None);
        assert_eq!(innermost_array_element(ClassId::new(10), &class_of), None);
        assert_eq!(innermost_array_element(ClassId::new(40), &class_of), None);
        assert_eq!(innermost_array_element(ClassId::new(50), &class_of), None);
        // A malformed self-loop terminates.
        let looped = [(60, true, Some(60))];
        assert_eq!(innermost_array_element(ClassId::new(60), &store(&looped)), None);
    }
}

#[cfg(all(test, feature = "zgc"))]
mod zgc_infallible_retirement_tests {
    use super::*;
    use crate::config::{GcAlgorithm, VmConfig};

    fn heap() -> SharedVm {
        SharedVm::new(VmConfig {
            gc_algorithm: GcAlgorithm::Zgc,
            max_heap_size: 16 << 20,
            initial_heap_size: 16 << 20,
            ..VmConfig::default()
        })
    }

    #[test]
    fn a_primitive_mirror_name_refusal_is_fallible_without_an_unwind_scope() {
        let shared = heap();
        assert!(!crate::runtime::native_oom::unwind_ok());
        // The mirror itself fits. Its name cannot fit even in an empty heap.
        // Previously this went through fatal_string_heap_exhausted despite
        // the caller choosing the fallible mirror API.
        let name = "x".repeat(32 << 20);
        assert!(try_get_or_create_primitive_mirror(&shared, &name).is_none());
        assert!(!shared.classes.primitive_mirrors.read().contains_key(&name));
        let mirror = try_get_or_create_primitive_mirror(&shared, "int").expect("recovery");
        assert_eq!(
            Some(mirror),
            try_get_or_create_primitive_mirror(&shared, "I")
        );
        assert!(!crate::runtime::native_oom::unwind_ok());
    }

    #[test]
    fn an_exhausted_heap_refuses_mirrors_and_streams_without_partial_publication() {
        let shared = heap();
        *shared.system_out.write() = None;
        *shared.system_err.write() = None;
        let before = shared.classes.primitive_mirrors.read().len();
        while shared
            .mem
            .heap
            .try_alloc_array_full(ClassId::new(0), ArrayElementType::Byte, 4096)
            .is_some()
        {}
        while shared
            .mem
            .heap
            .try_alloc_object_full(ClassId::new(0), 0)
            .is_some()
        {}
        assert!(try_get_or_create_primitive_mirror(&shared, "never-minted").is_none());
        assert_eq!(before, shared.classes.primitive_mirrors.read().len());
        assert!(matches!(
            shared.try_ensure_system_streams(),
            Err(crate::error::RuntimeError::OutOfMemoryError { .. })
        ));
        assert!(shared.system_out.read().is_none());
        assert!(shared.system_err.read().is_none());
        assert!(shared.try_get_class_lock_object(ClassId::new(0)).is_none());
    }

    #[test]
    fn an_infallible_string_inside_a_native_raises_the_allocation_unwind() {
        let shared = heap();
        let name = "x".repeat(32 << 20);
        let refusal =
            crate::runtime::native_oom::catch_alloc_oom(|| create_java_string(&shared, &name));
        assert!(refusal.is_err());
        assert!(!crate::runtime::native_oom::unwind_ok());
    }
}

#[cfg(test)]
mod zs4nd4_hidden_name_tests {
    use super::{
        create_java_string_uninterned, get_or_create_class_mirror, heap_is_zgc,
        hidden_mirror_name_unpooled, loader_class_name_unpooled, mirror_name_unpooled, SharedVm,
    };
    use crate::config::VmConfig;

    /// The minimal class file `this_name extends java/lang/Object`.
    fn zs4nd4_class_bytes(this_name: &str) -> Vec<u8> {
        let super_name = "java/lang/Object";
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x34];
        b.extend_from_slice(&[0x00, 0x05]);
        b.push(7);
        b.extend_from_slice(&[0x00, 0x02]);
        b.push(1);
        b.extend_from_slice(&u16::try_from(this_name.len()).unwrap().to_be_bytes());
        b.extend_from_slice(this_name.as_bytes());
        b.push(7);
        b.extend_from_slice(&[0x00, 0x04]);
        b.push(1);
        b.extend_from_slice(&u16::try_from(super_name.len()).unwrap().to_be_bytes());
        b.extend_from_slice(super_name.as_bytes());
        b.extend_from_slice(&[0x00, 0x21, 0x00, 0x01, 0x00, 0x03]);
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        b
    }

    /// zs s4/nd4: on ZGC a hidden class's mirror name is NOT pooled (the
    /// pool is a strong root there, so a pooled name outlived its unloaded
    /// class: `Zs2Nd2FfmMints define`), while a plain class's still is (the
    /// positive control: it proves the mirror writes a pooled name at all in
    /// this VM, so the hidden arm's absence is not vacuous). Kill arm
    /// (`CRATONVM_ZGC_HIDDEN_NAME_UNPOOLED=0`, read once per process): the
    /// test then asserts the name IS pooled.
    #[test]
    fn zs4nd4_a_hidden_class_mirror_name_stays_out_of_the_pool_on_zgc() {
        use crate::classloading::ClassLoaderId;
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        if !heap_is_zgc(&shared) {
            eprintln!("skipped: no ZGC heap in this build");
            return;
        }
        let define = |name: &str, hidden: bool| {
            shared
                .classes
                .class_manager_write()
                .define_class_with_options(
                    name,
                    &zs4nd4_class_bytes(if hidden { "zs4nd4/Hid" } else { name }),
                    ClassLoaderId::Application,
                    cratonvm_classloading::DefineClassOptions {
                        override_name: Some(name.to_string()),
                        hidden,
                        skip_verification: true,
                        ..Default::default()
                    },
                )
                .ok()
        };
        let (Some(plain), Some(hidden)) = (
            define("zs4nd4/Plain", false),
            define("zs4nd4/Hid/0x1", true),
        ) else {
            eprintln!("skipped: this unit-test VM cannot define an application class");
            return;
        };
        let pooled = |text: &str| shared.mem.string_pool.read().contains_key(text);
        let plain_mirror = get_or_create_class_mirror(&shared, plain);
        if !pooled("zs4nd4/Plain") {
            eprintln!("skipped: this VM's java/lang/Class has no `name` slot (nothing to pool)");
            return;
        }
        let hidden_mirror = get_or_create_class_mirror(&shared, hidden);
        assert_ne!(plain_mirror, hidden_mirror);
        assert!(
            shared
                .classes
                .class_manager
                .read()
                .get_class(hidden)
                .is_some_and(|c| c.is_hidden()),
            "the fixture defined a hidden class"
        );
        let enabled = hidden_mirror_name_unpooled(&shared);
        eprintln!(
            "zs4nd4 hidden name: unpooled={enabled} pooled_after_mint={}",
            pooled("zs4nd4/Hid/0x1")
        );
        assert_eq!(
            pooled("zs4nd4/Hid/0x1"),
            !enabled,
            "a hidden class's mirror name is pooled exactly when the switch is off"
        );
    }

    /// zt t7/cl: on ZGC the mirror name of a class with a defining-loader row
    /// (a loader object defined it: the classes that unload) is NOT pooled,
    /// while a class without a row still pools it (the positive control: the
    /// mirror writes a pooled name at all in this VM). Kill arm
    /// (`CRATONVM_ZGC_CLASS_NAME_UNPOOLED=0`, read once per process): the
    /// test then asserts the row's name IS pooled.
    #[test]
    fn zt7cl_a_loader_defined_class_mirror_name_stays_out_of_the_pool_on_zgc() {
        use crate::classloading::ClassLoaderId;
        let shared = SharedVm::new(VmConfig {
            gc_algorithm: crate::config::GcAlgorithm::Zgc,
            max_heap_size: 64 << 20,
            ..VmConfig::default()
        });
        if !heap_is_zgc(&shared) {
            eprintln!("skipped: no ZGC heap in this build");
            return;
        }
        let define = |name: &str| {
            shared
                .classes
                .class_manager_write()
                .define_class_with_options(
                    name,
                    &zs4nd4_class_bytes(name),
                    ClassLoaderId::Application,
                    cratonvm_classloading::DefineClassOptions {
                        override_name: Some(name.to_string()),
                        skip_verification: true,
                        ..Default::default()
                    },
                )
                .ok()
        };
        let (Some(plain), Some(loaded)) = (define("zt7cl/Plain"), define("zt7cl/Loaded")) else {
            eprintln!("skipped: this unit-test VM cannot define an application class");
            return;
        };
        // Any heap object stands for the loader: only the row's presence is
        // asked.
        let loader = create_java_string_uninterned(&shared, "zt7cl loader stand-in");
        cratonvm_native_builtins::classloader::register_defining_loader(
            &shared.loader_identity,
            loaded.as_u32(),
            loader,
        );
        let pooled = |text: &str| shared.mem.string_pool.read().contains_key(text);
        let _ = get_or_create_class_mirror(&shared, plain);
        if !pooled("zt7cl/Plain") {
            eprintln!("skipped: this VM's java/lang/Class has no `name` slot (nothing to pool)");
            return;
        }
        assert!(
            !mirror_name_unpooled(&shared, plain, false),
            "no row: the name is pooled as before"
        );
        let _ = get_or_create_class_mirror(&shared, loaded);
        let enabled = loader_class_name_unpooled(&shared);
        eprintln!(
            "zt7cl loader-defined name: unpooled={enabled} pooled_after_mint={}",
            pooled("zt7cl/Loaded")
        );
        assert_eq!(mirror_name_unpooled(&shared, loaded, false), enabled);
        assert_eq!(
            pooled("zt7cl/Loaded"),
            !enabled,
            "a loader-defined class's mirror name is pooled exactly when the switch is off"
        );
    }
}
