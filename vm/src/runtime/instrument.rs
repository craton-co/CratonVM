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
//!   * The per-VM transformer chain (`TransformerChains`) — one ordered list
//!     of `(transformer ObjectRef, canRetransform, nativeMethodPrefix)`
//!     entries per `vm_identity`, shared by every `InstrumentationImpl` in
//!     that VM and by nothing outside it.
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
//!   │  walk THIS VM's chain in registration order        │
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
use std::sync::{OnceLock, PoisonError, RwLock};

use cratonvm_native_api::{NativeCallback, NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::narrow_oop::ref_element_size;
use cratonvm_types::{
    error::{MethodCallFailed, MethodCallResult, VmError},
    ArrayElementType, ClassId, ObjectKind, ObjectRef, Value,
};

// ---------------------------------------------------------------------------
// Transformer chain
// ---------------------------------------------------------------------------

/// One entry in a VM's transformer chain (see `TransformerChains`).
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

/// `ClassFileTransformer` chains, **one per VM**, keyed on
/// `NativeContext::vm_identity` / `SharedVm::vm_identity`.
///
/// The Java spec mandates a single chain per JVM (per `Instrumentation`
/// instance, but agents share the same `Instrumentation`) — *per JVM*, not per
/// process. Order matters within a chain: the JDK runs transformers in
/// registration order, threading each output as the next input.
///
/// # Why this is keyed, and not a bare `Vec`
///
/// It used to be one process-wide `RwLock<Vec<TransformerEntry>>`. Entries hold
/// raw `ObjectRef`s — addresses in the heap of whichever VM registered them —
/// and this process can own several heaps at once (the inline test modules
/// build a `SharedVm` per test; `libcratonvm` can create more than one VM).
/// With a single chain:
///
///   * VM B's root scan reported VM A's transformer addresses to VM B's
///     collector, and VM B's post-move fixup rewrote VM A's entries through
///     VM B's relocation map — the "pointer into a heap this collector does
///     not own" hazard that already scoped the logmanager, security-manager
///     and `ObjectStreamClass` tables;
///   * a transformer installed in VM A was visible to, and ran against, class
///     loads in VM B;
///   * `reset_transformer_chain()` in one VM wiped another's chain.
///
/// See `vm-process-global-state-round-2.md`.
type TransformerChains = HashMap<usize, Vec<TransformerEntry>>;

fn transformer_chains() -> &'static RwLock<TransformerChains> {
    static INSTANCE: OnceLock<RwLock<TransformerChains>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Run `f` against `vm`'s chain under the write lock, creating an empty chain
/// if this VM has none yet. Recovers from lock poisoning rather than skipping:
/// the GC halves below must never silently no-op.
fn with_chain_mut<R>(vm: usize, f: impl FnOnce(&mut Vec<TransformerEntry>) -> R) -> R {
    let mut chains = transformer_chains()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    f(chains.entry(vm).or_default())
}

/// Run `f` against `vm`'s chain under the read lock. An absent VM reads as an
/// empty chain, so no entry is created just by looking.
fn with_chain<R>(vm: usize, f: impl FnOnce(&[TransformerEntry]) -> R) -> R {
    let chains = transformer_chains()
        .read()
        .unwrap_or_else(PoisonError::into_inner);
    match chains.get(&vm) {
        Some(chain) => f(chain.as_slice()),
        None => f(&[]),
    }
}

// ---------------------------------------------------------------------------
// GC-root integration
// ---------------------------------------------------------------------------
//
// The transformer chain stores live Java `ClassFileTransformer` instances as
// raw `ObjectRef`s (`TransformerEntry::transformer_ref`). Those references live
// outside the heap, so the collector's stack/static walk never reaches them:
// without the two halves below a moving collection would (a) reclaim a
// transformer no Java root still points at — Mockito/JaCoCo register a
// transformer once and hold it only on the Java agent side — and (b) leave
// every surviving `transformer_ref` pointing at the object's *old* address
// after compaction. Either one yields a use-after-free or a dispatch onto a
// relocated object the next time `run_transformer_chain` invokes `transform`.
//
// Both halves are driven from `memory::native_roots`' `"instrument-transformers"`
// VM root source, which passes the OWNING `SharedVm` — so a collection in VM B
// neither reports nor rewrites VM A's transformers. There is no lazy
// registration step any more: the source is compiled into `VM_ROOT_SOURCES`, so
// it is live from the VM's first collection rather than from the first
// `addTransformer` call.

/// Push every live transformer `ObjectRef` held by `vm`'s chain into `roots`
/// so the collector treats it as a GC root. Null refs are skipped.
///
/// A GC must never miss a root, so on lock poisoning we recover the inner
/// guard and scan anyway rather than silently dropping the roots (a poisoned
/// chain still holds valid `ObjectRef`s that must survive the collection).
pub(crate) fn scan_transformer_roots(vm: usize, roots: &mut Vec<ObjectRef>) {
    with_chain(vm, |chain| {
        for entry in chain {
            // `transformer_ref` is a non-null `ObjectRef`; the null-transformer
            // case is filtered out before an entry is ever pushed (see
            // `native_add_transformer0`). Guard anyway in case a future caller
            // seeds a sentinel.
            if !entry.transformer_ref.as_ptr().is_null() {
                roots.push(entry.transformer_ref);
            }
        }
    });
}

/// Rewrite every `ObjectRef` held by `vm`'s chain through the collector's
/// `old-addr -> new-addr` relocation `map` after a moving collection. Refs
/// absent from the map were not relocated and are left unchanged.
///
/// As with [`scan_transformer_roots`], recover from lock poisoning rather than
/// skip: an un-remapped ref left pointing at a relocated object is a
/// use-after-free, so the remap must run even on a poisoned lock.
///
/// An empty `map` means nothing moved (the non-moving sweep); early-return so a
/// VM with no transformers never touches the chain lock on that path.
pub(crate) fn remap_transformer_refs(vm: usize, map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    with_chain_mut(vm, |chain| {
        for entry in chain.iter_mut() {
            let old_addr = entry.transformer_ref.as_ptr() as usize;
            if let Some(&new_addr) = map.get(&old_addr) {
                // SAFETY: `new_addr` is the relocated address the collector
                // assigned to this same live object; it is non-null and
                // heap-aligned by construction of the relocation map. Mirrors
                // the remap convention used throughout `memory::gc`.
                entry.transformer_ref = unsafe { ObjectRef::from_raw(new_addr as *mut u8) };
            }
        }
    });
}

/// Drop `vm`'s row entirely. Called from `release_vm_native_state` when the
/// last `Arc<SharedVm>` goes away: the entries hold addresses in a heap that no
/// longer exists, and a later VM that happened to reuse the identity would
/// otherwise inherit them as roots.
///
/// Idempotent — a second call removes nothing.
pub fn forget_vm_transformers(vm: usize) {
    let mut chains = transformer_chains()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    chains.remove(&vm);
    // The load-time offer memo is keyed on the same identity and must go with
    // it — a later VM that reuses the identity would otherwise start life
    // believing it had already offered every class the previous one loaded.
    forget_vm_load_time_offers(vm);
}

/// Set once any VM in this process registers a transformer, and never cleared.
///
/// This is a **fast-path negative test only**, which is what makes a
/// process-global sound here despite the per-VM chains: `false` proves no VM has
/// a transformer, so the load path can skip the chain lock entirely; `true` only
/// sends the caller on to the per-VM check ([`transformers_armed`]), which is
/// the authoritative one. A stale `true` after a VM shuts down costs one map
/// probe, never a wrong answer.
///
/// It exists because [`pre_transform_for_load`] sits on the constant-pool
/// resolution path — every `new`, `checkcast`, `getfield` owner and method owner
/// in the VM — and taking an `RwLock` + hash probe there on every run that has
/// no agent at all is not a cost that fix should impose.
static ANY_TRANSFORMER_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Append `(transformer, canRetransform)` to `vm`'s chain. Used by both the
/// `addTransformer0` native and the `addTransformer` helper on
/// `cratonvm/Instrument`.  Order: appended at the end.
pub fn add_transformer_entry(vm: usize, entry: TransformerEntry) {
    ANY_TRANSFORMER_REGISTERED.store(true, std::sync::atomic::Ordering::Release);
    with_chain_mut(vm, |chain| chain.push(entry));
}

/// Remove the first entry in `vm`'s chain whose `transformer_ref`
/// pointer-equals `transformer`. Returns `true` if an entry was removed.
pub fn remove_transformer_entry(vm: usize, transformer: ObjectRef) -> bool {
    with_chain_mut(vm, |chain| {
        let before = chain.len();
        chain.retain(|e| e.transformer_ref.as_ptr() != transformer.as_ptr());
        before != chain.len()
    })
}

/// Snapshot of every transformer currently registered in `vm`'s chain, in
/// registration order. The snapshot is owned so the caller can iterate without
/// holding the chain lock — important because invoking
/// `transformer.transform(...)` re-enters the VM and may itself
/// register or remove transformers (HotSpot's chain is reentrant-safe
/// in the same way).
pub fn snapshot_transformer_chain(vm: usize) -> Vec<TransformerEntry> {
    with_chain(vm, |chain| chain.to_vec())
}

/// Returns the transformer count currently registered in `vm`'s chain.
pub fn transformer_count(vm: usize) -> usize {
    with_chain(vm, |chain| chain.len())
}

/// Reset `vm`'s chain. Used by VM shutdown / test isolation.
///
/// Clears the load-time offer memo with it: "start over" has to mean the next
/// transformer registered in this VM sees class loads again, not that it
/// inherits the previous chain's already-offered set.
pub fn reset_transformer_chain(vm: usize) {
    with_chain_mut(vm, |chain| chain.clear());
    forget_vm_load_time_offers(vm);
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
    vm: usize,
    transformer_ref: ObjectRef,
    can_retransform: bool,
    native_method_prefix: Option<String>,
) {
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref,
            can_retransform,
            native_method_prefix,
        },
    );
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
    let vm = ctx.vm_identity();
    if class_name == MOCKITO_INLINE_TRANSFORMER {
        for existing in snapshot_transformer_chain(vm) {
            let existing_name = ctx
                .class_name_of_id(ctx.class_id_of_object(existing.transformer_ref))
                .unwrap_or_default();
            if existing_name == MOCKITO_INLINE_TRANSFORMER {
                return;
            }
        }
    }
    add_transformer_entry(
        vm,
        TransformerEntry {
            transformer_ref: transformer,
            can_retransform,
            native_method_prefix: None,
        },
    );
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
fn native_remove_transformer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let transformer = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let removed = remove_transformer_entry(ctx.vm_identity(), transformer);
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
        // The JVMTI retransformation base: the class-bytes cache first (the
        // only source that knows dynamically-defined, hidden and
        // agent-redefined classes), then `<name>.class` off the classpath if
        // the cache's soft cap evicted it.
        let original = original_class_bytes(ctx, class_id);
        // No trustworthy base, no retransformation.
        //
        // `original_class_bytes` returns empty for three reasons — the byte
        // cache evicted the class and no classpath resource matched, the
        // resource that matched defines a different class, or it is not a
        // parseable class file — and its own contract says an empty result
        // "makes `native_retransform_classes0` skip the class, which is the
        // safe outcome". It did not: the chain below ran anyway and every
        // registered transformer was handed a **zero-length `byte[]`** as the
        // class file. ASM's `ClassReader` reads the header off that array
        // unconditionally, so Mockito's inline mock maker surfaced the skip as
        // `java.lang.ArrayIndexOutOfBoundsException` thrown from inside mock
        // creation — a face that reads like a broken agent rather than a cache
        // miss, and one this VM has been seen producing
        // (`infinispan-configurationbuilder-retransform-verify-20260805`).
        //
        // JVMTI has no notion of retransforming from nothing: a transformer's
        // `classfileBuffer` is defined to be the class file bytes. Skipping
        // leaves the class as it is — the mock silently fails to intercept,
        // which is what the previous behaviour achieved anyway, minus the
        // spurious exception.
        if original.is_empty() {
            tracing::warn!(
                "retransformClasses0: no retransformation base for `{}`; skipping \
                 (see the preceding diagnostic for which of the three reasons applied)",
                ctx.class_name_of_id(class_id).unwrap_or_default(),
            );
            continue;
        }
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
        if let Err(msg) = ctx.retransform_class(class_id, &original) {
            tracing::warn!("retransformClasses0: could not restore original bytes: {msg}");
            continue;
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
    with_chain_mut(ctx.vm_identity(), |chain| {
        for entry in chain.iter_mut() {
            if entry.transformer_ref.as_ptr() == transformer.as_ptr() {
                entry.native_method_prefix = prefix.clone();
                if prefix.is_some() {
                    tracing::debug!(
                        "instrumentation: native_method_prefix recorded but not yet \
                         consulted at native dispatch"
                    );
                }
                return;
            }
        }
    });
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

fn native_bridge_add_transformer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let transformer = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    add_transformer_entry(
        ctx.vm_identity(),
        TransformerEntry {
            transformer_ref: transformer,
            can_retransform: true,
            native_method_prefix: None,
        },
    );
    Ok(None)
}

