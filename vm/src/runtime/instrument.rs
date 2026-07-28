// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP2.4-A — `java.lang.instrument` runtime infrastructure.
//!
//! Provides the Rust-side state for the `Instrumentation` API exposed
//! by `sun.instrument.InstrumentationImpl` (and the public-facing
//! `java.lang.instrument.Instrumentation` interface). Mockito's
//! MockMaker agent + Jacoco's coverage agent both use this API to
//! redefine method bodies on already-loaded classes.
//!
//! This module owns:
//!
//!   * The process-wide [`TRANSFORMER_CHAIN`] — a single ordered list
//!     of `(transformer ObjectRef, canRetransform, nativeMethodPrefix)`
//!     entries shared by every `InstrumentationImpl`.
//!   * The native-method registrations on `sun/instrument/InstrumentationImpl`
//!     (`addTransformer0`, `removeTransformer`, `redefineClasses0`,
//!     `retransformClasses0`, `getAllLoadedClasses0`,
//!     `getInitiatedClasses0`, `isModifiableClass0`, `getObjectSize0`,
//!     `appendToBootstrapClassLoaderSearch0`,
//!     `appendToSystemClassLoaderSearch0`, `setNativeMethodPrefix0`,
//!     `isRetransformClassesSupported0`, `isRedefineClassesSupported0`,
//!     `isNativeMethodPrefixSupported0`).
//!   * A few in-process helper natives on `cratonvm/Instrument` so the
//!     `apps/instrument_probe` smoke fixture can run without a real
//!     `-javaagent:` (see `cratonvm.Instrument.{addTransformer,
//!     removeTransformer, getTransformerCount, getAllLoadedClasses,
//!     isModifiableClass, getObjectSize, redefineClass}`).
//!
//! # Transform pipeline
//!
//! ```text
//!  redefineClasses0([ClassDefinition...])  retransformClasses0([Class...])
//!         |                                       |
//!         v                                       v
//!   ┌────────────────────────────────────────────────────┐
//!   │  walk TRANSFORMER_CHAIN in registration order      │
//!   │  for each entry whose canRetransform == true:      │
//!   │      bytes = transformer.transform(loader,         │
//!   │                  className, classBeingRedefined,   │
//!   │                  protectionDomain, bytes)          │
//!   │      // null result → "no change", keep prior bytes│
//!   └────────────────────────────────────────────────────┘
//!         |
//!         v
//!   NativeContext::redefine_class(class_id, final_bytes)
//!         |
//!         v
//!   ClassManager::define_class_with_options(... allow_redefine=true ...)
//!   (Agent 2.4-B; this binding's redefine_class trait method routes here.)
//! ```
//!
//! # JVMTI integration
//!
//! `class_manager::redefine_class` already fires the JVMTI
//! `ClassFileLoadHook` event before parsing. Our retransform path
//! reuses that hook by piggy-backing on the same
//! `ClassManager::redefine_class` (or — until Agent 2.4-B's full
//! redefine path is wired in — `ClassManager::define_class_with_options`
//! with `allow_redefine = true`, which also fires the hook). That keeps
//! a single firing point for all redefine flows so JVMTI agents see
//! every transformation regardless of which API initiated it.

use std::collections::HashMap;
use std::sync::{Once, OnceLock, PoisonError, RwLock};

use cratonvm_native_api::{NativeCallback, NativeContext, NativeMethodRegistry};
use cratonvm_types::narrow_oop::ref_element_size;
use cratonvm_types::{
    error::{MethodCallFailed, MethodCallResult, VmError},
    ArrayElementType, ClassId, ObjectKind, ObjectRef, Value,
};

// ---------------------------------------------------------------------------
// Transformer chain
// ---------------------------------------------------------------------------

/// One entry in the process-wide [`TRANSFORMER_CHAIN`].
///
/// The Java `ClassFileTransformer` lives on the heap; we hold its
/// `ObjectRef`. `can_retransform` records the boolean flag passed to
/// `addTransformer(transformer, canRetransform)`. `native_method_prefix`
/// records the optional prefix set by
/// `setNativeMethodPrefix(transformer, prefix)`; it is recorded for
/// completeness but native-method-prefix dispatch is not yet wired into
/// the interpreter (the interpreter still resolves natives by exact
/// name).
#[derive(Debug, Clone)]
pub struct TransformerEntry {
    pub transformer_ref: ObjectRef,
    pub can_retransform: bool,
    pub native_method_prefix: Option<String>,
}

/// Process-wide `ClassFileTransformer` chain. The Java spec mandates a
/// single chain per JVM (per `Instrumentation` instance, but agents
/// share the same `Instrumentation`).  Order matters — JDK runs
/// transformers in registration order, threading each output as the
/// next input.
fn transformer_chain() -> &'static RwLock<Vec<TransformerEntry>> {
    static INSTANCE: OnceLock<RwLock<Vec<TransformerEntry>>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// GC-root integration
// ---------------------------------------------------------------------------
//
// The transformer chain stores live Java `ClassFileTransformer` instances as
// raw `ObjectRef`s (`TransformerEntry::transformer_ref`). Those references are
// process-global state that the collector's stack/static walk never reaches,
// so without the registration below a moving collection would (a) reclaim a
// transformer no Java root still points at — Mockito/JaCoCo register a
// transformer once and hold it only on the Java agent side — and (b) leave
// every surviving `transformer_ref` pointing at the object's *old* address
// after compaction. Either one yields a use-after-free or a dispatch onto a
// relocated object the next time `run_transformer_chain` invokes `transform`.
//
// We close that hole by registering this subsystem with the native-root
// registry: `scan_transformer_roots` folds every held ref into the root set
// (so the mark phase keeps the transformer alive) and `remap_transformer_refs`
// rewrites each held ref through the collector's relocation map after a moving
// collection. Registration is lazy and exactly-once (see
// [`add_transformer_entry`]).

/// Push every live transformer `ObjectRef` held by the chain into `roots`
/// so the collector treats it as a GC root. Null refs are skipped.
///
/// A GC must never miss a root, so on lock poisoning we recover the inner
/// guard and scan anyway rather than silently dropping the roots (a poisoned
/// chain still holds valid `ObjectRef`s that must survive the collection).
fn scan_transformer_roots(roots: &mut Vec<ObjectRef>) {
    let chain = transformer_chain()
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    for entry in chain.iter() {
        // `transformer_ref` is a non-null `ObjectRef`; the null-transformer
        // case is filtered out before an entry is ever pushed (see
        // `native_add_transformer0`). Guard anyway in case a future caller
        // seeds a sentinel.
        if !entry.transformer_ref.as_ptr().is_null() {
            roots.push(entry.transformer_ref);
        }
    }
}

/// Rewrite every held transformer `ObjectRef` through the collector's
/// `old-addr -> new-addr` relocation `map` after a moving collection. Refs
/// absent from the map were not relocated and are left unchanged.
///
/// As with [`scan_transformer_roots`], recover from lock poisoning rather than
/// skip: an un-remapped ref left pointing at a relocated object is a
/// use-after-free, so the remap must run even on a poisoned lock.
fn remap_transformer_refs(map: &HashMap<usize, usize>) {
    let mut chain = transformer_chain()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    for entry in chain.iter_mut() {
        let old_addr = entry.transformer_ref.as_ptr() as usize;
        if let Some(&new_addr) = map.get(&old_addr) {
            // SAFETY: `new_addr` is the relocated address the collector
            // assigned to this same live object; it is non-null and
            // heap-aligned by construction of the relocation map. Mirrors the
            // remap convention used throughout `memory::gc`.
            entry.transformer_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
        }
    }
}

/// Register [`scan_transformer_roots`]/[`remap_transformer_refs`] with the
/// native-root registry exactly once. Idempotent and cheap to call on every
/// transformer add; the `Once` collapses all but the first call to a load.
fn ensure_transformer_root_source_registered() {
    static REGISTERED: Once = Once::new();
    REGISTERED.call_once(|| {
        crate::memory::native_roots::register_native_root_source(
            scan_transformer_roots,
            remap_transformer_refs,
        );
    });
}

/// Append `(transformer, canRetransform)` to the global chain. Used by
/// both the `addTransformer0` native and the `addTransformer` helper on
/// `cratonvm/Instrument`.  Order: appended at the end.
pub fn add_transformer_entry(entry: TransformerEntry) {
    // Lazily wire the chain into the GC root set on first use. Doing it here
    // (rather than at startup) keeps the registry untouched for workloads that
    // never install a transformer, and guarantees the source is live before
    // any transformer ref can be reachable only from the chain.
    ensure_transformer_root_source_registered();
    if let Ok(mut chain) = transformer_chain().write() {
        chain.push(entry);
    }
}

/// Remove the first entry whose `transformer_ref` pointer-equals
/// `transformer`. Returns `true` if an entry was removed.
pub fn remove_transformer_entry(transformer: ObjectRef) -> bool {
    let mut chain = match transformer_chain().write() {
        Ok(g) => g,
        Err(_) => return false,
    };
    let before = chain.len();
    chain.retain(|e| e.transformer_ref.as_ptr() != transformer.as_ptr());
    before != chain.len()
}

/// Snapshot of every currently-registered transformer, in registration
/// order. The snapshot is owned so the caller can iterate without
/// holding the chain lock — important because invoking
/// `transformer.transform(...)` re-enters the VM and may itself
/// register or remove transformers (HotSpot's chain is reentrant-safe
/// in the same way).
pub fn snapshot_transformer_chain() -> Vec<TransformerEntry> {
    transformer_chain()
        .read()
        .map(|c| c.clone())
        .unwrap_or_default()
}

