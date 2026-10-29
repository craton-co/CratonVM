// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Minimal mock NativeContext for unit testing native method implementations.
//!
//! Provides a simple heap-backed context that supports string creation/reading,
//! field access, and array operations — enough to test most native methods
//! without pulling in the full VM.

use cratonvm_native_api::registry::GpuFutureResult;
use cratonvm_native_api::{
    AnnotationData, FieldMetadata, MethodMetadata, NativeClassAccess, NativeContext,
    NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess, StackTraceEntry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

// FIX(test-isolation): ROOT-CAUSE fix for parallel-test flakiness.
//
// Every `MockNativeContext` used to seed its per-instance identity
// counters (`next_ptr`, `next_class_id`, `next_native_tid`,
// `next_alloc_id`) at the SAME fixed low values (8 / 1 / 1 / 1). When
// tests in different modules run in PARALLEL, the identities they hand
// out collide inside PROCESS-GLOBAL, identity-keyed side maps that the
// production shims maintain (ReentrantLock / Condition / StampedLock
// maps keyed by `ObjectRef.as_ptr()` / `identity_hash_code`, the vertx
// thread-id-keyed side maps, etc.). Single-threaded
// the whole suite passes; only parallel runs flake.
//
// The fix: hand each `MockNativeContext` instance a globally-unique,
// non-overlapping BLOCK of every identity space, by reserving a unique
// monotonically-increasing sequence number per instance and striding
// each counter's base by that sequence. WITHIN-instance increment
// behaviour is unchanged (still += 8 per object, += 1 per class / tid /
// alloc), so per-ctx heap layouts and any field-offset / pointer
// arithmetic in tests stay byte-for-byte identical — only the BASE of
// each instance's block shifts.

/// Monotonic per-instance sequence. Each `MockNativeContext` reserves
/// one value via `fetch_add` and strides its identity bases by it.
static GLOBAL_CTX_SEQ: AtomicU64 = AtomicU64::new(0);

/// The moving mode's address space (JIT round 16 wave 8, lane mh; MH5-1, see
/// [`MockNativeContext::set_moving_heap`]): every relocated object gets a
/// fresh address from here, shared by every context so no address is ever
/// handed out twice. It starts at 2^46, far above the per-context blocks
/// (`PTR_STRIDE` x ~10K contexts is ~10 GiB) and below the 2^48 that the
/// JIT-facing helpers treat as a canonical object address.
static MOVING_NEXT_PTR: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(1 << 46);

// --- Per-instance strides (one block per ctx) ---------------------------
//
// A full test run creates on the order of ~10K MockNativeContexts. All
// strides below must give comfortable headroom past that before any
// space wraps; the analysis is noted at each constant.

/// Pointer-space stride, in bytes. 0x10_0000 == 1,048,576 bytes per
/// ctx; with the in-ctx step of +8 bytes/object that is ~131,072
/// objects per ctx before the next block begins. It is a multiple of 8,
/// so adding it to the base of 8 keeps every minted pointer 8-byte
/// aligned. Headroom (usize == 64-bit on all CI targets):
/// usize::MAX / 0x10_0000 ≈ 1.76e13 ctxs before pointer space wraps —
/// vastly more than the ~10K a run creates.
const PTR_STRIDE: usize = 0x10_0000;

/// Native-thread-id stride. 4096 ids per ctx (in-ctx step +1). Headroom
/// (u64): u64::MAX / 4096 ≈ 4.5e15 ctxs — far beyond ~10K.
const TID_STRIDE: u64 = 4096;

/// Native-allocation-id stride (i64). 1,000,000 ids per ctx (in-ctx
/// step +1). Headroom (i64): i64::MAX / 1_000_000 ≈ 9.2e12 ctxs.
const ALLOC_STRIDE: i64 = 1_000_000;

/// Seeded identity bases for one MockNativeContext instance. Computed
/// once per constructor by `reserve_identity_block()` so all
/// constructor paths share identical seeding logic (no drift).
struct IdentitySeed {
    next_ptr: usize,
    next_class_id: u32,
    next_native_tid: u64,
    next_alloc_id: i64,
}

/// FIX(test-isolation): reserve a globally-unique, non-overlapping block
/// of every identity space for a single MockNativeContext instance.
///
/// `next_class_id` is special-cased: rather than striding a u32 (which
/// would overflow after only ~1M ctxs with any reasonable stride), class
/// ids are pulled from a SINGLE process-global `AtomicU32` so each minted
/// class id is unique across all ctxs with no striding math and no
/// overflow-headroom worry until 4.29e9 *total* class allocations across
/// the whole process — far past a run's needs. The per-instance
/// `next_class_id` is simply seeded to the next free global value and the
/// in-ctx `+= 1` reserves the rest lazily; see `alloc_class_id`.
fn reserve_identity_block() -> IdentitySeed {
    let seq = GLOBAL_CTX_SEQ.fetch_add(1, Ordering::Relaxed);
    IdentitySeed {
        // base 8 keeps the first pointer non-null + 8-byte aligned;
        // PTR_STRIDE is a multiple of 8 so alignment is preserved.
        next_ptr: 8 + (seq as usize).wrapping_mul(PTR_STRIDE),
        // Globally-unique class ids: reserve a fresh block large enough
        // that the in-ctx `+= 1` walk never reaches the next ctx's base.
        // Each ctx reserves CLASS_BLOCK ids up front from the shared
        // global, guaranteeing no two ctxs ever overlap.
        next_class_id: alloc_class_block(),
        next_native_tid: 1 + seq.wrapping_mul(TID_STRIDE),
        next_alloc_id: 1 + (seq as i64).wrapping_mul(ALLOC_STRIDE),
    }
}

/// Per-ctx class-id block size pulled from the shared global counter.
/// 4096 ids per ctx (in-ctx step +1). Reserving from one shared
/// `AtomicU32` (rather than striding `seq`) avoids u32 overflow math:
/// headroom is u32::MAX / 4096 ≈ 1.05e6 ctxs before class-id space
/// wraps — comfortably past the ~10K a run creates.
const CLASS_BLOCK: u32 = 4096;

/// Shared global class-id allocator. Starts at 1 (0 is reserved as the
/// "synthetic / unknown class" id used throughout the mock, e.g. string
/// objects and class mirrors are minted with `ClassId::new(0)`).
static GLOBAL_CLASS_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

/// FIX(test-isolation): reserve a fresh, non-overlapping block of
/// `CLASS_BLOCK` class ids and return its base. Each MockNativeContext
/// seeds `next_class_id` from this; its in-ctx `+= 1` walk stays inside
/// the reserved block under normal test loads.
fn alloc_class_block() -> u32 {
    GLOBAL_CLASS_ID.fetch_add(CLASS_BLOCK, Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
extern "system" {
    fn GetModuleHandleA(lpModuleName: *const i8) -> *mut std::ffi::c_void;
    fn GetProcAddress(
        hModule: *mut std::ffi::c_void,
        lpProcName: *const i8,
    ) -> *mut std::ffi::c_void;
}

#[cfg(target_os = "windows")]
use GetModuleHandleA as winapi_GetModuleHandleA;
#[cfg(target_os = "windows")]
use GetProcAddress as winapi_GetProcAddress;

/// A mock heap object: either a regular object with fields, or an array.
enum HeapEntry {
    Object {
        class_id: ClassId,
        fields: Vec<Value>,
    },
    Array {
        elements: Vec<Value>,
        element_type: ArrayElementType,
    },
}

/// A minimal mock implementation of NativeContext for testing.
///
/// Uses `UnsafeCell` for the heap because the `NativeContext` trait requires
/// `set_field` and `set_array_element` to take `&self` (not `&mut self`).
/// This is safe in single-threaded test code.
/// Map a JDK-style `java.lang.reflect.{Field,Method,Constructor}` field
/// name to a synthetic slot index in the MockNativeContext heap. The
/// production code's `get_field_by_name` / `set_field_by_name` calls get
/// rewritten through this map so tests can still reason about a flat
/// synthetic layout (without a real class hierarchy).
///
/// C5 added the Field mapping (slots 0..6).
/// C6 adds the Method / Constructor mapping. Method keeps the legacy
/// Method slot layout so the `METHOD_NUM_FIELDS_LEGACY_FLOOR = 8`
/// allocation has room for all of them:
///   0 clazz, 1 name, 2 returnType, 3 parameterTypes,
///   4 modifiers, 5 slot, 6 callerSensitive, 7 override.
/// Constructor shares the same prefix (no `returnType`/`name`).
///
/// Resolve `field_name` against the PRODUCTION fabricated model for
/// `class_name` — `ClassManager::synthetic_stub_fields`, the same table that
/// sizes a bytecode `new` of the stub and that the VM resolves names against
/// when a class has no real bytes.
///
/// Without this the mock answers `None` for every modelled class that has no
/// hand-written `mock_*_field_slot` helper, so `set_field_by_name` is a
/// **silent no-op** — and a native that writes a field by name and then again
/// by raw index gets tested only on the raw half. That is how
/// `t19_n1_class_get_protection_domain0_with_code_source_returns_pd` came to
/// depend on a raw `set_field(pd, 0, …)` the real VM never needed: the by-name
/// write beside it did nothing here and everything there.
///
/// Consulted LAST, so every hand-written mapping above still wins.
fn mock_stub_model_field_slot(class_name: Option<&str>, field_name: &str) -> Option<usize> {
    let class_name = class_name?;
    cratonvm_classloading::synthetic_stub_field_model(class_name)
        .iter()
        .filter(|f| !f.is_static())
        .position(|f| &*f.name == field_name)
}

/// Returns `None` for unknown names — callers treat `None` as "not
/// present in the mock class layout" and silently skip the write (or
/// return `Int(0)` for reads).
fn mock_jdk_field_slot(name: &str) -> Option<usize> {
    // Shared synthetic slot namespace for Field / Method / Constructor
    // mirrors. No single test touches both a Field and a Method on the
    // same heap entry, so we can overlap `type` (Field) with
    // `returnType` (Method) without corruption. `parameterTypes`
    // (Method/Constructor) is placed outside the Field range.
    match name {
        "clazz" => Some(0),
        "name" => Some(1),
        "type" => Some(2),
        "returnType" => Some(2),
        "modifiers" => Some(3),
        "slot" => Some(4),
        "override" => Some(6), // AccessibleObject.override / accessible
        // C6 additions (no overlap with Field's 0..=6 range):
        "parameterTypes" => Some(7),
        "callerSensitive" => Some(8),
        // G2 additions: non-null array fields populated by
        // `create_method_object`. Real JDK `Method`/`Executable` declare
        // these; ByteBuddy's clinit relies on them being non-null.
        "exceptionTypes" => Some(9),
        "annotations" => Some(10),
        "parameterAnnotations" => Some(11),
        "annotationDefault" => Some(12),
        // C33: MemberName-specific names (overlap with Field is fine since
        // MemberName tests never touch Field mirrors on the same heap entry).
        "flags" => Some(3),
        _ => None,
    }
}

/// The one family for which `mock_jdk_field_slot` must OUTRANK the fabricated
/// model — and the reason it is a family and not "every class".
///
/// `mock_jdk_field_slot` used to be consulted class-blind and ahead of
/// [`mock_stub_model_field_slot`], so it answered for **any** modelled class
/// that happens to declare one of its fifteen names. Measured over the 333
/// classes `synthetic_stub_field_model` models: it shadowed `name` on
/// twenty-seven of them — `java.lang.Enum`, `java.security.Permission`,
/// `java.util.logging.Logger`, `org.xnio.Xnio`, `org.jboss.modules.Module`, … —
/// every one of which models `name` at slot 0 while the mirror namespace
/// answers 1. A native that writes `name` by name on any of those was tested
/// against a slot the VM does not use, which is this record's defect with the
/// sign flipped: not a silent no-op, a silently *wrong* field.
///
/// The mirrors themselves keep the flat namespace, because **production keeps
/// it too**: `create_method_object` writes the `METHOD_LEGACY_SLOT_*` indices
/// (`native-builtins/src/lang_class.rs`) — `clazz` 0, `name` 1, `returnType`
/// 2, `modifiers` 3, `slot` 4, `override` 6, `parameterTypes` 7 … — and every
/// reader goes through `method_*_field_value_or_legacy`, which falls back to
/// exactly those indices. The mirror is allocated at
/// `METHOD_NUM_FIELDS_LEGACY_FLOOR = 8`, so the model's slot for `modifiers`
/// (10) is past the end of the object and `set_field` discards the write in
/// silence. Reordering these two gives a reader and a writer different answers
/// for one name; that was measured, three tests red, and it is what
/// `the_hand_written_namespace_still_wins_for_the_reflect_mirrors` pins.
///
/// `None` is in the family so a mirror a test built without registering a
/// class name keeps resolving: every other table needs a class name, and
/// `mock_stub_model_field_slot` answers `None` without one anyway.
fn mock_reflect_mirror_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    let is_mirror = matches!(
        class_name,
        None | Some(
            "java/lang/reflect/Field"
                | "java/lang/reflect/Method"
                | "java/lang/reflect/Constructor"
                | "java/lang/reflect/Executable"
                | "java/lang/reflect/AccessibleObject"
                | "java/lang/invoke/MemberName"
        )
    );
    if is_mirror {
        mock_jdk_field_slot(name)
    } else {
        None
    }
}

/// **The** name -> slot chain. All three by-name entry points
/// (`get_field_by_name`, `set_field_by_name`, `resolve_field_index`,
/// `resolve_field_index_by_class_id`) resolve through this one function, and
/// that is the whole point: a reader and a writer that answer one name with
/// two slots is worse than either mapping being "wrong", and the mock had
/// three separate chains that could drift.
///
/// They already had. `java/lang/reflect/Parameter.name` answered slot 0 to a
/// writer going through `set_field_by_name` (which special-cased the class)
/// and slot 1 to a reader coming through `resolve_field_index_by_class_id`
/// (which did not). And `resolve_field_index` consulted **one** table —
/// `mock_undertow_exchange_field_slot` — so it answered `None` for every other
/// class in the tree, including `java/lang/Enum.name` and
/// `java/lang/Throwable.detailMessage`, which production resolves through it.
/// Every branch those guard was unreachable under the mock: the same
/// unfalsifiable-predicate shape §3 of this record fixed one entry point over.
///
/// ORDER IS LOAD-BEARING:
///
///  1. the reflect mirrors' flat namespace (see
///     [`mock_reflect_mirror_field_slot`]);
///  2. `ClassManager::synthetic_stub_fields` — what the VM itself resolves a
///     name against for a class with no real bytes, so the mock agrees with
///     the VM wherever the VM has an answer;
///  3. the hand-written per-class tables, which model *real* library classes
///     the fabricated model does not describe (a real Undertow exchange has
///     ~30 fields; the model is a seven-field stand-in);
///  4. `mock_jdk_field_slot`, class-blind and last — a name the model does not
///     declare is exactly the gap it was written to fill, and filling it can
///     no longer displace a modelled slot.
fn mock_field_slot(class_name: Option<&str>, field_name: &str) -> Option<usize> {
    if class_name == Some("java/lang/reflect/Parameter") {
        return mock_parameter_field_slot(field_name).or_else(|| mock_jdk_field_slot(field_name));
    }
    mock_reflect_mirror_field_slot(class_name, field_name)
        .or_else(|| mock_stub_model_field_slot(class_name, field_name))
        .or_else(|| mock_classloader_field_slot(class_name, field_name))
        .or_else(|| mock_buffer_field_slot(class_name, field_name))
        .or_else(|| mock_charset_field_slot(class_name, field_name))
        .or_else(|| mock_lucene_field_slot(class_name, field_name))
        .or_else(|| mock_concurrent_field_slot(class_name, field_name))
        .or_else(|| mock_infinispan_dcm_field_slot(class_name, field_name))
        .or_else(|| mock_h2_field_slot(class_name, field_name))
        .or_else(|| mock_liquibase_field_slot(class_name, field_name))
        .or_else(|| mock_stamped_lock_field_slot(class_name, field_name))
        .or_else(|| mock_undertow_exchange_field_slot(class_name, field_name))
        .or_else(|| mock_foreign_segment_field_slot(class_name, field_name))
        .or_else(|| mock_enum_field_slot(class_name, field_name))
        .or_else(|| mock_method_type_field_slot(class_name, field_name))
        .or_else(|| mock_jdk_field_slot(field_name))
}

/// `java.lang.Enum` carriers: `name` is slot 0 and `ordinal` slot 1.
///
/// Needed because the class-agnostic `mock_jdk_field_slot` fallback below is
/// the Field/Method/Constructor MIRROR namespace, where `clazz` is 0 and
/// `name` is 1. An enum reaching that fallback resolves `name` to slot 1 —
/// `ordinal`'s slot — so a decoder that asks
/// `resolve_field_index_by_class_id(cid, "name")` reads the ordinal, misses
/// the string, and silently falls back to the ordinal path. That is precisely
/// the name-before-ordinal precedence `http2.rs`'s decoder exists to
/// implement, so the mock could only ever fail a test of it.
///
/// Listed by name rather than detected: the mock has no notion of "is an
/// enum", so a carrier has to be added here when a test starts using it.
fn mock_enum_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    const ENUM_CARRIERS: &[&str] = &[
        "java/net/http/HttpClient$Version",
        "java/net/http/HttpClient$Redirect",
    ];
    if !ENUM_CARRIERS.contains(&class_name?) {
        return None;
    }
    match name {
        "name" => Some(0),
        "ordinal" => Some(1),
        _ => None,
    }
}

/// `java.lang.invoke.MethodType` as `lang_invoke::build_method_type_from_descriptor`
/// lays it out (the JDK 25 field order): `rtype` 0, `ptypes` 1, `form` 2.
/// Without it every `type`-reading MethodHandle check reads "unknown" in tests.
fn mock_method_type_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    if class_name != Some("java/lang/invoke/MethodType") {
        return None;
    }
    match name {
        "rtype" => Some(0),
        "ptypes" => Some(1),
        "form" => Some(2),
        _ => None,
    }
}

/// The real-JDK `jdk.internal.foreign` segment carriers.
///
/// `panama.rs` and `panama_libffi.rs` read these carriers BY NAME on purpose —
/// `segment_address` looks for `min`, `segment_byte_size` for `length`,
/// `heap_segment_view` for `base`/`offset`/`readOnly` — precisely because the
/// three classes do NOT share a slot layout, so reading "field 0" answers a
/// different thing for each. Without the family in this table every one of
/// those lookups missed, each reader fell through to its by-index fallback,
/// and the fallback answered the neighbouring field: `segment_byte_size` read
/// `readOnly` and returned 0, `segment_address` read `length` and returned the
/// byte size as an address, and `heap_segment_view` could not find `base` at
/// all and refused every heap access as unresolvable. That is the exact class
/// of bug these tests were written to pin, so the mock could only ever fail
/// them.
///
/// Layouts are javap's, 25.0.3+9-LTS. `AbstractMemorySegmentImpl` declares
/// `long length`, `boolean readOnly`, `MemorySessionImpl scope`; its two
/// subclasses are SIBLINGS and add different fields after those three:
///
/// * `HeapMemorySegmentImpl` (and its `$Of*` subclasses) — `long offset`,
///   `Object base`. No `min`: a heap segment has no machine address.
/// * `NativeMemorySegmentImpl` — `long min`.
/// * `MappedMemorySegmentImpl` — `long min`, then `Unmapper unmapper`.
///
/// `base` is deliberately absent from the two native rows: it is the field
/// `is_real_heap_segment` uses to recognise a heap carrier by shape, so
/// answering it for a native segment would misclassify it.
fn mock_foreign_segment_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    let class_name = class_name?;
    let abstract_slot = match name {
        "length" => Some(0),
        "readOnly" => Some(1),
        "scope" => Some(2),
        _ => None,
    };
    if class_name.contains("HeapMemorySegmentImpl") {
        return abstract_slot.or(match name {
            "offset" => Some(3),
            "base" => Some(4),
            _ => None,
        });
    }
    match class_name {
        "jdk/internal/foreign/NativeMemorySegmentImpl" => abstract_slot.or(match name {
            "min" => Some(3),
            _ => None,
        }),
        "jdk/internal/foreign/MappedMemorySegmentImpl" => abstract_slot.or(match name {
            "min" => Some(3),
            "unmapper" => Some(4),
            _ => None,
        }),
        _ => None,
    }
}

fn mock_parameter_field_slot(name: &str) -> Option<usize> {
    match name {
        "name" => Some(0),
        "modifiers" => Some(1),
        "executable" => Some(2),
        "index" => Some(3),
        _ => None,
    }
}

fn mock_charset_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("java/nio/charset/Charset"), "name") => Some(0),
        (Some("java/nio/charset/Charset"), "aliases") => Some(1),
        (Some("java/nio/charset/Charset"), "aliasSet") => Some(2),
        _ => None,
    }
}

fn mock_classloader_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (
            Some("com/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet"),
            "fBitCount" | "fByteCount" | "fBits1" | "fBits2" | "fByteArray",
        ) => match name {
            "fBitCount" => Some(0),
            "fByteCount" => Some(1),
            "fBits1" => Some(2),
            "fBits2" => Some(3),
            "fByteArray" => Some(4),
            _ => None,
        },
        (
            Some("jdk/xml/internal/XMLLimitAnalyzer"),
            "values" | "names" | "totalValue" | "caches" | "entityStart" | "entityEnd",
        ) => match name {
            "values" => Some(0),
            "names" => Some(1),
            "totalValue" => Some(2),
            "caches" => Some(3),
            "entityStart" => Some(4),
            "entityEnd" => Some(5),
            _ => None,
        },
        (
            Some("com/sun/org/apache/xerces/internal/impl/dv/xs/XSSimpleTypeDecl"),
            "fValidationDV" | "fFacetsDefined" | "fWhiteSpace",
        ) => match name {
            "fValidationDV" => Some(10),
            "fFacetsDefined" => Some(11),
            "fWhiteSpace" => Some(13),
            _ => None,
        },
        (
            Some("com/sun/org/apache/xerces/internal/impl/xs/traversers/XSDHandler$XSDKey"),
            "systemId" | "referType" | "referNS",
        ) => match name {
            "systemId" => Some(0),
            "referType" => Some(1),
            "referNS" => Some(2),
            _ => None,
        },
        (
            Some("com/sun/org/apache/xerces/internal/impl/XMLEntityScanner"),
            "fCurrentEntity" | "isExternal" | "offset" | "newlines" | "counted" | "fLimitAnalyzer"
            | "fSymbolTable",
        ) => match name {
            "fCurrentEntity" => Some(0),
            "isExternal" => Some(1),
            "offset" => Some(2),
            "newlines" => Some(3),
            "counted" => Some(4),
            "fLimitAnalyzer" => Some(5),
            "fSymbolTable" => Some(6),
            _ => None,
        },
        (
            Some("com/sun/xml/internal/stream/Entity$ScannedEntity"),
            "ch" | "position" | "count" | "columnNumber" | "lineNumber" | "isGE" | "name"
            | "fBufferSize",
        ) => match name {
            "ch" => Some(0),
            "position" => Some(1),
            "count" => Some(2),
            "columnNumber" => Some(3),
            "lineNumber" => Some(4),
            "isGE" => Some(5),
            "name" => Some(6),
            "fBufferSize" => Some(7),
            _ => None,
        },
        (
            Some("com/sun/org/apache/xerces/internal/xni/QName"),
            "prefix" | "localpart" | "rawname" | "uri",
        ) => match name {
            "prefix" => Some(0),
            "localpart" => Some(1),
            "rawname" => Some(2),
            "uri" => Some(3),
            _ => None,
        },
        (Some("com/sun/org/apache/xerces/internal/xni/XMLString"), "ch" | "offset" | "length") => {
            match name {
                "ch" => Some(0),
                "offset" => Some(1),
                "length" => Some(2),
                _ => None,
            }
        }
        (
            Some(
                "com/sun/org/apache/xerces/internal/impl/xs/opti/NodeImpl"
                | "com/sun/org/apache/xerces/internal/impl/xs/opti/ElementImpl"
                | "com/sun/org/apache/xerces/internal/impl/xs/opti/AttrImpl",
            ),
            "prefix" | "localpart" | "rawname" | "uri" | "nodeType" | "hidden" | "value",
        ) => match name {
            "prefix" => Some(0),
            "localpart" => Some(1),
            "rawname" => Some(2),
            "uri" => Some(3),
            "nodeType" => Some(4),
            "hidden" => Some(5),
            "value" => Some(6),
            _ => None,
        },
        (
            Some("com/sun/org/apache/xerces/internal/impl/xpath/regex/RangeToken"),
            "ranges" | "sorted" | "compacted",
        ) => match name {
            "ranges" => Some(0),
            "sorted" => Some(1),
            "compacted" => Some(2),
            _ => None,
        },
        (Some("liquibase/change/AbstractChange$1"), "this$0") => Some(0),
        (
            Some(
                "java/lang/ClassLoader"
                | "java/net/URLClassLoader"
                | "io/quarkus/bootstrap/classloading/QuarkusClassLoader",
            ),
            "ucp",
        ) => Some(0),
        (Some("jdk/internal/loader/URLClassPath" | "sun/misc/URLClassPath"), "path") => Some(0),
        (Some("java/net/URL"), "path") => Some(0),
        (
            Some("javax/security/auth/login/LoginContext"),
            "name" | "subject" | "callbackHandler" | "config",
        ) => match name {
            "name" => Some(0),
            "subject" => Some(1),
            "callbackHandler" => Some(2),
            "config" => Some(3),
            _ => None,
        },
        (
            Some("org/infinispan/manager/DefaultCacheManager"),
            "caches" | "globalComponentRegistry" | "configurationManager" | "defaultCacheName",
        ) => match name {
            "caches" => Some(0),
            "globalComponentRegistry" => Some(1),
            "configurationManager" => Some(2),
            "defaultCacheName" => Some(3),
            _ => None,
        },
        _ => None,
    }
}