fn native_bridge_remove_transformer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let transformer = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let removed = remove_transformer_entry(ctx.vm_identity(), transformer);
    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

fn native_bridge_get_transformer_count(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(
        Value::Int(transformer_count(ctx.vm_identity()) as i32),
    ))
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
/// 2. **Rust-side transformer chain for this VM** (the surface the
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
    // 1. (Removed) — calling InstrumentationImpl.transform(Module,
    //    ClassLoader, String, Class, ProtectionDomain, byte[], boolean)
    //    Java-side currently panics deep inside the JDK 25 transformer
    //    pipeline (length-6 array indexed at 6 — likely a JDK-internal
    //    array op our reflection/varhandle dispatch mis-counts). Instead
    //    we route every transformer registration through the rust-side
    //    chain by overriding the Java public method
    //    `InstrumentationImpl.addTransformer(transformer, canRetransform)`
    //    with a native that records the transformer in this VM's chain
    //    (see `register_instrumentation_natives`).
    //
    //    `inst_receiver` remains a parameter for future re-enablement of
    //    the Java-side dispatch; today it is unused on this path. We
    //    silence the unused-parameter lint at the call site by reading
    //    it explicitly.
    let _ = inst_receiver;

    let class_name_str = ctx.class_name_of_id(class_id).unwrap_or_default();
    // Redefine/retransform: the loader argument is the one HotSpot passes for
    // the class being redefined. We do not track a per-class loader OBJECT for
    // every class, so this path keeps the historical `null`; the load-time path
    // ([`run_load_time_transform_chain`]) does resolve it, because that is the
    // one an agent uses to decide whether a class is its to instrument.
    run_chain_over_bytes(
        ctx,
        &class_name_str,
        class_mirror,
        None,
        initial_bytes,
        retransform_only,
    )
}

/// The transformer-chain walk itself, over a class identified by NAME rather
/// than by `ClassId`.
///
/// Split out from [`run_transformer_chain`] because the load-time hook has no
/// `ClassId` to name the class with: the whole point is that the transformer
/// runs *before* the class is defined, exactly as `java.lang.instrument`
/// specifies. `class_mirror` is `None` and `loader` is the defining loader on
/// that path; on the redefine/retransform path the mirror is the live class and
/// `loader` is `None`.
fn run_chain_over_bytes(
    ctx: &mut dyn NativeContext,
    class_name_str: &str,
    class_mirror: Option<ObjectRef>,
    loader: Option<ObjectRef>,
    initial_bytes: &[u8],
    retransform_only: bool,
) -> Vec<u8> {
    let rust_chain = snapshot_transformer_chain(ctx.vm_identity());
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[RETRANSFORM]   run_transformer_chain: rust_chain={} entries, initial_bytes={}, retransform_only={retransform_only}",
            rust_chain.len(),
            initial_bytes.len()
        );
    }
    if rust_chain.is_empty() {
        return initial_bytes.to_vec();
    }
    // A transformer's `classfileBuffer` is defined by JVMTI to be the class
    // file bytes; there is no "transform from nothing". Handing a registered
    // transformer a zero-length `byte[]` is not a no-op — ASM's `ClassReader`
    // reads the header off it unconditionally, so ByteBuddy raises
    // `ArrayIndexOutOfBoundsException` and Mockito's inline mock maker
    // re-throws it from inside mock creation, where it reads as a broken agent
    // rather than the missing retransformation base it actually is.
    //
    // This is the funnel, not just the `retransformClasses0` caller: every path
    // that reaches a Java transformer — retransform, redefine, and the
    // load-time hook — goes through here, so the invariant holds even if a
    // future caller forgets it.
    if initial_bytes.is_empty() {
        tracing::debug!(
            "transformer chain: no class file bytes to transform for `{class_name_str}`; \
             skipping the chain rather than presenting an empty buffer"
        );
        return Vec::new();
    }
    // Resolve constants used on every iteration once.
    //
    // All three live across `alloc_byte_array` and the `transform` call below,
    // both of which allocate and can therefore move a young object. Pin them:
    // an unpinned `ObjectRef` held across an allocating call is the native
    // stale-local family, and here it would hand the transformer a relocated
    // (i.e. garbage) class name.
    let class_name_obj = ctx.create_string(class_name_str);
    let name_pin = ctx.pin_native_root(class_name_obj);
    let mirror_pin = class_mirror.map(|m| ctx.pin_native_root(m));
    let loader_pin = loader.map(|l| ctx.pin_native_root(l));

    let mut bytes_vec = initial_bytes.to_vec();

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
        let class_name_obj = ctx.read_native_pin(name_pin, class_name_obj);
        let mirror_arg = match (class_mirror, mirror_pin) {
            (Some(m), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, m))),
            _ => Value::Object(None),
        };
        let loader_arg = match (loader, loader_pin) {
            (Some(l), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, l))),
            _ => Value::Object(None),
        };
        // NOTE: invoke_virtual prepends the receiver itself, so args
        // here must NOT include the receiver. Pass only the 5 user args.
        let args = [
            // loader: the class's defining loader on the load-time path, and
            // `null` (the bootstrap loader) when we have none to surface.
            loader_arg,
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
    ctx.unpin_native_roots(name_pin);
    bytes_vec
}