/// Returns the currently-registered transformer count.
pub fn transformer_count() -> usize {
    transformer_chain().read().map(|c| c.len()).unwrap_or(0)
}

/// Reset the chain. Used by VM shutdown / test isolation.
pub fn reset_transformer_chain() {
    if let Ok(mut chain) = transformer_chain().write() {
        chain.clear();
    }
}

/// Public hook for Agent 2.4-C's `agent_loader.rs`. After a `-javaagent:`
/// JAR's `premain` returns, agents typically call `addTransformer` on
/// the Instrumentation argument; that path goes through `addTransformer0`
/// and lands in [`add_transformer_entry`] just like in-process callers.
/// This wrapper is exported so the agent loader can pre-register
/// transformers from within its own setup path if it ever needs to (the
/// premain dispatch already runs Java code that drives `addTransformer0`,
/// so the wrapper is mainly for symmetry / testing).
pub fn add_premain_transformer(
    transformer_ref: ObjectRef,
    can_retransform: bool,
    native_method_prefix: Option<String>,
) {
    add_transformer_entry(TransformerEntry {
        transformer_ref,
        can_retransform,
        native_method_prefix,
    });
}

// ---------------------------------------------------------------------------
// Spec helpers
// ---------------------------------------------------------------------------

/// `Instrumentation.isModifiableClass` rules per JDK 25:
///   * Primitive `Class` mirrors → false.
///   * Array `Class` mirrors → false.
///   * Hidden classes → false (cannot be redefined).
///   * Everything else → true.
pub fn is_modifiable_class(is_primitive: bool, is_array: bool, is_hidden: bool) -> bool {
    !(is_primitive || is_array || is_hidden)
}

/// Approximate the heap footprint of a Java object, mirroring HotSpot's
/// `Instrumentation.getObjectSize` semantics:
///
///   * Regular objects: `header_size + slots * slot_size`. We use the
///     same constants as the heap (32-byte header, 16-byte slots —
///     matches `cratonvm_types::heap_types::HEADER_SIZE` /
///     `SLOT_SIZE`).
///   * Primitive arrays: `header_size + length * element_size`,
///     8-byte aligned.
///   * Reference arrays: `header_size + length * 8`, 8-byte aligned.
///
/// Spec wiggle-room: HotSpot says "implementation-specific approximation"
/// — the only hard requirement is `> 0` for any live object.
pub fn approximate_object_size(
    kind: ObjectKind,
    element_type: ArrayElementType,
    length: usize,
    num_slots: usize,
) -> i64 {
    use cratonvm_types::{element_byte_size, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE};
    let header = HEADER_SIZE as i64;
    let body = match kind {
        ObjectKind::Object => (num_slots * SLOT_SIZE) as i64,
        ObjectKind::Array => match element_type {
            ArrayElementType::Reference => (length * ref_element_size()) as i64,
            other => {
                let raw = length.saturating_mul(element_byte_size(other));
                // 8-byte align like the heap does.
                let aligned = (raw + 7) & !7;
                aligned as i64
            }
        },
        // Round-9 GC fix: humongous continuation regions get a filler header.
        // Instrumentation should report the region body bytes (we don't have
        // access to region size here so report header-only as a best effort).
        ObjectKind::HumongousFiller => 0,
    };
    let total = header + body;
    // Guarantee strictly positive — even a zero-length empty array has
    // a non-empty header.
    total.max(1)
}

// ---------------------------------------------------------------------------
// Native handlers — sun.instrument.InstrumentationImpl
// ---------------------------------------------------------------------------

const MOCKITO_INLINE_TRANSFORMER: &str =
    "org/mockito/internal/creation/bytebuddy/InlineBytecodeGenerator";

/// Mockito's inline maker is process-wide, but Spring's temporary modified
/// class paths can load a second copy of its implementation.  Its transformer
/// state cannot be composed with a second copy: both attempt to weave the same
/// JDK class, but their independently generated dispatch identifiers cannot
/// share a transformed definition.  Keep the first process-wide maker for the
/// VM lifetime and ignore later copies.  All other transformer types retain
/// their specified registration order and multiplicity.
fn add_instrumentation_transformer(
    ctx: &mut dyn NativeContext,
    transformer: ObjectRef,
    can_retransform: bool,
) {
    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(transformer))
        .unwrap_or_default();
    if class_name == MOCKITO_INLINE_TRANSFORMER {
        for existing in snapshot_transformer_chain() {
            let existing_name = ctx
                .class_name_of_id(ctx.class_id_of_object(existing.transformer_ref))
                .unwrap_or_default();
            if existing_name == MOCKITO_INLINE_TRANSFORMER {
                return;
            }
        }
    }
    add_transformer_entry(TransformerEntry {
        transformer_ref: transformer,
        can_retransform,
        native_method_prefix: None,
    });
}

fn native_add_transformer0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let transformer = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None), // null transformer → silently ignore (HotSpot NPEs; we tolerate)
    };
    let can_retransform = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    add_instrumentation_transformer(ctx, transformer, can_retransform);
    Ok(None)
}

/// `boolean removeTransformer(ClassFileTransformer transformer)`.
/// Args: [this, transformer].
fn native_remove_transformer(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let transformer = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let removed = remove_transformer_entry(transformer);
    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

/// `boolean isModifiableClass0(Class<?>)`. Args: [this, classMirror].
fn native_is_modifiable_class0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (is_primitive, is_array, is_hidden) = mirror_classification(ctx, mirror);
    Ok(Some(Value::Int(
        if is_modifiable_class(is_primitive, is_array, is_hidden) {
            1
        } else {
            0
        },
    )))
}

/// `Class<?>[] getAllLoadedClasses0()`. Args: [this].
fn native_get_all_loaded_classes0(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let class_ids = ctx.list_loaded_class_ids();
    let class_class_id = ctx
        .class_id_by_name("java/lang/Class")
        .unwrap_or(ClassId::new(0));
    let arr = ctx.new_ref_array(class_class_id, class_ids.len());
    for (i, cid) in class_ids.into_iter().enumerate() {
        let mirror = ctx.get_class_mirror(cid);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `Class<?>[] getInitiatedClasses0(ClassLoader loader)`.
/// Args: [this, loaderObject].
fn native_get_initiated_classes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // We don't dereference the loader object — we use the bootstrap /
    // app-loader-id 0 path for null and a synthetic mapping for
    // non-null. The `list_initiated_class_ids(0)` call returns the
    // application-loader classes (the common case).
    let loader_id = match args.get(1) {
        Some(Value::Object(Some(_))) => 0u32,
        _ => 0u32,
    };
    let class_ids = ctx.list_initiated_class_ids(loader_id);
    let class_class_id = ctx
        .class_id_by_name("java/lang/Class")
        .unwrap_or(ClassId::new(0));
    let arr = ctx.new_ref_array(class_class_id, class_ids.len());
    for (i, cid) in class_ids.into_iter().enumerate() {
        let mirror = ctx.get_class_mirror(cid);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `long getObjectSize0(Object obj)`. Args: [this, obj].
fn native_get_object_size0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let kind = ctx.heap_kind_of(obj);
    let element_type = ctx.heap_element_type_of(obj);
    let length = if matches!(kind, ObjectKind::Array) {
        ctx.array_length(obj)
    } else {
        0
    };
    let slots = ctx.object_num_fields(obj);
    let size = approximate_object_size(kind, element_type, length, slots);
    Ok(Some(Value::Long(size)))
}

/// `void redefineClasses0(ClassDefinition[] defs)`. Args: [this, arr].
///
/// JDK ClassDefinition layout (real JDK 25):
///   * field[0]: `Class mClass`
///   * field[1]: `byte[] mClassFile`
///
/// We tolerate either name resolution: try field-name first, fall back
/// to slot 0/1 for hand-rolled allocations from in-process probes.
fn native_redefine_classes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let inst_receiver = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let n = ctx.array_length(arr);
    for i in 0..n {
        let elem = match ctx.get_array_element(arr, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let class_mirror = read_class_def_field(ctx, elem, "mClass", 0);
        let bytes_arr = read_class_def_field(ctx, elem, "mClassFile", 1);
        let (target_class, target_class_id) = match class_mirror {
            Some(m) => match ctx.class_id_from_mirror(m) {
                Some(cid) => (m, cid),
                None => continue,
            },
            None => continue,
        };
        let bytes_obj = match bytes_arr {
            Some(o) => o,
            None => continue,
        };
        let new_bytes = read_byte_array(ctx, bytes_obj);
        // Run the chain — only canRetransform=true entries fire on
        // redefineClasses (HotSpot fires every transformer, gated by
        // canRetransform, and threads outputs).
        let final_bytes = run_transformer_chain(
            ctx,
            target_class_id,
            Some(target_class),
            &new_bytes,
            /* retransform_only = */ false,
            inst_receiver,
        );
        if let Err(msg) = ctx.redefine_class(target_class_id, &final_bytes) {
            tracing::warn!("redefineClasses0: {msg}");
        }
    }
    Ok(None)
}

/// `void retransformClasses0(Class<?>[] classes)`. Args: [this, arr].
fn native_retransform_classes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let inst_receiver = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let n = ctx.array_length(arr);
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!("[RETRANSFORM] retransformClasses0 called with {n} classes");
    }
    for i in 0..n {
        let mirror = match ctx.get_array_element(arr, i) {
            Value::Object(Some(o)) => o,
            _ => continue,
        };
        let class_id = match ctx.class_id_from_mirror(mirror) {
            Some(cid) => cid,
            None => continue,
        };
        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
            let nm = ctx.class_name_of_id(class_id).unwrap_or_default();
            let ob = original_class_bytes(ctx, class_id);
            eprintln!("[RETRANSFORM]   [{i}] {nm} original_bytes={}", ob.len());
        }
        // Look up the original bytes via the application classpath
        // resource finder using `<name>.class` — we always cache them
        // under that path on define. For real hidden classes / proxy
        // classes this fails and we fall back to an empty buffer; the
        // transformer is still given a chance to swap in fresh bytes.
        let original = original_class_bytes(ctx, class_id);
        // `retransformClasses` starts from the original class file.  Keep the
        // live method metadata in that same state while Java transformers run:
        // Byte Buddy combines the supplied bytes with reflection over
        // `classBeingRedefined`, and otherwise observes the previous woven
        // method attributes against the original byte stream.  This matters
        // when a later temporary test loader installs a second transformer.
        //
        // The first in-place swap preserves the original-byte cache, so the
        // final transformed swap below still has the required JVMTI base for a
        // future retransformation.  Both swaps preserve class identity and do
        // not run initializers.
        if !original.is_empty() {
            if let Err(msg) = ctx.retransform_class(class_id, &original) {
                tracing::warn!("retransformClasses0: could not restore original bytes: {msg}");
                continue;
            }
        }
        let final_bytes = run_transformer_chain(
            ctx,
            class_id,
            Some(mirror),
            &original,
            /* retransform_only = */ true,
            inst_receiver,
        );
        if final_bytes.is_empty() {
            continue;
        }
        // retransform (not redefine): preserve the class's original cached
        // bytes so a subsequent retransform re-runs the chain from the
        // original, avoiding double-instrumentation (mockStatic+mock).
        if let Err(msg) = ctx.retransform_class(class_id, &final_bytes) {
            tracing::warn!("retransformClasses0: {msg}");
        }
    }
    Ok(None)
}