/// Real-JDK `java.nio.Buffer`/`ByteBuffer` field layout mirror for
/// `t27_tls.rs::bb_view` tests: mark@0, position@1, limit@2, capacity@3,
/// address@4, hb@5, offset@6. Gated on `java/nio/*ByteBuffer` class names so
/// no other mock heap entries are shadowed.
fn mock_buffer_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match class_name {
        Some(c) if c.starts_with("java/nio/") && c.ends_with("ByteBuffer") => match name {
            "mark" => Some(0),
            "position" => Some(1),
            "limit" => Some(2),
            "capacity" => Some(3),
            "address" => Some(4),
            "hb" => Some(5),
            "offset" => Some(6),
            _ => None,
        },
        _ => None,
    }
}

fn mock_lucene_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("org/apache/lucene/document/Document"), "fields") => Some(0),
        (Some(c), "fieldsData") if c.starts_with("org/apache/lucene/document/") => Some(0),
        _ => None,
    }
}

fn mock_concurrent_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("java/util/concurrent/LinkedBlockingQueue"), "count") => Some(1),
        _ => None,
    }
}

fn mock_infinispan_dcm_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("org/infinispan/manager/DefaultCacheManager"), "globalComponentRegistry") => Some(3),
        (Some("org/infinispan/manager/DefaultCacheManager"), "configurationManager") => Some(4),
        (Some("org/infinispan/manager/DefaultCacheManager"), "defaultCacheName") => Some(5),
        _ => None,
    }
}

fn mock_h2_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("org/h2/table/Column"), "name") => Some(0),
        (Some("org/h2/table/Column"), "table") => Some(1),
        (
            Some(
                "org/h2/engine/DbObject"
                | "org/h2/table/Table"
                | "org/h2/table/TableBase"
                | "org/h2/table/RegularTable",
            ),
            "id",
        ) => Some(0),
        (Some("org/h2/engine/Session" | "org/h2/engine/SessionLocal"), "serialId") => Some(0),
        _ => None,
    }
}

fn mock_liquibase_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("liquibase/change/AbstractChange$1"), "this$0") => Some(0),
        (
            Some("liquibase/change/ColumnConfig"),
            "name"
            | "computed"
            | "type"
            | "value"
            | "valueNumeric"
            | "valueDate"
            | "valueBoolean"
            | "valueBlobFile"
            | "valueClobFile"
            | "encoding"
            | "valueComputed"
            | "valueSequenceNext"
            | "valueSequenceCurrent"
            | "defaultValue"
            | "defaultValueNumeric"
            | "defaultValueDate"
            | "defaultValueBoolean"
            | "defaultValueComputed"
            | "defaultValueSequenceNext"
            | "defaultValueConstraintName"
            | "constraints"
            | "autoIncrement"
            | "generationType"
            | "defaultOnNull"
            | "startWith"
            | "incrementBy"
            | "remarks"
            | "descending"
            | "included"
            | "rawDateValue",
        ) => match name {
            "name" => Some(0),
            "computed" => Some(1),
            "type" => Some(2),
            "value" => Some(3),
            "valueNumeric" => Some(4),
            "valueDate" => Some(5),
            "valueBoolean" => Some(6),
            "valueBlobFile" => Some(7),
            "valueClobFile" => Some(8),
            "encoding" => Some(9),
            "valueComputed" => Some(10),
            "valueSequenceNext" => Some(11),
            "valueSequenceCurrent" => Some(12),
            "defaultValue" => Some(13),
            "defaultValueNumeric" => Some(14),
            "defaultValueDate" => Some(15),
            "defaultValueBoolean" => Some(16),
            "defaultValueComputed" => Some(17),
            "defaultValueSequenceNext" => Some(18),
            "defaultValueConstraintName" => Some(19),
            "constraints" => Some(20),
            "autoIncrement" => Some(21),
            "generationType" => Some(22),
            "defaultOnNull" => Some(23),
            "startWith" => Some(24),
            "incrementBy" => Some(25),
            "remarks" => Some(26),
            "descending" => Some(27),
            "included" => Some(28),
            "rawDateValue" => Some(29),
            _ => None,
        },
        _ => None,
    }
}

fn mock_stamped_lock_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("java/util/concurrent/locks/StampedLock"), "state") => Some(5),
        (Some("java/util/concurrent/locks/StampedLock$ReadLockView"), "this$0") => Some(0),
        (Some("java/util/concurrent/locks/StampedLock$WriteLockView"), "this$0") => Some(0),
        _ => None,
    }
}

fn mock_undertow_exchange_field_slot(class_name: Option<&str>, name: &str) -> Option<usize> {
    match (class_name, name) {
        (Some("io/undertow/server/HttpServerExchange"), "requestHeaders") => Some(2),
        (Some("io/undertow/server/HttpServerExchange"), "responseHeaders") => Some(3),
        (Some("io/undertow/server/HttpServerExchange"), "state") => Some(18),
        (Some("io/undertow/server/HttpServerExchange"), "requestMethod") => Some(19),
        (Some("io/undertow/server/HttpServerExchange"), "requestURI") => Some(21),
        (Some("io/undertow/server/HttpServerExchange"), "sender") => Some(30),
        // An enum that declares its OWN field called `name`, shadowing
        // `java.lang.Enum.name`. The real resolver returns the MOST-DERIVED
        // declaration, so it answers the subclass slot (2), not `Enum`'s own
        // slot 0 — exactly the trap `native_enum_name` must not fall into.
        // Spring Boot's `WebEndpointTest.Infrastructure` is the real instance.
        // See `lang_misc`'s
        // `enum_name_reads_enums_own_slot_not_a_shadowing_subclass_field`.
        (Some("test/ShadowedNameEnum"), "name") => Some(2),
        _ => None,
    }
}

pub(crate) type InvokeVirtualHook =
    fn(&mut MockNativeContext, ObjectRef, &str, &str, &[Value]) -> Option<MethodCallResult>;

pub(crate) type InvokeHook =
    fn(&mut MockNativeContext, &str, &str, &str, &[Value]) -> Option<MethodCallResult>;

pub(crate) struct MockNativeContext {
    pub(crate) reduce_signal_usage: bool,
    heap: UnsafeCell<Vec<HeapEntry>>,
    /// Maps ObjectRef pointer values to heap indices
    // addr-keyed: the mock heap's own address index (test-only); re-keyed by relocate_all.
    ptr_to_index: UnsafeCell<HashMap<usize, usize>>,
    /// class_id -> class_name mapping
    class_names: HashMap<u32, String>,
    /// Opt-in backing for `list_loaded_class_ids`, which defaults to "nothing
    /// is loaded" so a test that does not ask for it is unaffected. Populated
    /// only by `declare_loaded_class`.
    loaded_class_ids: Vec<u32>,
    /// Opt-in backing for `module_for_package`, which defaults to `None`.
    module_by_package: HashMap<String, String>,
    /// Opt-in: packages `is_package_open_unqualified` answers `false` for
    /// (every other package stays open, the default).
    closed_packages: std::collections::HashSet<String>,
    /// class_name -> class_id mapping
    name_to_id: HashMap<String, u32>,
    /// Synthetic lambda class id -> defining host internal name. Tests use
    /// this to exercise reflection contracts that hidden lambda classes share
    /// with their host, without registering the lambda in the class store.
    lambda_proxy_hosts_override: UnsafeCell<HashMap<u32, String>>,
    /// A scripted run-time image (`set_module_image`): class id -> module
    /// name, the boot configuration's modules, and resource name -> class-path
    /// URLs. Empty: no modules, no image, no URLs (the old answers).
    module_image_override: UnsafeCell<MockModuleImage>,
    /// Synthetic lambda class id -> scripted call-site metadata
    /// (`lambda_proxy_serial_metadata`).
    lambda_serial_metadata_override:
        UnsafeCell<HashMap<u32, cratonvm_native_api::LambdaSerialMetadata>>,
    next_class_id: u32,
    next_ptr: usize,
    properties: HashMap<String, String>,
    /// Native memory allocations for Panama tests
    native_allocs: UnsafeCell<HashMap<i64, (*mut u8, std::alloc::Layout)>>,
    next_alloc_id: UnsafeCell<i64>,
    /// Upcall table for Panama tests
    upcall_entries: UnsafeCell<Vec<cratonvm_native_api::ffi::UpcallEntry>>,
    /// invoke_virtual callback result (set by test to control upcall behavior)
    pub(crate) invoke_virtual_result: UnsafeCell<Option<MethodCallResult>>,
    /// Optional method-aware by-name Java-call hook.
    invoke_hook: UnsafeCell<Option<InvokeHook>>,
    /// Optional method-aware virtual-call hook for tests that need an invoked
    /// object to perform side effects before returning.
    pub(crate) invoke_virtual_hook: UnsafeCell<Option<InvokeVirtualHook>>,
    /// Per-native-call roots for tests that simulate a moving GC during
    /// `invoke_virtual` callbacks.
    native_pin_roots: UnsafeCell<Vec<ObjectRef>>,
    /// Open `NativeHandleScope` depths, as marks into `native_pin_roots`.
    /// See this type's `handle_scope_push`/`handle_scope_pop`.
    handle_scope_marks: UnsafeCell<Vec<usize>>,
    /// Process-global-style roots used by natives that need object handles
    /// across callbacks. The mock does not move objects, but implementing the
    /// API keeps tests on the same path as the real VM.
    // addr-keyed: not an address -- global-root handle numbers (test-only).
    global_roots: UnsafeCell<HashMap<usize, ObjectRef>>,
    next_global_root: UnsafeCell<usize>,
    /// JIT round 22 wave 1, lane review (item 1 of
    /// `docs/feature-designs/native-dispatch-architecture.md`): an
    /// OPT-IN per-VM reflect-door cache slot, answered by
    /// `reflect_door_cache_slot` only after
    /// [`MockNativeContext::enable_reflect_door_cache_slot`]. Off by default,
    /// so every test written against "mocks have no cache slot" is unchanged.
    reflect_door_slot: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    reflect_door_slot_enabled: std::cell::Cell<bool>,
    /// NEW-8: tracks class IDs that have been marked as hidden via
    /// `set_class_hidden`. Consulted by the `is_class_hidden` override.
    hidden_classes: UnsafeCell<std::collections::HashSet<u32>>,
    /// NEW-8: records the last name used for a `define_class_from_bytes`
    /// or `define_hidden_class_from_bytes` call so tests can assert on
    /// the mangled hidden-class name directly.
    pub(crate) last_defined_class_name: UnsafeCell<Option<String>>,
    /// C14: per-class overrides for access_flags (so tests can pretend a
    /// class is ACC_ENUM without going through real class loading).
    pub(crate) class_flags_override: UnsafeCell<HashMap<u32, u16>>,
    /// C14: per-class static-field `$VALUES` reference, used by the
    /// `getEnumConstantsShared` native test to verify the non-null
    /// array is returned with the right number of elements.
    pub(crate) enum_values_override: UnsafeCell<HashMap<u32, ObjectRef>>,
    /// Generic static-field backing store for tests that exercise
    /// reflection/Unsafe paths without a full VM static block.
    pub(crate) static_fields_override: UnsafeCell<HashMap<(u32, usize), Value>>,
    /// T19.N2: settable interrupt flag for the mock current thread.
    /// Exposed via `set_interrupted` helpers so tests (notably
    /// `Thread.sleep0` interrupt-handling) can simulate an interrupt
    /// being delivered during a blocking native call.
    pub(crate) interrupted_flag: UnsafeCell<bool>,
    /// What `is_jdk_only` answers (default `false`: compatible mode, as the
    /// `NativeContext` default). See `set_jdk_only`.
    pub(crate) jdk_only_flag: UnsafeCell<bool>,
    /// What `stack_trace_origin_record` answers (default `false`, as the
    /// trait default); the record slot below is offered either way, as the
    /// VM offers it. Interpreter round i3 wave 75, lane L2.
    pub(crate) stack_trace_origin_record_flag: std::cell::Cell<bool>,
    /// What `unsafe_byte_field_offsets` answers (default `false`, as the
    /// trait default). JIT round 20 wave 8c, lane huntDisp.
    pub(crate) unsafe_byte_field_offsets_flag: std::cell::Cell<bool>,
    /// What `jvmti_vm_object_alloc_armed` answers (default `false`, as the
    /// trait default). gcj j1/jt.
    pub(crate) jvmti_vm_object_alloc_armed_flag: std::cell::Cell<bool>,
    stack_trace_origin_records: std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>,
    /// What `stack_trace_fill_name_from_text` answers (default `false`, as
    /// the trait default). Interpreter round i3 wave 76, lane L2.
    pub(crate) stack_trace_fill_name_from_text_flag: std::cell::Cell<bool>,
    /// What `compat_stack_trace_keeps_array` answers (default `false`, as
    /// the trait default). Interpreter round i3 wave 77, lane L2.
    pub(crate) compat_stack_trace_keeps_array_flag: std::cell::Cell<bool>,
    /// What `clv_sub_fast_key` answers (default `false`, as the trait
    /// default). t1 wave 6, lane Y3.
    pub(crate) clv_sub_fast_key_flag: std::cell::Cell<bool>,
    /// T19.N1: per-class CodeSource URL (what `class_code_base` returns).
    /// Populated by tests to simulate a class loaded from a known URL.
    pub(crate) code_base_override: UnsafeCell<HashMap<u32, String>>,
    /// T19.N1: per-class signer certificate blocks (raw PKCS#7 bytes).
    /// Returned by `class_code_source_certs`; empty by default.
    pub(crate) code_source_certs_override: UnsafeCell<HashMap<u32, Vec<Vec<u8>>>>,
    /// What `vm_identity()` reports. Defaults to `0`, which is the value the
    /// `NativeContext` trait's own default impl returns — so every existing
    /// test sees exactly what it saw before.
    ///
    /// It is settable because the per-VM capability index
    /// (`cratonvm_native_api::install_capabilities`) is keyed on this value.
    /// A capability test that installed an `Enforce` policy under the shared
    /// identity `0` would be visible to every *other* test running in
    /// parallel with a mock context, and could refuse their I/O. Giving such a
    /// test its own identity keeps the policy where it belongs.
    vm_identity_override: std::cell::Cell<usize>,
    /// j27 wave 2 (lane VHCOMP): every `note_static_varhandle_target` call,
    /// as `(vh address, class id, field index, writable, desc)`.
    pub(crate) static_vh_notes: std::cell::RefCell<Vec<(usize, u32, usize, bool, Option<u8>)>>,
    blocking_begin_count: usize,
    blocking_end_count: usize,
    /// i66-L1: how many of `blocking_begin_count` were opened through
    /// `begin_vm_wait_region` (a wait HotSpot performs inside the VM).
    vm_wait_begin_count: usize,
    /// JIT round 16 wave 14 (lane ffm3): does `begin_native_region_lazy`
    /// accept (default `false`, the trait default's answer)? And the
    /// `(entered, left, last band)` it recorded.
    pub(crate) lazy_native_accept: bool,
    pub(crate) lazy_native_log: (usize, usize, (usize, usize)),
    /// t1 wave 7, lane G6: the `Reference` registry hooks a native called, in
    /// order: `("retire", addr)` for `mark_reference_manually_enqueued`,
    /// `("cancel", addr)` for `cancel_reference_allocation_note` (which answers
    /// `false`, the trait default). Recorded only; nothing else reads it.
    pub(crate) ref_hook_log: Vec<(&'static str, usize)>,
    /// WP0.2: per-class overrides for `declared_fields`. Empty vec by
    /// default (mock has no class metadata); tests can populate this
    /// to simulate a class with a specific declared field list for
    /// `ObjectStreamClass` / reflection-driven code.
    pub(crate) declared_fields_override: UnsafeCell<HashMap<u32, Vec<FieldMetadata>>>,
    /// MH5-1 (JIT round 16 wave 8, lane mh): the moving mode. See
    /// [`MockNativeContext::set_moving_heap`].
    moving: std::cell::Cell<bool>,
    /// JIT round 17 wave 2 (lane funnel): whether the moving mode also treats
    /// an ALLOCATION as a GC point. `true` for the strict mode
    /// ([`MockNativeContext::set_moving_heap`]), `false` for the VM-rule mode
    /// ([`MockNativeContext::set_moving_heap_vm_rule`]), whose GC points are
    /// exactly the VM's (`NativeHeapAccess`'s "GC points inside a native").
    moving_alloc_gc_point: std::cell::Cell<bool>,
    /// Interpreter round i3 wave 79, lane L1: whether the moving mode also
    /// relocates the heap when a blocking region ends through
    /// `end_blocking_region_refs` (a collection that completed while the
    /// thread was blocked), forwarding the refs the native passed. Off by
    /// default; see [`MockNativeContext::set_region_end_gc_point`].
    region_end_gc_point: std::cell::Cell<bool>,
    /// MH5-1: relocations so far; what `gc_collection_count` answers (`0` for
    /// a context that never moved, as before).
    relocations: std::cell::Cell<u64>,
    /// MH5-1: each entry's FIRST address, by heap index: the identity hash a
    /// moved object keeps (`identity_hash_code`).
    first_addr: UnsafeCell<Vec<usize>>,
    /// MH5-1: every address a relocation retired. A retired address is never
    /// handed out again, so a lookup or a store of one is a reference read
    /// across a GC point -- the defect the mode exists to make fail.
    // addr-keyed: VACATED mock addresses on purpose (test-only); never handed out again.
    retired: UnsafeCell<std::collections::HashSet<usize>>,
    /// Opt-in: make `get_field_by_name` answer an unresolvable name the way
    /// production does — `Value::Object(None)` — instead of this mock's
    /// legacy `Value::Int(0)`. See
    /// [`MockNativeContext::set_absent_field_answers_null`] for why the
    /// default is still the divergent answer and what it costs.
    pub(crate) absent_field_answers_null: std::cell::Cell<bool>,
    /// Next id handed out by `allocate_loader_id`. Starts at
    /// `ClassLoaderId::NATIVE_FIRST_USER_DEFINED` like the real VM's counter.
    pub(crate) next_loader_id: u32,
    /// Per-class overrides for `inner_classes` (the `InnerClasses` attribute
    /// entries, as `(inner_class_name, outer_class_name, inner_name,
    /// access_flags)`). Empty vec by default (mock has no class metadata);
    /// tests simulating a real JVM member/local/anonymous class (e.g.
    /// `java/util/Map$Entry`) populate this so `getSimpleName()`/
    /// `getSimpleBinaryName0()` can tell it apart from a top-level class
    /// whose literal binary name merely contains `$` (dynamically-generated
    /// proxies).
    pub(crate) inner_classes_override: UnsafeCell<HashMap<u32, Vec<(String, String, String, u16)>>>,
    /// JIT round 21 wave 22, lane hidden-resid: per-class `EnclosingMethod`
    /// (class, method, descriptor) answers for `enclosing_method`.
    pub(crate) enclosing_method_override: UnsafeCell<HashMap<u32, (String, String, String)>>,
    /// WP0.2: per-class overrides for `declared_methods`.
    pub(crate) declared_methods_override: UnsafeCell<HashMap<u32, Vec<MethodMetadata>>>,
    /// WP0.2: per-class super-class override (consulted by
    /// `superclass_of` / `is_subclass` walks). Default: no super
    /// (i.e. every class looks like java/lang/Object).
    pub(crate) superclass_override: UnsafeCell<HashMap<u32, ClassId>>,
    /// G2: per-class is-interface override. Tests that exercise the
    /// `Class.getSuperclass()`-returns-null-for-interfaces semantics
    /// (or `getMethods` interface skip) can populate this. Default
    /// `false` for any class id not in the map.
    pub(crate) is_interface_override: UnsafeCell<HashMap<u32, bool>>,
    /// WP0.2: per-class interface-list override (for `class_interfaces`
    /// and `is_subclass` against an interface parent).
    pub(crate) interfaces_override: UnsafeCell<HashMap<u32, Vec<ClassId>>>,
    /// WP0.2: process-wide ObjectStreamClass cache (ClassId.as_u32 →
    /// cached descriptor ObjectRef). Drives `osc_cache_get` /
    /// `osc_cache_put` so tests can assert identity.
    pub(crate) osc_cache_map: UnsafeCell<HashMap<u32, ObjectRef>>,
    /// JIT round 16 (lane mh, MH3-2): this mock VM's bound `MethodType`
    /// table, the same type the VM keeps on its class realm. Drives
    /// `bound_method_type_get` / `_put`.
    pub(crate) bound_method_types: cratonvm_native_api::method_type_table::MethodTypeTable,
    /// T19.H10: per-resource-name byte payload returned by `find_resource`.
    /// Empty by default (matches the fresh-VM semantics: no classpath jar
    /// has been mapped). Tests populate via `set_resource(name, bytes)` to
    /// simulate a classpath that contains the named resource.
    pub(crate) resources_override: UnsafeCell<HashMap<String, Vec<u8>>>,
    /// JPMS service-provider declarations keyed by slash-format service name.
    /// Tests populate this to exercise `ServiceLoader`'s module `provides`
    /// path without standing up a full ClassManager/module registry.
    pub(crate) module_providers_override: UnsafeCell<HashMap<String, Vec<String>>>,
    /// T19_K2: tracks each `register_native_thread` call so tests can
    /// assert that Vert.x / XNIO event loops correctly route through the
    /// new VM-tracking entry point. Each entry is `(name, daemon, alive)`.
    /// `unregister_native_thread` flips `alive` to false but leaves the
    /// entry in place for assertions.
    pub(crate) registered_native_threads: UnsafeCell<Vec<(String, bool, bool)>>,
    /// gcj j3/rt3 residual 5: the ids whose [`MockNativeThreadBlocker`]
    /// retired them -- written by the dying loop thread itself, as the VM's
    /// blocker marks its entry dead (the process-wide dead queues are gone).
    /// [`Self::registered_native_threads`] reads an id here as not alive.
    pub(crate) retired_native_threads: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    /// T19_K2: counter feeding ThreadId values handed back from
    /// `register_native_thread`. Starts at 1 (0 is reserved as "no
    /// registration" / mock not configured).
    ///
    /// FIX(test-isolation): the BASE of this counter is now shifted per
    /// instance (see `reserve_identity_block`) so ids are globally
    /// unique across parallel ctxs. The in-ctx step is still +1.
    pub(crate) next_native_tid: UnsafeCell<u64>,
    /// FIX(test-isolation): the seeded base of `next_native_tid` for this
    /// instance. The k-th `register_native_thread` call hands out
    /// `native_tid_base + k` and pushes to slot `k` of
    /// `registered_native_threads`, so the side-map / vec index for a
    /// given `thread_id` is `thread_id - native_tid_base` (NOT
    /// `thread_id - 1`, which only held when the base was the fixed 1).
    /// Storing the base keeps that index math correct after the base
    /// shifts, preserving the exact within-instance behaviour.
    pub(crate) native_tid_base: u64,
    /// T19_K4: tracks `set_native_thread_java_obj` calls. Maps the
    /// 1-based ThreadId to the raw `ObjectRef.as_ptr() as usize` of
    /// the attached `java.lang.Thread` mirror. Tests assert that
    /// Vert.x / XNIO event-loop spawns route a non-null mirror
    /// through this entry point.
    pub(crate) native_thread_java_objs: UnsafeCell<HashMap<u64, usize>>,
    /// T19_H15: tracks every path passed to `register_dynamic_classpath`.
    /// Tests assert that the JBoss module loader registers the full
    /// transitive dependency closure on the shared classpath when a
    /// module is loaded.  Insertion order is preserved.
    pub(crate) registered_classpath: UnsafeCell<Vec<String>>,
    /// WP8.11.5: per-class `nest_host` override, returned by the
    /// `nest_host_name` trait method. Empty by default (every class is
    /// its own nest host); tests populate this via
    /// `set_nest_host_override(child_id, "OuterClass")` to simulate a
    /// lookup class that is itself a nestmate of an outer.
    pub(crate) nest_host_override: UnsafeCell<HashMap<u32, String>>,
    /// getNestMembers0 regression: per-class `nest_members` override,
    /// returned by the `nest_member_names` trait method. Empty by default
    /// (no class carries a `NestMembers` attribute); tests populate this
    /// via `set_nest_members_override(host_id, vec!["Member1", "Member2"])`
    /// to simulate a nest host's `NestMembers` attribute contents.
    pub(crate) nest_members_override: UnsafeCell<HashMap<u32, Vec<String>>>,
    /// WP8.11.5: snapshot of the most recent `define_class_full` call's
    /// `DefineClassFull` options. Set by the override below;
    /// `last_define_full_opts()` reads it. Used by NESTMATE-propagation
    /// tests to assert exactly which `nest_host_class_name` reached the
    /// backend.
    pub(crate) last_define_full_opts: UnsafeCell<Option<cratonvm_native_api::DefineClassFull>>,
    /// CGLIB-η: snapshot of the most recent `define_class_full` call's
    /// `loader_id` argument, so loader-inheritance tests can assert the
    /// new class lands in the lookup class's loader namespace rather
    /// than always in the Application loader.
    pub(crate) last_define_full_loader: UnsafeCell<Option<u32>>,
    /// CGLIB-η: per-class `loader_id` override, returned by the
    /// `loader_id_of_class` trait method. Defaults to 2 (Application);
    /// tests populate this via `set_loader_id_override(cid, raw)` where
    /// `raw` follows the `loader_id_of_class` encoding (0=Bootstrap,
    /// 1=Extension, 2=Application, N>=3=UserDefined(N)).
    pub(crate) loader_id_override: UnsafeCell<HashMap<u32, i32>>,
    /// Test override for the current Java frame class ids, innermost first.
    /// Production uses the live interpreter stack; native unit tests can set a
    /// precise caller chain without standing up the VM.
    pub(crate) frame_class_ids_override: UnsafeCell<Vec<ClassId>>,
    /// Per-method return TYPE_USE annotation overrides for tests.
    pub(crate) method_return_type_annotations_override:
        UnsafeCell<HashMap<(u32, String, String), Vec<AnnotationData>>>,
    /// Per-method-parameter TYPE_USE annotation overrides for tests.
    pub(crate) method_parameter_type_annotations_override:
        UnsafeCell<HashMap<(u32, String, String), Vec<Vec<AnnotationData>>>>,
    /// Per-field TYPE_USE annotation overrides for tests.
    pub(crate) field_type_annotations_override:
        UnsafeCell<HashMap<(u32, String), Vec<AnnotationData>>>,
    /// 2026-07-11: per-handle canned answer for `gpu_future_take_result`.
    /// Empty by default, so the mock uses the trait's default (`None`)
    /// and `craton_gpu.rs`'s handlers fall through to the local
    /// synthetic `FutureState` map — same shape as `gpu_future_status`
    /// being unscripted. Tests that want to exercise the *real-registry*
    /// branch of `builtin_future_get_result` populate this via
    /// `set_gpu_future_take_result(handle, result)`.
    pub(crate) gpu_future_take_result_override: UnsafeCell<HashMap<u64, GpuFutureResult>>,
    /// GpuStream affinity: canned answer for `gpu_stream_create`.
    /// `None` by default (the trait's default, "no device"). Tests
    /// that want to verify `resolve_or_create_default_stream`'s
    /// caching behavior set this via `set_gpu_stream_create_result`.
    pub(crate) gpu_stream_create_override: UnsafeCell<Option<u64>>,
    /// GpuStream affinity: number of times `gpu_stream_create` has
    /// been called. Read via `gpu_stream_create_call_count()` — the
    /// caching test's whole point is that this stays `1` across
    /// repeated calls for the same executor handle.
    pub(crate) gpu_stream_create_calls: UnsafeCell<u32>,
    /// GpuStream affinity: every handle passed to `gpu_stream_release`,
    /// in call order. Read via `gpu_stream_release_calls()`.
    pub(crate) gpu_stream_release_calls: UnsafeCell<Vec<u64>>,
    /// Every handle passed to `gpu_release_submission`, in call order.
    /// Read via `gpu_release_submission_calls()`.
    ///
    /// The submission drain is a SECOND table keyed by the same handle as
    /// this crate's own `state::futures`, and the shim is the only thing
    /// that can bridge the two — see
    /// `release_future_forwards_the_drain_to_the_registry`.
    pub(crate) gpu_release_submission_calls: UnsafeCell<Vec<u64>>,
    /// gen r4w2/obs: what `gc_collectors` / `gc_memory_pools` answer. Empty by
    /// default (the trait's own default: no per-bean model, so the JMX natives
    /// keep their historical answer); set with `set_gc_beans`.
    gc_beans_override: std::cell::RefCell<(
        Vec<cratonvm_native_api::GcCollectorInfo>,
        Vec<cratonvm_native_api::GcMemoryPoolInfo>,
    )>,
    /// gen r4w3/obs2: every `set_gc_notification_enabled` call, in order, so a
    /// test can see what `GarbageCollectorExtImpl.setNotificationEnabled`
    /// reported to the VM.
    pub(crate) gc_notification_enabled_calls: std::cell::RefCell<Vec<(String, bool)>>,
    /// zs s3/ob2: what `non_heap_memory_pools` answers (empty by default, the
    /// trait's own default); set with `set_non_heap_pools`.
    non_heap_pools_override: std::cell::RefCell<Vec<cratonvm_native_api::GcMemoryPoolInfo>>,
    /// zs s4/rv3: how many times `non_heap_memory_pools` was asked.
    pub(crate) non_heap_pools_calls: std::cell::Cell<u32>,
    /// gc-common w4-d: how many times `run_finalization` was called, so a test
    /// can see `Runtime.runFinalization()` reach the VM's hook.
    pub(crate) run_finalization_calls: std::cell::Cell<u32>,
    /// gc-common w5-d: what `wait_for_reference_processing` answers, and how
    /// many times it was asked, so a test can see
    /// `JavaLangRefAccess.waitForReferenceProcessing()` reach the VM's hook.
    pub(crate) ref_processing_answer: std::cell::Cell<bool>,
    pub(crate) wait_for_reference_processing_calls: std::cell::Cell<u32>,
    /// gc-common w5-d: every object handed to `register_if_finalizable`.
    pub(crate) register_if_finalizable_calls: std::cell::RefCell<Vec<ObjectRef>>,
    /// Every pre-constructor finalizer withdrawal requested by a native door.
    pub(crate) withdraw_finalizable_before_constructor_calls:
        std::cell::RefCell<Vec<(ObjectRef, u64)>>,
    /// gc-common w6-d: what `reference_queue_wake_ups_delivered` answers.
    pub(crate) queue_wake_ups_delivered: std::cell::Cell<bool>,
    /// gc-common w6-d: every `monitor_wait` call, `(object, timeout_ms)`, so a
    /// test can see which monitor a native slept on and for how long.
    pub(crate) monitor_wait_calls: std::cell::RefCell<Vec<(ObjectRef, Option<u64>)>>,
    /// i65-L1: the kind of every `monitor_wait_as` call, in order (each is
    /// also recorded in `monitor_wait_calls`), so a test can see what JDK code
    /// a served stand-in's wait says it stands for.
    pub(crate) monitor_wait_kinds: std::cell::RefCell<Vec<cratonvm_native_api::BlockKind>>,
    /// i68-L1: every `begin_blocking_region_as_on` call, `(object, kind)`, in
    /// order (each also opens one ordinary region), so a test can see which
    /// object a stand-in publishes for its wait.
    pub(crate) region_wait_objects:
        std::cell::RefCell<Vec<(ObjectRef, cratonvm_native_api::BlockKind)>>,
    /// i68-L1: what `stand_in_wait_objects` answers (`true`, the VM's default).
    pub(crate) stand_in_wait_objects_answer: std::cell::Cell<bool>,
    /// i25-L7: monitors entered minus monitors exited, so a test can see a
    /// native leave a monitor it entered.
    pub(crate) monitor_depth: std::cell::Cell<i64>,
    /// i25-L7: when set, `monitor_wait` answers the interrupt a real wait
    /// raises (`InterruptedException`) instead of `Ok`.
    pub(crate) monitor_wait_interrupts: std::cell::Cell<bool>,
    /// JIT round 16 wave 13 (lane wmte): every
    /// `set_mh_wmte_raise_hint` call, in order, so a test can see a door
    /// set the hint before its exception and withdraw it after.
    pub(crate) mh_wmte_raise_hints:
        std::cell::RefCell<Vec<Option<cratonvm_native_api::MhWmteRaise>>>,
    /// i7-L2: the component each `new_ref_array` was given, keyed by the
    /// array's address, so a test can see a native type its `T[]` result
    /// (`ref_array_component`). `class_id_of_object` still answers 0 for
    /// arrays, as before.
    // addr-keyed: the mock heap's arrays (test-only); re-keyed by relocate_all.
    ref_array_components: std::cell::RefCell<HashMap<usize, ClassId>>,
    /// i17-L4: every name `ensure_class_initialized` was asked for, in order,
    /// so a test can see whether a native initialized a class.
    class_init_log: std::cell::RefCell<Vec<String>>,
    /// i17-L4: names whose `ensure_class_initialized` fails (as a `<clinit>`
    /// that threw would), after the class has an id.
    failing_class_inits: std::cell::RefCell<Vec<String>>,
    /// Round 13 wave 13 (lane ffm7; proposal FFM6-1): when set, what
    /// `current_thread_object` answers on every call, so a CONFINED FFM
    /// session made through this context belongs to "this thread". `None` (the
    /// default) keeps minting a fresh object per call, which several tests
    /// rely on (`phases_early`'s inherited-`ThreadLocal` tests predict the
    /// next minted address). See `stable_current_thread`.
    stable_thread_object: std::cell::Cell<Option<ObjectRef>>,
}

impl MockNativeContext {
    /// Give this mock a per-VM reflect-door cache slot (JIT round 22 wave 1,
    /// lane review): `reflect_door_cache_slot` answers it from now on.
    #[allow(dead_code)]
    pub(crate) fn enable_reflect_door_cache_slot(&self) {
        self.reflect_door_slot_enabled.set(true);
    }