/// Offer a class file to this VM's transformer chain **before it is defined** —
/// the `java.lang.instrument` transform-on-load contract.
///
/// `class_mirror` is `null` and `isRetransform` is false, which is what the spec
/// says a first definition looks like to a transformer. `loader_id` names the
/// class's defining loader; it is turned into the `ClassLoader` argument the
/// transformer receives, because that argument is how agents decide whether a
/// class is theirs to instrument (JaCoCo and most APM agents skip
/// `loader == null`, i.e. bootstrap classes, outright — passing `null` for
/// everything would make them silently skip the whole application).
///
/// Returns the possibly-rewritten bytes; a chain that declines every class
/// returns `initial_bytes` unchanged.
pub fn run_load_time_transform_chain(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    loader_id: cratonvm_types::ClassLoaderId,
    initial_bytes: &[u8],
) -> Vec<u8> {
    use cratonvm_types::ClassLoaderId;
    // Re-entrancy guard — see [`TRANSFORM_IN_FLIGHT`]. A transformer that,
    // while rewriting `X`, causes `X` itself to be loaded (ByteBuddy's type
    // pool does exactly this) would otherwise transform `X` to transform `X`
    // forever. Declining the nested offer costs coverage of one already-covered
    // class and is what makes the first `-javaagent:` run terminate.
    let already = TRANSFORM_IN_FLIGHT.with(|s| s.borrow().iter().any(|n| n == class_name));
    if already {
        return initial_bytes.to_vec();
    }
    TRANSFORM_IN_FLIGHT.with(|s| s.borrow_mut().push(class_name.to_string()));
    /// Pops the in-flight entry on every exit path, including an unwind out of
    /// the transformer.
    struct InFlightGuard;
    impl Drop for InFlightGuard {
        fn drop(&mut self) {
            TRANSFORM_IN_FLIGHT.with(|s| {
                s.borrow_mut().pop();
            });
        }
    }
    let _in_flight = InFlightGuard;

    let loader = match loader_id {
        // The bootstrap loader IS `null` in the Java API — not "unknown".
        ClassLoaderId::Bootstrap => None,
        // Extension/platform and application classes are reached through the
        // system class loader, which is what HotSpot passes for both.
        ClassLoaderId::Extension | ClassLoaderId::Application => {
            Some(cratonvm_native_builtins::classloader::get_or_create_app_loader(ctx))
        }
        // A user-defined loader defines its classes through
        // `ClassLoader.defineClass`, which carries its own receiver; this arm
        // is only reached if one is ever routed through the built-in delegation
        // chain, and the system loader is the closest true answer.
        ClassLoaderId::UserDefined(_) => {
            Some(cratonvm_native_builtins::classloader::get_or_create_app_loader(ctx))
        }
    }
    // This transformer entry point returns the bytes and has no error channel
    // to carry a refusal. If `--jdk-only` refuses the app loader, fall back to
    // the bootstrap loader (`None`) — the same answer this already gave for a
    // bootstrap class — and let the recorded violation stand.
    .and_then(|loader| loader.ok());
    run_chain_over_bytes(
        ctx,
        class_name,
        /* class_mirror = */ None,
        loader,
        initial_bytes,
        /* retransform_only = */ false,
    )
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
    let cached = ctx.class_bytes(class_id).filter(|b| !b.is_empty());
    let name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => {
            tracing::debug!("retransform: class id {class_id:?} has no name; skipping");
            return Vec::new();
        }
    };
    let resource = format!("{name}.class");
    // Only consulted when the cache missed — `find_resource` walks jars.
    let fallback = if cached.is_some() {
        None
    } else {
        ctx.find_resource(&resource).filter(|b| !b.is_empty())
    };
    // `None` when the VM recorded no fingerprint for this class (synthetic
    // stub), which the adjudicator reads as "cannot tell" rather than "wrong".
    let fallback_matches_base = fallback
        .as_deref()
        .and_then(|bytes| ctx.class_bytes_match_base(class_id, bytes));

    match adjudicate_retransform_base(&name, cached, fallback, fallback_matches_base) {
        Ok(bytes) => bytes,
        Err(refusal) => {
            refusal.log(&name, &resource);
            Vec::new()
        }
    }
}

/// Why a class has no usable JVMTI retransformation base.
///
/// Split out from [`original_class_bytes`] so the policy can be exercised
/// without a `NativeContext`: every arm here is a decision about bytes, and the
/// only reason it used to be untestable was that it was interleaved with four
/// trait calls.
#[derive(Debug, PartialEq, Eq)]
enum RetransformBaseRefusal {
    /// Cache evicted and no `<name>.class` anywhere on the classpath.
    NotFound,
    /// The classpath resource parses, but defines some other class (a shaded
    /// jar, or a second loader's copy under a different package).
    DefinesOtherClass(String),
    /// The classpath resource defines the right *name* but is not the class
    /// file this class was defined from — a different build. See
    /// `ClassManager::class_bytes_base_digest`.
    DifferentBuild(usize),
    /// The classpath resource is not a parseable class file at all.
    Unparseable,
}

impl RetransformBaseRefusal {
    fn log(&self, name: &str, resource: &str) {
        match self {
            RetransformBaseRefusal::NotFound => tracing::debug!(
                "retransform: no original bytes for `{name}` \
                 (class_bytes_cache miss and no `{resource}` on the classpath); skipping"
            ),
            RetransformBaseRefusal::DefinesOtherClass(found) => tracing::warn!(
                "retransform: refusing to seed `{name}` from classpath resource \
                 `{resource}` — those bytes define `{found}`. Seeding the \
                 transformer chain with a different class would install a wrong \
                 class body. Skipping this retransform."
            ),
            RetransformBaseRefusal::DifferentBuild(len) => tracing::warn!(
                "retransform: refusing to seed `{name}` from classpath resource \
                 `{resource}` — those {len} bytes are a different build of `{name}` \
                 than the one this class was defined from. The class was almost \
                 certainly defined by a loader that does not resolve to the \
                 application classpath. Skipping this retransform."
            ),
            RetransformBaseRefusal::Unparseable => tracing::warn!(
                "retransform: refusing to seed `{name}` from classpath resource \
                 `{resource}` — not a parseable class file. Skipping."
            ),
        }
    }
}