/// `void appendToBootstrapClassLoaderSearch0(String path)`.
fn native_append_to_bootstrap_search0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.get(1) {
        Some(Value::Object(Some(s))) => match ctx.read_string(*s) {
            Some(t) => t,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    ctx.register_bootstrap_classpath(&[path]);
    Ok(None)
}

/// `void appendToSystemClassLoaderSearch0(String path)`.
fn native_append_to_system_search0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.get(1) {
        Some(Value::Object(Some(s))) => match ctx.read_string(*s) {
            Some(t) => t,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    ctx.register_dynamic_classpath(&[path]);
    Ok(None)
}

/// `void appendToClassLoaderSearch0(long jvmtiEnv, String jar, boolean isBootstrap)`.
///
/// JDK 25's `InstrumentationImpl` routes both
/// `appendToBootstrapClassLoaderSearch(JarFile)` and
/// `appendToSystemClassLoaderSearch(JarFile)` through this single native;
/// the trailing boolean selects bootstrap (`true`) vs system (`false`).
/// ByteBuddy's inline mock maker uses the bootstrap form to inject its
/// `MockMethodDispatcher` helper so it is visible to redefined JDK classes.
/// Either way we make the JAR's classes loadable by registering it on the
/// dynamic classpath. Args: `[this, jvmtiEnv:long, jar:String, isBootstrap:bool]`.
fn native_append_to_classloader_search0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.get(2) {
        Some(Value::Object(Some(s))) => match ctx.read_string(*s) {
            Some(t) => t,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    let is_bootstrap = matches!(args.get(3), Some(Value::Int(v)) if *v != 0);
    if is_bootstrap {
        // Classes here MUST load with the bootstrap loader: Mockito asserts
        // its injected MockMethodDispatcher has a null class loader.
        ctx.register_bootstrap_classpath(&[path]);
    } else {
        ctx.register_dynamic_classpath(&[path]);
    }
    Ok(None)
}

/// `void setNativeMethodPrefix0(ClassFileTransformer transformer, String prefix)`.
/// We record the prefix on the matching chain entry. The interpreter
/// does not yet consult prefixes on native dispatch — recording the
/// state means agents that read it back via reflection see what they
/// set.
fn native_set_native_method_prefix0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let transformer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let prefix = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    if let Ok(mut chain) = transformer_chain().write() {
        for entry in chain.iter_mut() {
            if entry.transformer_ref.as_ptr() == transformer.as_ptr() {
                entry.native_method_prefix = prefix.clone();
                if prefix.is_some() {
                    tracing::debug!(
                        "instrumentation: native_method_prefix recorded but not yet \
                         consulted at native dispatch"
                    );
                }
                return Ok(None);
            }
        }
    }
    Ok(None)
}

/// `boolean isRetransformClassesSupported0()`. Args: [this].
fn native_is_retransform_supported0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `boolean isRedefineClassesSupported0()`. Args: [this].
fn native_is_redefine_supported0(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

/// `boolean isNativeMethodPrefixSupported0()`. Args: [this].
///
/// Returns FALSE, and that is deliberate — flipped from a constant `true` on
/// 2026-07-27 (stub-removal wave 2).
///
/// WARNING to anyone tempted to flip it back: `true` here was an UNBACKED
/// capability claim, unlike its `isRetransformClassesSupported0` /
/// `isRedefineClassesSupported0` siblings above, whose capability really is
/// implemented (`native_retransform_classes0` +
/// `NativeContext::retransform_class`, plus the shadow-suppression guards
/// `native_shadow_suppressed_by_redefine` / `redefine_immune_reflection_native`
/// in `vm/src/runtime/interpreter.rs`).
///
/// `setNativeMethodPrefix0` and the JDK-25 bulk `setNativeMethodPrefixes` only
/// RECORD the requested prefix on the `TransformerEntry`; NOTHING consults
/// `TransformerEntry::native_method_prefix` at native dispatch. So under the
/// old `true` an agent was told "supported", registered a `$$pfx$$`-style
/// wrapper, and the wrapper silently never bound — the failure surfaced
/// arbitrarily far away as "native prefixing mysteriously does nothing".
/// Answering `false` makes the JDK's own `Instrumentation.setNativeMethodPrefix`
/// throw `UnsupportedOperationException` at the point of use, which names the
/// limitation precisely.
///
/// TO MAKE THIS `true` AGAIN, one thing has to become real first:
/// `TransformerEntry::native_method_prefix` must actually be consulted on the
/// native-method dispatch path, so that a method the agent has wrapped resolves
/// to `<prefix><name>` when the unprefixed native is absent (JVMTI
/// SetNativeMethodPrefix semantics). Until then this must stay `false`.
/// Grep anchor: `native_method_prefix` in this file — if the only hits are
/// still the struct field, the recorders, and the tests, nothing consults it.
fn native_is_prefix_supported0(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// Native handlers — cratonvm.Instrument (in-process bridge)
// ---------------------------------------------------------------------------
//
// Each of these is a **static** native (no receiver), so args[0] is the
// first user argument. The InstrumentProbe app exercises these.

fn native_bridge_add_transformer(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let transformer = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    add_transformer_entry(TransformerEntry {
        transformer_ref: transformer,
        can_retransform: true,
        native_method_prefix: None,
    });
    Ok(None)
}

fn native_bridge_remove_transformer(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let transformer = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if remove_transformer_entry(transformer) {
        1
    } else {
        0
    })))
}

fn native_bridge_get_transformer_count(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Int(transformer_count() as i32)))
}

fn native_bridge_get_all_loaded_classes(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let class_ids = ctx.list_loaded_class_ids();
    let class_class_id = ctx
        .class_id_by_name("java/lang/Class")
        .unwrap_or(ClassId::new(0));
    let arr = ctx.new_ref_array(class_class_id, class_ids.len());
    for (i, cid) in class_ids.into_iter().enumerate() {
        let mirror = ctx.get_class_mirror(cid);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_bridge_is_modifiable_class(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let mirror = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (is_primitive, is_array, is_hidden) = mirror_classification(ctx, mirror);
    Ok(Some(Value::Int(
        if is_modifiable_class(is_primitive, is_array, is_hidden) {
            1
        } else {
            0
        },
    )))
}

fn native_bridge_get_object_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let kind = ctx.heap_kind_of(obj);
    let element_type = ctx.heap_element_type_of(obj);
    let length = if matches!(kind, ObjectKind::Array) {
        ctx.array_length(obj)
    } else {
        0
    };
    let slots = ctx.object_num_fields(obj);
    Ok(Some(Value::Long(approximate_object_size(
        kind,
        element_type,
        length,
        slots,
    ))))
}

fn native_bridge_redefine_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mirror = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = match ctx.class_id_from_mirror(mirror) {
        Some(cid) => cid,
        None => return Ok(Some(Value::Int(0))),
    };
    let bytes_arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bytes = read_byte_array(ctx, bytes_arr);
    let final_bytes = run_transformer_chain(
        ctx,
        class_id,
        Some(mirror),
        &bytes,
        /* retransform_only = */ false,
        /* inst_receiver = */ None,
    );
    let ok = ctx.redefine_class(class_id, &final_bytes).is_ok();
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a `Class` mirror from a `ClassDefinition`-shaped object, falling
/// back to slot 0 for hand-rolled definitions that didn't go through
/// the named-field path.
fn read_class_def_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field_name: &str,
    fallback_slot: usize,
) -> Option<ObjectRef> {
    // Try named lookup first.
    let by_name = ctx.get_field_by_name(obj, field_name);
    if let Value::Object(Some(o)) = by_name {
        return Some(o);
    }
    if let Value::Object(Some(o)) = ctx.get_field(obj, fallback_slot) {
        return Some(o);
    }
    None
}