    /// i7-L2: the component `new_ref_array` stamped on `arr`; `None` for an
    /// array made by `new_array` (the untyped `Object[]` path) or a non-array.
    #[allow(dead_code)]
    pub(crate) fn ref_array_component(&self, arr: ObjectRef) -> Option<ClassId> {
        self.ref_array_components
            .borrow()
            .get(&(arr.as_ptr() as usize))
            .copied()
    }

    /// i17-L4: the names `ensure_class_initialized` has been asked for.
    #[allow(dead_code)]
    pub(crate) fn class_init_requests(&self) -> Vec<String> {
        self.class_init_log.borrow().clone()
    }

    /// i17-L4: make every later `ensure_class_initialized(name)` fail with
    /// `NoClassDefFoundError`, as an initialization error would.
    #[allow(dead_code)]
    pub(crate) fn fail_class_init(&self, name: &str) {
        self.failing_class_inits.borrow_mut().push(name.to_string());
    }

    /// gen r4w2/obs: make this context describe per-generation JMX beans, as
    /// the VM does for the generational backend.
    #[allow(dead_code)]
    pub(crate) fn set_gc_beans(
        &self,
        collectors: Vec<cratonvm_native_api::GcCollectorInfo>,
        pools: Vec<cratonvm_native_api::GcMemoryPoolInfo>,
    ) {
        *self.gc_beans_override.borrow_mut() = (collectors, pools);
    }

    /// zs s3/ob2: make this context describe non-heap pools (the VM's ZGC
    /// `Metaspace`).
    #[allow(dead_code)]
    pub(crate) fn set_non_heap_pools(&self, pools: Vec<cratonvm_native_api::GcMemoryPoolInfo>) {
        *self.non_heap_pools_override.borrow_mut() = pools;
    }

    pub(crate) fn new() -> Self {
        // FIX(test-isolation): seed every identity counter from a
        // globally-unique, non-overlapping block so two ctxs in parallel
        // (in any module) never collide in process-global identity-keyed
        // side maps. Within-ctx increment behaviour is unchanged — only
        // the per-instance BASE shifts. This is the single seeding site;
        // any future constructor MUST route through `reserve_identity_block`
        // to avoid drift.
        let seed = reserve_identity_block();
        Self {
            reduce_signal_usage: false,
            heap: UnsafeCell::new(Vec::new()),
            ptr_to_index: UnsafeCell::new(HashMap::new()),
            class_names: HashMap::new(),
            loaded_class_ids: Vec::new(),
            module_by_package: HashMap::new(),
            closed_packages: std::collections::HashSet::new(),
            name_to_id: HashMap::new(),
            lambda_proxy_hosts_override: UnsafeCell::new(HashMap::new()),
            module_image_override: UnsafeCell::new(MockModuleImage::default()),
            lambda_serial_metadata_override: UnsafeCell::new(HashMap::new()),
            next_class_id: seed.next_class_id,
            // Base is 8 + seq*PTR_STRIDE, so the first pointer stays
            // 8-byte aligned and non-null; subsequent objects still += 8.
            next_ptr: seed.next_ptr,
            properties: HashMap::new(),
            native_allocs: UnsafeCell::new(HashMap::new()),
            next_alloc_id: UnsafeCell::new(seed.next_alloc_id),
            upcall_entries: UnsafeCell::new(Vec::new()),
            invoke_virtual_result: UnsafeCell::new(None),
            invoke_hook: UnsafeCell::new(None),
            invoke_virtual_hook: UnsafeCell::new(None),
            native_pin_roots: UnsafeCell::new(Vec::new()),
            handle_scope_marks: UnsafeCell::new(Vec::new()),
            global_roots: UnsafeCell::new(HashMap::new()),
            next_global_root: UnsafeCell::new(1),
            reflect_door_slot: std::sync::OnceLock::new(),
            reflect_door_slot_enabled: std::cell::Cell::new(false),
            hidden_classes: UnsafeCell::new(std::collections::HashSet::new()),
            last_defined_class_name: UnsafeCell::new(None),
            class_flags_override: UnsafeCell::new(HashMap::new()),
            enum_values_override: UnsafeCell::new(HashMap::new()),
            static_fields_override: UnsafeCell::new(HashMap::new()),
            interrupted_flag: UnsafeCell::new(false),
            jdk_only_flag: UnsafeCell::new(false),
            stack_trace_origin_record_flag: std::cell::Cell::new(false),
            unsafe_byte_field_offsets_flag: std::cell::Cell::new(false),
            jvmti_vm_object_alloc_armed_flag: std::cell::Cell::new(false),
            stack_trace_origin_records: std::sync::OnceLock::new(),
            stack_trace_fill_name_from_text_flag: std::cell::Cell::new(false),
            compat_stack_trace_keeps_array_flag: std::cell::Cell::new(false),
            clv_sub_fast_key_flag: std::cell::Cell::new(false),
            code_base_override: UnsafeCell::new(HashMap::new()),
            code_source_certs_override: UnsafeCell::new(HashMap::new()),
            vm_identity_override: std::cell::Cell::new(0),
            static_vh_notes: std::cell::RefCell::new(Vec::new()),
            blocking_begin_count: 0,
            blocking_end_count: 0,
            vm_wait_begin_count: 0,
            lazy_native_accept: false,
            lazy_native_log: (0, 0, (0, 0)),
            ref_hook_log: Vec::new(),
            declared_fields_override: UnsafeCell::new(HashMap::new()),
            moving: std::cell::Cell::new(false),
            moving_alloc_gc_point: std::cell::Cell::new(true),
            region_end_gc_point: std::cell::Cell::new(false),
            relocations: std::cell::Cell::new(0),
            first_addr: UnsafeCell::new(Vec::new()),
            retired: UnsafeCell::new(std::collections::HashSet::new()),
            absent_field_answers_null: std::cell::Cell::new(false),
            next_loader_id: cratonvm_types::ClassLoaderId::NATIVE_FIRST_USER_DEFINED,
            inner_classes_override: UnsafeCell::new(HashMap::new()),
            enclosing_method_override: UnsafeCell::new(HashMap::new()),
            declared_methods_override: UnsafeCell::new(HashMap::new()),
            superclass_override: UnsafeCell::new(HashMap::new()),
            is_interface_override: UnsafeCell::new(HashMap::new()),
            interfaces_override: UnsafeCell::new(HashMap::new()),
            osc_cache_map: UnsafeCell::new(HashMap::new()),
            bound_method_types: cratonvm_native_api::method_type_table::MethodTypeTable::new(),
            resources_override: UnsafeCell::new(HashMap::new()),
            module_providers_override: UnsafeCell::new(HashMap::new()),
            registered_native_threads: UnsafeCell::new(Vec::new()),
            retired_native_threads: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            next_native_tid: UnsafeCell::new(seed.next_native_tid),
            native_tid_base: seed.next_native_tid,
            native_thread_java_objs: UnsafeCell::new(HashMap::new()),
            registered_classpath: UnsafeCell::new(Vec::new()),
            nest_host_override: UnsafeCell::new(HashMap::new()),
            nest_members_override: UnsafeCell::new(HashMap::new()),
            last_define_full_opts: UnsafeCell::new(None),
            last_define_full_loader: UnsafeCell::new(None),
            loader_id_override: UnsafeCell::new(HashMap::new()),
            frame_class_ids_override: UnsafeCell::new(Vec::new()),
            method_return_type_annotations_override: UnsafeCell::new(HashMap::new()),
            method_parameter_type_annotations_override: UnsafeCell::new(HashMap::new()),
            field_type_annotations_override: UnsafeCell::new(HashMap::new()),
            gpu_future_take_result_override: UnsafeCell::new(HashMap::new()),
            gpu_stream_create_override: UnsafeCell::new(None),
            gpu_stream_create_calls: UnsafeCell::new(0),
            gpu_stream_release_calls: UnsafeCell::new(Vec::new()),
            gpu_release_submission_calls: UnsafeCell::new(Vec::new()),
            gc_beans_override: std::cell::RefCell::new((Vec::new(), Vec::new())),
            gc_notification_enabled_calls: std::cell::RefCell::new(Vec::new()),
            non_heap_pools_override: std::cell::RefCell::new(Vec::new()),
            non_heap_pools_calls: std::cell::Cell::new(0),
            run_finalization_calls: std::cell::Cell::new(0),
            ref_processing_answer: std::cell::Cell::new(false),
            wait_for_reference_processing_calls: std::cell::Cell::new(0),
            register_if_finalizable_calls: std::cell::RefCell::new(Vec::new()),
            withdraw_finalizable_before_constructor_calls: std::cell::RefCell::new(Vec::new()),
            queue_wake_ups_delivered: std::cell::Cell::new(false),
            monitor_wait_calls: std::cell::RefCell::new(Vec::new()),
            monitor_wait_kinds: std::cell::RefCell::new(Vec::new()),
            region_wait_objects: std::cell::RefCell::new(Vec::new()),
            stand_in_wait_objects_answer: std::cell::Cell::new(true),
            monitor_depth: std::cell::Cell::new(0),
            monitor_wait_interrupts: std::cell::Cell::new(false),
            mh_wmte_raise_hints: std::cell::RefCell::new(Vec::new()),
            ref_array_components: std::cell::RefCell::new(HashMap::new()),
            class_init_log: std::cell::RefCell::new(Vec::new()),
            failing_class_inits: std::cell::RefCell::new(Vec::new()),
            stable_thread_object: std::cell::Cell::new(None),
        }
    }

    /// Round 13 wave 13 (lane ffm7; FFM6-1): make `current_thread_object`
    /// answer ONE object from now on (minted here on the first call), so
    /// confined sessions made through this context pass their owner checks.
    /// Answers that object. The mock does not move objects, so it stays valid.
    #[allow(dead_code)]
    pub(crate) fn stable_current_thread(&mut self) -> ObjectRef {
        if let Some(thread) = self.stable_thread_object.get() {
            return thread;
        }
        let thread = self.alloc_object(ClassId::new(0), 2);
        self.stable_thread_object.set(Some(thread));
        thread
    }

    /// Give this context its own VM identity, so a per-VM policy installed
    /// for it (capabilities, and anything else keyed on `vm_identity()`) is
    /// invisible to the other mock contexts in the suite, which all report
    /// the default `0`. See `vm_identity_override`.
    #[allow(dead_code)]
    pub(crate) fn set_vm_identity(&self, id: usize) {
        self.vm_identity_override.set(id);
    }