/// Decide what a `retransformClasses` call may use as its base for `name`.
///
/// * `cached` — the class-bytes cache entry, when it survived eviction. This is
///   the authoritative answer and needs no adjudication: it *is* what the class
///   was defined from.
/// * `fallback` — `<name>.class` re-read off the classpath because the cache
///   missed. `find_resource` searches bootstrap → extension → application and
///   never consults the class's defining loader, so these bytes are a guess.
/// * `fallback_matches_base` — whether `fallback` fingerprint-matches the class
///   file this class was actually defined from. `None` = the VM recorded no
///   fingerprint and cannot tell.
///
/// The `Some(false)` arm is the one the name check could not make. A shaded jar
/// under a *different* package is caught by `this_class`; **a different build of
/// the same class is not**, and Spring Boot's test infrastructure produces that
/// state routinely by defining classes through `ModifiedClassPathClassLoader` /
/// `FilteredClassLoader` / per-test `URLClassLoader`s while a different version
/// of the same coordinate sits on the application classpath. Weaving build A's
/// method bodies and installing them over live build B is a silently wrong class
/// definition — exactly what the `DefinesOtherClass` arm already exists to
/// prevent, with a quieter face.
fn adjudicate_retransform_base(
    name: &str,
    cached: Option<Vec<u8>>,
    fallback: Option<Vec<u8>>,
    fallback_matches_base: Option<bool>,
) -> Result<Vec<u8>, RetransformBaseRefusal> {
    if let Some(bytes) = cached {
        return Ok(bytes);
    }
    let Some(fallback) = fallback else {
        return Err(RetransformBaseRefusal::NotFound);
    };
    match class_file_this_class(&fallback) {
        None => Err(RetransformBaseRefusal::Unparseable),
        Some(found) if found != name => Err(RetransformBaseRefusal::DefinesOtherClass(found)),
        Some(_) => {
            if fallback_matches_base == Some(false) {
                return Err(RetransformBaseRefusal::DifferentBuild(fallback.len()));
            }
            tracing::debug!(
                "retransform: `{name}` bytes came from the classpath, not the \
                 class_bytes_cache (16 MiB FIFO cap — likely evicted)"
            );
            Ok(fallback)
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

// ---------------------------------------------------------------------------
// Load-time transform (`java.lang.instrument` transform-on-load)
// ---------------------------------------------------------------------------
//
// `addTransformer` used to be accepted and then do nothing: the chain was only
// ever walked by `redefineClasses`/`retransformClasses`, so a transformer was
// never offered a class *being defined*. That is the worst shape a failure can
// take — `isRetransformClassesSupported()` answered `true`, `addTransformer`
// returned normally, and every bytecode-rewriting agent (JaCoCo, APM, tracing,
// most profilers) reported that it had installed and then instrumented nothing.
// See `java-agent-transformer-never-fires-and-attach-list-throws-FIXED-20260806.md`.
//
// The transform has to run where two things are true at once: the raw class file
// is in hand, and no class-manager lock is held (the transformer is Java code
// and will itself load classes). Neither is true inside `ClassManager`, so the
// work is split:
//
//   [`SharedVm::load_class_transformed`]  (this file, VM side, no lock held)
//        find bytes -> run the chain -> `ClassManager::stage_transformed_class`
//                                              |
//   `ClassManager::load_class`  <--------------+  consumes the staged entry in
//        place of the bytes parent delegation would have read
//
// so the definition the VM installs and the definition the agent produced are
// the same object by construction, and every downstream step (class-bytes cache,
// `ClassLoad`/`ClassPrepare`, the JIT's view) sees only the final bytes.

thread_local! {
    /// Names whose load-time transform is in flight **on this thread**.
    ///
    /// A transformer is Java: `transform()` allocates, calls library code, and
    /// loads classes — including, on its very first call, its own dependencies.
    /// Each of those loads re-enters the hook. Without this guard a transformer
    /// that touches a class it is itself being asked about recurses forever, and
    /// the first `-javaagent:` run dies in a stack overflow instead of an
    /// instrumented class.
    ///
    /// Skipping the nested offer costs coverage of exactly the classes the
    /// transformer pulled in while transforming, which is also what HotSpot's
    /// own re-entrancy rules produce.
    static TRANSFORM_IN_FLIGHT: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// How deep the supertype pre-stage walk may go. A class hierarchy deeper than
/// this is pathological; the bound keeps a malformed or cyclic class file from
/// turning the walk into an unbounded recursion.
const SUPERTYPE_STAGE_DEPTH: u32 = 24;

/// Class names already offered to `vm`'s load-time transformer chain.
///
/// # Why the load-time hook needs a memo at all
///
/// [`pre_transform_for_load`] does not sit at a class *definition* site — it
/// sits on the constant-pool resolution path, which runs for every `new`,
/// `checkcast`, `instanceof`, field owner and method owner the interpreter
/// executes. It approximated "this class is not defined yet, so a definition is
/// about to follow" with `ClassManager::resolve_fast_path_class_id`, and that
/// approximation has a hole with a name-shaped edge: when a class is defined by
/// a **user loader** *and* the same name is also reachable on the built-in
/// delegation chain, `resolve_fast_path_class_id` deliberately answers `None`
/// (it will not hand a `UserDefined` ClassId to a request the delegation chain
/// can answer itself). So for every such class the "already defined" early-out
/// never fires, and each resolution paid, in full:
///
///   * `find_class_bytes_for_transform` — a jar read + inflate + `to_vec`,
///   * the same again for the supertype/interface pre-stage walk,
///   * a Java `byte[]` allocation of the whole class file, and
///   * an interpreted call into every registered `transform`.
///
/// That is not a small constant. Under Mockito's inline mock maker — which
/// self-attaches a `ClassFileTransformer` in essentially every Spring Boot test
/// — a class whose test runs under a `URLClassLoader` (Spring Boot's
/// `@ClassPathExclusions` / `ModifiedClassPathClassLoader`, where *every*
/// application class takes the shadowed shape above) went from a 21 s pass to a
/// 300 s timeout with no forward progress at all.
///
/// # Why keying on the name alone is the right granularity
///
/// The seam this hook writes through — `ClassManager::stage_transformed_class`
/// / `pending_transformed_classes` — is itself keyed by name, with "a second
/// stage for the same name overwrites the first". A second offer for a name
/// therefore *cannot* reach a second definition even in principle; it can only
/// overwrite bytes staged for the first. Offering once per name per VM is
/// exactly the granularity the staging mechanism supports, so the memo costs no
/// coverage the seam could have delivered.
///
/// Keyed per VM for the same reason [`TransformerChains`] is: one process can
/// own several heaps, and a name offered in VM A says nothing about VM B.
/// Dropped by [`forget_vm_transformers`] when the VM goes away.
type LoadTimeOffered = HashMap<usize, std::collections::HashSet<Box<str>>>;

fn load_time_offered() -> &'static RwLock<LoadTimeOffered> {
    static INSTANCE: OnceLock<RwLock<LoadTimeOffered>> = OnceLock::new();
    INSTANCE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Claim the single load-time offer for `name` in `vm`.
///
/// Returns `true` for the caller that should go on and do the work, and
/// `false` for every caller after it. Read-locked on the repeat path (which is
/// the overwhelmingly common one — one `true` per class against arbitrarily
/// many `false`s), and write-locked only to record a first offer.
///
/// `CRATONVM_DBG=load-transform-no-memo` makes this always answer `true`, i.e.
/// restores the pre-fix "re-offer on every resolution" behaviour. It exists as
/// the red control for the fix above: with it set, the hang reproduces.
fn claim_load_time_offer(vm: usize, name: &str) -> bool {
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_LOAD_TRANSFORM_NO_MEMO").is_ok() {
        return true;
    }
    {
        let offered = load_time_offered()
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        if offered.get(&vm).is_some_and(|set| set.contains(name)) {
            return false;
        }
    }
    let mut offered = load_time_offered()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    offered.entry(vm).or_default().insert(name.into())
}

/// Drop `vm`'s load-time offer memo. Paired with [`forget_vm_transformers`]:
/// an identity that gets reused by a later VM must not inherit the names the
/// previous one already offered.
fn forget_vm_load_time_offers(vm: usize) {
    let mut offered = load_time_offered()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    offered.remove(&vm);
}

/// True when this VM has at least one registered `ClassFileTransformer`.
///
/// The load path consults this before doing anything else. The relaxed global
/// pre-check makes the no-agent answer a single atomic load — see
/// [`ANY_TRANSFORMER_REGISTERED`] for why a process-global is sound as a
/// negative test in front of the per-VM chain.
#[inline]
pub fn transformers_armed(vm: usize) -> bool {
    ANY_TRANSFORMER_REGISTERED.load(std::sync::atomic::Ordering::Acquire)
        && transformer_count(vm) > 0
}

/// Run the load-time transformer chain for `name` and stage the result for the
/// load that follows. Best-effort throughout: anything that cannot be answered
/// leaves the ordinary load completely unchanged.
///
/// Recurses into the class's supertypes first — see [`class_file_supertypes`]
/// for why they are unreachable otherwise — but only *stages* them. It never
/// forces a load, so the VM's class-loading order is not perturbed: a supertype
/// whose staged bytes are never asked for is simply never used.
pub fn pre_transform_for_load(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::JvmThread,
    name: &str,
    depth: u32,
) {
    if !transformers_armed(shared.vm_identity) {
        return;
    }
    // Array classes are synthesised from their component (JVMS §5.3.3) — there
    // is no class file to offer.
    if depth > SUPERTYPE_STAGE_DEPTH || name.starts_with('[') || name.is_empty() {
        return;
    }
    let reentrant = TRANSFORM_IN_FLIGHT.with(|s| s.borrow().iter().any(|n| n == name));
    if reentrant {
        return;
    }
    // One offer per name per VM. This is the load path's own bound on how much
    // work a registered transformer can cost: without it every *resolution* of
    // a class the "already defined" check below cannot recognise (see
    // [`LoadTimeOffered`] for the exact shape — a user-loader class whose name
    // the delegation chain also answers) re-read the class file, re-walked its
    // supertypes and re-entered Java. Claimed BEFORE the read lock below so the
    // repeat path is one hash probe and nothing else.
    if !claim_load_time_offer(shared.vm_identity, name) {
        return;
    }
    // Already defined: transform-on-load is over for this class. (A retransform
    // is the API for changing it now, and that path is separately wired.)
    {
        let cm = shared.classes.class_manager.read();
        if let Some(id) = cm.resolve_fast_path_class_id(name) {
            // A synthetic stub is not a real definition — but upgrading one is
            // `ClassManager::load_class`'s own business and it does not route
            // through the staged-bytes seam, so leave that case alone rather
            // than half-transform it.
            let _ = id;
            return;
        }
    }
    let found = {
        let cm = shared.classes.class_manager.read();
        cm.find_class_bytes_for_transform(name)
    };
    let (bytes, loader_id) = match found {
        Ok(pair) => pair,
        // Not on the built-in delegation chain (a user loader will supply it,
        // or it does not exist). The `ClassLoader.defineClass` hook covers the
        // former; either way there is nothing to transform here.
        Err(_) => return,
    };

    for supertype in class_file_supertypes(&bytes) {
        pre_transform_for_load(shared, thread, &supertype, depth + 1);
    }

    let transformed = {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        run_load_time_transform_chain(&mut ctx, name, loader_id, &bytes)
    };
    if transformed == bytes {
        // Every transformer declined. Staging identical bytes would only make
        // the load take a different route to the same definition.
        return;
    }
    // A transformer that returns something that is not a class file for THIS
    // class would install a wrong class body under the right name — the same
    // hazard `original_class_bytes` refuses on the retransform path. Refuse it
    // here for the same reason, and say so: a silently-wrong definition is far
    // harder to diagnose than a transform that visibly did not apply.
    match class_file_this_class(&transformed) {
        Some(found) if found == name => {}
        other => {
            tracing::warn!(
                "load-time transform of `{name}` produced bytes that define {:?}; \
                 keeping the original class file",
                other
            );
            return;
        }
    }
    shared
        .classes
        .class_manager_write()
        .stage_transformed_class(name, transformed, loader_id);
}

/// The `super_class` and `interfaces` entries of a class file, in internal form.
///
/// Used by the load-time transform ([`pre_transform_for_load`]) to reach the
/// supertypes a definition will pull in on its own. Those loads happen *inside*
/// `define_class_shared_with_options`, under the class-manager write lock, so
/// they can never call a Java transformer themselves — without this walk a
/// coverage agent would be offered `class Foo` and never `Foo`'s abstract base.
///
/// Returns an empty vec for anything it cannot parse; the caller treats that as
/// "no supertypes to pre-stage", which is a coverage limit, never a failure.
fn class_file_supertypes(bytes: &[u8]) -> Vec<String> {
    fn u16_at(b: &[u8], off: usize) -> Option<u16> {
        Some(u16::from_be_bytes([*b.get(off)?, *b.get(off + 1)?]))
    }

    fn parse(bytes: &[u8]) -> Option<Vec<String>> {
        if bytes.len() < 10 || bytes[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
            return None;
        }
        let cp_count = u16_at(bytes, 8)? as usize;
        if cp_count == 0 {
            return None;
        }
        // Same constant-pool walk as `class_file_this_class`; see there for why
        // long/double consuming two indices has to be honoured.
        let mut offsets: Vec<usize> = vec![0; cp_count];
        let mut pos = 10usize;
        let mut idx = 1usize;
        while idx < cp_count {
            offsets[idx] = pos;
            let tag = *bytes.get(pos)?;
            pos += 1;
            let (payload, slots) = match tag {
                1 => (2 + u16_at(bytes, pos)? as usize, 1),
                3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => (4, 1),
                5 | 6 => (8, 2),
                7 | 8 | 16 | 19 | 20 => (2, 1),
                15 => (3, 1),
                _ => return None,
            };
            pos = pos.checked_add(payload)?;
            if pos > bytes.len() {
                return None;
            }
            idx += slots;
        }

        // `pos` is now at access_flags. Layout: access_flags, this_class,
        // super_class, interfaces_count, interfaces[].
        let class_name_at = |cp_idx: usize| -> Option<String> {
            if cp_idx == 0 {
                // `super_class == 0` is legal and means java/lang/Object's own
                // class file, which has no super to stage.
                return None;
            }
            let entry = *offsets.get(cp_idx)?;
            if entry == 0 || *bytes.get(entry)? != 7 {
                return None;
            }
            let name_idx = u16_at(bytes, entry + 1)? as usize;
            let utf8 = *offsets.get(name_idx)?;
            if utf8 == 0 || *bytes.get(utf8)? != 1 {
                return None;
            }
            let len = u16_at(bytes, utf8 + 1)? as usize;
            let start = utf8 + 3;
            let raw = bytes.get(start..start.checked_add(len)?)?;
            std::str::from_utf8(raw).ok().map(|s| s.to_string())
        };

        let mut out = Vec::new();
        if let Some(sup) = class_name_at(u16_at(bytes, pos + 4)? as usize) {
            out.push(sup);
        }
        let iface_count = u16_at(bytes, pos + 6)? as usize;
        for i in 0..iface_count {
            if let Some(iface) = class_name_at(u16_at(bytes, pos + 8 + i * 2)? as usize) {
                out.push(iface);
            }
        }
        Some(out)
    }

    parse(bytes).unwrap_or_default()
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
    // Some JDK builds also have a one-arg variant that defaults
    // canRetransform=false.
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
    r.register_with_kind(
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
        NativeKind::Bridge,
    );
    r.register_with_kind(
        impl_class,
        "retransformClasses0",
        "(J[Ljava/lang/Class;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let classes = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_retransform_classes0(ctx, &[receiver, classes])
        }) as NativeCallback,
        NativeKind::Bridge,
    );
    // JDK 25 (J)[Ljava/lang/Class; variant — first arg is `long jvmtienv`.
    r.register_with_kind(
        impl_class,
        "getAllLoadedClasses0",
        "(J)[Ljava/lang/Class;",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            native_get_all_loaded_classes0(ctx, &[receiver])
        }) as NativeCallback,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        impl_class,
        "getInitiatedClasses0",
        "(JLjava/lang/ClassLoader;)[Ljava/lang/Class;",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let loader = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_get_initiated_classes0(ctx, &[receiver, loader])
        }) as NativeCallback,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        impl_class,
        "isModifiableClass0",
        "(JLjava/lang/Class;)Z",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let cls = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_is_modifiable_class0(ctx, &[receiver, cls])
        }) as NativeCallback,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        impl_class,
        "getObjectSize0",
        "(JLjava/lang/Object;)J",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let receiver = args.first().cloned().unwrap_or(Value::Object(None));
            let obj = args.get(2).cloned().unwrap_or(Value::Object(None));
            native_get_object_size0(ctx, &[receiver, obj])
        }) as NativeCallback,
        NativeKind::Bridge,
    );
    // JDK 25 unified append: appendToClassLoaderSearch0(long jvmtiEnv,
    // String jar, boolean isBootstrap). Drives both the bootstrap- and
    // system-classloader append forms (Mockito inline mock maker injects its
    // MockMethodDispatcher into the bootstrap loader via this path).
    r.register_with_kind(
        impl_class,
        "appendToClassLoaderSearch0",
        "(JLjava/lang/String;Z)V",
        native_append_to_classloader_search0,
        NativeKind::Bridge,
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
    r.register_with_kind(
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
            with_chain_mut(ctx.vm_identity(), |chain| {
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
            });
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // setHasRetransformableTransformers(long, boolean) — JVMTI capability flag toggle.
    //
    // KEEP (empty body, justified) — and not merely "we always claim retransform
    // support". The datum this setter carries is ALREADY HELD, more precisely,
    // by the transformer chain itself: `addTransformer(t, canRetransform)`
    // records `TransformerEntry::can_retransform` per transformer, and that is
    // exactly what the JDK's `TransformerManager` summarises into this one
    // process-wide boolean before handing it to JVMTI. There is nothing this
    // call could tell the VM that it does not already know at finer grain.
    //
    // The two effects the real JVMTI body has — adding `can_retransform_classes`
    // to the agent's capability set, and enabling `ClassFileLoadHook` — have no
    // CratonVM counterpart to toggle: the retransform path is unconditionally
    // live (`native_retransform_classes0` / `NativeContext::retransform_class`
    // re-run the chain from the preserved original bytes, and the
    // shadow-suppression guards in `interpreter.rs` —
    // `native_shadow_suppressed_by_redefine`, with the
    // `redefine_immune_reflection_native` allow-list — cede a redefined class's
    // methods to the woven bytecode), and the load hook IS the chain walk.
    // Recording the flag would create state nothing reads.
    r.register_with_kind(
        impl_class,
        "setHasRetransformableTransformers",
        "(JZ)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    // JDK 25 InstrumentationImpl natives take `long jvmtienv` (descriptor (J)Z).
    // Register both the (J)Z variant (the JDK 25 actual signature) and the
    // legacy ()Z variant (backstop in case some workloads see the older arity).
    r.register_with_kind(
        impl_class,
        "isRetransformClassesSupported0",
        "(J)Z",
        native_is_retransform_supported0,
        NativeKind::Bridge,
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
    // registered natively above and answers from this VM's chain and the
    // VM's own class tables. There is no JVMTI env to record, so an empty
    // body IS the implementation. This registration also appears in
    // `interpreter.rs`'s force-native-override table so it beats the real
    // bytecode, which would otherwise enter VM-private init we cannot honour.
    //
    // RESTORED 2026-08-11. `dc55e8057` deleted it as `method-nowhere` — true,
    // and it always will be: `<init>` is a constructor, so no image declares it
    // `ACC_NATIVE` and a census cannot tell CratonVM's deliberate no-op from a
    // dead row. Deleting it left three things pointing at nothing: the
    // `ctx.invoke("sun/instrument/InstrumentationImpl", "<init>", …)` in
    // `attach_agent_in_process` above, the force-native-override entry in
    // `native_override.rs`, and the paragraph you are reading. The invoke then
    // reaches the real ctor and enters exactly the VM-private init this comment
    // says cannot be honoured — on the self-attach path, which is the one every
    // `Mockito.mock()` arms.
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

    // ---- load-time transform: supertype pre-stage walk ----

    /// A class file with `this_class`, `super_class` and `interfaces`, built
    /// from the same constant-pool shape `minimal_class_file` uses.
    ///
    /// Each name gets a `CONSTANT_Class` + `CONSTANT_Utf8` pair, in order, so
    /// the pool indices are `1,2` for the first name, `3,4` for the second, and
    /// so on.
    fn class_file_with_hierarchy(this: &str, super_name: &str, interfaces: &[&str]) -> Vec<u8> {
        let names: Vec<&str> = std::iter::once(this)
            .chain(std::iter::once(super_name))
            .chain(interfaces.iter().copied())
            .collect();
        let mut b = Vec::new();
        b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
        b.extend_from_slice(&0u16.to_be_bytes());
        b.extend_from_slice(&65u16.to_be_bytes());
        // cp_count is one past the last used index; two entries per name.
        b.extend_from_slice(&((names.len() as u16) * 2 + 1).to_be_bytes());
        for (i, name) in names.iter().enumerate() {
            // CONSTANT_Class at index 2i+1, pointing at the CONSTANT_Utf8 that
            // immediately follows it at index 2i+2.
            b.push(7);
            b.extend_from_slice(&((i as u16) * 2 + 2).to_be_bytes());
            b.push(1);
            b.extend_from_slice(&(name.len() as u16).to_be_bytes());
            b.extend_from_slice(name.as_bytes());
        }
        b.extend_from_slice(&0x0021u16.to_be_bytes()); // access_flags
        b.extend_from_slice(&1u16.to_be_bytes()); // this_class  = #1
        b.extend_from_slice(&3u16.to_be_bytes()); // super_class = #3
        b.extend_from_slice(&(interfaces.len() as u16).to_be_bytes());
        for i in 0..interfaces.len() {
            b.extend_from_slice(&((i as u16) * 2 + 5).to_be_bytes());
        }
        b
    }

    /// The supertype walk is what lets a transformer see the abstract base of a
    /// class it is offered: supertypes are loaded from *inside*
    /// `define_class_shared_with_options`, under the class-manager write lock,
    /// where no Java transformer can run.
    #[test]
    fn supertypes_reports_superclass_and_every_interface() {
        let bytes = class_file_with_hierarchy(
            "com/app/Impl",
            "com/app/AbstractBase",
            &["com/app/Api", "java/io/Serializable"],
        );
        assert_eq!(
            class_file_this_class(&bytes).as_deref(),
            Some("com/app/Impl")
        );
        assert_eq!(
            class_file_supertypes(&bytes),
            vec![
                "com/app/AbstractBase".to_string(),
                "com/app/Api".to_string(),
                "java/io/Serializable".to_string(),
            ]
        );
    }

    /// `super_class == 0` is legal — it is what `java/lang/Object`'s own class
    /// file carries — and must not be reported as a supertype named "".
    #[test]
    fn supertypes_treats_a_zero_super_class_as_no_supertype() {
        let mut bytes = class_file_with_hierarchy("java/lang/Object", "unused/Placeholder", &[]);
        // Overwrite super_class (the u2 after magic..cp, access_flags,
        // this_class) with 0. Locate it by walking back from the tail: the
        // trailer is access_flags(2) + this_class(2) + super_class(2) +
        // interfaces_count(2) with no interfaces.
        let len = bytes.len();
        bytes[len - 4..len - 2].copy_from_slice(&0u16.to_be_bytes());
        assert!(class_file_supertypes(&bytes).is_empty());
    }

    /// Anything unparseable means "no supertypes to pre-stage" — a coverage
    /// limit, never a failure that could break a class load.
    #[test]
    fn supertypes_of_garbage_is_empty_not_a_panic() {
        assert!(class_file_supertypes(&[]).is_empty());
        assert!(class_file_supertypes(b"PK\x03\x04not a class").is_empty());
        let mut truncated = class_file_with_hierarchy("a/B", "a/C", &[]);
        truncated.truncate(truncated.len() - 3);
        // Truncated inside the trailer: the walk must decline, not index past
        // the end.
        let _ = class_file_supertypes(&truncated);
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

    // ---- Retransformation base adjudication --------------------------------
    //
    // `adjudicate_retransform_base` is the whole policy `original_class_bytes`
    // applies once it has gathered its four inputs. Driving it directly is the
    // only way to test the version-skew arm: producing the state for real needs
    // two builds of one class, a user-defined loader, and 16 MiB of class bytes
    // loaded in between to evict the cache.

    /// A class file that is a *different build of the same class*: same
    /// `this_class`, different bytes. Achieved by appending a trailing
    /// attribute-shaped tail, which `class_file_this_class` (a header-only
    /// walker) neither reads nor cares about — which is precisely why the name
    /// check cannot tell the two apart.
    fn other_build_of(name: &str) -> Vec<u8> {
        let mut b = minimal_class_file(name);
        b.extend_from_slice(&[0u8; 32]);
        b
    }

    #[test]
    fn retransform_base_prefers_the_cached_bytes() {
        let cached = minimal_class_file("com/foo/Bar");
        let got = adjudicate_retransform_base(
            "com/foo/Bar",
            Some(cached.clone()),
            // A fallback is never even consulted when the cache hit — including
            // its digest verdict, which here says "wrong".
            Some(other_build_of("com/foo/Bar")),
            Some(false),
        );
        assert_eq!(got.as_deref(), Ok(&cached[..]));
    }

    #[test]
    fn retransform_base_accepts_a_matching_classpath_fallback() {
        let bytes = minimal_class_file("com/foo/Bar");
        let got = adjudicate_retransform_base("com/foo/Bar", None, Some(bytes.clone()), Some(true));
        assert_eq!(got.as_deref(), Ok(&bytes[..]));
    }

    #[test]
    fn retransform_base_accepts_a_fallback_the_vm_cannot_adjudicate() {
        // `None` = no fingerprint recorded (synthetic stub). Unchanged from the
        // pre-fingerprint behaviour: the name check is all we have, so use it.
        let bytes = minimal_class_file("com/foo/Bar");
        let got = adjudicate_retransform_base("com/foo/Bar", None, Some(bytes.clone()), None);
        assert_eq!(got.as_deref(), Ok(&bytes[..]));
    }

    #[test]
    fn retransform_base_refuses_a_different_build_of_the_same_class() {
        // THE REGRESSION. Before the fingerprint check these bytes were
        // accepted — `this_class` says `com/foo/Bar` and that was the entire
        // test — and the transformer wove a class body that was then installed
        // over a live class it did not come from. The state arises whenever a
        // class is defined through a loader that does not resolve to the
        // application classpath (Spring Boot's `ModifiedClassPathClassLoader`,
        // `FilteredClassLoader`, per-test `URLClassLoader`s) while a different
        // build of the same coordinate sits on that classpath, and the 16 MiB
        // class-bytes cache has since evicted the real base.
        let got = adjudicate_retransform_base(
            "com/foo/Bar",
            None,
            Some(other_build_of("com/foo/Bar")),
            Some(false),
        );
        assert_eq!(
            got,
            Err(RetransformBaseRefusal::DifferentBuild(
                other_build_of("com/foo/Bar").len()
            )),
            "a same-named class file from a different build must not seed a retransform"
        );
    }

    #[test]
    fn retransform_base_refuses_a_resource_defining_another_class() {
        let got = adjudicate_retransform_base(
            "com/foo/Bar",
            None,
            Some(minimal_class_file("shaded/com/foo/Bar")),
            Some(true),
        );
        assert_eq!(
            got,
            Err(RetransformBaseRefusal::DefinesOtherClass(
                "shaded/com/foo/Bar".to_string()
            ))
        );
    }

    #[test]
    fn retransform_base_refuses_an_unparseable_resource() {
        let got = adjudicate_retransform_base(
            "com/foo/Bar",
            None,
            Some(b"PK\x03\x04 this is a jar, not a class".to_vec()),
            None,
        );
        assert_eq!(got, Err(RetransformBaseRefusal::Unparseable));
    }

    #[test]
    fn retransform_base_refuses_when_nothing_was_found() {
        // The arm that used to be a silent `Vec::new()` the CALLER then handed
        // to every registered transformer as a zero-length `byte[]`. Nothing
        // here can prove the caller now skips — `run_transformer_chain`'s own
        // empty-buffer guard does that — but the refusal must at least be
        // distinguishable from "here are your bytes".
        let got = adjudicate_retransform_base("com/foo/Bar", None, None, None);
        assert_eq!(got, Err(RetransformBaseRefusal::NotFound));
    }

    // -----------------------------------------------------------------------
    // Transformer-chain tests
    //
    // Every test below uses its OWN `vm_identity`, so they are genuinely
    // isolated from one another under `cargo test`'s parallel harness. They
    // used to share one process-global chain and lean on
    // `reset_transformer_chain()` at the top of each test for isolation, which
    // only ever worked because no two of them happened to interleave.
    // `unique_vm()` hands out a fresh identity per call; `fake_objref` refs are
    // synthetic non-heap addresses, never dereferenced.
    // -----------------------------------------------------------------------

    /// A `vm_identity` no other test can collide with. Real identities are
    /// `SharedVm` addresses; these are small counter values, which no real VM
    /// can produce, so a test row can never shadow a production row either.
    fn unique_vm() -> usize {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn add_then_remove_transformer_roundtrip() {
        let vm = unique_vm();
        let t1 = fake_objref(0x1000);
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: t1,
                can_retransform: false,
                native_method_prefix: None,
            },
        );
        assert_eq!(transformer_count(vm), 1);
        let removed = remove_transformer_entry(vm, t1);
        assert!(removed);
        assert_eq!(transformer_count(vm), 0);
        forget_vm_transformers(vm);
    }

    #[test]
    fn remove_unknown_transformer_returns_false() {
        let vm = unique_vm();
        let t = fake_objref(0x2000);
        assert!(!remove_transformer_entry(vm, t));
        forget_vm_transformers(vm);
    }

    #[test]
    fn snapshot_preserves_order() {
        let vm = unique_vm();
        let t1 = fake_objref(0x1000);
        let t2 = fake_objref(0x2000);
        let t3 = fake_objref(0x3000);
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: t1,
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: t2,
                can_retransform: false,
                native_method_prefix: Some("$pfx".into()),
            },
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: t3,
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        let snap = snapshot_transformer_chain(vm);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x1000);
        assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
        assert_eq!(snap[1].native_method_prefix.as_deref(), Some("$pfx"));
        assert_eq!(snap[2].transformer_ref.as_ptr() as usize, 0x3000);
        forget_vm_transformers(vm);
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
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x100),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x200),
                can_retransform: false,
                native_method_prefix: None,
            },
        );
        assert_eq!(transformer_count(vm), 2);
        reset_transformer_chain(vm);
        assert_eq!(transformer_count(vm), 0);
        forget_vm_transformers(vm);
    }

    #[test]
    fn scan_transformer_roots_yields_every_ref() {
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x1000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x2000),
                can_retransform: false,
                native_method_prefix: None,
            },
        );
        let mut roots = Vec::new();
        scan_transformer_roots(vm, &mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(addrs, vec![0x1000, 0x2000]);
        forget_vm_transformers(vm);
    }

    #[test]
    fn scan_transformer_roots_empty_chain_pushes_nothing() {
        let vm = unique_vm();
        let mut roots = Vec::new();
        scan_transformer_roots(vm, &mut roots);
        assert!(roots.is_empty());
    }

    #[test]
    fn remap_transformer_refs_rewrites_relocated_entries() {
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x1000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x2000),
                can_retransform: false,
                native_method_prefix: None,
            },
        );
        // Relocate only the first entry; the second is absent from the map
        // and must be left untouched.
        let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        map.insert(0x1000, 0x9000);
        remap_transformer_refs(vm, &map);
        let snap = snapshot_transformer_chain(vm);
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x9000);
        assert_eq!(snap[1].transformer_ref.as_ptr() as usize, 0x2000);
        forget_vm_transformers(vm);
    }

    #[test]
    fn remap_transformer_refs_empty_map_is_noop() {
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x3000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        remap_transformer_refs(vm, &cratonvm_types::PointerMap::default());
        let snap = snapshot_transformer_chain(vm);
        assert_eq!(snap[0].transformer_ref.as_ptr() as usize, 0x3000);
        forget_vm_transformers(vm);
    }

    #[test]
    fn add_premain_transformer_appends_to_chain() {
        let vm = unique_vm();
        let t = fake_objref(0x4000);
        add_premain_transformer(vm, t, true, Some("__".into()));
        let snap = snapshot_transformer_chain(vm);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].can_retransform);
        assert_eq!(snap[0].native_method_prefix.as_deref(), Some("__"));
        forget_vm_transformers(vm);
    }

    // ----- per-VM isolation (PROCESS-GLOBAL-STATE ROUND 2) ------------------

    /// The core isolation property: two VMs' chains are disjoint. Under the
    /// old single process-global `Vec`, `vm_b`'s chain contained `vm_a`'s
    /// transformer and both counts read 2.
    #[test]
    fn chains_are_per_vm() {
        let vm_a = unique_vm();
        let vm_b = unique_vm();
        let ta = fake_objref(0xA000);
        let tb = fake_objref(0xB000);
        add_transformer_entry(
            vm_a,
            TransformerEntry {
                transformer_ref: ta,
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm_b,
            TransformerEntry {
                transformer_ref: tb,
                can_retransform: false,
                native_method_prefix: None,
            },
        );
        assert_eq!(transformer_count(vm_a), 1);
        assert_eq!(transformer_count(vm_b), 1);
        assert_eq!(
            snapshot_transformer_chain(vm_a)[0].transformer_ref.as_ptr() as usize,
            0xA000
        );
        assert_eq!(
            snapshot_transformer_chain(vm_b)[0].transformer_ref.as_ptr() as usize,
            0xB000
        );
        // A remove aimed at B's transformer must not touch A's chain, and
        // vice versa.
        assert!(!remove_transformer_entry(vm_a, tb));
        assert_eq!(transformer_count(vm_a), 1);
        // Resetting one VM must leave the other alone.
        reset_transformer_chain(vm_a);
        assert_eq!(transformer_count(vm_a), 0);
        assert_eq!(transformer_count(vm_b), 1);
        forget_vm_transformers(vm_a);
        forget_vm_transformers(vm_b);
    }

    /// The GC halves are the load-bearing ones: VM B's collection must neither
    /// report VM A's addresses as roots (a pointer into a heap B does not own)
    /// nor rewrite A's entries through B's relocation map.
    #[test]
    fn gc_halves_only_touch_the_owning_vm() {
        let vm_a = unique_vm();
        let vm_b = unique_vm();
        add_transformer_entry(
            vm_a,
            TransformerEntry {
                transformer_ref: fake_objref(0xA000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        add_transformer_entry(
            vm_b,
            TransformerEntry {
                transformer_ref: fake_objref(0xB000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );

        let mut roots_b = Vec::new();
        scan_transformer_roots(vm_b, &mut roots_b);
        let addrs: Vec<usize> = roots_b.iter().map(|r| r.as_ptr() as usize).collect();
        assert_eq!(
            addrs,
            vec![0xB000],
            "VM B's scan must not report VM A's transformer address"
        );

        // B relocates the address A's entry happens to live at. A must be
        // untouched.
        let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        map.insert(0xA000, 0xDEAD_0000);
        remap_transformer_refs(vm_b, &map);
        assert_eq!(
            snapshot_transformer_chain(vm_a)[0].transformer_ref.as_ptr() as usize,
            0xA000,
            "VM B's post-move fixup must not rewrite VM A's entries"
        );
        forget_vm_transformers(vm_a);
        forget_vm_transformers(vm_b);
    }

    /// VM teardown drops the row. Without it, a long-lived host process that
    /// creates and disposes of VMs accumulates chains full of addresses into
    /// heaps that no longer exist, and a later VM that reuses the identity
    /// inherits them as roots.
    #[test]
    fn forget_vm_transformers_drops_the_row_and_is_idempotent() {
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0xC000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        assert_eq!(transformer_count(vm), 1);
        forget_vm_transformers(vm);
        assert_eq!(transformer_count(vm), 0);
        let mut roots = Vec::new();
        scan_transformer_roots(vm, &mut roots);
        assert!(
            roots.is_empty(),
            "a released VM's transformers must not be reported as roots"
        );
        // Second call is a no-op, not a panic.
        forget_vm_transformers(vm);
        assert_eq!(transformer_count(vm), 0);
    }

    /// Reading an unknown VM must not create a row — otherwise the map grows
    /// without bound on every `run_transformer_chain` in a VM that never
    /// installed a transformer.
    #[test]
    fn reading_an_unknown_vm_does_not_create_a_row() {
        let vm = unique_vm();
        assert_eq!(transformer_count(vm), 0);
        assert!(snapshot_transformer_chain(vm).is_empty());
        let chains = transformer_chains()
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        assert!(
            !chains.contains_key(&vm),
            "read-only access must not allocate a chain for the VM"
        );
    }

    #[test]
    fn approximate_object_size_object() {
        // header + 5 legacy Value slots * 16. Derived, because these four
        // sizes moved together by exactly the 8 bytes the 2026-08-06 header
        // shrink returned, and a literal only records which day it was written.
        let sz = approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 5);
        assert_eq!(
            sz as usize,
            cratonvm_types::HEADER_SIZE + 5 * cratonvm_types::SLOT_SIZE
        );
    }

    #[test]
    fn approximate_object_size_byte_array() {
        // Header + 10 bytes aligned up to 8.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Byte, 10, 0);
        assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 16);
    }

    #[test]
    fn approximate_object_size_long_array() {
        // Header + 4 * 8.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Long, 4, 0);
        assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 32);
    }

    #[test]
    fn approximate_object_size_ref_array() {
        // Header + 3 refs * 8, rounded to the 8-byte grid.
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Reference, 3, 0);
        assert_eq!(sz as usize, cratonvm_types::HEADER_SIZE + 24);
    }

    #[test]
    fn approximate_object_size_empty_array_is_positive() {
        let sz = approximate_object_size(ObjectKind::Array, ArrayElementType::Int, 0, 0);
        assert!(sz > 0);
    }

    /// The registrar must cover every `InstrumentationImpl` native the JDK
    /// declares — **at the descriptor the JDK declares it with**.
    ///
    /// Until 2026-08-11 this asserted thirteen spellings with no leading
    /// `long`: `redefineClasses0([ClassDefinition;)V`,
    /// `getAllLoadedClasses0()[Class;`, `isModifiableClass0(Class;)Z` and the
    /// rest. **Every native on this class takes `long jvmtienv` first**, on
    /// both Temurin 21.0.12+8 and 25.0.4+7 (`javap -p -s --module
    /// java.instrument`), so those thirteen could never bind and the test
    /// agreed with thirteen registrations that could never be reached. Both
    /// sides were wrong together, which is why it stayed green for so long.
    ///
    /// `dc55e8057` deleted the unbindable registrations as dead — correctly —
    /// and this test went red. The fix is the real descriptors, not the
    /// registrations back.
    ///
    /// Three of the old assertions have no JDK counterpart at all and are gone
    /// rather than corrected: `isRedefineClassesSupported0`,
    /// `isNativeMethodPrefixSupported0` and `setNativeMethodPrefix0` are not
    /// native on any supported image, and `appendTo{Bootstrap,System}
    /// ClassLoaderSearch0` is one JDK method, `appendToClassLoaderSearch0`,
    /// taking `(long, String, boolean)`.
    #[test]
    fn register_natives_includes_required_methods() {
        let mut r = NativeMethodRegistry::new();
        register_instrumentation_natives(&mut r);
        let impl_class = "sun/instrument/InstrumentationImpl";
        // (method, descriptor) — each one verified present and ACC_NATIVE on
        // BOTH supported images. Keep this list and the image in step: a
        // descriptor here that the JDK does not declare is a registration that
        // binds to nothing, and this assertion would hide it.
        for (name, descriptor) in [
            (
                "redefineClasses0",
                "(J[Ljava/lang/instrument/ClassDefinition;)V",
            ),
            ("retransformClasses0", "(J[Ljava/lang/Class;)V"),
            ("getAllLoadedClasses0", "(J)[Ljava/lang/Class;"),
            (
                "getInitiatedClasses0",
                "(JLjava/lang/ClassLoader;)[Ljava/lang/Class;",
            ),
            ("isModifiableClass0", "(JLjava/lang/Class;)Z"),
            ("getObjectSize0", "(JLjava/lang/Object;)J"),
            ("isRetransformClassesSupported0", "(J)Z"),
            ("appendToClassLoaderSearch0", "(JLjava/lang/String;Z)V"),
            ("setHasRetransformableTransformers", "(JZ)V"),
            ("setNativeMethodPrefixes", "(J[Ljava/lang/String;Z)V"),
        ] {
            assert!(
                r.find(impl_class, name, descriptor).is_some(),
                "{impl_class}.{name}{descriptor} is declared native by JDK 21                  and 25 and must be registered"
            );
        }
        // The no-`jvmtienv` spellings must NOT come back. Re-adding one makes
        // the assertions above pass while binding nothing, which is exactly the
        // state this test was in before 2026-08-11.
        for (name, descriptor) in [
            (
                "redefineClasses0",
                "([Ljava/lang/instrument/ClassDefinition;)V",
            ),
            ("retransformClasses0", "([Ljava/lang/Class;)V"),
            ("getAllLoadedClasses0", "()[Ljava/lang/Class;"),
            ("isModifiableClass0", "(Ljava/lang/Class;)Z"),
            ("getObjectSize0", "(Ljava/lang/Object;)J"),
        ] {
            assert!(
                r.find(impl_class, name, descriptor).is_none(),
                "{impl_class}.{name}{descriptor} has no leading `long jvmtienv`                  and is declared by no supported JDK image; registering it                  binds nothing and hides the arity that does"
            );
        }
        // CratonVM's own convenience surface on the same class: no-`0`, no
        // `jvmtienv`. These are ours, not the JDK's, and the census scores them
        // `method-nowhere` for that reason.
        for (name, descriptor) in [
            (
                "removeTransformer",
                "(Ljava/lang/instrument/ClassFileTransformer;)Z",
            ),
            (
                "addTransformer",
                "(Ljava/lang/instrument/ClassFileTransformer;)V",
            ),
            (
                "addTransformer",
                "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
            ),
        ] {
            assert!(
                r.find(impl_class, name, descriptor).is_some(),
                "{impl_class}.{name}{descriptor} is CratonVM's own entry point                  and must stay registered"
            );
        }
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

    // ---- Load-time transform: one offer per name per VM ----------------
    //
    // The bug these pin: `pre_transform_for_load` sits on the constant-pool
    // resolution path, so a name whose "already defined" early-out cannot fire
    // (a user-loader class the delegation chain also answers) was re-offered —
    // class file re-read, supertypes re-walked, Java re-entered — on EVERY
    // resolution. See [`LoadTimeOffered`].
    //
    // Identities are picked high and distinct so these never collide with a
    // real VM identity or with each other under the test harness's shared
    // process.

    #[test]
    fn load_time_offer_is_claimed_exactly_once_per_name() {
        let vm = 0x10ad_0001_usize;
        assert!(
            claim_load_time_offer(vm, "com/foo/Bar"),
            "the first resolution must do the work"
        );
        for _ in 0..1000 {
            assert!(
                !claim_load_time_offer(vm, "com/foo/Bar"),
                "every later resolution of the same name must be a no-op"
            );
        }
        // A different name is still its own first offer.
        assert!(claim_load_time_offer(vm, "com/foo/Baz"));
        forget_vm_load_time_offers(vm);
    }

    #[test]
    fn load_time_offer_memo_is_per_vm() {
        let a = 0x10ad_0002_usize;
        let b = 0x10ad_0003_usize;
        assert!(claim_load_time_offer(a, "com/foo/Bar"));
        assert!(
            claim_load_time_offer(b, "com/foo/Bar"),
            "a name offered in VM A says nothing about VM B — its heap, its \
             transformer chain, its class file"
        );
        assert!(!claim_load_time_offer(a, "com/foo/Bar"));
        forget_vm_load_time_offers(a);
        forget_vm_load_time_offers(b);
    }

    #[test]
    fn forgetting_a_vm_drops_its_offer_memo() {
        let vm = 0x10ad_0004_usize;
        assert!(claim_load_time_offer(vm, "com/foo/Bar"));
        assert!(!claim_load_time_offer(vm, "com/foo/Bar"));
        // An identity a later VM reuses must not inherit the previous VM's
        // already-offered set, or that VM's agent never sees a class load.
        forget_vm_transformers(vm);
        assert!(claim_load_time_offer(vm, "com/foo/Bar"));
        forget_vm_load_time_offers(vm);
    }

    #[test]
    fn resetting_the_chain_drops_the_offer_memo() {
        let vm = 0x10ad_0005_usize;
        assert!(claim_load_time_offer(vm, "com/foo/Bar"));
        assert!(!claim_load_time_offer(vm, "com/foo/Bar"));
        reset_transformer_chain(vm);
        assert!(
            claim_load_time_offer(vm, "com/foo/Bar"),
            "\"start over\" has to mean the next transformer sees class loads"
        );
        forget_vm_load_time_offers(vm);
    }
}