/// Read a Java `byte[]` array into a `Vec<u8>`. Returns an empty vec
/// for null / non-array / wrong-element-type inputs.
fn read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    if !matches!(ctx.heap_kind_of(arr), ObjectKind::Array) {
        return Vec::new();
    }
    if !matches!(ctx.heap_element_type_of(arr), ArrayElementType::Byte) {
        return Vec::new();
    }
    let n = ctx.array_length(arr);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push((v & 0xff) as u8),
            _ => out.push(0),
        }
    }
    out
}

/// Allocate a Java `byte[]` with the given contents.
fn alloc_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i8 as i32));
    }
    arr
}

/// Walk the registered transformer chains and produce the final
/// transformed byte buffer. Returns the input unchanged when no
/// transformer modifies it.
///
/// There are two transformer-registration surfaces and both must run:
///
/// 1. **JDK Java-side `TransformerManager`** (the surface real
///    `-javaagent:` agents use). The JDK's `InstrumentationImpl.addTransformer`
///    stores the transformer in a Java-side list field; our
///    `addTransformer0` native is **never reached** because the
///    real-JDK Java code never calls it. To deliver bytes to those
///    transformers we must call the package-private Java method
///    `InstrumentationImpl.transform(Module, ClassLoader, String,
///    Class, ProtectionDomain, byte[], boolean isRetransform)` —
///    that method picks `mRetransfomableTransformerManager` vs
///    `mTransformerManager` based on the boolean and delegates to
///    `TransformerManager.transform(...)` which iterates the
///    registered transformers and calls each one's `transform`
///    via `invokeinterface` on the 6-arg `(Module, ClassLoader,
///    String, Class, ProtectionDomain, byte[])[B` signature. The
///    interface's default 6-arg method delegates to the legacy
///    5-arg form, so transformers that override either signature
///    work.
///
/// 2. **Rust-side [`TRANSFORMER_CHAIN`]** (the surface the
///    in-process `cratonvm.Instrument.addTransformer` bridge and
///    [`add_premain_transformer`] use). For each entry we invoke
///    the legacy 5-arg `ClassFileTransformer.transform(ClassLoader,
///    String, Class, ProtectionDomain, byte[])` directly via
///    `invoke_virtual`.
///
/// Any transformer that returns null is treated as "no change"; the
/// previous bytes flow into the next transformer. Any thrown
/// exception is logged and the previous bytes are kept (per the JDK
/// spec — transformer failures must not break class loading).
///
/// `retransform_only`: when `true`, only entries with
/// `can_retransform == true` participate (rust-side chain) and the
/// `isRetransform` argument to the JDK transform is set true. When
/// `false` (`redefineClasses` path) every entry participates and
/// `isRetransform` is false.
///
/// `inst_receiver`: the `sun.instrument.InstrumentationImpl` mirror
/// for the active agent, when one is available. Required to invoke
/// the Java-side transformer chain. When `None`, only the rust-side
/// chain runs (in-process bridge path).
fn run_transformer_chain(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    class_mirror: Option<ObjectRef>,
    initial_bytes: &[u8],
    retransform_only: bool,
    inst_receiver: Option<ObjectRef>,
) -> Vec<u8> {
    let rust_chain = snapshot_transformer_chain();
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[RETRANSFORM]   run_transformer_chain: rust_chain={} entries, initial_bytes={}, retransform_only={retransform_only}",
            rust_chain.len(),
            initial_bytes.len()
        );
    }
    if rust_chain.is_empty() && inst_receiver.is_none() {
        return initial_bytes.to_vec();
    }
    // Resolve constants used on every iteration once.
    let class_name_str = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| String::new());
    let class_name_obj = ctx.create_string(&class_name_str);
    let mirror_arg = match class_mirror {
        Some(m) => Value::Object(Some(m)),
        None => Value::Object(None),
    };

    let mut bytes_vec = initial_bytes.to_vec();

    // 1. (Removed) — calling InstrumentationImpl.transform(Module,
    //    ClassLoader, String, Class, ProtectionDomain, byte[], boolean)
    //    Java-side currently panics deep inside the JDK 25 transformer
    //    pipeline (length-6 array indexed at 6 — likely a JDK-internal
    //    array op our reflection/varhandle dispatch mis-counts). Instead
    //    we route every transformer registration through the rust-side
    //    chain by overriding the Java public method
    //    `InstrumentationImpl.addTransformer(transformer, canRetransform)`
    //    with a native that records the transformer in [`TRANSFORMER_CHAIN`]
    //    (see `register_instrumentation_natives`).
    //
    //    `inst_receiver` remains a parameter for future re-enablement of
    //    the Java-side dispatch; today it is unused on this path. We
    //    silence the unused-parameter lint at the call site by reading
    //    it explicitly.
    let _ = inst_receiver;

    // 2. Rust-side chain: every registered transformer (whether the
    //    agent registered it via the public Java `addTransformer` (now
    //    a native, see [`register_instrumentation_natives`]), or via
    //    the internal `addTransformer0`, or via the in-process
    //    [`cratonvm.Instrument.addTransformer`] bridge).
    for entry in rust_chain {
        if retransform_only && !entry.can_retransform {
            continue;
        }
        let bytes_obj = alloc_byte_array(ctx, &bytes_vec);
        // NOTE: invoke_virtual prepends the receiver itself, so args
        // here must NOT include the receiver. Pass only the 5 user args.
        let args = [
            // loader: use null — matches what HotSpot does when the
            // class was loaded by the bootstrap or when we don't have
            // a per-class ClassLoader instance to surface.
            Value::Object(None),
            Value::Object(Some(class_name_obj)),
            mirror_arg,
            // protectionDomain: null is spec-legal.
            Value::Object(None),
            Value::Object(Some(bytes_obj)),
        ];
        let descriptor = "(Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/lang/Class;\
                          Ljava/security/ProtectionDomain;[B)[B";
        let result = ctx.invoke_virtual(entry.transformer_ref, "transform", descriptor, &args);
        let dbg = cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok();
        match result {
            Ok(Some(Value::Object(Some(out_obj)))) => {
                let out_bytes = read_byte_array(ctx, out_obj);
                if dbg {
                    eprintln!(
                        "[RETRANSFORM]     transformer {:?} -> {} bytes (was {})",
                        entry.transformer_ref,
                        out_bytes.len(),
                        bytes_vec.len()
                    );
                }
                if !out_bytes.is_empty() {
                    bytes_vec = out_bytes;
                }
            }
            Ok(_) => {
                if dbg {
                    eprintln!(
                        "[RETRANSFORM]     transformer {:?} -> null/void (no change)",
                        entry.transformer_ref
                    );
                }
            }
            Err(MethodCallFailed::ExceptionThrown(_)) => {
                tracing::warn!(
                    "transformer threw an exception while transforming {class_name_str}; \
                     keeping prior bytes"
                );
            }
            Err(MethodCallFailed::InternalError(VmError::Internal { ref message, .. })) => {
                tracing::warn!(
                    "transformer internal error while transforming {class_name_str}: \
                     {message}; keeping prior bytes"
                );
            }
            Err(_) => {
                tracing::warn!(
                    "transformer failed while transforming {class_name_str}; keeping prior bytes"
                );
            }
        }
    }
    bytes_vec
}

/// Original bytes for `class_id`, used by retransformClasses0 to seed
/// the transformer chain with the bytecode the class was loaded from.
/// Falls back to looking up the resource by `<name>.class` on the
/// application classpath when the per-class cache is unreachable.
/// Observability audit (2026-07-26) — DEFECT FIXED.
///
/// The preferred source, `ctx.class_bytes(class_id)`, reads the
/// `ClassManager::class_bytes_cache`. That cache is **FIFO-evicted at a
/// 16 MiB soft cap** (`classloading::DEFAULT_CLASS_BYTES_CACHE_CAP`). Any
/// real application blows through 16 MiB of class bytes during startup, so by
/// the time an agent calls `retransformClasses` — Mockito's inline mock maker
/// and JaCoCo both do, late, on demand — the target's original bytes have
/// very often been evicted. The fallback then reads
/// `find_resource("<name>.class")` off the **application classpath**, which
/// is not the class's defining loader.
///
/// Before this fix the fallback bytes were used unconditionally. The failure
/// scenario: two loaders each define their own `com/foo/Bar`, or a shaded jar
/// carries a `com/foo/Bar.class` that shadows the one actually loaded. The
/// retransform chain would then be seeded with a *different class body*, the
/// transformer would instrument that, and `retransform_class` would install
/// the result over the live class. A silently wrong class definition is far
/// harder to diagnose than a failed retransform, and there is no signal at
/// all that the cache missed.
///
/// We now (a) verify the fallback bytes really are a class file whose
/// `this_class` matches the class we were asked about, rejecting them
/// otherwise, and (b) log the cache miss so the eviction is visible. An
/// empty result makes `native_retransform_classes0` skip the class, which is
/// the safe outcome.
fn original_class_bytes(ctx: &dyn NativeContext, class_id: ClassId) -> Vec<u8> {
    // Prefer the ClassManager's class_bytes_cache (populated by every
    // define_class_with_options call). This is the only path that finds
    // dynamically-defined / hidden / agent-redefined classes; the
    // classpath find_resource path only sees on-disk class files.
    if let Some(bytes) = ctx.class_bytes(class_id) {
        if !bytes.is_empty() {
            return bytes;
        }
    }
    let name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let resource = format!("{name}.class");
    let fallback = ctx.find_resource(&resource).unwrap_or_default();
    if fallback.is_empty() {
        tracing::debug!(
            "retransform: no original bytes for `{name}` \
             (class_bytes_cache miss and no `{resource}` on the classpath); skipping"
        );
        return Vec::new();
    }
    match class_file_this_class(&fallback) {
        Some(found) if found == name => {
            tracing::debug!(
                "retransform: `{name}` bytes came from the classpath, not the \
                 class_bytes_cache (16 MiB FIFO cap — likely evicted)"
            );
            fallback
        }
        Some(found) => {
            tracing::warn!(
                "retransform: refusing to seed `{name}` from classpath resource \
                 `{resource}` — those bytes define `{found}`. Seeding the \
                 transformer chain with a different class would install a wrong \
                 class body. Skipping this retransform."
            );
            Vec::new()
        }
        None => {
            tracing::warn!(
                "retransform: refusing to seed `{name}` from classpath resource \
                 `{resource}` — not a parseable class file. Skipping."
            );
            Vec::new()
        }
    }
}