    /// CGLIB-η: declare that `class_id` belongs to the given raw loader
    /// (using the `loader_id_of_class` encoding: 0=Bootstrap,
    /// 1=Extension, 2=Application, N>=3=UserDefined(N)).
    #[allow(dead_code)]
    pub(crate) fn set_loader_id_override(&self, class_id: ClassId, raw_loader: i32) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.loader_id_override.get()).insert(class_id.as_u32(), raw_loader);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_frame_class_ids(&self, frame_class_ids: Vec<ClassId>) {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.frame_class_ids_override.get() = frame_class_ids;
        }
    }

    /// Script a run-time image: `class_modules` (class id, module name), the
    /// boot configuration's `boot_modules`, and the class-path `urls`
    /// (resource name, URL; a name may repeat) `find_all_resource_urls`
    /// answers. Interpreter round i5 wave 105, lane L3 (`Class.getResource`'s
    /// named-module arm).
    #[allow(dead_code)]
    pub(crate) fn set_module_image(
        &self,
        class_modules: &[(ClassId, &str)],
        boot_modules: &[&str],
        urls: &[(&str, &str)],
    ) {
        // SAFETY: single-threaded test code.
        let image = unsafe { &mut *self.module_image_override.get() };
        image.present = true;
        image.class_modules = class_modules
            .iter()
            .map(|(cid, m)| (cid.as_u32(), m.to_string()))
            .collect();
        image.boot_modules = boot_modules.iter().map(|m| m.to_string()).collect();
        image.urls.clear();
        for (name, url) in urls {
            image
                .urls
                .entry(name.to_string())
                .or_default()
                .push(url.to_string());
        }
    }

    /// Script the defining host reported for a synthetic lambda class.
    #[allow(dead_code)]
    pub(crate) fn set_lambda_proxy_host(&self, class_id: ClassId, host: &str) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_proxy_hosts_override.get()).insert(class_id.as_u32(), host.to_string());
        }
    }

    /// Script the call-site metadata reported for a synthetic lambda class.
    #[allow(dead_code)]
    pub(crate) fn set_lambda_serial_metadata(
        &self,
        class_id: ClassId,
        meta: cratonvm_native_api::LambdaSerialMetadata,
    ) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_serial_metadata_override.get()).insert(class_id.as_u32(), meta);
        }
    }

    #[allow(dead_code)]
    pub(crate) fn global_root_count(&self) -> usize {
        // SAFETY: single-threaded test code.
        unsafe { (&*self.global_roots.get()).len() }
    }

    /// gc-common w30-a: model a moving collection of a global root's referent
    /// -- the collector re-addresses the root, the handle stays. Answers
    /// whether `handle` was a live root.
    #[allow(dead_code)]
    pub(crate) fn w30a_retarget_global_root_for_test(&self, handle: usize, to: ObjectRef) -> bool {
        // SAFETY: single-threaded test code.
        match unsafe { (&mut *self.global_roots.get()).get_mut(&handle) } {
            Some(slot) => {
                *slot = to;
                true
            }
            None => false,
        }
    }

    /// CGLIB-η: read the `loader_id` of the most recent
    /// `define_class_full` call. `None` if no call has been made yet.
    #[allow(dead_code)]
    pub(crate) fn last_define_full_loader(&self) -> Option<u32> {
        // SAFETY: single-threaded test code.
        unsafe { *self.last_define_full_loader.get() }
    }

    #[allow(dead_code)]
    pub(crate) fn set_method_return_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
        annotations: Vec<AnnotationData>,
    ) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.method_return_type_annotations_override.get()).insert(
                (
                    class_id.as_u32(),
                    method_name.to_string(),
                    method_desc.to_string(),
                ),
                annotations,
            );
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_method_parameter_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
        annotations: Vec<Vec<AnnotationData>>,
    ) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.method_parameter_type_annotations_override.get()).insert(
                (
                    class_id.as_u32(),
                    method_name.to_string(),
                    method_desc.to_string(),
                ),
                annotations,
            );
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_field_type_annotations(
        &self,
        class_id: ClassId,
        field_name: &str,
        annotations: Vec<AnnotationData>,
    ) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.field_type_annotations_override.get())
                .insert((class_id.as_u32(), field_name.to_string()), annotations);
        }
    }

    /// WP8.11.5: declare that `child_id` has a nest host named `host`.
    /// Subsequent calls to `nest_host_name(child_id)` return `Some(host)`.
    #[allow(dead_code)]
    pub(crate) fn set_nest_host_override(&self, child_id: ClassId, host: &str) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.nest_host_override.get()).insert(child_id.as_u32(), host.to_string());
        }
    }

    /// getNestMembers0 regression: declare that `host_id` has a
    /// `NestMembers` attribute listing `members`.
    /// Subsequent calls to `nest_member_names(host_id)` return `members`.
    #[allow(dead_code)]
    pub(crate) fn set_nest_members_override(&self, host_id: ClassId, members: Vec<String>) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.nest_members_override.get()).insert(host_id.as_u32(), members);
        }
    }

    /// 2026-07-11: script `gpu_future_take_result(handle)` to answer
    /// `Some(result)`. Lets `craton_gpu.rs` unit tests exercise
    /// `builtin_future_get_result`'s real-registry branch (the one that
    /// calls `ctx.gpu_future_take_result` before ever consulting the
    /// local synthetic `FutureState` map) without a real GPU submission
    /// registry, mirroring how `record_done_scalar` stamps the
    /// synthetic-map fallback directly.
    #[allow(dead_code)]
    pub(crate) fn set_gpu_future_take_result(&self, handle: u64, result: GpuFutureResult) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.gpu_future_take_result_override.get()).insert(handle, result);
        }
    }

    /// GpuStream affinity: script `gpu_stream_create()` to answer
    /// `result` on every subsequent call. Lets `craton_gpu.rs` unit
    /// tests exercise `resolve_or_create_default_stream`'s
    /// create-once-then-cache behavior without a real GPU stream
    /// registry.
    #[allow(dead_code)]
    pub(crate) fn set_gpu_stream_create_result(&self, result: Option<u64>) {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.gpu_stream_create_override.get() = result;
        }
    }

    /// GpuStream affinity: how many times `gpu_stream_create` has
    /// been invoked so far.
    #[allow(dead_code)]
    pub(crate) fn gpu_stream_create_call_count(&self) -> u32 {
        // SAFETY: single-threaded test code.
        unsafe { *self.gpu_stream_create_calls.get() }
    }

    /// GpuStream affinity: every handle passed to `gpu_stream_release`
    /// so far, in call order.
    #[allow(dead_code)]
    pub(crate) fn gpu_stream_release_calls(&self) -> Vec<u64> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.gpu_stream_release_calls.get()).clone() }
    }

    /// Every handle the shim forwarded to `gpu_release_submission`, in
    /// call order.
    pub(crate) fn gpu_release_submission_calls(&self) -> Vec<u64> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.gpu_release_submission_calls.get()).clone() }
    }

    /// WP8.11.5: read the most recent `DefineClassFull` options passed
    /// through `define_class_full`. `None` if no call has been made yet.
    #[allow(dead_code)]
    pub(crate) fn last_define_full_opts(&self) -> Option<cratonvm_native_api::DefineClassFull> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.last_define_full_opts.get()).clone() }
    }

    /// T19_H15 — read a snapshot of the dynamic classpath entries that
    /// have been registered via `register_dynamic_classpath`.
    /// Returns the absolute path strings in insertion order.
    pub(crate) fn registered_classpath_snapshot(&self) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.registered_classpath.get()).clone() }
    }

    /// T19_K4: read the recorded mirror pointer for `thread_id`, or 0
    /// if no mirror was attached. Tests use this to confirm that
    /// Vert.x / XNIO event-loop spawns hand a real `java.lang.Thread`
    /// mirror through `set_native_thread_java_obj`.
    #[allow(dead_code)]
    pub(crate) fn native_thread_java_obj_ptr(&self, thread_id: u64) -> usize {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.native_thread_java_objs.get())
                .get(&thread_id)
                .copied()
                .unwrap_or(0)
        }
    }

    /// T19.H10: register a synthetic resource that `find_resource` will
    /// return. `name` is the classpath-relative path (no leading slash).
    /// Subsequent `find_resource(name)` returns a clone of `bytes`.
    #[allow(dead_code)]
    pub(crate) fn set_resource(&self, name: &str, bytes: Vec<u8>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.resources_override.get()).insert(name.to_string(), bytes) };
    }

    /// Register JPMS `provides` implementations for `service_name`.
    ///
    /// Both names use binary slash format, matching `ModuleDescriptor` storage:
    /// `org/example/SPI` -> `org/example/Provider`.
    #[allow(dead_code)]
    pub(crate) fn set_module_providers(&self, service_name: &str, providers: Vec<&str>) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.module_providers_override.get()).insert(
                service_name.to_string(),
                providers.into_iter().map(str::to_string).collect(),
            );
        }
    }

    /// T19_K2: read the list of `register_native_thread` calls. Each
    /// entry is `(name, daemon, alive)`. Tests use this to assert that
    /// Vert.x / XNIO event-loop spawns route through the VM-tracking
    /// path with the right daemon flag.
    ///
    /// gcj j3/rt3 residual 5: an entry its thread retired
    /// ([`MockNativeThreadBlocker::retire`]) reads `alive == false`.
    #[allow(dead_code)]
    pub(crate) fn registered_native_threads(&self) -> Vec<(String, bool, bool)> {
        // SAFETY: single-threaded test code.
        let mut v = unsafe { (*self.registered_native_threads.get()).clone() };
        let retired = self
            .retired_native_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for id in retired {
            if let Some(entry) = self.native_tid_slot(id).and_then(|i| v.get_mut(i)) {
                entry.2 = false;
            }
        }
        v
    }

    /// gcj j3/rt3 residual 5: the ids retired so far, in order.
    #[allow(dead_code)]
    pub(crate) fn retired_native_threads(&self) -> Vec<u64> {
        self.retired_native_threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Slot of `field_name` among the instance fields a test declared for
    /// `obj`'s class via `set_declared_fields`. This is the one source of
    /// field-name → slot mapping in the mock that a test controls; the
    /// `mock_*_field_slot` tables below it are fixed, per-class special cases.
    ///
    /// Consulted FIRST by `get_field_by_name` / `set_field_by_name`, so a test
    /// that declares a class's real layout gets by-name access consistent with
    /// it. Without this, a test that models the real `java.lang.ClassLoader`
    /// layout would still have `set_field_by_name(loader, "parent", …)`
    /// silently do nothing (there is no `parent` entry in any table), which
    /// reads exactly like the production code failing to write it.
    fn declared_field_slot(&self, obj: ObjectRef, field_name: &str) -> Option<usize> {
        // SAFETY: single-threaded test code.
        let overrides = unsafe { &*self.declared_fields_override.get() };
        overrides
            .get(&self.class_id_of_object(obj).as_u32())?
            .iter()
            .find(|f| !f.is_static && f.name == field_name)
            .map(|f| f.slot_index)
    }

    /// WP0.2: push declared field metadata for `class_id`.  Overrides
    /// the default (empty) `declared_fields` return.
    #[allow(dead_code)]
    pub(crate) fn set_declared_fields(&self, class_id: ClassId, fields: Vec<FieldMetadata>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.declared_fields_override.get()).insert(class_id.as_u32(), fields) };
    }

    /// Make `get_field_by_name` answer an unresolvable name the way the
    /// production `NativeContextImpl` does.
    ///
    /// Production (`vm/src/vm/vm_exec.rs:10613-10623`) resolves the name in the
    /// receiver's class hierarchy and returns **`Value::Object(None)`** when it
    /// cannot; that is also the documented trait contract
    /// (`native-api/src/registry.rs:2351-2354`, "Returns `Value::Object(None)`
    /// if the field is not found"). This mock's default answer is
    /// `Value::Int(0)` — see the DIVERGENCE note on
    /// [`NativeHeapAccess::get_field_by_name`]'s impl below for why the default
    /// has not been flipped.
    ///
    /// Turn this on in any test whose subject distinguishes "absent" from
    /// "null" — `field_read::declares_field`, or any dual-layout discriminator
    /// that reads a witness field's VALUE. Without it the test is measuring the
    /// mock, not the VM.
    ///
    /// Both arms of a dual-layout discriminator are reachable with this on:
    ///
    /// * REAL-layout receiver: `ensure_class_initialized("p/RealShaped")`, then
    ///   `set_declared_fields(cid, ..)` naming the real JDK class's own private
    ///   fields. `resolve_field_index_by_class_id` finds them (that is the
    ///   class-side witness) and `get_field_by_name` reads their slots.
    /// * FABRICATED receiver: a class name that
    ///   `cratonvm_classloading::synthetic_stub_field_model` does not model and
    ///   no `mock_*_field_slot` table names, with no `set_declared_fields`
    ///   call. The witness answers `None` and the by-name read answers
    ///   `Object(None)` — exactly what the VM would answer.
    #[allow(dead_code)]
    pub(crate) fn set_absent_field_answers_null(&self, faithful: bool) {
        self.absent_field_answers_null.set(faithful);
    }

    /// Register `class_id`'s own `InnerClasses` attribute entries. Overrides
    /// the default (empty) `inner_classes` return so tests can simulate a
    /// genuine JVM member/local/anonymous class.
    #[allow(dead_code)]
    pub(crate) fn set_inner_classes(
        &self,
        class_id: ClassId,
        entries: Vec<(String, String, String, u16)>,
    ) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.inner_classes_override.get()).insert(class_id.as_u32(), entries) };
    }

    /// Register `class_id`'s `EnclosingMethod` attribute (JIT round 21 wave
    /// 22, lane hidden-resid). Overrides the default `None`.
    #[allow(dead_code)]
    pub(crate) fn set_enclosing_method(&self, class_id: ClassId, class_name: &str, method: &str, desc: &str) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.enclosing_method_override.get()).insert(
                class_id.as_u32(),
                (class_name.to_string(), method.to_string(), desc.to_string()),
            )
        };
    }

    /// WP0.2: push declared method metadata for `class_id`.  Overrides
    /// the default (empty) `declared_methods` return.
    #[allow(dead_code)]
    pub(crate) fn set_declared_methods(&self, class_id: ClassId, methods: Vec<MethodMetadata>) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.declared_methods_override.get()).insert(class_id.as_u32(), methods) };
    }

    /// A SECOND class carrying a name that is already registered — the other
    /// loader's copy.
    ///
    /// Deliberately does not touch `name_to_id`: `(loader, name)` is the real
    /// identity and a mock keyed on name alone cannot hold two entries, so the
    /// new id gets a name while by-name resolution keeps answering the first
    /// copy. That is exactly what a caller with no loader context observes on
    /// the real VM, and the state the reflective-coercion loader-split arm
    /// exists for.
    ///
    /// Only sound for code that reads `class_name_of_id` / `superclass_of` off
    /// an id it already holds. Do not use it in a test that exercises name
    /// resolution — there the second copy is invisible by construction.
    #[allow(dead_code)]
    pub(crate) fn declare_second_copy(&mut self, name: &str) -> ClassId {
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        ClassId::new(id)
    }

    /// Declare a class that `list_loaded_class_ids` will also report.
    ///
    /// `declare_second_copy` above names an id WITHOUT adding it to that list,
    /// deliberately; this is the version for code that walks the loaded set --
    /// `boot_loader::native_get_system_package_names` is the caller it was added
    /// for. The list stays empty unless a test calls this.
    #[allow(dead_code)]
    pub(crate) fn declare_loaded_class(&mut self, internal_name: &str) -> ClassId {
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, internal_name.to_string());
        self.loaded_class_ids.push(id);
        ClassId::new(id)
    }

    /// Say which module owns a package, in the slash form
    /// `NativeContext::module_for_package` takes.
    #[allow(dead_code)]
    pub(crate) fn declare_module_package(&mut self, package_slash: &str, module: &str) {
        self.module_by_package
            .insert(package_slash.to_string(), module.to_string());
    }

    /// [`Self::declare_module_package`] for a package its module does NOT open
    /// to everyone.
    #[allow(dead_code)]
    pub(crate) fn declare_closed_module_package(&mut self, package_slash: &str, module: &str) {
        self.declare_module_package(package_slash, module);
        self.closed_packages.insert(package_slash.to_string());
    }

    /// WP0.2: set the direct super-class of `class_id`.
    #[allow(dead_code)]
    pub(crate) fn set_superclass(&self, class_id: ClassId, super_id: ClassId) {
        unsafe { (*self.superclass_override.get()).insert(class_id.as_u32(), super_id) };
    }

    /// Set `class_access_flags` for `class_id`.
    ///
    /// The default is `0` — i.e. **not public**, and that is not an accident
    /// to be worked around: a test that never calls this is testing a
    /// package-private class. It matters for anything reading
    /// [`crate::lang_class::mirror_is_public`], because `Lookup.UNCONDITIONAL`'s
    /// access rule is about the target CLASS rather than the member. A
    /// `publicLookup()` test whose target class was left at the default is
    /// refused before the member is looked at at all, and would then pass for
    /// the wrong reason wherever the assertion is "this throws".
    #[allow(dead_code)]
    pub(crate) fn set_class_access_flags(&self, class_id: ClassId, flags: u16) {
        // SAFETY: single-threaded test code.
        unsafe { (*self.class_flags_override.get()).insert(class_id.as_u32(), flags) };
    }

    /// WP0.2: set the interface list of `class_id`.
    #[allow(dead_code)]
    pub(crate) fn set_interfaces(&self, class_id: ClassId, ifaces: Vec<ClassId>) {
        unsafe { (*self.interfaces_override.get()).insert(class_id.as_u32(), ifaces) };
    }

    /// G2: mark `class_id` as an interface (overrides default `false`).
    /// Required by tests that exercise the
    /// `Class.getSuperclass()`-returns-null-for-interfaces semantics or
    /// the `getMethods` interface skip.
    #[allow(dead_code)]
    pub(crate) fn set_is_interface(&self, class_id: ClassId, is_iface: bool) {
        unsafe { (*self.is_interface_override.get()).insert(class_id.as_u32(), is_iface) };
    }

    /// T19.N2: set the mock current-thread interrupted flag.
    ///
    /// Allows tests to simulate an interrupt arriving mid-operation, e.g.
    /// a background thread spawning `ctx.set_interrupted(true)` while the
    /// main test thread is inside a chunked `Thread.sleep0` loop.
    #[allow(dead_code)]
    pub(crate) fn set_interrupted(&self, v: bool) {
        // SAFETY: single-threaded test code; no aliasing.
        unsafe {
            *self.interrupted_flag.get() = v;
        }
    }

    /// Make `is_jdk_only` answer `v`, for natives whose behaviour differs
    /// between `--compatible` and `--jdk-only`.
    #[allow(dead_code)]
    pub(crate) fn set_jdk_only(&self, v: bool) {
        // SAFETY: single-threaded test code; no aliasing.
        unsafe {
            *self.jdk_only_flag.get() = v;
        }
    }

    fn heap_mut(&self) -> &mut Vec<HeapEntry> {
        // SAFETY: only used in single-threaded test code
        unsafe { &mut *self.heap.get() }
    }

    fn heap_ref(&self) -> &Vec<HeapEntry> {
        // SAFETY: only used in single-threaded test code
        unsafe { &*self.heap.get() }
    }

    // addr-keyed: an accessor of ptr_to_index (see its disposition).
    fn ptr_map_mut(&self) -> &mut HashMap<usize, usize> {
        unsafe { &mut *self.ptr_to_index.get() }
    }

    // addr-keyed: an accessor of ptr_to_index (see its disposition).
    fn ptr_map_ref(&self) -> &HashMap<usize, usize> {
        unsafe { &*self.ptr_to_index.get() }
    }

    fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
        // MH5-1: in the STRICT moving mode an allocation is a GC point. Every
        // object moves BEFORE the new one is placed; the new entry's own
        // references are the allocating call's (a `create_string`'s array, a
        // mirror's name), which the VM roots across its own allocation, so
        // they are forwarded with the rest. The VM-rule mode (round 17 wave 2,
        // lane funnel) places it in place: the VM's allocation doors never
        // collect.
        let mut entry = entry;
        if self.moving.get() && self.moving_alloc_gc_point.get() {
            let forward = self.relocate_all();
            match &mut entry {
                HeapEntry::Object { fields: vals, .. }
                | HeapEntry::Array { elements: vals, .. } => {
                    for v in vals.iter_mut() {
                        self.forward_value(&forward, v);
                    }
                }
            }
        }
        self.alloc_entry_in_place(entry)
    }

    /// Place `entry` at a fresh address, moving nothing: `alloc_entry` in the
    /// default mode, and the allocations the VM makes without a GC point
    /// (`init_string_from_units`' non-collecting array) in the moving one.
    fn alloc_entry_in_place(&mut self, entry: HeapEntry) -> ObjectRef {
        let heap = self.heap_mut();
        let index = heap.len();
        heap.push(entry);
        let ptr_val = self.next_ptr;
        self.next_ptr += 8; // keep 8-byte aligned
        self.ptr_map_mut().insert(ptr_val, index);
        // SAFETY: single-threaded test code; no aliasing of the cell.
        unsafe { (*self.first_addr.get()).push(ptr_val) };
        // SAFETY: ptr_val is non-null (>= 8) and 8-byte aligned
        unsafe { ObjectRef::from_raw(ptr_val as *mut u8) }
    }

    fn entry_index(&self, obj: ObjectRef) -> usize {
        let ptr_val = obj.as_ptr() as usize;
        match self.ptr_map_ref().get(&ptr_val) {
            Some(index) => *index,
            None => {
                self.assert_not_retired(ptr_val, "used");
                panic!("invalid ObjectRef in mock heap")
            }
        }
    }

    /// MH5-1: panic, naming the defect, when `addr` is an address the moving
    /// mode retired.
    fn assert_not_retired(&self, addr: usize, what: &str) {
        // SAFETY: single-threaded test code; no aliasing of the cell.
        let retired = unsafe { &*self.retired.get() };
        assert!(
            !retired.contains(&addr),
            "stale ObjectRef {addr:#x} {what} after the moving mock heap relocated it: a \
             reference was held across a GC point (an allocation or a Java call) \
             without a pin"
        );
    }

    /// MH5-1 (JIT round 16 wave 8, lane mh; proposal MH5-1 of
    /// `docs/feature-designs/jit-ir-optimizations.md`): turn the moving mode
    /// on or off. In it, every allocation and every Java call
    /// (`invoke` / `invoke_virtual`) first RELOCATES every object of this heap
    /// to a fresh address, as a moving collector may at those GC points, and
    /// forwards every root the VM would forward: the native pin stack (handle
    /// scopes and `read_native_pin` included), the global roots, references
    /// inside heap objects, the per-VM tables this mock keeps (bound
    /// `MethodType`s, the `ObjectStreamClass` cache, enum `$VALUES`, static
    /// fields, the stable thread object, a scripted `invoke_virtual` result),
    /// and the arguments of the Java call itself. The old address is retired:
    /// using it -- a field read, a class query, storing it into an object --
    /// panics with "stale ObjectRef", so a native that keeps an unpinned
    /// reference across a GC point fails its test instead of passing it.
    /// `identity_hash_code` keeps the first address's hash, and
    /// `gc_collection_count` counts the relocations (the address-keyed memos'
    /// epoch). The test's own locals are not roots: read results through what
    /// the native returned, or through a global root (`add_global_root`).
    ///
    /// This is the STRICT mode: it treats every allocation as a GC point too,
    /// which the VM does not (`NativeHeapAccess`'s "GC points inside a
    /// native"). A native that passes it also passes the VM's rule; a native
    /// that relies on the rule (an unpinned reference across an allocation)
    /// is tested with [`Self::set_moving_heap_vm_rule`] instead.
    pub(crate) fn set_moving_heap(&self, on: bool) {
        self.moving.set(on);
        self.moving_alloc_gc_point.set(true);
    }

    /// JIT round 17 wave 2 (lane funnel; page
    /// `r17-native-alloc-gc-point-rule-differs-between-api-and-moving-mock-FIXED-20261006.md`):
    /// the moving mode with the VM's GC points and no others. Every Java call
    /// (`invoke` / `invoke_virtual`) and every `reclaim_before_alloc_retry`
    /// relocates the heap as [`Self::set_moving_heap`] describes; an
    /// allocation places its object without moving anything, as the VM's
    /// allocation doors do.
    pub(crate) fn set_moving_heap_vm_rule(&self, on: bool) {
        self.moving.set(on);
        self.moving_alloc_gc_point.set(!on);
    }

    /// Interpreter round i3 wave 79, lane L1: in the moving mode, also treat
    /// the end of a blocking region (`end_blocking_region_refs`) as a GC
    /// point, as a collection that completed while the thread was parked is
    /// in the VM. The refs passed to it are forwarded; a native that keeps a
    /// pre-region local instead trips the retired-address check.
    pub(crate) fn set_region_end_gc_point(&self, on: bool) {
        self.region_end_gc_point.set(on);
    }

    /// MH5-1: relocations so far.
    pub(crate) fn relocations_for_test(&self) -> u64 {
        self.relocations.get()
    }

    /// MH5-1: forward a value through a relocation's map (references this
    /// heap does not hold are left as they are).
    fn forward_value(&self, forward: &cratonvm_types::PointerMap, v: &mut Value) {
        if let Value::Object(Some(o)) = v {
            let addr = o.as_ptr() as usize;
            match forward.get(&addr) {
                // SAFETY: a fresh, non-null, 8-byte aligned address.
                Some(&new) => *o = unsafe { ObjectRef::from_raw(new as *mut u8) },
                None => self.assert_not_retired(addr, "forwarded"),
            }
        }
    }

    /// MH5-1: forward a bare reference (a root that is not a `Value`).
    fn forward_ref(forward: &cratonvm_types::PointerMap, o: &mut ObjectRef) {
        if let Some(&new) = forward.get(&(o.as_ptr() as usize)) {
            // SAFETY: a fresh, non-null, 8-byte aligned address.
            *o = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    }

    /// MH5-1: move every object of this heap to a fresh address and forward
    /// every root and every reference field. Answers the forwarding map (old
    /// address to new) so a caller can forward the values it is about to hand
    /// on (the Java call's arguments, a new entry's fields).
    fn relocate_all(&self) -> cratonvm_types::PointerMap {
        let map = self.ptr_map_mut();
        let mut forward = cratonvm_types::PointerMap::with_capacity(map.len());
        // addr-keyed: local to this relocation (test-only).
        let mut moved: HashMap<usize, usize> = HashMap::with_capacity(map.len());
        let n = map.len();
        let base = MOVING_NEXT_PTR.fetch_add(n * 8, Ordering::Relaxed);
        // A stable order (by heap index), so a run's addresses do not depend
        // on the hash map's iteration order.
        let mut olds: Vec<(usize, usize)> = map.iter().map(|(a, i)| (*a, *i)).collect();
        olds.sort_by_key(|(_, i)| *i);
        // SAFETY: single-threaded test code; no aliasing of the cells below.
        let retired = unsafe { &mut *self.retired.get() };
        for (k, (old, index)) in olds.into_iter().enumerate() {
            let new = base + k * 8;
            forward.insert(old, new);
            moved.insert(new, index);
            retired.insert(old);
        }
        *map = moved;
        for entry in self.heap_mut().iter_mut() {
            match entry {
                HeapEntry::Object { fields: vals, .. }
                | HeapEntry::Array { elements: vals, .. } => {
                    for v in vals.iter_mut() {
                        if let Value::Object(Some(o)) = v {
                            Self::forward_ref(&forward, o);
                        }
                    }
                }
            }
        }
        unsafe {
            for pin in (*self.native_pin_roots.get()).iter_mut() {
                Self::forward_ref(&forward, pin);
            }
            for root in (*self.global_roots.get()).values_mut() {
                Self::forward_ref(&forward, root);
            }
            for desc in (*self.osc_cache_map.get()).values_mut() {
                Self::forward_ref(&forward, desc);
            }
            for values in (*self.enum_values_override.get()).values_mut() {
                Self::forward_ref(&forward, values);
            }
            for v in (*self.static_fields_override.get()).values_mut() {
                if let Value::Object(Some(o)) = v {
                    Self::forward_ref(&forward, o);
                }
            }
            if let Some(Ok(Some(Value::Object(Some(o))))) = &mut *self.invoke_virtual_result.get() {
                Self::forward_ref(&forward, o);
            }
            for addr in (*self.native_thread_java_objs.get()).values_mut() {
                if let Some(&new) = forward.get(addr) {
                    *addr = new;
                }
            }
        }
        if let Some(mut thread) = self.stable_thread_object.get() {
            Self::forward_ref(&forward, &mut thread);
            self.stable_thread_object.set(Some(thread));
        }
        {
            let mut components = self.ref_array_components.borrow_mut();
            // addr-keyed: local -- the re-keyed ref_array_components (test-only).
            let rekeyed: HashMap<usize, ClassId> = components
                .drain()
                .map(|(addr, cid)| (forward.get(&addr).copied().unwrap_or(addr), cid))
                .collect();
            *components = rekeyed;
        }
        for (obj, _) in self.monitor_wait_calls.borrow_mut().iter_mut() {
            Self::forward_ref(&forward, obj);
        }
        for obj in self.register_if_finalizable_calls.borrow_mut().iter_mut() {
            Self::forward_ref(&forward, obj);
        }
        self.bound_method_types.remap_roots(&forward);
        // MH16-10 (j27 wave 4, lane MHROWS2): this VM identity's `valueOf`
        // caches are VM roots, remapped as the VM's GC remaps them (only the
        // slots naming an object this relocation moved change).
        crate::lang_math::gc_update_value_of_cache_refs(self.vm_identity_override.get(), &forward);
        self.relocations.set(self.relocations.get() + 1);
        forward
    }

    /// Allocate a bare heap object and return its `ObjectRef`. Used by tests
    /// that only need a distinct object identity (e.g. a dummy `Supplier`)
    /// without caring about its class or fields. Class id 0 + four zeroed
    /// slots mirrors `new_object`'s default layout.
    pub(crate) fn fresh_object_ref(&mut self) -> ObjectRef {
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Int(0); 4],
        })
    }

    /// Script the next `invoke_virtual` upcall's result (consumed once by the
    /// mock's `invoke_virtual`). Mirrors directly writing
    /// `invoke_virtual_result`; provided as a method so tests read cleanly.
    pub(crate) fn set_invoke_virtual_result(&self, result: MethodCallResult) {
        // SAFETY: single-threaded test context; no aliasing of the cell.
        unsafe { *self.invoke_virtual_result.get() = Some(result) };
    }

    /// Install a by-name Java-call hook; `None` uses the default mock answer.
    pub(crate) fn set_invoke_hook(&self, hook: InvokeHook) {
        // SAFETY: single-threaded test context; no aliasing of the cell.
        unsafe { *self.invoke_hook.get() = Some(hook) };
    }

    /// Install a method-aware virtual-call hook. Returning `Some(result)` from
    /// the hook handles the call; returning `None` falls through to the legacy
    /// single-shot scripted result.
    pub(crate) fn set_invoke_virtual_hook(&self, hook: InvokeVirtualHook) {
        // SAFETY: single-threaded test context; no aliasing of the cell.
        unsafe { *self.invoke_virtual_hook.get() = Some(hook) };
    }

    pub(crate) fn remap_native_pin_addr_for_test(&self, old_addr: usize, new_addr: usize) {
        let pins = unsafe { &mut *self.native_pin_roots.get() };
        for pin in pins.iter_mut() {
            if pin.as_ptr() as usize == old_addr {
                *pin = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    }

    pub(crate) fn native_pin_count_for_test(&self) -> usize {
        unsafe { (&*self.native_pin_roots.get()).len() }
    }

    pub(crate) fn blocking_region_counts(&self) -> (usize, usize) {
        (self.blocking_begin_count, self.blocking_end_count)
    }

    /// i66-L1: regions opened through `begin_vm_wait_region` (each also
    /// counted in `blocking_region_counts`).
    pub(crate) fn vm_wait_region_count(&self) -> usize {
        self.vm_wait_begin_count
    }

    /// FIX(test-isolation): map a `thread_id` handed out by
    /// `register_native_thread` back to its 0-based slot in
    /// `registered_native_threads`. The k-th registration returns
    /// `native_tid_base + k`, so the index is `thread_id -
    /// native_tid_base`. Returns `None` if `thread_id` is below the
    /// instance base (i.e. not a tid this ctx ever minted), which the
    /// callers treat exactly like the old out-of-range case.
    fn native_tid_slot(&self, thread_id: u64) -> Option<usize> {
        thread_id
            .checked_sub(self.native_tid_base)
            .map(|i| i as usize)
    }
}

impl cratonvm_native_api::NativeClassAccess for MockNativeContext {
    /// A class the mock knows by name loads (its mirror, no initialization);
    /// any other name is `Ok(None)`, as before. `Class.forName(Module,
    /// String)` loads through this without initializing since interpreter
    /// round i1 wave 47 (lane L5), as HotSpot does.
    fn load_class(&mut self, name: &str) -> MethodCallResult {
        match self.class_id_by_name(name) {
            Some(class_id) => Ok(Some(Value::Object(Some(self.get_class_mirror(class_id))))),
            None => Ok(None),
        }
    }

    fn class_name_of_id(&self, class_id: ClassId) -> Option<String> {
        self.class_names.get(&class_id.as_u32()).cloned()
    }

    fn list_loaded_class_ids(&self) -> Vec<ClassId> {
        self.loaded_class_ids
            .iter()
            .copied()
            .map(ClassId::new)
            .collect()
    }

    fn module_for_package(&self, pkg: &str) -> Option<String> {
        self.module_by_package.get(pkg).cloned()
    }

    fn class_id_of_object(&self, obj: ObjectRef) -> ClassId {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { class_id, .. } => *class_id,
            HeapEntry::Array { .. } => ClassId::new(0),
        }
    }

    fn class_id_from_mirror(&self, mirror: ObjectRef) -> Option<ClassId> {
        match self.get_field(mirror, 0) {
            Value::Int(raw) if raw >= 0 => Some(ClassId::new(raw as u32)),
            _ => None,
        }
    }

    /// `false`, unless this thread named the method with
    /// [`mock_method_exists`] (zt t7/cl).
    fn method_exists(&self, class_name: &str, method_name: &str, descriptor: &str) -> bool {
        MOCK_METHODS_EXIST.with(|m| {
            m.borrow()
                .iter()
                .any(|(c, n, d)| c == class_name && n == method_name && d == descriptor)
        })
    }

    fn ensure_class_initialized(&mut self, name: &str) -> Result<ClassId, MethodCallFailed> {
        self.class_init_log.borrow_mut().push(name.to_string());
        if self.failing_class_inits.borrow().iter().any(|n| n == name) {
            return Err(MethodCallFailed::from(
                cratonvm_types::error::LinkageError::NoClassDefFoundError {
                    class_name: name.to_string(),
                },
            ));
        }
        if let Some(&id) = self.name_to_id.get(name) {
            return Ok(ClassId::new(id));
        }
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        Ok(ClassId::new(id))
    }

    fn is_subclass(&self, child: ClassId, parent: ClassId) -> bool {
        if child == parent {
            return true;
        }
        // SAFETY: single-threaded test code.
        let supers = unsafe { &*self.superclass_override.get() };
        let ifaces = unsafe { &*self.interfaces_override.get() };
        let mut cur = Some(child);
        while let Some(cid) = cur {
            if let Some(list) = ifaces.get(&cid.as_u32()) {
                if list.contains(&parent) {
                    return true;
                }
                // Also recurse into each interface's super-interfaces
                // so `is_subclass(Foo, Serializable)` works even if
                // `Foo`'s declared interface is `MyMarker extends
                // Serializable`. Guarded against cycles via a shallow
                // visited set — tests don't construct deep hierarchies.
                for i in list {
                    if self.is_subclass(*i, parent) {
                        return true;
                    }
                }
            }
            if let Some(&sup) = supers.get(&cid.as_u32()) {
                if sup == parent {
                    return true;
                }
                cur = Some(sup);
            } else {
                cur = None;
            }
        }
        false
    }

    fn superclass_of(&self, class_id: ClassId) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        let supers = unsafe { &*self.superclass_override.get() };
        supers.get(&class_id.as_u32()).copied()
    }

    fn class_id_by_name(&self, name: &str) -> Option<ClassId> {
        self.name_to_id.get(name).map(|&id| ClassId::new(id))
    }

    /// The mock's loader model (JIT round 16 wave 17, lane mhapp): a class's
    /// loader is its `set_loader_id_override` value. A class of `near`'s own
    /// loader carrying `name` (a `declare_second_copy` of another loader's
    /// class, say) wins, the lowest id first; otherwise the loader-blind
    /// answer, as delegation would give.
    fn class_id_by_name_near(&self, name: &str, near: ClassId) -> Option<ClassId> {
        // SAFETY: single-threaded test code.
        let loaders = unsafe { &*self.loader_id_override.get() };
        if let Some(&loader) = loaders.get(&near.as_u32()) {
            let own = self
                .class_names
                .iter()
                .filter(|(id, n)| n.as_str() == name && loaders.get(*id) == Some(&loader))
                .map(|(id, _)| *id)
                .min();
            if let Some(id) = own {
                return Some(ClassId::new(id));
            }
        }
        self.class_id_by_name(name)
    }

    fn loader_id_of_class(&self, class_id: ClassId) -> i32 {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.loader_id_override.get())
                .get(&class_id.as_u32())
                .copied()
                .unwrap_or(2) // default to app loader
        }
    }

    fn loader_id_of_class_definite(&self, class_id: ClassId) -> Option<i32> {
        // Known only where a test said so (`set_loader_id_override`).
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.loader_id_override.get())
                .get(&class_id.as_u32())
                .copied()
        }
    }

    fn is_record_class(&self, _class_id: ClassId) -> bool {
        false
    }

    fn record_components(&self, _class_id: ClassId) -> Vec<(String, String)> {
        Vec::new()
    }

    fn is_sealed_class(&self, _class_id: ClassId) -> bool {
        false
    }

    fn permitted_subclasses(&self, _class_id: ClassId) -> Vec<String> {
        Vec::new()
    }

    fn enclosing_method(&self, class_id: ClassId) -> Option<(String, String, String)> {
        // SAFETY: single-threaded test code.
        let overrides = unsafe { &*self.enclosing_method_override.get() };
        overrides.get(&class_id.as_u32()).cloned()
    }

    fn inner_classes(&self, class_id: ClassId) -> Vec<(String, String, String, u16)> {
        // SAFETY: single-threaded test code.
        let overrides = unsafe { &*self.inner_classes_override.get() };
        overrides
            .get(&class_id.as_u32())
            .cloned()
            .unwrap_or_default()
    }

    fn declared_fields(&self, class_id: ClassId) -> Vec<FieldMetadata> {
        // SAFETY: single-threaded test code.
        let overrides = unsafe { &*self.declared_fields_override.get() };
        overrides
            .get(&class_id.as_u32())
            .map(|v| {
                v.iter()
                    .map(|f| FieldMetadata {
                        name: f.name.clone(),
                        descriptor: f.descriptor.clone(),
                        access_flags: f.access_flags,
                        slot_index: f.slot_index,
                        declaring_class_id: f.declaring_class_id,
                        is_static: f.is_static,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn declared_methods(&self, class_id: ClassId) -> Vec<MethodMetadata> {
        let overrides = unsafe { &*self.declared_methods_override.get() };
        overrides
            .get(&class_id.as_u32())
            .map(|v| {
                v.iter()
                    .map(|m| MethodMetadata {
                        name: m.name.clone(),
                        descriptor: m.descriptor.clone(),
                        access_flags: m.access_flags,
                        declaring_class_id: m.declaring_class_id,
                        exceptions: m.exceptions.clone(),
                        signature: m.signature.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn class_interfaces(&self, class_id: ClassId) -> Vec<ClassId> {
        let overrides = unsafe { &*self.interfaces_override.get() };
        overrides
            .get(&class_id.as_u32())
            .cloned()
            .unwrap_or_default()
    }

    fn class_access_flags(&self, class_id: ClassId) -> u16 {
        let overrides = unsafe { &*self.class_flags_override.get() };
        overrides.get(&class_id.as_u32()).copied().unwrap_or(0)
    }

    fn primitive_class_mirror(&mut self, name: &str) -> ObjectRef {
        // FIX(class_id-0 name shadow): give the mirror a real, distinct
        // class id via `ensure_class_initialized` (idempotent per name)
        // instead of the shared placeholder `0` — this mock never
        // populates `class_names[0]`, so `mirror_class_name`'s
        // class-id-first lookup returned None/empty for every primitive
        // mirror, shadowing the correct name already stored in slot 1.
        let class_id = self.ensure_class_initialized(name).unwrap().as_u32();
        let name_obj = self.create_string(name);
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![Value::Int(class_id as i32), Value::Object(Some(name_obj))],
        })
    }

    fn class_annotations(&self, _class_id: ClassId) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn method_annotations(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn field_annotations(&self, _class_id: ClassId, _field_name: &str) -> Vec<AnnotationData> {
        Vec::new()
    }

    fn method_return_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<AnnotationData> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.method_return_type_annotations_override.get())
                .get(&(
                    class_id.as_u32(),
                    method_name.to_string(),
                    method_desc.to_string(),
                ))
                .cloned()
                .unwrap_or_default()
        }
    }

    fn method_parameter_type_annotations(
        &self,
        class_id: ClassId,
        method_name: &str,
        method_desc: &str,
    ) -> Vec<Vec<AnnotationData>> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.method_parameter_type_annotations_override.get())
                .get(&(
                    class_id.as_u32(),
                    method_name.to_string(),
                    method_desc.to_string(),
                ))
                .cloned()
                .unwrap_or_default()
        }
    }

    fn field_type_annotations(&self, class_id: ClassId, field_name: &str) -> Vec<AnnotationData> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.field_type_annotations_override.get())
                .get(&(class_id.as_u32(), field_name.to_string()))
                .cloned()
                .unwrap_or_default()
        }
    }

    fn module_name_of_class(&self, class_id: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.module_image_override.get())
                .class_modules
                .get(&class_id.as_u32())
                .cloned()
        }
    }

    fn boot_configuration_module_names(&self) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.module_image_override.get()).boot_modules.clone() }
    }

    fn runtime_image_defines_class(&self, _internal: &str) -> Option<bool> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.module_image_override.get()).present.then_some(true) }
    }

    fn find_all_resource_urls(&self, name: &str) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.module_image_override.get())
                .urls
                .get(name)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn lambda_proxy_host(&self, class_id: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_proxy_hosts_override.get())
                .get(&class_id.as_u32())
                .cloned()
        }
    }

    fn lambda_proxy_serial_metadata(
        &self,
        class_id: ClassId,
    ) -> Option<cratonvm_native_api::LambdaSerialMetadata> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.lambda_serial_metadata_override.get())
                .get(&class_id.as_u32())
                .cloned()
        }
    }

    fn find_resource(&self, name: &str) -> Option<Vec<u8>> {
        // T19.H10: route through the per-instance override so tests can
        // simulate a classpath containing real resources (e.g. the
        // `keycloak-version.properties` blob the KC26 launcher reads).
        // Strip leading slash to match the production class-path search,
        // which trims absolute resource names before looking them up.
        let trimmed = name.trim_start_matches('/');
        // SAFETY: single-threaded test code.
        unsafe { (*self.resources_override.get()).get(trimmed).cloned() }
    }

    fn service_providers_from_modules(&self, service_class: &str) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.module_providers_override.get())
                .get(service_class)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn list_application_class_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn register_dynamic_classpath(&mut self, paths: &[String]) {
        // T19_H15: record paths so tests can assert the JBoss module
        // loader registers the transitive linkage closure correctly.
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.registered_classpath.get()).extend(paths.iter().cloned());
        }
    }

    fn define_class_from_bytes(&mut self, name: &str, bytes: &[u8]) -> Option<ClassId> {
        // NEW-8 mock: remember the name so tests can inspect it, then
        // mint a fresh ClassId. A bytes slice starting with the magic
        // CAFEBABE is accepted; anything else returns None so error
        // paths in the caller can be tested.
        if bytes.len() < 4 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return None;
        }
        unsafe {
            *self.last_defined_class_name.get() = Some(name.to_string());
        }
        let id = self.next_class_id;
        self.next_class_id += 1;
        self.class_names.insert(id, name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        Some(ClassId::new(id))
    }

    fn set_class_hidden(&mut self, class_id: ClassId) {
        unsafe {
            (*self.hidden_classes.get()).insert(class_id.as_u32());
        }
    }

    fn is_class_hidden(&self, class_id: ClassId) -> bool {
        unsafe { (*self.hidden_classes.get()).contains(&class_id.as_u32()) }
    }

    /// WP8.11.5: override the default `define_class_full` so the mock
    /// captures the full `DefineClassFull` options (notably
    /// `nest_host_class_name`). The default impl in the trait would
    /// collapse the call to `define_class_from_bytes` and lose the
    /// option, defeating NESTMATE-propagation tests.
    fn define_class_full(
        &mut self,
        name: &str,
        bytes: &[u8],
        loader_id: u32,
        opts: cratonvm_native_api::DefineClassFull,
    ) -> Result<ClassId, String> {
        if MOCK_DEFINE_FULL_PANICS.with(|c| c.get()) {
            panic!("zs9nd8 mock: define_class_full panics");
        }
        // SAFETY: single-threaded test code.
        unsafe {
            *self.last_define_full_opts.get() = Some(opts.clone());
            *self.last_define_full_loader.get() = Some(loader_id);
        }
        // Use override_name if present (hidden-class mangled name path).
        let stored_name = opts.override_name.as_deref().unwrap_or(name);
        match self.define_class_from_bytes(stored_name, bytes) {
            Some(cid) => {
                if opts.hidden {
                    self.set_class_hidden(cid);
                }
                Ok(cid)
            }
            None => Err(format!("define_class_full failed for {stored_name}")),
        }
    }

    /// WP8.11.5: read the per-class nest-host override populated by
    /// `set_nest_host_override`. `None` => the class is its own nest
    /// host (default).
    fn nest_host_name(&self, class_id: ClassId) -> Option<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.nest_host_override.get())
                .get(&class_id.as_u32())
                .cloned()
        }
    }

    /// getNestMembers0 regression: read the per-class nest-members override
    /// populated by `set_nest_members_override`. Empty by default (no
    /// `NestMembers` attribute).
    fn nest_member_names(&self, class_id: ClassId) -> Vec<String> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.nest_members_override.get())
                .get(&class_id.as_u32())
                .cloned()
                .unwrap_or_default()
        }
    }

    fn define_class_with_loader(
        &mut self,
        _name: &str,
        _bytes: &[u8],
        _loader_id: u32,
    ) -> Option<ClassId> {
        None
    }

    fn class_id_by_name_and_loader(&self, _name: &str, _loader_id: u32) -> Option<ClassId> {
        None
    }

    /// zt t18/df7: [`mock_unbound_define`]'s rows, bound by name
    /// (`class_id_defined_by_loader_exact` answers it afterwards).
    fn bind_unbound_define(&mut self, class_id: ClassId) -> Option<bool> {
        let row = MOCK_UNBOUND_DEFINES.with(|rows| {
            let mut rows = rows.borrow_mut();
            let at = rows.iter().position(|r| r.0 == class_id.as_u32())?;
            Some(rows.remove(at))
        })?;
        MOCK_BOUND_DEFINES.with(|bound| bound.borrow_mut().push(row));
        Some(true)
    }

    /// zt t18/df7: [`mock_unbound_define`]'s rows, withdrawn (counted by
    /// [`mock_withdrawn_defines`]).
    fn withdraw_unbound_define(&mut self, class_id: ClassId) -> bool {
        let removed = MOCK_UNBOUND_DEFINES.with(|rows| {
            let mut rows = rows.borrow_mut();
            let at = rows.iter().position(|r| r.0 == class_id.as_u32());
            at.map(|at| rows.remove(at)).is_some()
        });
        if removed {
            MOCK_WITHDRAWN_DEFINES.with(|n| n.set(n.get() + 1));
        }
        removed
    }

    /// zt t18/df7: [`mock_unbound_define`]'s rows (an empty `name`: any of
    /// the loader's).
    fn unbound_define_of(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        MOCK_UNBOUND_DEFINES.with(|rows| {
            rows.borrow()
                .iter()
                .find(|r| r.2 == loader_id && (name.is_empty() || r.1 == name))
                .map(|r| ClassId::new(r.0))
        })
    }

    /// zt t31: [`mock_track_builtin_loader_records`]'s claims.
    fn claim_builtin_loader_class_record(&self, class_id: ClassId) -> bool {
        MOCK_BUILTIN_RECORD_CLAIMS.with(|c| {
            c.borrow_mut()
                .as_mut()
                .is_none_or(|claims| claims.insert(class_id.as_u32()))
        })
    }

    /// zt t18/df7: a name [`mock_unbound_define`] bound, else the trait's
    /// fallback (`class_id_by_name_and_loader`).
    fn class_id_defined_by_loader_exact(&self, name: &str, loader_id: u32) -> Option<ClassId> {
        MOCK_BOUND_DEFINES
            .with(|bound| {
                bound
                    .borrow()
                    .iter()
                    .find(|r| r.2 == loader_id && r.1 == name)
                    .map(|r| ClassId::new(r.0))
            })
            .or_else(|| self.class_id_by_name_and_loader(name, loader_id))
    }

    fn allocate_loader_id(&mut self) -> u32 {
        // The real implementation (`vm_exec::allocate_loader_id`) hands out a
        // monotonic counter starting at
        // `ClassLoaderId::NATIVE_FIRST_USER_DEFINED` — 0/1/2 are reserved for
        // Bootstrap/Extension/Application and are never a user-loader id.
        // This used to return a constant `0`, which is the ONE value every
        // caller in `classloader.rs` reads as "no namespace assigned", so any
        // test exercising an id-assigning path silently measured the
        // unassigned case. Mirror the real counter instead; it is per-mock,
        // so tests stay independent of each other.
        let id = self.next_loader_id;
        self.next_loader_id += 1;
        id
    }

    fn method_parameter_annotations(
        &self,
        _: ClassId,
        _: &str,
        _: &str,
    ) -> Vec<Vec<cratonvm_native_api::AnnotationData>> {
        Vec::new()
    }

    fn method_annotation_default(
        &self,
        _: ClassId,
        _: &str,
        _: &str,
    ) -> Option<cratonvm_native_api::AnnotationElementValue> {
        None
    }

    fn class_signature(&self, _class_id: ClassId) -> Option<String> {
        None
    }

    fn method_signature(
        &self,
        _class_id: ClassId,
        _method_name: &str,
        _method_desc: &str,
    ) -> Option<String> {
        None
    }

    fn field_signature(&self, _class_id: ClassId, _field_name: &str) -> Option<String> {
        None
    }

    /// T19.N1: honour any test-provided `code_base_override`; defaults to
    /// `None` (bootstrap/synthetic class) when no override is set.
    fn class_code_base(&self, class_id: ClassId) -> Option<String> {
        let overrides = unsafe { &*self.code_base_override.get() };
        overrides.get(&class_id.as_u32()).cloned()
    }

    /// T19.N1: honour any test-provided `code_source_certs_override`;
    /// defaults to empty (unsigned class).
    fn class_code_source_certs(&self, class_id: ClassId) -> Vec<Vec<u8>> {
        let overrides = unsafe { &*self.code_source_certs_override.get() };
        overrides
            .get(&class_id.as_u32())
            .cloned()
            .unwrap_or_default()
    }

    fn is_package_exported_unqualified(&self, _module_name: &str, _pkg: &str) -> bool {
        true
    }

    fn is_package_exported_to(&self, _module_name: &str, _pkg: &str, _to_module: &str) -> bool {
        true
    }

    fn is_package_open_unqualified(&self, _module_name: &str, pkg: &str) -> bool {
        !self.closed_packages.contains(pkg)
    }

    fn is_package_open_to(&self, _module_name: &str, _pkg: &str, _to_module: &str) -> bool {
        true
    }

    fn check_deep_reflection_access(
        &self,
        _accessor_class_id: ClassId,
        _target_class_id: ClassId,
    ) -> Result<(), String> {
        Ok(())
    }
}

impl cratonvm_native_api::NativeInvokeAccess for MockNativeContext {
    fn invoke(
        &mut self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // MH5-1: a Java call is a GC point; its arguments are the callee's.
        let moved_args;
        let args = if self.moving.get() {
            let forward = self.relocate_all();
            let mut copy = args.to_vec();
            for v in copy.iter_mut() {
                self.forward_value(&forward, v);
            }
            moved_args = copy;
            &moved_args[..]
        } else {
            args
        };
        if let Some(hook) = unsafe { *self.invoke_hook.get() } {
            if let Some(result) = hook(self, class_name, method_name, descriptor, args) {
                return result;
            }
        }
        if class_name == "java/lang/Class"
            && method_name == "getName"
            && descriptor == "()Ljava/lang/String;"
        {
            if let Some(Value::Object(Some(mirror))) = args.first() {
                if let Value::Int(raw_id) = self.get_field(*mirror, 0) {
                    let class_name = self
                        .class_name_of_id(ClassId::new(raw_id as u32))
                        .unwrap_or_default()
                        .replace('/', ".");
                    let name_obj = self.create_string(&class_name);
                    return Ok(Some(Value::Object(Some(name_obj))));
                }
            }
        }
        Ok(None)
    }

    fn invoke_virtual(
        &mut self,
        receiver: ObjectRef,
        method_name: &str,
        descriptor: &str,
        args: &[Value],
    ) -> MethodCallResult {
        // MH5-1: a Java call is a GC point; its receiver and arguments are the
        // callee's.
        let moved_args;
        let (receiver, args) = if self.moving.get() {
            let forward = self.relocate_all();
            let mut recv = Value::Object(Some(receiver));
            self.forward_value(&forward, &mut recv);
            let mut copy = args.to_vec();
            for v in copy.iter_mut() {
                self.forward_value(&forward, v);
            }
            moved_args = copy;
            match recv {
                Value::Object(Some(r)) => (r, &moved_args[..]),
                _ => (receiver, &moved_args[..]),
            }
        } else {
            (receiver, args)
        };
        if let Some(hook) = unsafe { *self.invoke_virtual_hook.get() } {
            if let Some(result) = hook(self, receiver, method_name, descriptor, args) {
                return result;
            }
        }
        let result = unsafe { &mut *self.invoke_virtual_result.get() };
        if let Some(r) = result.take() {
            r
        } else {
            Ok(None)
        }
    }
}