/// Extract the internal-form `this_class` name (e.g. `java/lang/String`) from
/// raw class-file bytes, without a full parse.
///
/// Returns `None` for anything that is not a well-formed enough class file to
/// answer the question. Used by [`original_class_bytes`] to make sure a
/// classpath-sourced fallback really describes the class being retransformed.
fn class_file_this_class(bytes: &[u8]) -> Option<String> {
    fn u16_at(b: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
    }

    if bytes.len() < 10 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let cp_count = u16_at(bytes, 8)? as usize;
    if cp_count == 0 {
        return None;
    }

    // Byte offset of each constant-pool entry's *tag*, indexed by CP index.
    // Index 0 is unused; long/double consume two indices.
    let mut offsets: Vec<usize> = vec![0; cp_count];
    let mut pos = 10usize;
    let mut idx = 1usize;
    while idx < cp_count {
        offsets[idx] = pos;
        let tag = *bytes.get(pos)?;
        pos += 1;
        let (payload, slots) = match tag {
            1 => {
                // Utf8: u2 length + that many bytes
                let len = u16_at(bytes, pos)? as usize;
                (2 + len, 1)
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => (4, 1), // Integer/Float/refs/NameAndType/(Invoke)Dynamic
            5 | 6 => (8, 2),                              // Long/Double take two CP slots
            7 | 8 | 16 | 19 | 20 => (2, 1),               // Class/String/MethodType/Module/Package
            15 => (3, 1),                                 // MethodHandle
            _ => return None,                             // unknown tag — bail rather than guess
        };
        pos = pos.checked_add(payload)?;
        if pos > bytes.len() {
            return None;
        }
        idx += slots;
    }

    // access_flags (u2), this_class (u2)
    let this_class_idx = u16_at(bytes, pos + 2)? as usize;
    let class_entry = *offsets.get(this_class_idx)?;
    if class_entry == 0 || *bytes.get(class_entry)? != 7 {
        return None;
    }
    let name_idx = u16_at(bytes, class_entry + 1)? as usize;
    let utf8_entry = *offsets.get(name_idx)?;
    if utf8_entry == 0 || *bytes.get(utf8_entry)? != 1 {
        return None;
    }
    let len = u16_at(bytes, utf8_entry + 1)? as usize;
    let start = utf8_entry + 3;
    let raw = bytes.get(start..start.checked_add(len)?)?;
    std::str::from_utf8(raw).ok().map(|s| s.to_string())
}

/// Inspect a `Class<?>` mirror and return `(is_primitive, is_array, is_hidden)`.
fn mirror_classification(ctx: &dyn NativeContext, mirror: ObjectRef) -> (bool, bool, bool) {
    // `class_id_from_mirror` returns None for primitive mirrors; we use
    // that as the primitive marker.
    let class_id = ctx.class_id_from_mirror(mirror);
    let is_primitive = class_id.is_none();
    if let Some(cid) = class_id {
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        let is_array = name.starts_with('[');
        let is_hidden = ctx.is_class_hidden(cid);
        return (false, is_array, is_hidden);
    }
    (is_primitive, false, false)
}

// ---------------------------------------------------------------------------
// Self-attach — com.sun.tools.attach.VirtualMachine (in-process dynamic agent)
// ---------------------------------------------------------------------------
//
// Tools that ship as a `java.lang.instrument` agent but are launched *without*
// `-javaagent:` (Mockito's inline mock maker, JaCoCo, several profilers) load
// their agent at runtime by *attaching to their own JVM*. ByteBuddy's
// `ByteBuddyAgent.install()` drives this: when `jdk.attach.allowAttachSelf` is
// `true` (CratonVM defaults it on — see `vm_init.rs`) it takes the in-process
// branch `Attacher.install(VirtualMachine.class, pid, agentJar, false, arg)`,
// which reflectively calls, on `com.sun.tools.attach.VirtualMachine`:
//
//   1. static `attach(String pid)`            -> a VirtualMachine handle
//   2. instance `loadAgent(String jar, String options)`
//   3. instance `detach()`
//
// HotSpot's real implementation speaks an out-of-process socket/pipe protocol
// to the target VM's attach listener. CratonVM has no attach listener and the
// target is always *this* process, so we intercept those three methods with
// natives that perform the agent load directly in-process: parse the agent
// JAR's manifest for its `Agent-Class`/`Launcher-Agent-Class`, make its
// classes loadable, build an `Instrumentation` mirror (the same one the
// `-javaagent:` premain path uses), and invoke the agent's
// `agentmain(String, Instrumentation)`. For ByteBuddy that runs
// `net.bytebuddy.agent.Installer.agentmain`, which stores the Instrumentation
// in a static field that `ByteBuddyAgent.doGetInstrumentation()` then reads
// back — completing self-attach so the inline mock maker initializes.

const VM_ATTACH_CLASS: &str = "com/sun/tools/attach/VirtualMachine";

/// `static VirtualMachine attach(String id)`. Args (static): `[idString]`.
///
/// We support only self-attach, so the requested process id is irrelevant:
/// return a bare `VirtualMachine` handle whose `loadAgent`/`detach` are the
/// natives below. The real `attach` static body (which spins up the
/// `AttachProvider` SPI and fails on CratonVM) is shadowed by this native.
fn native_vm_attach(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ctx.new_object(VM_ATTACH_CLASS)
}

/// `void loadAgent(String agentJar, String options)` /
/// `void loadAgent(String agentJar)`. Args: `[this, agentJar, options?]`.
fn native_vm_load_agent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let agent_jar = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let agent_jar = match agent_jar {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::warn!("self-attach loadAgent: null/empty agent JAR path");
            return Ok(None);
        }
    };
    let options = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    run_self_attach(ctx, &agent_jar, &options)
}