// gc-common w8-b: a refusal knob for the fallible allocators, so a test can
// drive a native's reclaim-and-retry ladder. Thread-local, not a field: the
// knob belongs to the test body that sets it, and tests in this crate run in
// parallel on separate threads. With the knob at its default (no refusals,
// reclaim declines) every fallible allocator below behaves exactly like the
// trait default it overrides.
thread_local! {
    static MOCK_ALLOC_REFUSALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MOCK_RECLAIM_HELPS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MOCK_RECLAIMS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    // zr z8/nr: whether `reclaim_after_failed_alloc_retry` pays a skipped
    // clear-soft rung (the VM's ZGC room-skip repay), and how often it ran.
    static MOCK_FAILED_RETRY_PAYS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MOCK_FAILED_RETRY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// zr z8/nr: make `reclaim_after_failed_alloc_retry` answer `pays` (and
/// reset its call counter).
pub(crate) fn mock_failed_retry_pays(pays: bool) {
    MOCK_FAILED_RETRY_PAYS.with(|c| c.set(pays));
    MOCK_FAILED_RETRY_CALLS.with(|c| c.set(0));
}

/// zr z8/nr: how many times `reclaim_after_failed_alloc_retry` ran.
pub(crate) fn mock_failed_retry_calls() -> usize {
    MOCK_FAILED_RETRY_CALLS.with(std::cell::Cell::get)
}

/// zs s5/nd5: the mock's heap-exhaustion unwind payload; the mock's
/// `native_alloc_oom_failure` recognises it as the VM's does `NativeAllocOom`.
#[derive(Debug)]
pub(crate) struct MockAllocOom;

thread_local! {
    // zs s6/rv4: `create_string` raises the heap-exhaustion unwind
    // ([`MockAllocOom`]), as the VM's does on an exhausted heap on a thread
    // that may unwind.
    static MOCK_CREATE_STRING_OOM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// zs s6/rv4: make this thread's `create_string` raise [`MockAllocOom`].
pub(crate) fn mock_create_string_raises_oom(on: bool) {
    MOCK_CREATE_STRING_OOM.with(|c| c.set(on));
}

thread_local! {
    /// zs s9/nd8: this thread's `define_class_full` panics (the backend panic
    /// the definers' `catch_unwind` arms turn into `ClassFormatError`).
    static MOCK_DEFINE_FULL_PANICS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// zt t31: this thread's mock built-in loader record claims
    /// (`claim_builtin_loader_class_record`): `None` (the default) tracks
    /// nothing and every claim is granted, as the trait's default.
    static MOCK_BUILTIN_RECORD_CLAIMS: std::cell::RefCell<Option<std::collections::HashSet<u32>>> =
        const { std::cell::RefCell::new(None) };
}

/// zt t31: track this thread's built-in loader record claims (`true`, empty)
/// or stop tracking them (`false`).
#[allow(dead_code)]
pub(crate) fn mock_track_builtin_loader_records(on: bool) {
    MOCK_BUILTIN_RECORD_CLAIMS.with(|c| *c.borrow_mut() = on.then(Default::default));
}

thread_local! {
    /// zt t18/df7: this thread's mock unbound defines, `(class id, name,
    /// loader id)` (the class manager's `unbound_defines`), the ones bound
    /// since, and how many were withdrawn.
    static MOCK_UNBOUND_DEFINES: std::cell::RefCell<Vec<(u32, String, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MOCK_BOUND_DEFINES: std::cell::RefCell<Vec<(u32, String, u32)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static MOCK_WITHDRAWN_DEFINES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// zt t18/df7: give this thread's mocks an unbound define of `name` into
/// `loader_id` held by `class_id` (empty the tables first with
/// [`mock_unbound_defines_reset`]).
#[allow(dead_code)]
pub(crate) fn mock_unbound_define(class_id: u32, name: &str, loader_id: u32) {
    MOCK_UNBOUND_DEFINES.with(|rows| {
        rows.borrow_mut()
            .push((class_id, name.to_string(), loader_id))
    });
}

/// zt t18/df7: empty this thread's mock unbound, bound and withdrawn tables.
#[allow(dead_code)]
pub(crate) fn mock_unbound_defines_reset() {
    MOCK_UNBOUND_DEFINES.with(|rows| rows.borrow_mut().clear());
    MOCK_BOUND_DEFINES.with(|rows| rows.borrow_mut().clear());
    MOCK_WITHDRAWN_DEFINES.with(|n| n.set(0));
}

/// zt t18/df7: how many mock unbound defines were withdrawn since the reset.
#[allow(dead_code)]
pub(crate) fn mock_withdrawn_defines() -> usize {
    MOCK_WITHDRAWN_DEFINES.with(std::cell::Cell::get)
}

/// zs s9/nd8: make this thread's `define_class_full` panic.
pub(crate) fn mock_define_full_panics(on: bool) {
    MOCK_DEFINE_FULL_PANICS.with(|c| c.set(on));
}

/// zs s10/nd9: how this thread's mock answers the reserved-mirror doors
/// (`try_reserve_class_mirror`, and `native_doors_collect_on_refusal`, which
/// gates them). `Off` (default): the trait defaults, no reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MockReservation {
    /// No reservation (`Unsupported`, doors do not collect): the default.
    Off,
    /// Every reservation is served.
    Serve,
    /// Every reservation is refused and the reclaim ladder finds nothing: a
    /// spent ladder.
    Spent,
}

thread_local! {
    static MOCK_RESERVATION: std::cell::Cell<MockReservation> =
        const { std::cell::Cell::new(MockReservation::Off) };
    /// Reservations served / refused since the last [`mock_reservation`].
    static MOCK_RESERVATION_CALLS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

thread_local! {
    /// zt t6/nd4: whether this thread's mocks answer `heap_is_zgc` (default
    /// `false`, as the trait default; [`mock_heap_is_zgc`]).
    static MOCK_HEAP_IS_ZGC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// zt t8/x5: what this thread's mocks answer `zgc_soft_max_heap_flag`
    /// (default `None`, the trait's; [`mock_zgc_soft_max_flag`]).
    static MOCK_ZGC_SOFT_MAX_FLAG: std::cell::Cell<Option<(u64, u64, u8)>> =
        const { std::cell::Cell::new(None) };
    /// zt t12/tg5: `zgc_stop_director_tick_before_exit` calls on this thread's
    /// mocks, with the mock's blocking-region begin count at each call
    /// ([`mock_exit_director_stops`]).
    static MOCK_EXIT_DIRECTOR_STOPS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// zt t12/tg5: take the record of this thread's mocks'
/// `zgc_stop_director_tick_before_exit` calls (the blocking-region begin
/// count at each call), leaving it empty.
pub(crate) fn mock_exit_director_stops() -> Vec<usize> {
    MOCK_EXIT_DIRECTOR_STOPS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

/// zt t8/x5: make this thread's mocks answer `zgc_soft_max_heap_flag()` with
/// `row` (`(soft maximum, -Xmx, origin kind)`; `None`: no ZGC heap).
pub(crate) fn mock_zgc_soft_max_flag(row: Option<(u64, u64, u8)>) {
    MOCK_ZGC_SOFT_MAX_FLAG.with(|c| c.set(row));
}

thread_local! {
    /// zt t7/cl: the methods this thread's mocks answer `method_exists` for
    /// (default none: the old constant `false`; [`mock_method_exists`]).
    static MOCK_METHODS_EXIST: std::cell::RefCell<Vec<(String, String, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// zt t7/cl: make this thread's mocks answer `method_exists` `true` for the
/// triple `(class_name, method_name, descriptor)`.
pub(crate) fn mock_method_exists(class_name: &str, method_name: &str, descriptor: &str) {
    MOCK_METHODS_EXIST.with(|m| {
        m.borrow_mut().push((
            class_name.to_string(),
            method_name.to_string(),
            descriptor.to_string(),
        ))
    });
}

/// gcj j1/rf: one keep-alive call a mock received (see
/// [`mock_take_keep_alive_log`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MockKeepAlive {
    /// `gc_reference_keep_alive(referent)`.
    Reference(usize),
    /// `gc_referent_slot_read_keep_alive(receiver, slot, referent)`.
    ReferentSlot {
        receiver: usize,
        slot: usize,
        referent: usize,
    },
}

thread_local! {
    /// gcj j1/rf: the keep-alive calls this thread's mocks received.
    static MOCK_KEEP_ALIVE_LOG: std::cell::RefCell<Vec<MockKeepAlive>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// gcj j1/rf: the keep-alive calls this thread's mocks received since the
/// last take, in order.
pub(crate) fn mock_take_keep_alive_log() -> Vec<MockKeepAlive> {
    MOCK_KEEP_ALIVE_LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// zt t6/nd4: make this thread's mocks answer `heap_is_zgc() == on`, so a
/// test can reach a ZGC-gated collecting arm (with [`mock_reservation`] not
/// `Off` for `native_doors_collect_on_refusal`).
pub(crate) fn mock_heap_is_zgc(on: bool) {
    MOCK_HEAP_IS_ZGC.with(|c| c.set(on));
}

thread_local! {
    /// zt t27/zc10: whether this thread's mocks answer
    /// `zgc_concurrent_mark_active` (default `false`, the trait's;
    /// [`mock_zgc_concurrent_mark_active`]).
    static MOCK_ZGC_CONC_MARK_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// zt t27/zc10: make this thread's mocks answer
/// `zgc_concurrent_mark_active() == on` (the inherited-`ThreadLocal` take's
/// shadow rows, `CRATONVM_ZGC_ITL_DEFER_IN_CYCLE`).
pub(crate) fn mock_zgc_concurrent_mark_active(on: bool) {
    MOCK_ZGC_CONC_MARK_ACTIVE.with(|c| c.set(on));
}

/// zs s10/nd9: set this thread's reservation mode and reset its counts.
pub(crate) fn mock_reservation(mode: MockReservation) {
    MOCK_RESERVATION.with(|c| c.set(mode));
    MOCK_RESERVATION_CALLS.with(|c| c.set((0, 0)));
}

/// zs s10/nd9: `(served, refused)` reservations since [`mock_reservation`].
pub(crate) fn mock_reservation_calls() -> (u32, u32) {
    MOCK_RESERVATION_CALLS.with(std::cell::Cell::get)
}

/// zs s10/nd9 (`zgc-zs9rv6-wave-s8-review-residuals-FIXED-20261019.md` item 4):
/// what one definer body answered on a spent ladder and then with room.
#[derive(Debug)]
pub(crate) struct SpentDefineRow {
    /// The spent run answered `OutOfMemoryError`.
    pub(crate) spent_oome: bool,
    /// The spent run reached `define_class_full` (published something).
    pub(crate) spent_defined: bool,
    /// The spent run left the pin stack as it found it.
    pub(crate) spent_pins_balanced: bool,
    /// Reservations refused during the spent run.
    pub(crate) spent_refused: u32,
    /// The run with room answered `Ok` and reached `define_class_full`
    /// (`None` on the kill arm, which does not run it).
    pub(crate) retry_defined: Option<bool>,
    /// The run with room left the pin stack as it found it.
    pub(crate) retry_pins_balanced: Option<bool>,
}

/// zs s10/nd9: drive one definer body (`run`) through a spent reclaim
/// ladder at its mirror reservation, then (default arm) again with room.
/// `kill` turns `CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER` off for
/// the spent run (the control: no reservation, the body defines first).
pub(crate) fn spent_define_row<T: std::fmt::Debug>(
    ctx: &mut MockNativeContext,
    kill: bool,
    mut run: impl FnMut(&mut MockNativeContext) -> Result<T, MethodCallFailed>,
) -> SpentDefineRow {
    // SAFETY: single-threaded test code.
    let clear = |ctx: &MockNativeContext| unsafe { *ctx.last_define_full_opts.get() = None };
    let pins = ctx.native_pin_count_for_test();
    clear(ctx);
    mock_reservation(MockReservation::Spent);
    let spent = {
        let _off = kill.then(|| {
            cratonvm_types::flags::override_thread(
                cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                    "CRATONVM_ZGC_DEFINE_MIRROR_RESERVED_EVERY_DEFINER",
                    Some("0"),
                )]),
            )
        });
        run(ctx)
    };
    let spent_refused = mock_reservation_calls().1;
    let spent_oome = matches!(
        &spent,
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                cratonvm_types::error::RuntimeError::OutOfMemoryError { .. }
            )
        ))
    );
    let spent_defined = ctx.last_define_full_opts().is_some();
    let spent_pins_balanced = ctx.native_pin_count_for_test() == pins;
    let (retry_defined, retry_pins_balanced) = if kill {
        (None, None)
    } else {
        clear(ctx);
        mock_reservation(MockReservation::Serve);
        let retry = run(ctx);
        let defined = retry.is_ok() && ctx.last_define_full_opts().is_some();
        if !defined {
            eprintln!("zs10nd9 row: the retry did not define: {retry:?}");
        }
        (Some(defined), Some(ctx.native_pin_count_for_test() == pins))
    };
    mock_reservation(MockReservation::Off);
    if !spent_oome && !kill {
        eprintln!("zs10nd9 row: the spent run answered {spent:?}");
    }
    SpentDefineRow {
        spent_oome,
        spent_defined,
        spent_pins_balanced,
        spent_refused,
        retry_defined,
        retry_pins_balanced,
    }
}

/// zs s10/nd9: the default arm's verdict -- refused before the define, the
/// `OutOfMemoryError`, no pin left; the retry defines, no pin left.
pub(crate) fn assert_reserved_row(what: &str, row: &SpentDefineRow) {
    assert!(
        row.spent_refused >= 1,
        "{what}: the reservation was asked: {row:?}"
    );
    assert!(
        row.spent_oome,
        "{what}: a spent ladder is the define's OOME: {row:?}"
    );
    assert!(
        !row.spent_defined,
        "{what}: nothing defined on a spent ladder: {row:?}"
    );
    assert!(
        row.spent_pins_balanced,
        "{what}: the spent run left a pin: {row:?}"
    );
    assert_eq!(
        row.retry_defined,
        Some(true),
        "{what}: the retry defines: {row:?}"
    );
    assert_eq!(
        row.retry_pins_balanced,
        Some(true),
        "{what}: the retry left a pin: {row:?}"
    );
}

/// zs s10/nd9: the control arm's verdict -- no reservation asked, the body
/// defined before any refusal could stop it.
pub(crate) fn assert_kill_row(what: &str, row: &SpentDefineRow) {
    assert_eq!(row.spent_refused, 0, "{what} kill: no reservation: {row:?}");
    assert!(
        row.spent_defined,
        "{what} kill: the body defined first: {row:?}"
    );
}

/// Make the next `refusals` fallible allocations on this thread
/// (`try_alloc_object`, `try_new_array`, `try_new_ref_array`) answer `None`,
/// and set whether `reclaim_before_alloc_retry` reports a retry as worth
/// making. Resets the reclaim counter.
pub(crate) fn mock_refuse_allocations(refusals: usize, reclaim_helps: bool) {
    MOCK_ALLOC_REFUSALS.with(|c| c.set(refusals));
    MOCK_RECLAIM_HELPS.with(|c| c.set(reclaim_helps));
    MOCK_RECLAIMS.with(|c| c.set(0));
}

/// How many times `reclaim_before_alloc_retry` ran on this thread since the
/// last [`mock_refuse_allocations`].
pub(crate) fn mock_reclaims() -> usize {
    MOCK_RECLAIMS.with(std::cell::Cell::get)
}

/// Consume one pending refusal; `true` when this allocation must refuse.
fn mock_take_refusal() -> bool {
    MOCK_ALLOC_REFUSALS.with(|c| {
        let left = c.get();
        if left > 0 {
            c.set(left - 1);
        }
        left > 0
    })
}

impl cratonvm_native_api::NativeHeapAccess for MockNativeContext {
    fn mark_reference_manually_enqueued(&mut self, reference_obj: ObjectRef) {
        self.ref_hook_log
            .push(("retire", reference_obj.as_ptr() as usize));
    }

    fn cancel_reference_allocation_note(&mut self, reference_obj: ObjectRef) -> bool {
        self.ref_hook_log
            .push(("cancel", reference_obj.as_ptr() as usize));
        false
    }

    fn new_object(&mut self, class_name: &str) -> MethodCallResult {
        let cid = self.ensure_class_initialized(class_name)?;
        let obj = self.alloc_entry(HeapEntry::Object {
            class_id: cid,
            fields: vec![Value::Int(0); 4],
        });
        Ok(Some(Value::Object(Some(obj))))
    }

    fn identity_hash_code(&self, obj: ObjectRef) -> i32 {
        // MH5-1: a moved object keeps the hash of its first address (the
        // default mode never moves, so that is the address itself).
        if self.moving.get() {
            let index = self.entry_index(obj);
            // SAFETY: single-threaded test code; no aliasing of the cell.
            return unsafe { (&*self.first_addr.get())[index] } as i32;
        }
        obj.as_ptr() as i32
    }

    fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { fields, .. } => fields.get(index).copied().unwrap_or(Value::Int(0)),
            _ => Value::Int(0),
        }
    }

    fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
        if let Value::Object(Some(v)) = value {
            self.assert_not_retired(v.as_ptr() as usize, "stored");
        }
        let idx = self.entry_index(obj);
        match &mut self.heap_mut()[idx] {
            HeapEntry::Object { fields, .. } => {
                if index >= fields.len() {
                    fields.resize(index + 1, Value::Int(0));
                }
                fields[index] = value;
            }
            _ => {}
        }
    }

    /// # DIVERGENCE FROM PRODUCTION — read this before reasoning from a result
    ///
    /// **Production answers `Value::Object(None)` for a name it cannot
    /// resolve.** `NativeContextImpl::get_field_by_name`
    /// (`vm/src/vm/vm_exec.rs:10613-10623`) calls
    /// `resolve_field_index_in_hierarchy` and, on `None`, returns
    /// `Value::Object(None)`. The trait says so too
    /// (`native-api/src/registry.rs:2351-2354`). `native-api/src/test_mock.rs`
    /// agrees with production.
    ///
    /// **This mock answers `Value::Int(0)` instead**, unless a test opts in via
    /// [`MockNativeContext::set_absent_field_answers_null`].
    ///
    /// The consequence: a discriminator that reads a witness field's VALUE and
    /// treats `Object(None)` as "no such field" takes the OPPOSITE arm here
    /// from the arm it takes in the VM, so a unit test of it can be green while
    /// production is wrong. Two instances have already been paid for on this
    /// campaign — `lookup_define.rs`'s
    /// `alloc_lookup_for_still_writes_the_synthetic_indices` (green while
    /// production took the other arm; see its doc comment) and the corrected
    /// rationale on `classloader.rs`'s `url_field_read` fallback, which used to
    /// cite this mock's `Int(0)` as production behaviour. The remedy the campaign settled on is
    /// the CLASS-SIDE witness (`resolve_field_index_by_class_id(..).is_none()`,
    /// e.g. `classloader::cl_has_synthetic_layout`), which this mock answers
    /// faithfully; prefer it, and where you must test the value side, set the
    /// flag.
    ///
    /// Do NOT quote `Int(0)` as production behaviour. The tag production really
    /// does produce here is a different thing: an unwritten REFERENCE slot of a
    /// name that DOES resolve reads back as `Int(0)` because the zeroed 16-byte
    /// slot decodes to the niche-0 discriminant (`native-builtins/src/field_read.rs`
    /// module docs). That hazard is about a resolvable field, not an absent one.
    ///
    /// Why the default is still `Int(0)`: flipping it changes the answer under
    /// ~3 900 `native-builtins` tests reached from ~1 700 `get_field_by_name`
    /// call sites, and at least `field_read.rs`'s
    /// `by_name_read_of_an_unresolvable_name_is_int_zero_not_null` and
    /// `int_field_strict_refuses_an_unresolvable_field` assert the current
    /// answer directly. The flip is worth doing, but it has to be done by
    /// someone who can run `cargo test -p cratonvm-native-builtins` and fix the
    /// fallout in the same change.
    fn get_field_by_name(&self, obj: ObjectRef, field_name: &str) -> Value {
        // Mock: no class hierarchy parse. Tests that use
        // `make_field_mirror` write the synthetic 7-slot Field layout
        // directly; map the JDK Field field names to the corresponding
        // synthetic slot so the production code's `get_field_by_name`
        // path still reads the right value.
        let class_name = self.class_name_of_id(self.class_id_of_object(obj));
        // A field the test declared outranks every table; everything after it
        // is `mock_field_slot`, the single chain all four entry points share.
        let slot = self
            .declared_field_slot(obj, field_name)
            .or_else(|| mock_field_slot(class_name.as_deref(), field_name));
        match slot {
            Some(slot) => self.get_field(obj, slot),
            // See the DIVERGENCE note above: production returns
            // `Value::Object(None)` here (`vm/src/vm/vm_exec.rs:10621`).
            None if self.absent_field_answers_null.get() => Value::Object(None),
            None => Value::Int(0),
        }
    }

    fn set_field_by_name(&self, obj: ObjectRef, field_name: &str, value: Value) {
        let class_name = self.class_name_of_id(self.class_id_of_object(obj));
        // A field the test declared outranks every table; everything after it
        // is `mock_field_slot`, the single chain all four entry points share.
        let slot = self
            .declared_field_slot(obj, field_name)
            .or_else(|| mock_field_slot(class_name.as_deref(), field_name));
        if let Some(slot) = slot {
            self.set_field(obj, slot, value);
        }
        // Unknown name → silently ignore (matches real-JDK mode when
        // the field doesn't exist in the class hierarchy).
    }

    /// Resolve by class NAME. Production reaches for this one constantly —
    /// `java/lang/Enum.name`, `java/lang/Throwable.detailMessage`,
    /// `java/lang/StackTraceElement.declaringClass`, the whole
    /// `jdk.internal.foreign` segment family — and it used to consult a single
    /// hand-written table, so it answered `None` for all of them and every
    /// branch behind it was dead code under the mock.
    fn resolve_field_index(&self, class_name: &str, field_name: &str) -> Option<usize> {
        // Route through the class-id form when the mock knows the class, so a
        // field a test declared via `set_declared_fields` wins here too.
        if let Some(&id) = self.name_to_id.get(class_name) {
            return self.resolve_field_index_by_class_id(ClassId::new(id), field_name);
        }
        mock_field_slot(Some(class_name), field_name)
    }

    fn resolve_field_index_by_class_id(
        &self,
        class_id: ClassId,
        field_name: &str,
    ) -> Option<usize> {
        // Fields a test declared via `set_declared_fields` resolve here too.
        // The real implementation (`resolve_field_index_in_hierarchy`) walks
        // the class hierarchy over exactly this metadata. Without it a
        // predicate of the form "does this class declare <a field only the
        // REAL JDK class has>" is unfalsifiable under the mock — it could
        // only ever answer `None`, and a test of it would pass vacuously.
        // `classloader::cl_has_synthetic_layout` is such a predicate.
        if let Some(slot) = self
            .declared_fields(class_id)
            .iter()
            .find(|f| !f.is_static && f.name == field_name)
            .map(|f| f.slot_index)
        {
            return Some(slot);
        }
        // Then the same tail `get_field_by_name` / `set_field_by_name` use, in
        // the SAME order. The VM resolves a name against
        // `ClassManager::synthetic_stub_fields` for any class with no real
        // bytes, so a mock answering `None` for those is answering "no such
        // field" about fields the VM does resolve — which makes a predicate of
        // the form "does this class declare <a field only the REAL JDK class
        // has>" unfalsifiable, and a test of it vacuous.
        // (`classloader::cl_has_synthetic_layout` is such a predicate.)
        //
        // Then the SAME chain the by-name entry points use — see
        // `mock_field_slot`, which is the only place the order is written down.
        let class_name = self.class_name_of_id(class_id);
        mock_field_slot(class_name.as_deref(), field_name)
    }

    fn new_array(&mut self, element_type: ArrayElementType, length: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Int(0); length],
            element_type,
        })
    }

    fn new_ref_array(&mut self, class_id: ClassId, length: usize) -> ObjectRef {
        let arr = self.alloc_entry(HeapEntry::Array {
            elements: vec![Value::Object(None); length],
            element_type: ArrayElementType::Reference,
        });
        self.ref_array_components
            .borrow_mut()
            .insert(arr.as_ptr() as usize, class_id);
        arr
    }

    fn try_new_ref_array(&mut self, class_id: ClassId, length: usize) -> Option<ObjectRef> {
        if mock_take_refusal() {
            return None;
        }
        Some(self.new_ref_array(class_id, length))
    }

    fn try_new_array(
        &mut self,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if mock_take_refusal() {
            return None;
        }
        Some(self.new_array(element_type, length))
    }

    fn reclaim_before_alloc_retry(&mut self) -> bool {
        // Round 17 wave 2 (lane funnel): a reclaim collects in the VM, so it
        // is a GC point in both moving modes.
        if self.moving.get() {
            let _ = self.relocate_all();
        }
        MOCK_RECLAIMS.with(|c| c.set(c.get() + 1));
        MOCK_RECLAIM_HELPS.with(std::cell::Cell::get)
    }

    fn reclaim_after_failed_alloc_retry(&mut self) -> bool {
        MOCK_FAILED_RETRY_CALLS.with(|c| c.set(c.get() + 1));
        let pays = MOCK_FAILED_RETRY_PAYS.with(std::cell::Cell::get);
        if pays && self.moving.get() {
            let _ = self.relocate_all();
        }
        pays
    }

    // gc-common w20-b handoff: the VM's `init_string_from_units` allocates
    // with the NON-collecting `try_alloc_array_full` and can refuse; the
    // w8-b refusal knob reaches it here so the callers' retry ladder
    // (`lang_string::init_string_from_units_or_oom`) is testable. With no
    // pending refusal this is the trait default (the mock never moves an
    // object, so it needs no pin).
    fn init_string_from_units(&mut self, this: ObjectRef, units: &[u16]) -> bool {
        if mock_take_refusal() {
            return false;
        }
        // The VM's allocation here is the NON-collecting one, so even the
        // moving mode places it without a GC point (MH5-1).
        let arr = self.alloc_entry_in_place(HeapEntry::Array {
            elements: vec![Value::Int(0); units.len()],
            element_type: ArrayElementType::Char,
        });
        for (i, &u) in units.iter().enumerate() {
            self.set_array_element(arr, i, Value::Int(u as i32));
        }
        self.set_field(this, 0, Value::Object(Some(arr)));
        true
    }

    fn array_length(&self, obj: ObjectRef) -> usize {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Array { elements, .. } => elements.len(),
            _ => 0,
        }
    }

    fn object_is_array(&self, obj: ObjectRef) -> bool {
        let idx = self.entry_index(obj);
        matches!(&self.heap_ref()[idx], HeapEntry::Array { .. })
    }

    fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Array { elements, .. } => {
                elements.get(index).copied().unwrap_or(Value::Int(0))
            }
            _ => Value::Int(0),
        }
    }

    fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
        if let Value::Object(Some(v)) = value {
            self.assert_not_retired(v.as_ptr() as usize, "stored");
        }
        let idx = self.entry_index(obj);
        match &mut self.heap_mut()[idx] {
            HeapEntry::Array { elements, .. } => {
                if index < elements.len() {
                    elements[index] = value;
                }
            }
            _ => {}
        }
    }

    fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { .. } => ObjectKind::Object,
            HeapEntry::Array { .. } => ObjectKind::Array,
        }
    }

    fn heap_element_type_of(&self, obj: ObjectRef) -> ArrayElementType {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Array { element_type, .. } => *element_type,
            _ => ArrayElementType::Reference,
        }
    }

    fn create_string(&mut self, text: &str) -> ObjectRef {
        if MOCK_CREATE_STRING_OOM.with(std::cell::Cell::get) {
            std::panic::panic_any(MockAllocOom);
        }
        // Create char array
        let chars: Vec<u16> = text.encode_utf16().collect();
        let arr = self.alloc_entry(HeapEntry::Array {
            elements: chars.iter().map(|&c| Value::Int(c as i32)).collect(),
            element_type: ArrayElementType::Char,
        });
        // Resolve the canonical String class id so callers that inspect the
        // object's class (e.g. ObjectOutputStream's String fast-path) see
        // `java/lang/String` rather than the class-0 ("Object") default.
        let sid = self
            .ensure_class_initialized("java/lang/String")
            .map(|c| c.as_u32())
            .unwrap_or(0);
        // Create string object: field 0 = char[], field 1 = hash (0)
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(sid),
            fields: vec![Value::Object(Some(arr)), Value::Int(0)],
        })
    }

    fn read_string(&self, obj: ObjectRef) -> Option<String> {
        let arr_ref = match self.get_field(obj, 0) {
            Value::Object(Some(arr)) => arr,
            _ => return None,
        };
        let len = self.array_length(arr_ref);
        let mut chars = Vec::with_capacity(len);
        for i in 0..len {
            match self.get_array_element(arr_ref, i) {
                Value::Int(v) => chars.push(v as u16),
                _ => chars.push(0),
            }
        }
        // LOSSY, like production: an unpaired surrogate becomes U+FFFD rather
        // than turning the whole read into `None`. `String::from_utf16(..).ok()`
        // was the divergence — it made a string carrying a lone surrogate
        // unreadable under the mock, so `read_string_chars`, whose entire
        // purpose is to carry the units `read_string` cannot, could not be
        // contrasted against `read_string` at all.
        Some(String::from_utf16_lossy(&chars))
    }

    fn native_doors_collect_on_refusal(&self) -> bool {
        MOCK_RESERVATION.with(std::cell::Cell::get) != MockReservation::Off
    }

    /// zt t6/nd4: [`mock_heap_is_zgc`]'s answer (default `false`, the trait's).
    fn heap_is_zgc(&self) -> bool {
        MOCK_HEAP_IS_ZGC.with(std::cell::Cell::get)
    }

    /// zt t27/zc10: [`mock_zgc_concurrent_mark_active`]'s answer (default
    /// `false`, the trait's).
    fn zgc_concurrent_mark_active(&self) -> bool {
        MOCK_ZGC_CONC_MARK_ACTIVE.with(std::cell::Cell::get)
    }

    /// zt t8/x5: [`mock_zgc_soft_max_flag`]'s answer (default `None`).
    fn zgc_soft_max_heap_flag(&self) -> Option<(u64, u64, u8)> {
        MOCK_ZGC_SOFT_MAX_FLAG.with(std::cell::Cell::get)
    }

    /// zt t12/tg5: recorded for [`mock_exit_director_stops`].
    fn zgc_stop_director_tick_before_exit(&mut self) {
        let begun = self.blocking_begin_count;
        MOCK_EXIT_DIRECTOR_STOPS.with(|c| c.borrow_mut().push(begun));
    }

    /// gcj j1/rf: recorded for [`mock_take_keep_alive_log`].
    fn gc_reference_keep_alive(&mut self, referent: ObjectRef) {
        MOCK_KEEP_ALIVE_LOG.with(|l| {
            l.borrow_mut().push(MockKeepAlive::Reference(referent.as_ptr() as usize))
        });
    }

    /// gcj j1/rf: recorded for the unknown-strength referent read used by
    /// Unsafe, reflection, and handle getters.
    fn gc_referent_read_keep_alive(&mut self, receiver: ObjectRef, slot: usize, value: Value) {
        if slot == 0 && !self.object_is_array(receiver) {
            if let Value::Object(Some(referent)) = value {
                self.gc_referent_slot_read_keep_alive(receiver, slot, referent);
            }
        }
    }

    /// gcj j1/rf: recorded for [`mock_take_keep_alive_log`].
    fn gc_referent_slot_read_keep_alive(
        &mut self,
        receiver: ObjectRef,
        slot: usize,
        referent: ObjectRef,
    ) {
        MOCK_KEEP_ALIVE_LOG.with(|l| {
            l.borrow_mut().push(MockKeepAlive::ReferentSlot {
                receiver: receiver.as_ptr() as usize,
                slot,
                referent: referent.as_ptr() as usize,
            })
        });
    }

    /// zs s10/nd9: the reserved-mirror door under [`mock_reservation`]: a
    /// plain object stands for the mirror (the adoption below declines, so
    /// the definer mints as before).
    fn try_reserve_class_mirror(
        &mut self,
        _pooled_name: Option<&str>,
    ) -> cratonvm_native_api::MirrorReservation {
        match MOCK_RESERVATION.with(std::cell::Cell::get) {
            MockReservation::Off => cratonvm_native_api::MirrorReservation::Unsupported,
            MockReservation::Serve => {
                MOCK_RESERVATION_CALLS.with(|c| {
                    let (s, r) = c.get();
                    c.set((s + 1, r));
                });
                let object = self.alloc_object(ClassId::new(1), 4);
                cratonvm_native_api::MirrorReservation::Reserved(object)
            }
            MockReservation::Spent => {
                MOCK_RESERVATION_CALLS.with(|c| {
                    let (s, r) = c.get();
                    c.set((s, r + 1));
                });
                cratonvm_native_api::MirrorReservation::Refused
            }
        }
    }

    fn get_class_mirror(&mut self, class_id: ClassId) -> ObjectRef {
        let name = self
            .class_names
            .get(&class_id.as_u32())
            .cloned()
            .unwrap_or_else(|| format!("unknown_{}", class_id.as_u32()));
        let name_obj = self.create_string(&name);
        self.alloc_entry(HeapEntry::Object {
            class_id: ClassId::new(0),
            fields: vec![
                Value::Int(class_id.as_u32() as i32),
                Value::Object(Some(name_obj)),
            ],
        })
    }

    fn alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        self.alloc_entry(HeapEntry::Object {
            class_id,
            fields: vec![Value::Int(0); num_fields],
        })
    }

    // A mock override, sanctioned by `native-api/tests/layout_alias_coverage.rs`:
    // the mock never reaches the VM, so it is outside the layout-alias census
    // whatever it does.
    fn try_alloc_object(&mut self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        if mock_take_refusal() {
            return None;
        }
        Some(self.alloc_object(class_id, num_fields))
    }

    fn object_num_fields(&self, obj: ObjectRef) -> usize {
        let idx = self.entry_index(obj);
        match &self.heap_ref()[idx] {
            HeapEntry::Object { fields, .. } => fields.len(),
            _ => 0,
        }
    }

    // -- WP0.2 ObjectStreamClass cache overrides --

    fn osc_cache_get(&self, class_id: ClassId) -> Option<ObjectRef> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.osc_cache_map.get()).get(&class_id.as_u32()).copied() }
    }

    fn osc_cache_put(&self, class_id: ClassId, desc: ObjectRef) -> ObjectRef {
        // SAFETY: single-threaded test code.
        let map = unsafe { &mut *self.osc_cache_map.get() };
        *map.entry(class_id.as_u32()).or_insert(desc)
    }

    fn bound_method_type_get(&self, desc: &str) -> Option<ObjectRef> {
        self.bound_method_types.get(desc)
    }

    fn bound_method_type_put(&self, desc: &str, mt: ObjectRef) -> ObjectRef {
        self.bound_method_types.insert_if_absent(desc, mt)
    }

    fn get_field_volatile(&self, obj: ObjectRef, index: usize) -> Value {
        self.get_field(obj, index)
    }

    fn set_field_volatile(&self, obj: ObjectRef, index: usize, value: Value) {
        self.set_field(obj, index, value)
    }

    fn compare_and_swap_field(
        &mut self,
        obj: ObjectRef,
        index: usize,
        expected: Value,
        new_val: Value,
    ) -> bool {
        // t1 wave 8 (lane RL-ATOM; JIT round 21 lane unsafearr's mock fix
        // `9a8b6c8fe3`): a heap array's slot is its element, as the VM's CAS
        // takes either.
        if self.heap_kind_of(obj) == ObjectKind::Array {
            if self.get_array_element(obj, index) != expected {
                return false;
            }
            self.set_array_element(obj, index, new_val);
            return true;
        }
        let current = self.get_field(obj, index);
        if current == expected {
            self.set_field(obj, index, new_val);
            true
        } else {
            false
        }
    }

    fn allocate_instance(&mut self, class_name: &str) -> Option<ObjectRef> {
        let cid = self.ensure_class_initialized(class_name).ok()?;
        Some(self.alloc_object(cid, 4))
    }

    fn pin_native_root(&mut self, obj: ObjectRef) -> usize {
        let roots = unsafe { &mut *self.native_pin_roots.get() };
        let idx = roots.len();
        roots.push(obj);
        idx
    }

    // --- Handle scopes -----------------------------------------------------
    //
    // `NativeContext`'s DEFAULT `handle_root` pins (it delegates to
    // `pin_native_root`) while the default `handle_scope_push`/`pop` are
    // no-ops. That pairing is asymmetric: every native written against
    // `NativeHandleScope` would appear to leak a pin per rooted object for the
    // whole life of the mock, so a test asserting "this native releases its
    // roots" could never pass. The VM overrides all four; the mock now does
    // too, mapping scopes onto its existing pin stack so the balance is real.

    fn handle_scope_push(&mut self) {
        let depth = unsafe { (&*self.native_pin_roots.get()).len() };
        unsafe { (&mut *self.handle_scope_marks.get()).push(depth) };
    }

    fn handle_scope_pop(&mut self) {
        if let Some(base) = unsafe { (&mut *self.handle_scope_marks.get()).pop() } {
            self.unpin_native_roots(base);
        }
    }

    fn handle_get(&self, slot: u32) -> Option<ObjectRef> {
        // The mock never moves objects, so the pinned reference IS the
        // current address. `None` past the end lets `NativeHandle::get` fall
        // back, matching the trait's contract for a popped scope.
        unsafe { (&*self.native_pin_roots.get()).get(slot as usize).copied() }
    }

    fn read_native_pin(&self, handle: usize, fallback: ObjectRef) -> ObjectRef {
        let roots = unsafe { &*self.native_pin_roots.get() };
        // gc-common w21: a read past the end of the pin stack is a pin that a
        // release of an OLDER pin already dropped (`unpin_native_roots`
        // truncates). The VM silently answers the fallback, which after a
        // moving collection is a stale address (`common-w20v`); a test says so.
        assert!(
            handle == usize::MAX || handle < roots.len(),
            "read_native_pin({handle}) on a pin stack of {} -- the pin was released \
             (an older pin's unpin_native_roots truncates every newer one)",
            roots.len()
        );
        roots.get(handle).copied().unwrap_or(fallback)
    }

    fn unpin_native_roots(&mut self, base: usize) {
        let roots = unsafe { &mut *self.native_pin_roots.get() };
        if base < roots.len() {
            roots.truncate(base);
        }
    }

    fn add_global_root(&mut self, obj: ObjectRef) -> usize {
        let next = unsafe { &mut *self.next_global_root.get() };
        let handle = *next;
        *next = next.saturating_add(1).max(1);
        unsafe { &mut *self.global_roots.get() }.insert(handle, obj);
        handle
    }

    fn resolve_global_root(&self, handle: usize) -> Option<ObjectRef> {
        unsafe { &*self.global_roots.get() }.get(&handle).copied()
    }

    fn remove_global_root(&mut self, handle: usize) -> bool {
        unsafe { &mut *self.global_roots.get() }
            .remove(&handle)
            .is_some()
    }

    fn discover_reference(
        &mut self,
        _ref_type: u8,
        _reference_obj: ObjectRef,
        _referent: ObjectRef,
        _queue: Option<ObjectRef>,
    ) {
    }

    fn heap_allocated_bytes(&self) -> usize {
        0
    }
}

impl cratonvm_native_api::NativeThreadAccess for MockNativeContext {
    fn thread_id(&self) -> u64 {
        1
    }

    fn monitor_wait(&mut self, obj: ObjectRef, timeout_ms: Option<u64>) -> MethodCallResult {
        self.monitor_wait_calls.borrow_mut().push((obj, timeout_ms));
        if self.monitor_wait_interrupts.get() {
            return Err(cratonvm_types::error::RuntimeError::InterruptedException.into());
        }
        Ok(None)
    }

    fn monitor_wait_as(
        &mut self,
        obj: ObjectRef,
        timeout_ms: Option<u64>,
        kind: cratonvm_native_api::BlockKind,
    ) -> MethodCallResult {
        self.monitor_wait_kinds.borrow_mut().push(kind);
        self.monitor_wait(obj, timeout_ms)
    }

    fn monitor_notify(&mut self, _obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn monitor_notify_all(&mut self, _obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_start(&mut self, _thread_obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_join(&mut self, _thread_obj: ObjectRef) -> MethodCallResult {
        Ok(None)
    }

    fn thread_is_alive(&self, _thread_obj: ObjectRef) -> bool {
        false
    }

    fn current_thread_object(&mut self) -> ObjectRef {
        if let Some(thread) = self.stable_thread_object.get() {
            return thread;
        }
        self.alloc_object(ClassId::new(0), 2)
    }

    fn thread_interrupt(&mut self, _thread_obj: ObjectRef) {}

    fn is_interrupted(&self, clear: bool) -> bool {
        // SAFETY: single-threaded test code; no aliasing.
        let slot = unsafe { &mut *self.interrupted_flag.get() };
        let val = *slot;
        if val && clear {
            *slot = false;
        }
        val
    }

    fn park(&mut self, _timeout: Option<std::time::Duration>) {}

    fn unpark(&self, _thread_obj: ObjectRef) {}

    fn get_scoped_value(&self, _key_id: u64) -> Option<Value> {
        None
    }

    fn push_scoped_value(&mut self, _key_id: u64, _value: Value) {}

    fn pop_scoped_value(&mut self) {}

    fn scoped_value_depth(&self) -> usize {
        0
    }

    fn monitor_enter(&mut self, _obj: ObjectRef) {
        self.monitor_depth.set(self.monitor_depth.get() + 1);
    }
    fn monitor_exit(&mut self, _obj: ObjectRef) {
        self.monitor_depth.set(self.monitor_depth.get() - 1);
    }

    fn active_thread_count(&self) -> i32 {
        1
    }

    fn enumerate_threads(&self, _max: usize) -> Vec<ObjectRef> {
        Vec::new()
    }

    fn begin_blocking_region(&mut self) {
        self.blocking_begin_count += 1;
    }

    fn begin_blocking_region_as_on(
        &mut self,
        obj: ObjectRef,
        kind: cratonvm_native_api::BlockKind,
    ) {
        self.region_wait_objects.borrow_mut().push((obj, kind));
        self.begin_blocking_region_as(kind);
    }

    fn stand_in_wait_objects(&self) -> bool {
        self.stand_in_wait_objects_answer.get()
    }

    fn begin_vm_wait_region(&mut self) {
        self.vm_wait_begin_count += 1;
        self.blocking_begin_count += 1;
    }

    fn end_blocking_region(&mut self) {
        self.blocking_end_count += 1;
    }

    fn end_blocking_region_refs(&mut self, refs: &mut [Value]) {
        self.end_blocking_region();
        if self.moving.get() && self.region_end_gc_point.get() {
            let forward = self.relocate_all();
            for v in refs.iter_mut() {
                self.forward_value(&forward, v);
            }
            // The VM's stop-the-world epilogue
            // (`gc_and_alloc::process_references_after_gc`) re-addresses the
            // weak lock-key slots through the collection's pointer map, so a
            // moved object keeps its key. Do the same here, or a later
            // `gc_stable_weak_lock_key` of the moved object mints a fresh key
            // and every stand-in side table keyed by it misses (wave 79 host
            // run: the StampedLock unlock after a region-end relocation).
            let vm = cratonvm_native_api::NativeSystemAccess::vm_identity(self);
            crate::gc_sweep_and_remap_lock_keys(vm, &|_| true, &forward);
        }
    }

    fn begin_native_region_lazy(&mut self, band_lo: usize, band_hi: usize) -> bool {
        if !self.lazy_native_accept {
            return false;
        }
        self.lazy_native_log.0 += 1;
        self.lazy_native_log.2 = (band_lo, band_hi);
        true
    }

    fn end_native_region_lazy(&mut self) {
        self.lazy_native_log.1 += 1;
    }

    fn set_mh_wmte_raise_hint(&mut self, raise: Option<cratonvm_native_api::MhWmteRaise>) {
        self.mh_wmte_raise_hints.borrow_mut().push(raise);
    }
}

impl cratonvm_native_api::NativeExceptionAccess for MockNativeContext {
    /// zs s5/nd5: the mock's stand-in for the VM's `NativeAllocOom` payload
    /// ([`MockAllocOom`]).
    fn native_alloc_oom_failure(
        &self,
        payload: &(dyn std::any::Any + Send),
    ) -> Option<MethodCallFailed> {
        payload.downcast_ref::<MockAllocOom>().map(|_| {
            cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: "Java heap space (mock)".to_string(),
            }
            .into()
        })
    }

    fn capture_stack_trace(&mut self) -> Vec<StackTraceEntry> {
        Vec::new()
    }

    fn frame_class_ids(&self) -> Vec<ClassId> {
        // SAFETY: single-threaded test code.
        unsafe { (*self.frame_class_ids_override.get()).clone() }
    }
}

impl cratonvm_native_api::NativeGpuAccess for MockNativeContext {
    /// 2026-07-11: honour any test-provided `gpu_future_take_result_override`;
    /// defaults to the trait's default (`None`, "no GPU offload") when no
    /// override is set for `handle`. See `set_gpu_future_take_result`.
    fn gpu_future_take_result(&self, handle: u64) -> Option<GpuFutureResult> {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.gpu_future_take_result_override.get())
                .get(&handle)
                .copied()
        }
    }