/// `void detach()`. No out-of-process connection to tear down.
fn native_vm_detach(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// Load the agent JAR at `agent_jar` into the running VM and invoke its
/// `agentmain`, mirroring the JVM's dynamic-attach agent-load sequence.
fn run_self_attach(
    ctx: &mut dyn NativeContext,
    agent_jar: &str,
    options: &str,
) -> MethodCallResult {
    use std::path::Path;

    // 1. Parse the agent JAR manifest. Dynamic attach resolves the agent
    //    entry point from `Launcher-Agent-Class` (JEP 330 style) or, more
    //    commonly, `Agent-Class`.
    let manifest = cratonvm_classloading::ClassPath::read_jar_manifest(Path::new(agent_jar));
    let (agent_class, can_redefine, can_retransform) = match &manifest {
        Some(m) => {
            let class = m
                .attributes
                .get("Launcher-Agent-Class")
                .or_else(|| m.attributes.get("Agent-Class"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let bool_attr = |k: &str| {
                m.attributes
                    .get(k)
                    .map(|v| v.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
            };
            (
                class,
                bool_attr("Can-Redefine-Classes"),
                bool_attr("Can-Retransform-Classes"),
            )
        }
        None => (None, false, false),
    };
    let agent_class = match agent_class {
        Some(c) => c,
        None => {
            tracing::warn!(
                "self-attach loadAgent: `{agent_jar}` has no Agent-Class/Launcher-Agent-Class \
                 manifest attribute"
            );
            return Ok(None);
        }
    };

    // 2. Make the agent's classes loadable. The agent JAR is frequently the
    //    library's own JAR (already on the app classpath) but may be a temp
    //    JAR that ByteBuddy extracted; add it either way (idempotent).
    ctx.register_dynamic_classpath(&[agent_jar.to_string()]);

    let agent_internal = agent_class.replace('.', "/");
    if let Err(e) = ctx.load_class(&agent_internal) {
        tracing::warn!("self-attach loadAgent: cannot load Agent-Class `{agent_class}`: {e:?}");
        return Ok(None);
    }

    // 3. Build the Instrumentation mirror (sun.instrument.InstrumentationImpl),
    //    pinning it across the allocating calls that follow.
    let inst = match ctx.new_object("sun/instrument/InstrumentationImpl")? {
        Some(Value::Object(Some(o))) => o,
        _ => {
            tracing::warn!("self-attach loadAgent: could not allocate InstrumentationImpl");
            return Ok(None);
        }
    };
    let pin = ctx.pin_native_root(inst);

    // Best-effort init: ctor is (jvmtiEnv:long, agentArgs:String,
    // isRedefineClasses:bool, isRetransformClasses:bool). The mirror's
    // observable behaviour comes from the natives we register, so a failed
    // ctor is non-fatal.
    {
        let args_str = ctx.create_string(options);
        let inst_now = ctx.read_native_pin(pin, inst);
        let _ = ctx.invoke(
            "sun/instrument/InstrumentationImpl",
            "<init>",
            "(JLjava/lang/String;ZZ)V",
            &[
                Value::Object(Some(inst_now)),
                Value::Long(0),
                Value::Object(Some(args_str)),
                Value::Int(if can_redefine { 1 } else { 0 }),
                Value::Int(if can_retransform { 1 } else { 0 }),
            ],
        );
    }

    // 4. Invoke agentmain(String, Instrumentation), falling back to the
    //    single-arg agentmain(String) form per the instrument spec.
    let two_arg = "(Ljava/lang/String;Ljava/lang/instrument/Instrumentation;)V";
    let one_arg = "(Ljava/lang/String;)V";
    let has_two = ctx.method_exists(&agent_internal, "agentmain", two_arg);
    let has_one = !has_two && ctx.method_exists(&agent_internal, "agentmain", one_arg);

    let opts_obj = ctx.create_string(options);
    let inst_now = ctx.read_native_pin(pin, inst);
    let result = if has_two {
        ctx.invoke(
            &agent_internal,
            "agentmain",
            two_arg,
            &[Value::Object(Some(opts_obj)), Value::Object(Some(inst_now))],
        )
    } else if has_one {
        ctx.invoke(
            &agent_internal,
            "agentmain",
            one_arg,
            &[Value::Object(Some(opts_obj))],
        )
    } else {
        tracing::warn!(
            "self-attach loadAgent: Agent-Class `{agent_class}` has no agentmain(String[,Instrumentation])"
        );
        Ok(None)
    };
    ctx.unpin_native_roots(pin);

    // Propagate a thrown agent exception (the real JVM would surface
    // AgentInitializationException); a normal Installer.agentmain just stores
    // the Instrumentation and returns void.
    result.map(|_| None)
}

/// Register the in-process self-attach surface on
/// `com.sun.tools.attach.VirtualMachine`.
pub fn register_self_attach_natives(r: &mut NativeMethodRegistry) {
    r.register(
        VM_ATTACH_CLASS,
        "attach",
        "(Ljava/lang/String;)Lcom/sun/tools/attach/VirtualMachine;",
        native_vm_attach,
    );
    r.register(
        VM_ATTACH_CLASS,
        "loadAgent",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        native_vm_load_agent,
    );
    r.register(
        VM_ATTACH_CLASS,
        "loadAgent",
        "(Ljava/lang/String;)V",
        native_vm_load_agent,
    );
    r.register(VM_ATTACH_CLASS, "detach", "()V", native_vm_detach);
}

// ---------------------------------------------------------------------------
// Registration entry points
// ---------------------------------------------------------------------------

/// Register every native handler this WP owns into `r`.
///
/// Wired into `vm/src/vm/vm_init.rs` alongside the other Wave-2 native
/// registrations (proxy, ServiceLoader, etc.).
pub fn register_instrumentation_natives(r: &mut NativeMethodRegistry) {
    let impl_class = "sun/instrument/InstrumentationImpl";
    // Two-arg add (transformer, canRetransform).
    r.register(
        impl_class,
        "addTransformer0",
        "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
        native_add_transformer0,
    );
    // Some JDK builds also have a one-arg variant that defaults
    // canRetransform=false.
    r.register(
        impl_class,
        "addTransformer0",
        "(Ljava/lang/instrument/ClassFileTransformer;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let with_can = [
                args.first().cloned().unwrap_or(Value::Object(None)),
                args.get(1).cloned().unwrap_or(Value::Object(None)),
                Value::Int(0),
            ];
            native_add_transformer0(ctx, &with_can)
        }) as NativeCallback,
    );
    // JDK 25 InstrumentationImpl.addTransformer(transformer, canRetransform) is
    // a Java method that stores the transformer in `mTransformerManager` /
    // `mRetransfomableTransformerManager`. We don't drive those Java fields
    // when we run our own rust-side chain, so any agent that calls this
    // public method (the spec-blessed entry point) would have its
    // transformer recorded only in JDK Java fields that our retransform
    // path can't see. Override the Java method with a native that records
    // the transformer in our rust-side chain — that way every agent
    // registration funnels into one place regardless of which API surface
    // the agent uses.
    r.register(
        impl_class,
        "addTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let transformer = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let can_retransform = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            add_instrumentation_transformer(ctx, transformer, can_retransform);
            Ok(None)
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "addTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let transformer = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            add_instrumentation_transformer(ctx, transformer, false);
            Ok(None)
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "removeTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;)Z",
        native_remove_transformer,
    );
    r.register(
        impl_class,
        "redefineClasses0",
        "(J[Ljava/lang/instrument/ClassDefinition;)V",
        // Some JDK builds prefix with the native_id long; just discard
        // it and forward to the standard handler.
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            // args = [this, nativeId(long takes 2 slots in JVM stacks
            // but is a single Value::Long here), defs[]]
            // Skip the long argument and pass [this, defs[]].
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let defs = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_redefine_classes0(ctx, &[receiver, defs])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "redefineClasses0",
        "([Ljava/lang/instrument/ClassDefinition;)V",
        native_redefine_classes0,
    );
    r.register(
        impl_class,
        "retransformClasses0",
        "(J[Ljava/lang/Class;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let classes = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_retransform_classes0(ctx, &[receiver, classes])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "retransformClasses0",
        "([Ljava/lang/Class;)V",
        native_retransform_classes0,
    );
    r.register(
        impl_class,
        "getAllLoadedClasses0",
        "()[Ljava/lang/Class;",
        native_get_all_loaded_classes0,
    );
    // JDK 25 (J)[Ljava/lang/Class; variant — first arg is `long jvmtienv`.
    r.register(
        impl_class,
        "getAllLoadedClasses0",
        "(J)[Ljava/lang/Class;",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            native_get_all_loaded_classes0(ctx, &[receiver])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "getInitiatedClasses0",
        "(Ljava/lang/ClassLoader;)[Ljava/lang/Class;",
        native_get_initiated_classes0,
    );
    r.register(
        impl_class,
        "getInitiatedClasses0",
        "(JLjava/lang/ClassLoader;)[Ljava/lang/Class;",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let loader = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_get_initiated_classes0(ctx, &[receiver, loader])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "isModifiableClass0",
        "(Ljava/lang/Class;)Z",
        native_is_modifiable_class0,
    );
    r.register(
        impl_class,
        "isModifiableClass0",
        "(JLjava/lang/Class;)Z",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let cls = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_is_modifiable_class0(ctx, &[receiver, cls])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "getObjectSize0",
        "(Ljava/lang/Object;)J",
        native_get_object_size0,
    );
    r.register(
        impl_class,
        "getObjectSize0",
        "(JLjava/lang/Object;)J",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let obj = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_get_object_size0(ctx, &[receiver, obj])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "appendToBootstrapClassLoaderSearch0",
        "(Ljava/lang/String;)V",
        native_append_to_bootstrap_search0,
    );
    r.register(
        impl_class,
        "appendToBootstrapClassLoaderSearch0",
        "(JLjava/lang/String;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let path = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_append_to_bootstrap_search0(ctx, &[receiver, path])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "appendToSystemClassLoaderSearch0",
        "(Ljava/lang/String;)V",
        native_append_to_system_search0,
    );
    // JDK 25 unified append: appendToClassLoaderSearch0(long jvmtiEnv,
    // String jar, boolean isBootstrap). Drives both the bootstrap- and
    // system-classloader append forms (Mockito inline mock maker injects its
    // MockMethodDispatcher into the bootstrap loader via this path).
    r.register(
        impl_class,
        "appendToClassLoaderSearch0",
        "(JLjava/lang/String;Z)V",
        native_append_to_classloader_search0,
    );
    r.register(
        impl_class,
        "appendToSystemClassLoaderSearch0",
        "(JLjava/lang/String;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let path = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_append_to_system_search0(ctx, &[receiver, path])
        }) as NativeCallback,
    );
    r.register(
        impl_class,
        "setNativeMethodPrefix0",
        "(Ljava/lang/instrument/ClassFileTransformer;Ljava/lang/String;)V",
        native_set_native_method_prefix0,
    );
    // JDK 25 setNativeMethodPrefixes(long, String[], boolean) — bulk variant,
    // and the form `Instrumentation.setNativeMethodPrefix` actually calls on
    // JDK 25. It used to discard the array outright, so on a JDK-25 class
    // library the single-transformer `setNativeMethodPrefix0` handler above was
    // never reached and an agent's prefixes vanished with no diagnostic while
    // `isNativeMethodPrefixSupported0` still answered "supported".
    //
    // The array is the owning `TransformerManager`'s prefixes in transformer
    // order, so replay it positionally over the chain entries whose
    // retransformability matches the flag. NOTE: as with the single-transformer
    // path, the recorded prefix is still NOT consulted at native dispatch (see
    // the debug note in `native_set_native_method_prefix0`) — this records the
    // agent's intent so it is observable rather than silently dropped.
    //
    // Since 2026-07-27 `isNativeMethodPrefixSupported0` answers FALSE, so the
    // JDK's own `Instrumentation.setNativeMethodPrefix` now throws
    // `UnsupportedOperationException` before it ever reaches this native. This
    // handler is kept (rather than reverted to a no-op) so that the recording
    // is already correct on the day the capability becomes real — see the
    // "TO MAKE THIS `true` AGAIN" note on `native_is_prefix_supported0`.
    r.register(
        impl_class,
        "setNativeMethodPrefixes",
        "(J[Ljava/lang/String;Z)V",
        |ctx, args| {
            let prefixes = match args.get(2) {
                Some(Value::Object(Some(arr))) => *arr,
                _ => return Ok(None),
            };
            let is_retransformable = args.get(3).and_then(Value::as_int).unwrap_or(0) != 0;
            let len = ctx.array_length(prefixes);
            let mut collected: Vec<Option<String>> = Vec::with_capacity(len);
            for index in 0..len {
                collected.push(match ctx.get_array_element(prefixes, index) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                });
            }
            if let Ok(mut chain) = transformer_chain().write() {
                let mut next = collected.into_iter();
                for entry in chain.iter_mut() {
                    if entry.can_retransform != is_retransformable {
                        continue;
                    }
                    match next.next() {
                        Some(prefix) => entry.native_method_prefix = prefix,
                        None => break,
                    }
                }
            }
            Ok(None)
        },
    );
    // setHasRetransformableTransformers(long, boolean) — JVMTI capability flag toggle.
    // We always claim retransform support; this setter is a no-op.
    // KEEP (constant, justified): unlike the prefix capability below, the
    // retransform claim IS backed — `native_retransform_classes0` /
    // `NativeContext::retransform_class` re-run the transformer chain from the
    // preserved original bytes, and the shadow-suppression guards in
    // `interpreter.rs` (`native_shadow_suppressed_by_redefine`, with the
    // `redefine_immune_reflection_native` allow-list) cede a redefined class's
    // methods to the woven bytecode. So there is no capability bit to gate on
    // and nothing for this setter to record.
    r.register(
        impl_class,
        "setHasRetransformableTransformers",
        "(JZ)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        impl_class,
        "setHasRetransformableTransformers",
        "(Z)V",
        |_ctx, _args| Ok(None),
    );
    // JDK 25 InstrumentationImpl natives take `long jvmtienv` (descriptor (J)Z).
    // Register both the (J)Z variant (the JDK 25 actual signature) and the
    // legacy ()Z variant (backstop in case some workloads see the older arity).
    r.register(
        impl_class,
        "isRetransformClassesSupported0",
        "(J)Z",
        native_is_retransform_supported0,
    );
    r.register(
        impl_class,
        "isRetransformClassesSupported0",
        "()Z",
        native_is_retransform_supported0,
    );
    r.register(
        impl_class,
        "isRedefineClassesSupported0",
        "(J)Z",
        native_is_redefine_supported0,
    );
    r.register(
        impl_class,
        "isRedefineClassesSupported0",
        "()Z",
        native_is_redefine_supported0,
    );
    r.register(
        impl_class,
        "isNativeMethodPrefixSupported0",
        "(J)Z",
        native_is_prefix_supported0,
    );
    r.register(
        impl_class,
        "isNativeMethodPrefixSupported0",
        "()Z",
        native_is_prefix_supported0,
    );

    // WP2.4 v3 follow-up — public-name aliases. The bench/wave2-4 agent
    // calls `isRetransformClassesSupported()` (no `0` suffix); in stock
    // OpenJDK this is a Java method on `InstrumentationImpl` that
    // forwards to the `0`-suffixed native. In synthetic-jdk mode our
    // class layout doesn't carry that Java method body, so the bytecode
    // resolution fails with `NoSuchMethodError`. Registering the
    // public-name native aliases routes the call to the same handler
    // and unblocks the agent's premain.
    r.register(
        impl_class,
        "isRetransformClassesSupported",
        "()Z",
        native_is_retransform_supported0,
    );
    r.register(
        impl_class,
        "isRedefineClassesSupported",
        "()Z",
        native_is_redefine_supported0,
    );
    r.register(
        impl_class,
        "isNativeMethodPrefixSupported",
        "()Z",
        native_is_prefix_supported0,
    );
    r.register(
        impl_class,
        "isModifiableClass",
        "(Ljava/lang/Class;)Z",
        native_is_modifiable_class0,
    );
    r.register(
        impl_class,
        "getObjectSize",
        "(Ljava/lang/Object;)J",
        native_get_object_size0,
    );
    r.register(
        impl_class,
        "getAllLoadedClasses",
        "()[Ljava/lang/Class;",
        native_get_all_loaded_classes0,
    );
    r.register(
        impl_class,
        "retransformClasses",
        "([Ljava/lang/Class;)V",
        native_retransform_classes0,
    );
    r.register(
        impl_class,
        "redefineClasses",
        "([Ljava/lang/instrument/ClassDefinition;)V",
        native_redefine_classes0,
    );
    // Constructor `<init>(JLjava/lang/String;ZZ)V` — JDK's
    // `sun.instrument.InstrumentationImpl(jvmtienv, agentArgs, isRedefine,
    // isRetransform)`; called from `native_self_attach_load_agent` above (the
    // `-javaagent:` path in `agent_loader::build_instrumentation_mirror` skips
    // the ctor entirely and hands back a bare allocation).
    //
    // KEEP (empty body, justified): the real ctor's only durable effects are
    // the `mNativeAgent` / `mEnvironmentSupports*` fields and a
    // `TransformerManager`, and NONE of them is readable here — every method
    // that would consult them (`addTransformer`, `retransformClasses`,
    // `redefineClasses`, `isRetransformClassesSupported`,
    // `isRedefineClassesSupported`, `isNativeMethodPrefixSupported`,
    // `isModifiableClass`, `getObjectSize`, `getAllLoadedClasses`) is
    // registered natively above and answers from `TRANSFORMER_CHAIN` and the
    // VM's own class tables. There is no JVMTI env to record, so an empty
    // body IS the implementation. This registration also appears in
    // `interpreter.rs`'s force-native-override table so it beats the real
    // bytecode, which would otherwise enter VM-private init we cannot honour.
    r.register(
        impl_class,
        "<init>",
        "(JLjava/lang/String;ZZ)V",
        |_ctx, _args| Ok(None),
    );

    // ---- in-process probe bridge ----
    let bridge_class = "cratonvm/Instrument";
    r.register(
        bridge_class,
        "addTransformer",
        "(Ljava/lang/Object;)V",
        native_bridge_add_transformer,
    );
    r.register(
        bridge_class,
        "removeTransformer",
        "(Ljava/lang/Object;)Z",
        native_bridge_remove_transformer,
    );
    r.register(
        bridge_class,
        "getTransformerCount",
        "()I",
        native_bridge_get_transformer_count,
    );
    r.register(
        bridge_class,
        "getAllLoadedClasses",
        "()[Ljava/lang/Class;",
        native_bridge_get_all_loaded_classes,
    );
    r.register(
        bridge_class,
        "isModifiableClass",
        "(Ljava/lang/Class;)Z",
        native_bridge_is_modifiable_class,
    );
    r.register(
        bridge_class,
        "getObjectSize",
        "(Ljava/lang/Object;)J",
        native_bridge_get_object_size,
    );
    r.register(
        bridge_class,
        "redefineClass",
        "(Ljava/lang/Class;[B)Z",
        native_bridge_redefine_class,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_objref(addr: usize) -> ObjectRef {
        // Build a dummy ObjectRef from a raw addr. Only used inside
        // unit tests where we never dereference it.
        unsafe { std::mem::transmute::<usize, ObjectRef>(addr) }
    }

    // ---- Observability audit (2026-07-26): retransform seed validation ----

    /// Build a minimal, well-formed class file whose only constant-pool
    /// entries are the `this_class` Class entry and its Utf8 name.
    fn minimal_class_file(name: &str) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]); // magic
        b.extend_from_slice(&0u16.to_be_bytes()); // minor
        b.extend_from_slice(&52u16.to_be_bytes()); // major
        b.extend_from_slice(&3u16.to_be_bytes()); // cp_count (indices 1..2)
                                                  // #1: CONSTANT_Class -> name_index 2
        b.push(7);
        b.extend_from_slice(&2u16.to_be_bytes());
        // #2: CONSTANT_Utf8
        b.push(1);
        b.extend_from_slice(&(name.len() as u16).to_be_bytes());
        b.extend_from_slice(name.as_bytes());
        // access_flags, this_class
        b.extend_from_slice(&0x0021u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b
    }

    #[test]
    fn obsaudit_this_class_extracted_from_minimal_class_file() {
        let bytes = minimal_class_file("com/foo/Bar");
        assert_eq!(
            class_file_this_class(&bytes).as_deref(),
            Some("com/foo/Bar")
        );
    }

    /// Long/Double constant-pool entries occupy two indices. If the walker
    /// got that wrong every subsequent offset would shift and `this_class`
    /// would resolve to the wrong entry — silently, since the result is still
    /// a plausible string.
    #[test]
    fn obsaudit_this_class_handles_long_double_double_slots() {
        let name = "a/B";
        let mut b = Vec::new();
        b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&52u16.to_be_bytes());
        // indices: 1 = Long (eats 1 and 2), 3 = Class, 4 = Utf8 => cp_count 5
        b.extend_from_slice(&5u16.to_be_bytes());
        b.push(5); // CONSTANT_Long
        b.extend_from_slice(&1234i64.to_be_bytes());
        b.push(7); // #3 CONSTANT_Class -> name_index 4
        b.extend_from_slice(&4u16.to_be_bytes());
        b.push(1); // #4 CONSTANT_Utf8
        b.extend_from_slice(&(name.len() as u16).to_be_bytes());
        b.extend_from_slice(name.as_bytes());
        b.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags
        b.extend_from_slice(&3u16.to_be_bytes()); // this_class = #3

        assert_eq!(class_file_this_class(&b).as_deref(), Some("a/B"));
    }

    #[test]
    fn obsaudit_this_class_rejects_non_class_files() {
        assert_eq!(class_file_this_class(&[]), None);
        assert_eq!(class_file_this_class(b"PK\x03\x04not a class"), None);
        // Right magic, truncated before the constant pool.
        assert_eq!(
            class_file_this_class(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52]),
            None
        );
        // Truncated mid-Utf8: length claims more bytes than are present.
        let mut short = minimal_class_file("com/foo/Bar");
        short.truncate(short.len() - 6);
        assert_eq!(class_file_this_class(&short), None);
    }

    /// The point of the extractor: a classpath resource that defines a
    /// *different* class must be distinguishable from the right one, so
    /// `original_class_bytes` can refuse to seed a retransform with it. The
    /// scenario is a shaded jar (or a second class loader) carrying its own
    /// `com/foo/Bar.class` while the live `com/foo/Bar` came from elsewhere
    /// and has since been evicted from the 16 MiB class_bytes_cache.
    #[test]
    fn obsaudit_this_class_distinguishes_shadowed_resource() {
        let wanted = "com/foo/Bar";
        let shadowed = minimal_class_file("shaded/com/foo/Bar");
        let found = class_file_this_class(&shadowed).expect("parseable");
        assert_ne!(
            found, wanted,
            "a shadowed resource must not be mistaken for the requested class"
        );
    }

    #[test]
    fn add_then_remove_transformer_roundtrip() {
        reset_transformer_chain();
        let t1 = fake_objref(0x1000);
        add_transformer_entry(TransformerEntry {
            transformer_ref: t1,
            can_retransform: false,
            native_method_prefix: None,
        });
        assert_eq!(transformer_count(), 1);
        let removed = remove_transformer_entry(t1);
        assert!(removed);
        assert_eq!(transformer_count(), 0);
    }

    #[test]
    fn remove_unknown_transformer_returns_false() {
        reset_transformer_chain();
        let t = fake_objref(0x2000);
        assert!(!remove_transformer_entry(t));
    }

    #[test]
    fn snapshot_preserves_order() {
        reset_transformer_chain();
        let t1 = fake_objref(0x1000);
        let t2 = fake_objref(0x2000);
        let t3 = fake_objref(0x3000);
        add_transformer_entry(TransformerEntry {
            transformer_ref: t1,
            can_retransform: true,
            native_method_prefix: None,
        });
        add_transformer_entry(TransformerEntry {
            transformer_ref: t2,
            can_retransform: false,
            native_method_prefix: Some("$pfx".into()),
        });
        add_transformer_entry(TransformerEntry {
            transformer_ref: t3,
            can_retransform: true,
            native_method_prefix: None,
        });
        let snap = snapshot_transformer_chain();
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x1000);
        assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
        assert_eq!(snap[1].native_method_prefix.as_deref(), Some("$pfx"));
        assert_eq!(snap[2].transformer_ref.as_ptr() as usize, 0x3000);
        // Cleanup.
        reset_transformer_chain();
    }

    #[test]
    fn is_modifiable_rejects_primitive_array_hidden() {
        assert!(is_modifiable_class(false, false, false));
        assert!(!is_modifiable_class(true, false, false));
        assert!(!is_modifiable_class(false, true, false));
        assert!(!is_modifiable_class(false, false, true));
        assert!(!is_modifiable_class(true, true, true));
    }

    #[test]
    fn reset_clears_transformer_chain() {
        reset_transformer_chain();
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x100),
            can_retransform: true,
            native_method_prefix: None,
        });
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x200),
            can_retransform: false,
            native_method_prefix: None,
        });
        assert_eq!(transformer_count(), 2);
        reset_transformer_chain();
        assert_eq!(transformer_count(), 0);
    }

    #[test]
    fn scan_transformer_roots_yields_every_ref() {
        reset_transformer_chain();
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x1000),
            can_retransform: true,
            native_method_prefix: None,
        });
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x2000),
            can_retransform: false,
            native_method_prefix: None,
        });
        let mut roots = Vec::new();
        scan_transformer_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(addrs, vec![0x1000, 0x2000]);
        reset_transformer_chain();
    }

    #[test]
    fn scan_transformer_roots_empty_chain_pushes_nothing() {
        reset_transformer_chain();
        let mut roots = Vec::new();
        scan_transformer_roots(&mut roots);
        assert!(roots.is_empty());
    }

    #[test]
    fn remap_transformer_refs_rewrites_relocated_entries() {
        reset_transformer_chain();
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x1000),
            can_retransform: true,
            native_method_prefix: None,
        });
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x2000),
            can_retransform: false,
            native_method_prefix: None,
        });
        // Relocate only the first entry; the second is absent from the map
        // and must be left untouched.
        let mut map: HashMap<usize, usize> = HashMap::new();
        map.insert(0x1000, 0x9000);
        remap_transformer_refs(&map);
        let snap = snapshot_transformer_chain();
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x9000);
        assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
        reset_transformer_chain();
    }

    #[test]
    fn remap_transformer_refs_empty_map_is_noop() {
        reset_transformer_chain();
        add_transformer_entry(TransformerEntry {
            transformer_ref: fake_objref(0x3000),
            can_retransform: true,
            native_method_prefix: None,
        });
        remap_transformer_refs(&HashMap::new());
        let snap = snapshot_transformer_chain();
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x3000);
        reset_transformer_chain();
    }

    #[test]
    fn add_premain_transformer_appends_to_chain() {
        reset_transformer_chain();
        let t = fake_objref(0x4000);
        add_premain_transformer(t, true, Some("__".into()));
        let snap = snapshot_transformer_chain();
        assert_eq!(snap.len(), 1);
        assert!(snap[0].can_retransform);
        assert_eq!(snap[0].native_method_prefix.as_deref(), Some("__"));
        reset_transformer_chain();
    }

    #[test]
    fn approximate_object_size_object() {
        // The compact header folds locking state into its metadata words:
        // header (32) + 5 legacy Value slots * 16 = 112.
        let sz = approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 5);
        assert_eq!(sz, 112);
    }

    #[test]
    fn approximate_object_size_byte_array() {
        // Header (32) + 10 bytes aligned up to 8 = 48.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Byte, 10, 0);
        assert_eq!(sz, 48);
    }

    #[test]
    fn approximate_object_size_long_array() {
        // Header (32) + 4 * 8 = 64.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Long, 4, 0);
        assert_eq!(sz, 64);
    }

    #[test]
    fn approximate_object_size_ref_array() {
        // Header (32) + 3 refs * 8 = 56.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Reference, 3, 0);
        assert_eq!(sz, 56);
    }

    #[test]
    fn approximate_object_size_empty_array_is_positive() {
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Int, 0, 0);
        assert!(sz > 0);
    }

    #[test]
    fn register_natives_includes_required_methods() {
        let mut r = NativeMethodRegistry::new();
        register_instrumentation_natives(&mut r);
        let impl_class = "sun/instrument/InstrumentationImpl";
        assert!(r
            .find(
                impl_class,
                "addTransformer0",
                "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
            )
            .is_some());
        assert!(r
            .find(
                impl_class,
                "removeTransformer",
                "(Ljava/lang/instrument/ClassFileTransformer;)Z",
            )
            .is_some());
        assert!(r
            .find(
                impl_class,
                "redefineClasses0",
                "([Ljava/lang/instrument/ClassDefinition;)V",
            )
            .is_some());
        assert!(r
            .find(impl_class, "retransformClasses0", "([Ljava/lang/Class;)V",)
            .is_some());
        assert!(r
            .find(impl_class, "getAllLoadedClasses0", "()[Ljava/lang/Class;")
            .is_some());
        assert!(r
            .find(impl_class, "isModifiableClass0", "(Ljava/lang/Class;)Z")
            .is_some());
        assert!(r
            .find(impl_class, "getObjectSize0", "(Ljava/lang/Object;)J")
            .is_some());
        assert!(r
            .find(impl_class, "isRetransformClassesSupported0", "()Z")
            .is_some());
        assert!(r
            .find(impl_class, "isRedefineClassesSupported0", "()Z")
            .is_some());
        assert!(r
            .find(impl_class, "isNativeMethodPrefixSupported0", "()Z")
            .is_some());
        assert!(r
            .find(
                impl_class,
                "appendToBootstrapClassLoaderSearch0",
                "(Ljava/lang/String;)V",
            )
            .is_some());
        assert!(r
            .find(
                impl_class,
                "appendToSystemClassLoaderSearch0",
                "(Ljava/lang/String;)V",
            )
            .is_some());
        assert!(r
            .find(
                impl_class,
                "setNativeMethodPrefix0",
                "(Ljava/lang/instrument/ClassFileTransformer;Ljava/lang/String;)V",
            )
            .is_some());
        assert!(r
            .find(
                impl_class,
                "getInitiatedClasses0",
                "(Ljava/lang/ClassLoader;)[Ljava/lang/Class;",
            )
            .is_some());
        // Bridge surface for the in-process probe.
        let bridge = "cratonvm/Instrument";
        assert!(r
            .find(bridge, "addTransformer", "(Ljava/lang/Object;)V")
            .is_some());
        assert!(r
            .find(bridge, "removeTransformer", "(Ljava/lang/Object;)Z")
            .is_some());
        assert!(r.find(bridge, "getTransformerCount", "()I").is_some());
        assert!(r
            .find(bridge, "getAllLoadedClasses", "()[Ljava/lang/Class;")
            .is_some());
        assert!(r
            .find(bridge, "isModifiableClass", "(Ljava/lang/Class;)Z")
            .is_some());
        assert!(r
            .find(bridge, "getObjectSize", "(Ljava/lang/Object;)J")
            .is_some());
        assert!(r
            .find(bridge, "redefineClass", "(Ljava/lang/Class;[B)Z")
            .is_some());
    }
}