    /// GpuStream affinity: counts the call and returns whatever
    /// `set_gpu_stream_create_result` scripted (`None` by default —
    /// the trait's own default, "no device"). See
    /// `gpu_stream_create_call_count`.
    fn gpu_stream_create(&mut self) -> Option<u64> {
        // SAFETY: single-threaded test code.
        unsafe {
            *self.gpu_stream_create_calls.get() += 1;
            *self.gpu_stream_create_override.get()
        }
    }

    /// GpuStream affinity: records `handle` for
    /// `gpu_stream_release_calls()` to read back.
    fn gpu_stream_release(&mut self, handle: u64) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.gpu_stream_release_calls.get()).push(handle);
        }
    }

    fn gpu_release_submission(&mut self, handle: u64) {
        // SAFETY: single-threaded test code.
        unsafe {
            (*self.gpu_release_submission_calls.get()).push(handle);
        }
    }
}

impl cratonvm_native_api::NativeSystemAccess for MockNativeContext {
    fn note_static_varhandle_target(
        &self,
        vh: ObjectRef,
        class_id: ClassId,
        field_index: usize,
        writable: bool,
        desc: &dyn Fn() -> Option<u8>,
    ) {
        self.static_vh_notes.borrow_mut().push((
            vh.as_ptr() as usize,
            class_id.as_u32(),
            field_index,
            writable,
            desc(),
        ));
    }

    fn reflect_door_cache_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        self.reflect_door_slot_enabled
            .get()
            .then_some(&self.reflect_door_slot)
    }

    fn is_jdk_only(&self) -> bool {
        // SAFETY: single-threaded test code; no aliasing.
        unsafe { *self.jdk_only_flag.get() }
    }

    fn stack_trace_origin_record(&self) -> bool {
        self.stack_trace_origin_record_flag.get()
    }

    fn unsafe_byte_field_offsets(&self) -> bool {
        self.unsafe_byte_field_offsets_flag.get()
    }

    fn jvmti_vm_object_alloc_armed(&self) -> bool {
        self.jvmti_vm_object_alloc_armed_flag.get()
    }

    fn stack_trace_origin_record_slot(
        &self,
    ) -> Option<&std::sync::OnceLock<Box<dyn std::any::Any + Send + Sync>>> {
        Some(&self.stack_trace_origin_records)
    }

    fn stack_trace_fill_name_from_text(&self) -> bool {
        self.stack_trace_fill_name_from_text_flag.get()
    }

    fn compat_stack_trace_keeps_array(&self) -> bool {
        self.compat_stack_trace_keeps_array_flag.get()
    }

    fn clv_sub_fast_key(&self) -> bool {
        self.clv_sub_fast_key_flag.get()
    }

    // Mirror the production NativeContext memory bridge, BOTH halves:
    // `vm_exec` routes a TAGGED `Unsafe.allocateMemory` handle to the arena
    // store and only falls through to a raw copy for an untagged address. This
    // mock used to do the raw copy unconditionally, so a test that handed it an
    // arena handle dereferenced the tag bit as an address — an access violation
    // on the first byte, which is how `set_memory_off_heap_fills_in_bulk_and_
    // stays_in_bounds` found this. The t27_tls direct-buffer tests still hand
    // this mock genuine malloc pointers (Vec backing stores), and those keep
    // taking the raw path.
    fn copy_from_native_memory(&self, addr: i64, out: &mut [u8]) -> bool {
        if crate::unsafe_arena_addr_is_tagged(addr) {
            return crate::unsafe_arena_copy_out(addr, out);
        }
        if addr <= 0 {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(addr as usize as *const u8, out.as_mut_ptr(), out.len());
        }
        true
    }

    fn copy_to_native_memory(&mut self, addr: i64, data: &[u8]) -> bool {
        if crate::unsafe_arena_addr_is_tagged(addr) {
            return crate::unsafe_arena_copy_in(addr, data);
        }
        if addr <= 0 {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), addr as usize as *mut u8, data.len());
        }
        true
    }

    fn reduce_signal_usage(&self) -> bool {
        self.reduce_signal_usage
    }

    fn supports_real_proxy_generation(&self) -> bool {
        false
    }

    fn record_printed_value(&mut self, _value: Value) {}

    fn record_printed_line(&mut self, _text: String) {}

    fn get_system_stream(&self, _name: &str) -> Option<ObjectRef> {
        None
    }

    fn get_system_property(&self, key: &str) -> Option<String> {
        self.properties.get(key).cloned()
    }

    fn set_system_property(&mut self, key: &str, value: &str) -> Option<String> {
        self.properties.insert(key.to_string(), value.to_string())
    }

    fn is_interface_class(&self, class_id: ClassId) -> bool {
        // SAFETY: single-threaded test code.
        let map = unsafe { &*self.is_interface_override.get() };
        map.get(&class_id.as_u32()).copied().unwrap_or(false)
    }

    fn register_native_thread(&mut self, name: &str, daemon: bool, join_handle_ptr: usize) -> u64 {
        // Drop the JoinHandle if one was passed — the mock context
        // doesn't model a real registry that can `.join()` on it,
        // and leaking it would prevent the OS thread from being
        // reaped at process exit.
        if join_handle_ptr != 0 {
            // SAFETY: caller built this via Box::into_raw; we
            // reconstruct and drop it.
            let _ = unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        }
        // SAFETY: single-threaded test code.
        let next = unsafe { &mut *self.next_native_tid.get() };
        let tid = *next;
        *next += 1;
        // SAFETY: same.
        unsafe {
            (*self.registered_native_threads.get()).push((name.to_string(), daemon, true));
        }
        tid
    }

    /// gcj j3/rt3 residual 5: a registered id gets a blocker whose
    /// [`MockNativeThreadBlocker::retire`] records the death, as the VM's
    /// marks the entry dead on the dying thread.
    fn native_thread_blocker(
        &self,
        thread_id: u64,
    ) -> Option<std::sync::Arc<dyn cratonvm_native_api::NativeThreadBlocker>> {
        if thread_id == 0 {
            return None;
        }
        // SAFETY: single-threaded test code.
        let known = self
            .native_tid_slot(thread_id)
            .is_some_and(|i| i < unsafe { (*self.registered_native_threads.get()).len() });
        if !known {
            return None;
        }
        Some(std::sync::Arc::new(MockNativeThreadBlocker {
            thread_id,
            retired: std::sync::Arc::clone(&self.retired_native_threads),
        }))
    }

    fn unregister_native_thread(&mut self, thread_id: u64) {
        if thread_id == 0 {
            return;
        }
        // FIX(test-isolation): index relative to this instance's tid
        // base (see `native_tid_slot`), not `thread_id - 1` — the base
        // is now shifted per instance for global uniqueness.
        let idx = match self.native_tid_slot(thread_id) {
            Some(i) => i,
            None => return,
        };
        // SAFETY: single-threaded test code.
        unsafe {
            let v = &mut *self.registered_native_threads.get();
            if let Some(entry) = v.get_mut(idx) {
                entry.2 = false;
            }
        }
    }

    fn attach_join_handle_to_native_thread(
        &mut self,
        thread_id: u64,
        join_handle_ptr: usize,
    ) -> bool {
        if thread_id == 0 || join_handle_ptr == 0 {
            return false;
        }
        // SAFETY: caller built via Box::into_raw; we drop the box
        // (mock doesn't model joining).
        let _ = unsafe { Box::from_raw(join_handle_ptr as *mut std::thread::JoinHandle<()>) };
        // FIX(test-isolation): base-relative index (see native_tid_slot).
        let idx = match self.native_tid_slot(thread_id) {
            Some(i) => i,
            None => return false,
        };
        // SAFETY: single-threaded test code.
        unsafe {
            let v = &*self.registered_native_threads.get();
            v.get(idx).is_some()
        }
    }

    fn set_native_thread_java_obj(&mut self, thread_id: u64, java_thread_obj: ObjectRef) -> bool {
        if thread_id == 0 {
            return false;
        }
        // FIX(test-isolation): base-relative index (see native_tid_slot).
        let idx = match self.native_tid_slot(thread_id) {
            Some(i) => i,
            None => return false,
        };
        // SAFETY: single-threaded test code.
        unsafe {
            let v = &*self.registered_native_threads.get();
            if v.get(idx).is_none() {
                return false;
            }
            (*self.native_thread_java_objs.get())
                .insert(thread_id, java_thread_obj.as_ptr() as usize);
        }
        true
    }

    fn static_field_index_by_name(&self, class_id: ClassId, field_name: &str) -> Option<usize> {
        let fields = unsafe { &*self.declared_fields_override.get() };
        if let Some(slot) = fields.get(&class_id.as_u32()).and_then(|fields| {
            fields
                .iter()
                .find(|f| f.is_static && f.name == field_name)
                .map(|f| f.slot_index)
        }) {
            return Some(slot);
        }

        let overrides = unsafe { &*self.enum_values_override.get() };
        if field_name == "$VALUES" && overrides.contains_key(&class_id.as_u32()) {
            Some(0)
        } else {
            None
        }
    }

    fn get_static_field(&self, class_id: ClassId, field_index: usize) -> Value {
        let statics = unsafe { &*self.static_fields_override.get() };
        if let Some(value) = statics.get(&(class_id.as_u32(), field_index)) {
            return *value;
        }

        let overrides = unsafe { &*self.enum_values_override.get() };
        if field_index == 0 {
            if let Some(&arr) = overrides.get(&class_id.as_u32()) {
                return Value::Object(Some(arr));
            }
        }
        Value::Int(0)
    }

    fn set_static_field(&mut self, class_id: ClassId, field_index: usize, value: Value) {
        let statics = unsafe { &mut *self.static_fields_override.get() };
        statics.insert((class_id.as_u32(), field_index), value);
    }

    /// Overrides the trait default (`0`) with whatever `set_vm_identity`
    /// installed, so a test can own a private VM identity. Untouched contexts
    /// still report `0`.
    fn vm_identity(&self) -> usize {
        self.vm_identity_override.get()
    }

    fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
        // Leak a static table for testing — tests won't actually use file I/O
        use std::sync::OnceLock;
        static FD_TABLE: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> =
            OnceLock::new();
        FD_TABLE.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
    }

    fn allocate_native_memory(&mut self, size: usize, align: usize) -> Option<(i64, *mut u8)> {
        let align = align.max(1);
        let size = size.max(1);
        let layout = std::alloc::Layout::from_size_align(size, align).ok()?;
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return None;
        }
        let allocs = unsafe { &mut *self.native_allocs.get() };
        let id_ref = unsafe { &mut *self.next_alloc_id.get() };
        let id = *id_ref;
        *id_ref += 1;
        allocs.insert(id, (ptr, layout));
        Some((id, ptr))
    }

    fn free_native_memory(&mut self, alloc_id: i64) {
        let allocs = unsafe { &mut *self.native_allocs.get() };
        if let Some((ptr, layout)) = allocs.remove(&alloc_id) {
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }

    fn load_native_library(&mut self, _path: &str) -> Result<i64, MethodCallFailed> {
        Ok(0)
    }

    fn find_native_symbol(&self, _lib_index: i64, name: &str) -> Option<usize> {
        // For testing, resolve symbols from the C runtime via platform-specific lookup
        #[cfg(target_os = "windows")]
        {
            use std::ffi::CString;
            let c_name = CString::new(name).ok()?;
            // Try msvcrt first, then ucrtbase
            let libs = ["msvcrt.dll\0", "ucrtbase.dll\0"];
            for lib in &libs {
                let handle = unsafe { winapi_GetModuleHandleA(lib.as_ptr() as *const i8) };
                if !handle.is_null() {
                    let addr = unsafe { winapi_GetProcAddress(handle, c_name.as_ptr()) };
                    if !addr.is_null() {
                        return Some(addr as usize);
                    }
                }
            }
            None
        }
        #[cfg(not(target_os = "windows"))]
        {
            use std::ffi::CString;
            let c_name = CString::new(name).ok()?;
            let addr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c_name.as_ptr()) };
            if addr.is_null() {
                None
            } else {
                Some(addr as usize)
            }
        }
    }

    fn register_upcall(&mut self, entry: cratonvm_native_api::ffi::UpcallEntry) -> usize {
        let entries = unsafe { &mut *self.upcall_entries.get() };
        let slot = entries.len();
        entries.push(entry);
        slot
    }

    fn get_upcall_info(&self, slot: usize) -> Option<(ObjectRef, Vec<i32>, i32)> {
        let entries = unsafe { &*self.upcall_entries.get() };
        entries
            .get(slot)
            .map(|e| (e.target, e.param_kinds.clone(), e.return_kind))
    }

    fn loaded_class_count(&self) -> usize {
        0
    }

    fn gc_collection_count(&self) -> u64 {
        // MH5-1: the moving mode's relocations (0 for a context that never
        // moved, as before).
        self.relocations.get()
    }

    fn gc_collectors(&self) -> Vec<cratonvm_native_api::GcCollectorInfo> {
        self.gc_beans_override.borrow().0.clone()
    }

    fn gc_memory_pools(&self) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
        self.gc_beans_override.borrow().1.clone()
    }

    fn non_heap_memory_pools(&self) -> Vec<cratonvm_native_api::GcMemoryPoolInfo> {
        self.non_heap_pools_calls
            .set(self.non_heap_pools_calls.get() + 1);
        self.non_heap_pools_override.borrow().clone()
    }

    /// gen r4w3/obs2: recorded, and "a bean of that name exists" iff the
    /// context describes collectors (`set_gc_beans`), as the VM answers.
    fn set_gc_notification_enabled(&self, collector: &str, enabled: bool) -> bool {
        self.gc_notification_enabled_calls
            .borrow_mut()
            .push((collector.to_string(), enabled));
        self.gc_beans_override
            .borrow()
            .0
            .iter()
            .any(|c| c.name == collector)
    }

    fn force_gc(&mut self) {}

    fn run_finalization(&mut self) {
        self.run_finalization_calls
            .set(self.run_finalization_calls.get() + 1);
    }

    fn wait_for_reference_processing(&mut self) -> bool {
        self.wait_for_reference_processing_calls
            .set(self.wait_for_reference_processing_calls.get() + 1);
        self.ref_processing_answer.get()
    }

    fn register_if_finalizable(&mut self, obj: ObjectRef) {
        self.register_if_finalizable_calls.borrow_mut().push(obj);
    }

    fn withdraw_finalizable_before_constructor(&mut self, obj: ObjectRef, epoch: u64) {
        self.withdraw_finalizable_before_constructor_calls
            .borrow_mut()
            .push((obj, epoch));
    }

    fn reference_queue_wake_ups_delivered(&self) -> bool {
        self.queue_wake_ups_delivered.get()
    }
}

impl Drop for MockNativeContext {
    fn drop(&mut self) {
        let allocs = unsafe { &mut *self.native_allocs.get() };
        for (_, (ptr, layout)) in allocs.drain() {
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }
}

/// [`MockNativeContext::set_module_image`]'s scripted image.
#[derive(Default)]
pub(crate) struct MockModuleImage {
    present: bool,
    class_modules: HashMap<u32, String>,
    boot_modules: Vec<String>,
    urls: HashMap<String, Vec<String>>,
}

/// Create a fresh MockNativeContext for testing.
pub(crate) fn mock_ctx() -> MockNativeContext {
    MockNativeContext::new()
}

/// MH5-1 (JIT round 16 wave 8, lane mh): the moving mode itself.
#[cfg(test)]
mod moving_heap_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    fn panics_stale(f: impl FnOnce()) -> bool {
        match catch_unwind(AssertUnwindSafe(f)) {
            Ok(()) => false,
            Err(e) => {
                let text = e
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default();
                text.contains("stale ObjectRef")
            }
        }
    }

    /// Off by default: nothing moves, the collection count stays 0, an
    /// address is its identity hash -- exactly what every existing test saw.
    #[test]
    fn the_default_mode_never_moves() {
        let mut ctx = MockNativeContext::new();
        let a = ctx.create_string("a");
        let _ = ctx.create_string("b");
        assert_eq!(ctx.relocations_for_test(), 0);
        assert_eq!(ctx.gc_collection_count(), 0);
        assert_eq!(ctx.read_string(a).as_deref(), Some("a"));
        assert_eq!(ctx.identity_hash_code(a), a.as_ptr() as i32);
    }

    /// An allocation moves every object; roots (pins, global roots) and
    /// reference fields follow, the old address is retired, the identity
    /// hash stays, and the collection count moves.
    #[test]
    fn an_allocation_moves_every_object_and_forwards_the_roots() {
        let mut ctx = MockNativeContext::new();
        let a = ctx.create_string("a");
        let hash = ctx.identity_hash_code(a);
        let global = ctx.add_global_root(a);
        let pin = ctx.pin_native_root(a);
        ctx.set_moving_heap(true);
        let _ = ctx.alloc_object(ClassId::new(0), 1);
        assert_eq!(ctx.relocations_for_test(), 1);
        assert_eq!(ctx.gc_collection_count(), 1);
        let moved = ctx
            .resolve_global_root(global)
            .expect("the global root is kept");
        assert_ne!(moved, a, "the object moved");
        assert_eq!(ctx.read_native_pin(pin, a), moved, "the pin was forwarded");
        // The String's char[] field was forwarded with it.
        assert_eq!(ctx.read_string(moved).as_deref(), Some("a"));
        assert_eq!(ctx.identity_hash_code(moved), hash);
        assert!(panics_stale(|| {
            let _ = ctx.get_field(a, 0);
        }));
        ctx.unpin_native_roots(pin);
        assert_eq!(ctx.native_pin_count_for_test(), 0);
    }

    /// Storing a retired reference into a live object is the defect's other
    /// half (the store succeeds in a real VM and the field dangles).
    #[test]
    fn storing_a_stale_reference_panics() {
        let mut ctx = MockNativeContext::new();
        let a = ctx.create_string("a");
        ctx.set_moving_heap(true);
        let holder = ctx.alloc_object(ClassId::new(0), 1);
        assert!(panics_stale(|| ctx.set_field(
            holder,
            0,
            Value::Object(Some(a))
        )));
        let arr = ctx.new_array(ArrayElementType::Reference, 1);
        assert!(panics_stale(|| ctx.set_array_element(
            arr,
            0,
            Value::Object(Some(a))
        )));
    }

    /// A Java call is a GC point: the callee sees its receiver and arguments
    /// at their new addresses, and a scripted result is forwarded too.
    #[test]
    fn a_java_call_moves_the_heap_and_forwards_its_arguments() {
        fn hook(
            ctx: &mut MockNativeContext,
            receiver: ObjectRef,
            _name: &str,
            _desc: &str,
            args: &[Value],
        ) -> Option<MethodCallResult> {
            // Both current: a stale one panics here.
            let _ = ctx.class_id_of_object(receiver);
            match args.first() {
                Some(Value::Object(Some(o))) => Some(Ok(Some(Value::Object(Some(*o))))),
                _ => None,
            }
        }
        let mut ctx = MockNativeContext::new();
        let receiver = ctx.create_string("r");
        let arg = ctx.create_string("x");
        ctx.set_invoke_virtual_hook(hook);
        ctx.set_moving_heap(true);
        let answer = ctx.invoke_virtual(
            receiver,
            "m",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[Value::Object(Some(arg))],
        );
        assert_eq!(ctx.relocations_for_test(), 1);
        match answer {
            Ok(Some(Value::Object(Some(o)))) => {
                assert_ne!(o, arg, "the argument moved");
                assert_eq!(ctx.read_string(o).as_deref(), Some("x"));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Round 17 wave 2 (lane funnel): the VM-rule mode moves the heap at a
    /// reclaim and at a Java call, never at an allocation, so an unpinned
    /// reference held across allocations stays current; switching back to
    /// the strict mode restores the allocation GC point.
    #[test]
    fn the_vm_rule_mode_moves_at_java_calls_and_reclaims_only() {
        let mut ctx = MockNativeContext::new();
        let a = ctx.create_string("a");
        ctx.set_moving_heap_vm_rule(true);
        let holder = ctx.alloc_object(ClassId::new(0), 1);
        let _ = ctx.new_array(ArrayElementType::Int, 4);
        let _ = ctx.create_string("b");
        assert_eq!(
            ctx.relocations_for_test(),
            0,
            "an allocation is no GC point"
        );
        ctx.set_field(holder, 0, Value::Object(Some(a)));
        assert_eq!(ctx.read_string(a).as_deref(), Some("a"));
        let pin = ctx.pin_native_root(holder);
        let _ = ctx.reclaim_before_alloc_retry();
        assert_eq!(ctx.relocations_for_test(), 1, "a reclaim is a GC point");
        let moved = ctx.read_native_pin(pin, holder);
        assert_ne!(moved, holder, "the pinned holder was forwarded");
        assert!(panics_stale(|| {
            let _ = ctx.get_field(holder, 0);
        }));
        ctx.unpin_native_roots(pin);
        let _ = ctx.invoke("x/Y", "m", "()V", &[]);
        assert_eq!(ctx.relocations_for_test(), 2, "a Java call is a GC point");
        ctx.set_moving_heap(true);
        let _ = ctx.alloc_object(ClassId::new(0), 1);
        assert_eq!(ctx.relocations_for_test(), 3, "strict again");
    }

    /// The VM's `init_string_from_units` allocates without a GC point, so
    /// its receiver stays where it is.
    #[test]
    fn init_string_from_units_is_no_gc_point() {
        let mut ctx = MockNativeContext::new();
        let s = ctx.alloc_object(ClassId::new(0), 2);
        ctx.set_moving_heap(true);
        assert!(ctx.init_string_from_units(s, &[0x68, 0x69]));
        assert_eq!(ctx.relocations_for_test(), 0);
        assert_eq!(ctx.read_string(s).as_deref(), Some("hi"));
    }
}

/// gcj j3/rt3 residual 5: the mock's [`cratonvm_native_api::NativeThreadBlocker`].
/// The blocked-region hooks do nothing (the mock has no barrier); `retire`
/// records the id once, from the dying thread.
pub(crate) struct MockNativeThreadBlocker {
    thread_id: u64,
    retired: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
}

impl cratonvm_native_api::NativeThreadBlocker for MockNativeThreadBlocker {
    fn publish_os_tid(&self) {}

    fn enter_blocked(&self) {}

    fn leave_blocked(&self) {}

    fn retire(&self) -> bool {
        let mut retired = self.retired.lock().unwrap_or_else(|e| e.into_inner());
        if retired.contains(&self.thread_id) {
            return false;
        }
        retired.push(self.thread_id);
        true
    }
}
