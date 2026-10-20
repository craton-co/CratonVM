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
    error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError},
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
/// `setNativeMethodPrefix(transformer, prefix)`; a native method whose
/// implementation is not found under its own name is linked by the name with
/// the prefixes removed ([`native_method_prefixes`], read by
/// `vm_exec::prefixed_native_link`; interpreter round i1 wave 44, lane L3).
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

/// Approximate the heap footprint of a Java object from its shape alone —
/// the FALLBACK for [`object_size_of`], used when the context cannot read the
/// object's header (`NativeHeapAccess::heap_object_footprint` is `None`):
///
///   * Regular objects: `HEADER_SIZE + num_slots * SLOT_SIZE` (16-byte header,
///     16-byte slots, `cratonvm_types::heap_types`). This is the LEGACY
///     layout only: on a compact object `num_slots` is the field count and
///     the real body is smaller.
///   * Primitive arrays: `HEADER_SIZE + length * element_size`, 8-byte
///     aligned.
///   * Reference arrays: `HEADER_SIZE + length * ref_element_size()`, 8-byte
///     aligned.
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

/// `Instrumentation.getObjectSize` for `obj`: the bytes the allocator charged
/// for it, read from its header, else [`approximate_object_size`].
///
/// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): both
/// natives used to answer `approximate_object_size` with
/// `object_num_fields`, which is the FIELD COUNT on a compact object — the
/// default shape on all three backends — so a class of eight `boolean`s
/// reported `16 + 8 * 16 = 144` bytes. The header-derived footprint is what
/// the heap counters already charge (`native_object_footprint`), so the two
/// reports now agree about the same object, and a legacy object is sized
/// exactly as before.
pub(crate) fn object_size_of(ctx: &dyn NativeContext, obj: ObjectRef) -> i64 {
    // Interpreter round i2 wave 62, lane L1: HotSpot's number where its
    // layout can be modelled (`CRATONVM_HOTSPOT_OBJECT_SIZE`).
    if ctx.hotspot_object_sizes() {
        if let Some(size) = hotspot_object_size(ctx, obj) {
            return size;
        }
    }
    if let Some(size) = size_from_footprint(ctx.heap_object_footprint(obj)) {
        return size;
    }
    let kind = ctx.heap_kind_of(obj);
    let element_type = ctx.heap_element_type_of(obj);
    let length = if matches!(kind, ObjectKind::Array) {
        ctx.array_length(obj)
    } else {
        0
    };
    let slots = ctx.object_num_fields(obj);
    approximate_object_size(kind, element_type, length, slots)
}

/// HotSpot 25's `getObjectSize` answer for `obj` under its default flags
/// (compressed references and class pointers, 8-byte object alignment, no
/// compact headers), modelled from the object's shape (interpreter round i2
/// wave 62, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-get-object-size-answers-this-vms-footprint-not-hotspots-FIXED-20261026.md`).
/// MEASURED on HotSpot 25.0.3 with `tools/probes/interp/L1/L1W62GetObjectSizeShapes.java`:
///
/// * an array is `align8(16 + length * element)`, a reference element 4
///   bytes;
/// * an instance is its field layout's end, 8-byte aligned
///   ([`hotspot_instance_size`]): a 12-byte header, then each class's fields
///   from `java.lang.Object` down, placed as HotSpot's `FieldLayoutBuilder`
///   places them, with the fields HotSpot injects into a few JDK classes
///   ([`hotspot_injected_fields`]).
///
/// * a `java.lang.Class` mirror is 120 bytes (`java.lang.Class`'s instance
///   with the fields HotSpot injects), then the mirrored class's static
///   fields laid out the same way.
///
/// `None` (the caller answers this VM's footprint) for
/// `jdk.internal.vm.StackChunk`, whose size is its stack's.
fn hotspot_object_size(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<i64> {
    const ARRAY_BASE: u64 = 16;
    const REFERENCE: u64 = 4;
    if matches!(ctx.heap_kind_of(obj), ObjectKind::Array) {
        let element = match ctx.heap_element_type_of(obj) {
            ArrayElementType::Reference => REFERENCE,
            other => cratonvm_types::element_byte_size(other) as u64, // Widening: an element size
        };
        let length = ctx.array_length(obj) as u64; // Widening: an array length
        let bytes = ARRAY_BASE.checked_add(length.checked_mul(element)?)?;
        return i64::try_from(align_to_object(bytes)).ok();
    }
    if !matches!(ctx.heap_kind_of(obj), ObjectKind::Object) {
        return None;
    }
    let own_class = ctx.class_id_of_object(obj);
    if ctx.class_name_of_id(own_class)? == "java/lang/Class" {
        // A mirror: `java.lang.Class`'s own instance (with the fields HotSpot
        // injects), then the mirrored class's static fields, laid out as an
        // instance's are from there (`InstanceMirrorKlass`). A primitive or
        // array class has none.
        let statics: Vec<u8> = ctx
            .class_id_from_mirror(obj)
            .map(|id| {
                ctx.declared_fields(id)
                    .iter()
                    .filter(|f| f.is_static)
                    .filter_map(|f| f.descriptor.bytes().next())
                    .map(hotspot_field_size)
                    .collect()
            })
            .unwrap_or_default();
        return i64::try_from(hotspot_layout_size(CLASS_MIRROR_STATICS_OFFSET, &[statics])).ok();
    }
    // Remembered per class (`NativeRealm::hotspot_size_memo`), checked
    // against the class's field count.
    let tag = ctx.class_num_total_fields(own_class);
    if let Some(size) = ctx.hotspot_size_memo(own_class, tag) {
        return i64::try_from(size).ok();
    }
    // The class chain, `java.lang.Object` first.
    let mut chain = Vec::new();
    let mut class = Some(own_class);
    while let Some(id) = class {
        let name = ctx.class_name_of_id(id)?;
        if name == "jdk/internal/vm/StackChunk" {
            return None;
        }
        let mut sizes: Vec<u8> = ctx
            .declared_fields(id)
            .iter()
            .filter(|f| !f.is_static)
            .filter_map(|f| f.descriptor.bytes().next())
            .map(hotspot_field_size)
            .collect();
        sizes.extend(
            hotspot_injected_fields(&name)
                .iter()
                .map(|&t| hotspot_field_size(t)),
        );
        chain.push(sizes);
        class = ctx.superclass_of(id);
        if chain.len() > 1024 {
            return None;
        }
    }
    chain.reverse();
    let size = hotspot_instance_size(&chain);
    ctx.remember_hotspot_size(own_class, tag, size);
    i64::try_from(size).ok()
}

/// The fields HotSpot 25 injects into JDK classes (`javaClasses.hpp`,
/// `ALL_INJECTED_FIELDS`), as descriptor tags, for the classes
/// [`hotspot_object_size`] models (`J` for an `intptr_t`).
fn hotspot_injected_fields(class_name: &str) -> &'static [u8] {
    match class_name {
        "java/lang/String" => b"B",
        // `jvmti_thread_state`, `jvmti_VTMS_transition_disable_count`,
        // `jvmti_is_in_VTMS_transition`, `jfr_epoch` (a JFR build; without
        // it the size is the same).
        "java/lang/Thread" => b"JIZS",
        "java/lang/ClassLoader" => b"J",
        "java/lang/Module" => b"J",
        "java/lang/InternalError" => b"Z",
        "java/lang/StackFrameInfo" => b"S",
        "java/lang/invoke/MemberName" => b"J",
        "java/lang/invoke/ResolvedMethodName" => b"JL",
        "java/lang/invoke/MethodHandleNatives$CallSiteContext" => b"JJ",
        _ => b"",
    }
}

/// The bytes a field of descriptor tag `tag` takes in HotSpot's layout, a
/// reference 4 (compressed); also its alignment. A reference is told apart
/// by the size 0 marker [`HOTSPOT_OOP`].
fn hotspot_field_size(tag: u8) -> u8 {
    match tag {
        b'J' | b'D' => 8,
        b'I' | b'F' => 4,
        b'S' | b'C' => 2,
        b'B' | b'Z' => 1,
        _ => HOTSPOT_OOP,
    }
}

/// [`hotspot_field_size`]'s marker for a reference field (4 bytes, placed
/// after the class's primitive fields).
const HOTSPOT_OOP: u8 = 0;

/// `bytes` rounded up to HotSpot's 8-byte object alignment.
fn align_to_object(bytes: u64) -> u64 {
    (bytes + 7) & !7
}

/// One block of a modelled HotSpot field layout ([`hotspot_instance_size`]):
/// `size` bytes at `offset`, free or taken. The last block is free and open.
#[derive(Clone, Copy)]
struct LayoutBlock {
    offset: u32,
    size: u32,
    free: bool,
}

/// The size of the last, open block of a layout.
const OPEN_BLOCK: u32 = u32::MAX;

/// HotSpot 25's instance size for an object whose classes, from
/// `java.lang.Object` down, declare instance fields of the sizes in `chain`
/// ([`hotspot_field_size`]; [`HOTSPOT_OOP`] for a reference): the model of
/// `FieldLayoutBuilder::compute_regular_layout` (no `@Contended`). A 12-byte
/// header; each class's primitive fields, largest first, then its references,
/// each placed by `FieldLayout::add`: in the smallest free hole between the
/// header and the open end that fits it at its own alignment (the holes a
/// superclass left included), else at the open end; when a search for a
/// size failed, the next field of that size goes to the end at once. The
/// size is the open end's offset, 8-byte aligned.
fn hotspot_instance_size(chain: &[Vec<u8>]) -> u64 {
    const HEADER: u32 = 12;
    hotspot_layout_size(HEADER, chain)
}

/// Where HotSpot 25 lays out a mirror's static fields: the size of a
/// `java.lang.Class` instance (MEASURED: `Object.class`, an array class and a
/// primitive class are each 120 bytes, `L1W62GetObjectSizeShapes`).
const CLASS_MIRROR_STATICS_OFFSET: u32 = 120;

/// [`hotspot_instance_size`]'s layout from a reserved prefix of `reserved`
/// bytes (an instance's header, or a mirror's `java.lang.Class` part).
fn hotspot_layout_size(reserved: u32, chain: &[Vec<u8>]) -> u64 {
    let mut blocks = vec![
        LayoutBlock {
            offset: 0,
            size: reserved,
            free: false,
        },
        LayoutBlock {
            offset: reserved,
            size: OPEN_BLOCK,
            free: true,
        },
    ];
    for class in chain {
        let mut primitives: Vec<u32> = class
            .iter()
            .filter(|&&s| s != HOTSPOT_OOP)
            .map(|&s| u32::from(s))
            .collect();
        primitives.sort_unstable_by(|a, b| b.cmp(a));
        let references = vec![4u32; class.iter().filter(|&&s| s == HOTSPOT_OOP).count()];
        place_fields(&mut blocks, &primitives);
        place_fields(&mut blocks, &references);
    }
    let end = blocks
        .last()
        .map_or(u64::from(reserved), |b| u64::from(b.offset));
    align_to_object(end)
}

/// `FieldLayout::add` over `sizes` (each also its alignment), see
/// [`hotspot_instance_size`].
fn place_fields(blocks: &mut Vec<LayoutBlock>, sizes: &[u32]) {
    let fits = |b: &LayoutBlock, size: u32| {
        let adjustment = (size - b.offset % size) % size;
        b.size == OPEN_BLOCK || b.size >= size + adjustment
    };
    let mut last_size = 0u32;
    let mut last_found = false;
    for &size in sizes {
        let last = blocks.len() - 1;
        let candidate = if size == last_size && !last_found {
            last
        } else {
            last_size = size;
            last_found = true;
            let mut best: Option<usize> = None;
            // From the block before the open end back to (not including)
            // the first block, the header.
            for i in (1..last).rev() {
                let b = &blocks[i];
                let smaller = match best {
                    Some(c) => b.size < blocks[c].size,
                    None => true,
                };
                if b.free && fits(b, size) && smaller {
                    best = Some(i);
                }
            }
            best.unwrap_or_else(|| {
                last_found = false;
                last
            })
        };
        let mut slot = candidate;
        let adjustment = (size - blocks[slot].offset % size) % size;
        if adjustment > 0 {
            let at = blocks[slot].offset;
            blocks.insert(
                slot,
                LayoutBlock {
                    offset: at,
                    size: adjustment,
                    free: true,
                },
            );
            slot += 1;
            take_front(&mut blocks[slot], adjustment);
        }
        let at = blocks[slot].offset;
        blocks.insert(
            slot,
            LayoutBlock {
                offset: at,
                size,
                free: false,
            },
        );
        slot += 1;
        take_front(&mut blocks[slot], size);
        if blocks[slot].size == 0 {
            blocks.remove(slot);
        }
    }
}

/// Shrink free block `b` by `bytes` at its front.
fn take_front(b: &mut LayoutBlock, bytes: u32) {
    b.offset += bytes;
    if b.size != OPEN_BLOCK {
        b.size -= bytes;
    }
}

/// A header-derived footprint as a `getObjectSize` answer: saturated into
/// `i64` and kept strictly positive (the spec's one hard requirement).
fn size_from_footprint(bytes: Option<usize>) -> Option<i64> {
    bytes.map(|b| i64::try_from(b).unwrap_or(i64::MAX).max(1))
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
///
/// Interpreter round i6 wave 120, lane L3: an agent whose transformers the
/// JDK's own `TransformerManager`s keep (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`,
/// [`jdk_managed_instrumentation`]) never reaches this function: its
/// `addTransformer` runs the JDK body, which registers every copy, as HotSpot
/// does. The filter applies to the VM's own chain only.
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

/// `void addTransformer(ClassFileTransformer[, boolean canRetransform])`,
/// the public methods. Args: [this, transformer(, canRetransform)]. The
/// JDK method's checks first (`throw_instrument_misuse`): a `null`
/// transformer, then a retransform-capable one for an agent without
/// `Can-Retransform-Classes`.
fn native_add_transformer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    can_retransform: bool,
) -> MethodCallResult {
    // Interpreter round i6 wave 119, lane L3 (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`):
    // the JDK's own body, with its checks and its `TransformerManager`s.
    if let Some(inst) = jdk_managed_instrumentation(&*ctx, instrumentation_receiver(args)) {
        let transformer = args.get(1).copied().unwrap_or(Value::Object(None));
        return ctx.invoke_special_bytecode_only(
            INSTRUMENTATION_IMPL,
            "addTransformer",
            "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
            &[
                Value::Object(Some(inst)),
                transformer,
                Value::Int(i32::from(can_retransform)),
            ],
        );
    }
    let transformer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'transformer' in addTransformer"),
            ))
        }
    };
    if can_retransform
        && !agent_capability(&*ctx, instrumentation_receiver(args), CAN_RETRANSFORM_FIELD)
    {
        return Err(throw_instrument_misuse(
            ctx,
            UOE_CLASS,
            Some("adding retransformable transformers is not supported in this environment"),
        ));
    }
    add_instrumentation_transformer(ctx, transformer, can_retransform);
    Ok(None)
}

/// `boolean removeTransformer(ClassFileTransformer transformer)`.
/// Args: [this, transformer].
fn native_remove_transformer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 119 (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`): the JDK's own body.
    if let Some(inst) = jdk_managed_instrumentation(&*ctx, instrumentation_receiver(args)) {
        let transformer = args.get(1).copied().unwrap_or(Value::Object(None));
        return ctx.invoke_special_bytecode_only(
            INSTRUMENTATION_IMPL,
            "removeTransformer",
            "(Ljava/lang/instrument/ClassFileTransformer;)Z",
            &[Value::Object(Some(inst)), transformer],
        );
    }
    let transformer = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'transformer' in removeTransformer"),
            ))
        }
    };
    let removed = remove_transformer_entry(ctx.vm_identity(), transformer);
    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

/// `boolean isModifiableClass0(Class<?>)`. Args: [this, classMirror].
fn native_is_modifiable_class0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mirror = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'theClass' in isModifiableClass"),
            ))
        }
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
    // HotSpot has the eight primitive array classes from start-up, so they
    // are always in its answer; here they are made on first use, so they
    // are made now (interpreter round i1 wave 42, lane L3;
    // `L3W42InitiatedArrays`, row `all`).
    for name in PRIMITIVE_ARRAY_CLASSES {
        let _ =
            ctx.array_class_id_for_loader(name, cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP);
    }
    let mut class_ids = ctx.list_loaded_class_ids();
    // And the lambda proxy classes, which HotSpot lists as the hidden classes
    // they are; the VM mints them outside the class store (wave 42 follow-up,
    // lane L3; `L3W42InitiatedArrays`, row `all-hidden`). `isModifiableClass`
    // answers `false` for them (`mirror_classification`).
    class_ids.extend(ctx.list_lambda_proxy_class_ids());
    class_mirror_array(ctx, class_ids)
}

/// The eight primitive array classes, which HotSpot creates at start-up and
/// lists in `getAllLoadedClasses` and in every loader's `getInitiatedClasses`.
const PRIMITIVE_ARRAY_CLASSES: [&str; 8] = ["[Z", "[C", "[F", "[D", "[B", "[S", "[I", "[J"];

/// A `Class[]` of the mirrors of `class_ids`. The array is pinned across the
/// mirror lookups, which allocate a mirror not made yet (an array class's,
/// typically): it used to be held raw, so a collection there could move it
/// under the next store (interpreter round i1 wave 42, lane L3).
fn class_mirror_array(ctx: &mut dyn NativeContext, class_ids: Vec<ClassId>) -> MethodCallResult {
    let class_class_id = ctx
        .class_id_by_name("java/lang/Class")
        .unwrap_or(ClassId::new(0));
    let arr = ctx.new_ref_array(class_class_id, class_ids.len());
    let pin = ctx.pin_native_root(arr);
    let mut arr = arr;
    for (i, cid) in class_ids.into_iter().enumerate() {
        let mirror = ctx.get_class_mirror(cid);
        arr = ctx.read_native_pin(pin, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let arr = ctx.read_native_pin(pin, arr);
    ctx.unpin_native_roots(pin);
    Ok(Some(Value::Object(Some(arr))))
}

/// `Class<?>[] getInitiatedClasses0(ClassLoader loader)`.
/// Args: [this, loaderObject].
fn native_get_initiated_classes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The loader's namespace, in `ClassLoaderId::to_native_id`'s wire form
    // (interpreter round i1 wave 41, lane L3): `null` is the bootstrap loader
    // (`0`); a loader object is the namespace its defines record
    // (`classloader::loader_namespace_id`: the platform and application
    // loaders' reserved ids, a user loader's own). Its `0` for "some other
    // built-in loader" keeps the application namespace this native always
    // answered. Until wave 41 every call answered the application
    // namespace, whatever the loader: `getInitiatedClasses(null)` had no
    // `java.lang.String` and a user loader's call missed the classes it
    // defined (`tools/probes/interp/L3/L3W40InitiatedClassesByLoader.java`).
    // Since wave 42 (lane L3) the list is HotSpot's shape
    // ([`initiated_class_ids`], which the C table's `GetClassLoaderClasses`
    // shares since interpreter round i2 wave 54).
    let loader = match args.get(1) {
        Some(Value::Object(Some(loader))) => Some(*loader),
        _ => None,
    };
    let class_ids = initiated_class_ids(ctx, loader);
    class_mirror_array(ctx, class_ids)
}

/// `long getObjectSize0(Object obj)`. Args: [this, obj].
fn native_get_object_size0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'objectToSize' in getObjectSize"),
            ))
        }
    };
    Ok(Some(Value::Long(object_size_of(ctx, obj))))
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
    // The JDK method's checks, in its order (`throw_instrument_misuse`):
    // the agent's `Can-Redefine-Classes`, a `null` array, a `null` element.
    if !agent_capability(&*ctx, inst_receiver, CAN_REDEFINE_FIELD) {
        return Err(throw_instrument_misuse(
            ctx,
            UOE_CLASS,
            Some("redefineClasses is not supported in this environment"),
        ));
    }
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'definitions' in redefineClasses"),
            ))
        }
    };
    let n = ctx.array_length(arr);
    if has_null_element(&*ctx, arr, n) {
        return Err(throw_instrument_misuse(
            ctx,
            NPE_CLASS,
            Some("element of 'definitions' is null in redefineClasses"),
        ));
    }
    // A primitive, array or hidden class refuses the whole call before any
    // transformer runs (`first_unmodifiable_class`). Nothing is pinned or
    // opened yet, and the check allocates nothing.
    if let Some(refusal) = first_unmodifiable_class(&*ctx, arr, n, |ctx, definition| {
        read_class_def_field(ctx, definition, "mClass", 0)
    }) {
        return Err(throw_redefinition_refusal(
            ctx,
            "redefineClasses0",
            &refusal,
        ));
    }
    // GC-SAFETY: same hazard as `native_retransform_classes0` below, same
    // shape. `run_transformer_chain` runs agent bytecode and `redefine_class`
    // defines a class; both allocate, so `arr` and every reference read out of
    // it must be pinned and re-read rather than carried across.
    let arr_pin = ctx.pin_native_root(arr);
    let mut arr = arr;
    let inst_pin = inst_receiver.map(|r| ctx.pin_native_root(r));
    let mut inst_receiver = inst_receiver;
    // One loop-exit handshake for the whole call (interpreter round i1 wave
    // 25, lane L6; `begin_redefinition_batch`).
    let batch = ctx.begin_redefinition_batch();
    // Every definition's final class file, with its retransformation base,
    // collected before any class changes (`install_redefinitions`).
    // gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    // finding 3): an unwind out of a transformer chain or the install (the
    // native funnel's OOM scope catches it and throws `OutOfMemoryError`)
    // still closes the batch and releases the array pin: left open,
    // `thread.redefinition_batch` stayed `Some` and every later batch on
    // this thread skipped its loop-exit handshake. Re-raised unchanged.
    let installed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut definitions: Vec<PendingRedefinition> = Vec::with_capacity(n);
        for i in 0..n {
            arr = ctx.read_native_pin(arr_pin, arr);
            if let (Some(pin), Some(r)) = (inst_pin, inst_receiver) {
                inst_receiver = Some(ctx.read_native_pin(pin, r));
            }
            let elem = match ctx.get_array_element(arr, i) {
                Value::Object(Some(o)) => o,
                // Reported, not swallowed — the same rule the retransform twin
                // now follows. A dropped element is a class the agent asked to
                // redefine and did not get, with no exception to say so.
                other => {
                    eprintln!(
                        "[instrument] redefineClasses0: element {i} of {n} is not a \
                         ClassDefinition ({other:?}); that class is NOT redefined and the \
                         agent is not told"
                    );
                    continue;
                }
            };
            let elem_pin = ctx.pin_native_root(elem);
            let class_mirror = read_class_def_field(ctx, elem, "mClass", 0);
            let bytes_arr = read_class_def_field(ctx, elem, "mClassFile", 1);
            let (target_class, target_class_id) = match class_mirror {
                Some(m) => match ctx.class_id_from_mirror(m) {
                    Some(cid) => (m, cid),
                    None => {
                        eprintln!(
                            "[instrument] redefineClasses0: element {i} of {n} has a mClass \
                             with no class id; that class is NOT redefined and the agent is \
                             not told"
                        );
                        ctx.unpin_native_roots(elem_pin);
                        continue;
                    }
                },
                None => {
                    eprintln!(
                        "[instrument] redefineClasses0: element {i} of {n} has no mClass; \
                         that definition is NOT applied and the agent is not told"
                    );
                    ctx.unpin_native_roots(elem_pin);
                    continue;
                }
            };
            let class_pin = ctx.pin_native_root(target_class);
            let bytes_obj = match bytes_arr {
                Some(o) => o,
                None => {
                    eprintln!(
                        "[instrument] redefineClasses0: element {i} of {n} has no mClassFile; \
                         that definition is NOT applied and the agent is not told"
                    );
                    ctx.unpin_native_roots(elem_pin);
                    continue;
                }
            };
            let new_bytes = read_byte_array(ctx, bytes_obj);
            let target_class = ctx.read_native_pin(class_pin, target_class);
            // Run the chain: every transformer, the non-retransformable ones
            // first (see `run_chain_over_bytes`), each output threaded into the
            // next. The second answer is the class file as it was before the
            // first retransform-capable transformer changed it, which JVMTI keeps
            // as the class's retransformation base (`L3W27RedefineKeepsTheBase`).
            let (final_bytes, retransform_base) = run_transformer_chain(
                ctx,
                target_class_id,
                Some(target_class),
                &new_bytes,
                /* retransform_only = */ false,
                inst_receiver,
            );
            definitions.push(PendingRedefinition {
                class_id: target_class_id,
                bytes: final_bytes,
                retransform_base,
            });
            ctx.unpin_native_roots(elem_pin);
        }
        install_redefinitions(ctx, &definitions, /* retransform = */ false)
    }));
    ctx.end_redefinition_batch(batch);
    ctx.unpin_native_roots(arr_pin);
    let refused = match installed {
        Ok(refused) => refused,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    match refused {
        None => Ok(None),
        Some(refusal) => Err(throw_redefinition_refusal(
            ctx,
            "redefineClasses0",
            &refusal,
        )),
    }
}

/// One class of a `redefineClasses` / `retransformClasses` call, after the
/// transformer chain ran over it and before anything was installed.
struct PendingRedefinition {
    class_id: ClassId,
    /// The chain's output: the class file to install.
    bytes: Vec<u8>,
    /// For a redefinition, the class file as it was before the first
    /// retransform-capable transformer changed it (`run_chain_over_bytes`'s
    /// second answer); `None` when none did. For a retransformation, the base
    /// the chain was handed, which the class keeps if it keeps none yet
    /// (interpreter round i1 wave 29, lane L3).
    retransform_base: Option<Vec<u8>>,
}

/// Install a `redefineClasses` / `retransformClasses` call's classes as JVMTI
/// does (interpreter round i1 wave 27, lane L3): every class is checked
/// before the first one changes (`VM_RedefineClasses::load_new_class_versions`
/// refuses the whole call on the first refusal), then each is redefined. The
/// first refusal is returned for the caller to throw once it has closed its
/// batch and released its pins; `None` when every class was installed.
///
/// The check verifies the new bodies too (since interpreter round i1 wave 28,
/// lane L3; `L3/L3W28VerifyBeforeInstall`), so a verification refusal of any
/// class leaves every class unchanged. A refusal from the redefinition itself
/// (bytes a native `ClassFileLoadHook` substituted, which the check never saw)
/// stops the call there, with the classes before it redefined.
///
/// Until wave 27 each class was redefined as soon as its chain ran, and a
/// refusal was logged and skipped: `redefineClasses` / `retransformClasses`
/// returned normally with the class unchanged, where HotSpot throws
/// (`L3/L3W26RejectedRedefinition`, `L3/L3W27RedefinitionRefusals`).
fn install_redefinitions(
    ctx: &mut dyn NativeContext,
    definitions: &[PendingRedefinition],
    retransform: bool,
) -> Option<cratonvm_native_api::InstrumentRefusal> {
    for definition in definitions {
        if let Err(refusal) = ctx.check_class_redefinition(definition.class_id, &definition.bytes) {
            return Some(refusal);
        }
    }
    // One operation for the other threads: none runs between two classes of
    // the call (interpreter round i1 wave 37, lane L3;
    // `L3/L3W37RedefineTwoClassesAtOnce`). After the checks, which may run a
    // class loader's Java.
    let classes: Vec<(ClassId, &[u8])> = definitions
        .iter()
        .map(|definition| (definition.class_id, definition.bytes.as_slice()))
        .collect();
    let held = ctx.begin_redefinition_install(&classes);
    // gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    // finding 1): the install fence and the code-cache publication hold are
    // lowered on EVERY way out, a heap-exhaustion unwind included
    // (`native_oom::catch_alloc_oom_on` -- since gci i18/na the JVMTI
    // `RedefineClasses` / `RetransformClasses` contexts run under it, as the
    // native funnel always ran `redefineClasses0`). Without this an unwind
    // between the two calls left the fence raised and the publications held
    // for the rest of the process: every later interpreter loop of a fenced
    // class waited forever. The payload is re-raised unchanged.
    let installed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut refused = None;
        for definition in definitions {
            if let Err(refusal) = ctx.redefine_class_as_instrument(
                definition.class_id,
                &definition.bytes,
                retransform,
                definition.retransform_base.as_deref(),
            ) {
                refused = Some(refusal);
                break;
            }
        }
        refused
    }));
    ctx.end_redefinition_install(held);
    match installed {
        Ok(refused) => refused,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// The first class of a `redefineClasses` / `retransformClasses` call that
/// JVMTI may not change -- a primitive, an array or a hidden class, the
/// classes `isModifiableClass` answers `false` for -- as the refusal
/// libinstrument throws: `UnmodifiableClassException` with no message.
/// HotSpot refuses the whole call on it before any transformer runs
/// (`JvmtiEnv::RetransformClasses`, `VM_RedefineClasses::doit_prologue`);
/// CratonVM used to run the chain over whatever bytes it found, or skip the
/// class, and return normally (interpreter round i1 wave 27, lane L3;
/// `L3/L3W27RedefinitionRefusals`, rows `unmodifiable-*`).
///
/// `mirror_of` reads element `i`'s `Class` (the element itself, or a
/// `ClassDefinition`'s `mClass`); an element it cannot read is left to the
/// main loop's own reporting. Nothing here allocates, so the array needs no
/// refresh between elements.
fn first_unmodifiable_class(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    n: usize,
    mirror_of: impl Fn(&dyn NativeContext, ObjectRef) -> Option<ObjectRef>,
) -> Option<cratonvm_native_api::InstrumentRefusal> {
    for i in 0..n {
        let Value::Object(Some(elem)) = ctx.get_array_element(arr, i) else {
            continue;
        };
        let Some(mirror) = mirror_of(ctx, elem) else {
            continue;
        };
        let (is_primitive, is_array, is_hidden) = mirror_classification(ctx, mirror);
        if !is_modifiable_class(is_primitive, is_array, is_hidden) {
            return Some(cratonvm_native_api::InstrumentRefusal {
                throwable: "java/lang/instrument/UnmodifiableClassException",
                message: None,
                detail: format!(
                    "element {i} of {n} is not modifiable (primitive={is_primitive}, \
                     array={is_array}, hidden={is_hidden})"
                ),
            });
        }
    }
    None
}

/// Whether the agent's array (`n` elements) holds a `null`, which the JDK
/// refuses before anything runs (`redefineClasses`' element check,
/// libinstrument's for `retransformClasses`). Allocates nothing.
fn has_null_element(ctx: &dyn NativeContext, arr: ObjectRef, n: usize) -> bool {
    (0..n).any(|index| matches!(ctx.get_array_element(arr, index), Value::Object(None)))
}

/// The throwable `refusal` names, built as libinstrument builds it (a
/// `(String)` constructor with HotSpot's message, or the no-argument one),
/// for the native to throw. The VM's own account of the refusal is logged at
/// `warn`, where the refusal alone used to go.
#[cold]
#[inline(never)]
fn throw_redefinition_refusal(
    ctx: &mut dyn NativeContext,
    api: &str,
    refusal: &cratonvm_native_api::InstrumentRefusal,
) -> MethodCallFailed {
    tracing::warn!("{api}: {}", refusal.detail);
    match build_throwable(ctx, refusal.throwable, refusal.message) {
        Some(failed) => failed,
        None => MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::UnsupportedOperationException {
                message: format!("class redefinition failed: {}", refusal.detail),
            },
        )),
    }
}

/// `throwable` (internal name) built with `message` -- its `(String)`
/// constructor, or the no-argument one for `None` -- as the failure a native
/// returns to throw it. `None` when the constructor answered no object;
/// a constructor that threw (an `OutOfMemoryError`, say) is what the caller
/// sees.
fn build_throwable(
    ctx: &mut dyn NativeContext,
    throwable: &'static str,
    message: Option<&str>,
) -> Option<MethodCallFailed> {
    let built = match message {
        Some(text) => {
            let message = ctx.create_string(text);
            ctx.new_object_initialized(
                throwable,
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(message))],
            )
        }
        None => ctx.new_object_initialized(throwable, "()V", &[]),
    };
    match built {
        Ok(Some(Value::Object(Some(thrown)))) => Some(MethodCallFailed::ExceptionThrown(thrown)),
        Err(failed) => Some(failed),
        Ok(_) => None,
    }
}

/// The `NullPointerException` / `UnsupportedOperationException` the JDK's
/// `InstrumentationImpl` throws from the Java half of a method this file
/// answers natively (interpreter round i1 wave 42, lane L3). The public
/// methods are registered natives here because the `InstrumentationImpl`
/// the VM hands an agent is a bare allocation (`agent_loader`'s
/// `build_instrumentation_mirror`, the no-op `<init>` below), whose
/// `TransformerManager`s do not exist; so their argument and capability
/// checks have to be made here, with HotSpot's messages
/// (`tools/probes/interp/L3/L3W42InstrumentArgumentChecks.java`,
/// `L3W42InstrumentCapabilities.java`). Until wave 42 a `null` was ignored
/// or answered `false` / `0`, and every capability was granted.
#[cold]
#[inline(never)]
fn throw_instrument_misuse(
    ctx: &mut dyn NativeContext,
    throwable: &'static str,
    message: Option<&'static str>,
) -> MethodCallFailed {
    match build_throwable(ctx, throwable, message) {
        Some(failed) => failed,
        None if throwable == NPE_CLASS => {
            MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::NullPointerException {
                message: message.map(str::to_string),
            }))
        }
        None => MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::UnsupportedOperationException {
                message: message.unwrap_or_default().to_string(),
            },
        )),
    }
}

const NPE_CLASS: &str = "java/lang/NullPointerException";
const UOE_CLASS: &str = "java/lang/UnsupportedOperationException";
const IAE_CLASS: &str = "java/lang/IllegalArgumentException";

/// The `InstrumentationImpl` field the VM sets, with the agent's two
/// capability fields, when it creates the object for an agent
/// ([`stamp_agent_capabilities`]); its Java meaning is "the retransform
/// answer is cached", which a stamped object's answer is.
const CAPABILITIES_STAMPED_FIELD: &str = "mEnvironmentSupportsRetransformClassesKnown";
const CAN_REDEFINE_FIELD: &str = "mEnvironmentSupportsRedefineClasses";
const CAN_RETRANSFORM_FIELD: &str = "mEnvironmentSupportsRetransformClasses";
/// The JDK constructor's `environmentSupportsNativeMethodPrefix`, which the
/// JDK's `isNativeMethodPrefixSupported()` returns (wave 44, lane L3).
const CAN_SET_NATIVE_METHOD_PREFIX_FIELD: &str = "mEnvironmentSupportsNativeMethodPrefix";

/// Record an agent's `Can-Redefine-Classes` / `Can-Retransform-Classes` /
/// `Can-Set-Native-Method-Prefix` manifest answers on the
/// `InstrumentationImpl` the VM made for it, in the fields the JDK's
/// constructor and `isRetransformClassesSupported` fill, for
/// [`agent_capability`] to read. Per agent, as JVMTI's capabilities are
/// (interpreter round i1 wave 42, lane L3; the prefix since wave 44).
fn stamp_capabilities(
    ctx: &dyn NativeContext,
    inst: ObjectRef,
    can_redefine: bool,
    can_retransform: bool,
    can_set_native_method_prefix: bool,
) {
    ctx.set_field_by_name(
        inst,
        CAN_REDEFINE_FIELD,
        Value::Int(i32::from(can_redefine)),
    );
    ctx.set_field_by_name(
        inst,
        CAN_RETRANSFORM_FIELD,
        Value::Int(i32::from(can_retransform)),
    );
    ctx.set_field_by_name(
        inst,
        CAN_SET_NATIVE_METHOD_PREFIX_FIELD,
        Value::Int(i32::from(can_set_native_method_prefix)),
    );
    ctx.set_field_by_name(inst, CAPABILITIES_STAMPED_FIELD, Value::Int(1));
}

/// [`stamp_capabilities`] for the `-javaagent:` path
/// (`agent_loader::build_instrumentation_mirror`), which holds no native
/// context of its own.
pub(crate) fn stamp_agent_capabilities(
    shared: &crate::vm::SharedVm,
    thread: &mut crate::threading::JvmThread,
    inst: ObjectRef,
    can_redefine: bool,
    can_retransform: bool,
    can_set_native_method_prefix: bool,
) {
    let ctx = crate::vm::NativeContextImpl { shared, thread };
    stamp_capabilities(
        &ctx,
        inst,
        can_redefine,
        can_retransform,
        can_set_native_method_prefix,
    );
}

/// Whether the agent that owns `receiver` (an `InstrumentationImpl`) may use
/// the capability in `field`: its manifest's answer when the VM stamped the
/// object ([`stamp_capabilities`]), and `true`, as every call answered before
/// wave 42, for an object nothing stamped (no receiver, or one made by a
/// path that does not record capabilities).
fn agent_capability(ctx: &dyn NativeContext, receiver: Option<ObjectRef>, field: &str) -> bool {
    let Some(inst) = receiver else {
        return true;
    };
    if !matches!(ctx.get_field_by_name(inst, CAPABILITIES_STAMPED_FIELD), Value::Int(v) if v != 0) {
        return true;
    }
    matches!(ctx.get_field_by_name(inst, field), Value::Int(v) if v != 0)
}

/// Whether the agent that owns `receiver` may set native-method prefixes:
/// its manifest's `Can-Set-Native-Method-Prefix` when the VM stamped the
/// object ([`stamp_capabilities`]), and `false`, the answer every call gave
/// before wave 44, for an object nothing stamped.
fn agent_can_set_native_method_prefix(
    ctx: &dyn NativeContext,
    receiver: Option<ObjectRef>,
) -> bool {
    let Some(inst) = receiver else {
        return false;
    };
    matches!(ctx.get_field_by_name(inst, CAPABILITIES_STAMPED_FIELD), Value::Int(v) if v != 0)
        && matches!(
            ctx.get_field_by_name(inst, CAN_SET_NATIVE_METHOD_PREFIX_FIELD),
            Value::Int(v) if v != 0
        )
}

/// The native-method prefixes `vm`'s agents set, in the order JVMTI applies
/// them when it retries a failed native link (`SetNativeMethodPrefixes`):
/// the prefixes of the transformers that cannot retransform, then those of
/// the ones that can (the JDK's two `TransformerManager`s, each a JVMTI
/// environment, in the order the environments were created), each group in
/// registration order; a `null` or empty prefix is none. Empty for a VM
/// without prefixes. Read only where a native method's link has already
/// failed (`vm_exec::prefixed_native_link`; interpreter round i1 wave 44,
/// lane L3).
pub(crate) fn native_method_prefixes(vm: usize) -> Vec<String> {
    with_chain(vm, |chain| {
        let prefix_of = |entry: &TransformerEntry| {
            entry
                .native_method_prefix
                .clone()
                .filter(|prefix| !prefix.is_empty())
        };
        chain
            .iter()
            .filter(|entry| !entry.can_retransform)
            .filter_map(prefix_of)
            .chain(
                chain
                    .iter()
                    .filter(|entry| entry.can_retransform)
                    .filter_map(prefix_of),
            )
            .collect()
    })
}

/// `void setNativeMethodPrefix(ClassFileTransformer, String)`, the public
/// method. Args: [this, transformer, prefix]. The JDK method's checks, in
/// its order (`UnsupportedOperationException` without the capability, a
/// `null` transformer, a transformer this VM's chain does not hold), then the
/// prefix recorded on the transformer's entry (interpreter round i1 wave 44,
/// lane L3). A registered native because the JDK body reads the
/// `TransformerManager`s the VM's bare `InstrumentationImpl` does not have.
fn native_set_native_method_prefix(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if !agent_can_set_native_method_prefix(&*ctx, instrumentation_receiver(args)) {
        return Err(throw_instrument_misuse(
            ctx,
            UOE_CLASS,
            Some("setNativeMethodPrefix is not supported in this environment"),
        ));
    }
    let transformer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("null passed as 'transformer' in setNativeMethodPrefix"),
            ))
        }
    };
    let prefix = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let recorded = with_chain_mut(ctx.vm_identity(), |chain| {
        match chain
            .iter_mut()
            .find(|entry| entry.transformer_ref.as_ptr() == transformer.as_ptr())
        {
            Some(entry) => {
                entry.native_method_prefix = prefix;
                true
            }
            None => false,
        }
    });
    if !recorded {
        return Err(throw_instrument_misuse(
            ctx,
            IAE_CLASS,
            Some("transformer not registered in setNativeMethodPrefix"),
        ));
    }
    Ok(None)
}

/// The receiver of an `InstrumentationImpl` native (`args[0]`).
/// `sun/instrument/InstrumentationImpl`, the JDK class every agent is handed.
const INSTRUMENTATION_IMPL: &str = "sun/instrument/InstrumentationImpl";

/// `CRATONVM_INSTRUMENT_JDK_TRANSFORMERS` (default on since wave 133, `=0` off): the next step of
/// `i42-L3-proposal-run-the-jdks-own-instrumentationimpl` (items 2 and 3).
/// Read per call (an agent's `addTransformer` / `removeTransformer` and the
/// two `setHas*Transformers` leaves, all rare). Interpreter round i6 wave
/// 119, lane L3.
fn instrument_jdk_transformers() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_INSTRUMENT_JDK_TRANSFORMERS")
}

/// `receiver`, when the JDK's own transformer bodies serve it: the switch is
/// on, and it is an `InstrumentationImpl` whose JDK constructor ran (its
/// `mTransformerManager` is set; `agent_loader::build_instrumentation_mirror`
/// under `CRATONVM_INSTRUMENT_JDK_CONSTRUCTOR`). Then:
///
/// * `addTransformer` / `removeTransformer` run the JDK's bodies, which keep
///   the transformers in the object's `TransformerManager`s and call the
///   `setHasTransformers` / `setHasRetransformableTransformers` leaves;
/// * those leaves file the object itself in this VM's chain, once per group
///   ([`note_jdk_transformer_group`]);
/// * the chain walk hands such an entry's class file to the JDK's own
///   `InstrumentationImpl.transform(.., isRetransformer)`, which runs the
///   group's transformers in the JDK's order (`run_chain_over_bytes`).
///
/// `None` otherwise: the agent keeps the VM's own chain, as before.
fn jdk_managed_instrumentation(
    ctx: &dyn NativeContext,
    receiver: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let inst = receiver?;
    if !instrument_jdk_transformers() {
        return None;
    }
    let class = ctx.class_name_of_id(ctx.class_id_of_object(inst))?;
    if class != INSTRUMENTATION_IMPL {
        return None;
    }
    matches!(
        ctx.get_field_by_name(inst, "mTransformerManager"),
        Value::Object(Some(_))
    )
    .then_some(inst)
}

/// The `setHasTransformers` / `setHasRetransformableTransformers` leaf
/// (`(long, boolean)`) for a JDK-managed `InstrumentationImpl`
/// ([`jdk_managed_instrumentation`]): `has` files the object in this VM's
/// chain as the entry of its `retransformable` group (once), `!has` removes
/// that entry. The chain roots the object (`scan_transformer_roots`).
/// Interpreter round i6 wave 119, lane L3.
fn note_jdk_transformer_group(
    ctx: &dyn NativeContext,
    inst: ObjectRef,
    retransformable: bool,
    has: bool,
) {
    // Interpreter round i6 wave 124, lane L3 (`i42-L3`, the orchestrator's
    // w122 run): the group entry is a registered transformer like any other,
    // so it arms the load-path fast-path test as `add_transformer_entry`
    // does. Before this the push below left `ANY_TRANSFORMER_REGISTERED`
    // false when the JDK-managed agent was the only one, `transformers_armed`
    // answered `false`, and `pre_transform_for_load` offered no class at load
    // (`L3W119JdkTransformers`: `load: Late.v=v000 seen=[]`); retransformation
    // does not ask that test, so it reached the group. Reached only with
    // `CRATONVM_INSTRUMENT_JDK_TRANSFORMERS` on (`jdk_managed_instrumentation`).
    if has {
        ANY_TRANSFORMER_REGISTERED.store(true, std::sync::atomic::Ordering::Release);
    }
    with_chain_mut(ctx.vm_identity(), |chain| {
        let filed = chain.iter().position(|e| {
            e.transformer_ref.as_ptr() == inst.as_ptr() && e.can_retransform == retransformable
        });
        match (has, filed) {
            (true, None) => chain.push(TransformerEntry {
                transformer_ref: inst,
                can_retransform: retransformable,
                native_method_prefix: None,
            }),
            (false, Some(at)) => {
                chain.remove(at);
            }
            _ => {}
        }
    });
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[instrument] JDK transformer group retransformable={retransformable} has={has}"
        );
    }
}

/// A public `InstrumentationImpl` method of the second family
/// (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`; interpreter round i6 wave 120,
/// lane L3, item 2 of `i42-L3-proposal-run-the-jdks-own-instrumentationimpl`):
/// for a JDK-managed receiver ([`jdk_managed_instrumentation`]) the JDK's own
/// body `name descriptor`, which reaches this VM only through its `ACC_NATIVE`
/// leaf (`isRetransformClassesSupported0`, `isModifiableClass0`,
/// `getObjectSize0`, `getAllLoadedClasses0`, `retransformClasses0`,
/// `redefineClasses0`) or the capability fields the JDK constructor and the
/// stamp set; for every other receiver `fallback`, the native as before.
fn jdk_body_or(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
    fallback: NativeCallback,
) -> MethodCallResult {
    if let Some(inst) = jdk_managed_instrumentation(&*ctx, instrumentation_receiver(args)) {
        let mut jdk_args = Vec::with_capacity(args.len().max(1));
        jdk_args.push(Value::Object(Some(inst)));
        jdk_args.extend_from_slice(args.get(1..).unwrap_or(&[]));
        return ctx.invoke_special_bytecode_only(INSTRUMENTATION_IMPL, name, descriptor, &jdk_args);
    }
    fallback(ctx, args)
}

/// `isRetransformClassesSupported()`: [`jdk_body_or`].
fn public_is_retransform_supported(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(ctx, args, "isRetransformClassesSupported", "()Z", native_is_retransform_supported0)
}

/// `isRedefineClassesSupported()`: [`jdk_body_or`].
fn public_is_redefine_supported(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(ctx, args, "isRedefineClassesSupported", "()Z", native_is_redefine_supported0)
}

/// `isNativeMethodPrefixSupported()`: [`jdk_body_or`].
fn public_is_prefix_supported(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(ctx, args, "isNativeMethodPrefixSupported", "()Z", native_is_prefix_supported0)
}

/// `isModifiableClass(Class)`: [`jdk_body_or`].
fn public_is_modifiable_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(ctx, args, "isModifiableClass", "(Ljava/lang/Class;)Z", native_is_modifiable_class0)
}

/// `getObjectSize(Object)`: [`jdk_body_or`].
fn public_get_object_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(ctx, args, "getObjectSize", "(Ljava/lang/Object;)J", native_get_object_size0)
}

/// `getAllLoadedClasses()`: [`jdk_body_or`].
fn public_get_all_loaded_classes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(
        ctx,
        args,
        "getAllLoadedClasses",
        "()[Ljava/lang/Class;",
        native_get_all_loaded_classes0,
    )
}

/// `retransformClasses(Class...)`: [`jdk_body_or`].
fn public_retransform_classes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(
        ctx,
        args,
        "retransformClasses",
        "([Ljava/lang/Class;)V",
        native_retransform_classes0,
    )
}

/// `redefineClasses(ClassDefinition...)`: [`jdk_body_or`].
fn public_redefine_classes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jdk_body_or(
        ctx,
        args,
        "redefineClasses",
        "([Ljava/lang/instrument/ClassDefinition;)V",
        native_redefine_classes0,
    )
}

/// Is `transformer` (a chain entry's object) an `InstrumentationImpl` filed by
/// [`note_jdk_transformer_group`]? Asked per entry of a chain walk.
fn is_jdk_transformer_group(ctx: &dyn NativeContext, transformer: ObjectRef) -> bool {
    ctx.class_name_of_id(ctx.class_id_of_object(transformer))
        .is_some_and(|name| name == INSTRUMENTATION_IMPL)
}

fn instrumentation_receiver(args: &[Value]) -> Option<ObjectRef> {
    match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

/// `void retransformClasses0(Class<?>[] classes)`. Args: [this, arr].
fn native_retransform_classes0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let inst_receiver = match args.first() {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    // The JDK method's checks, in its order (`throw_instrument_misuse`): the
    // agent's `Can-Retransform-Classes`, then a `null` array (HotSpot's
    // message is the helpful `NullPointerException`'s for `classes.length`),
    // then -- libinstrument's own, before JVMTI sees the call -- a `null`
    // element, a `NullPointerException` without a message.
    if !agent_capability(&*ctx, inst_receiver, CAN_RETRANSFORM_FIELD) {
        return Err(throw_instrument_misuse(
            ctx,
            UOE_CLASS,
            Some("retransformClasses is not supported in this environment"),
        ));
    }
    let arr = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("Cannot read the array length because \"classes\" is null"),
            ))
        }
    };
    let n = ctx.array_length(arr);
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!("[RETRANSFORM] retransformClasses0 called with {n} classes");
    }
    if has_null_element(&*ctx, arr, n) {
        return Err(throw_instrument_misuse(ctx, NPE_CLASS, None));
    }
    // A primitive, array or hidden class refuses the whole call before any
    // transformer runs (`first_unmodifiable_class`). Nothing is pinned or
    // opened yet, and the check allocates nothing.
    if let Some(refusal) = first_unmodifiable_class(&*ctx, arr, n, |_, class| Some(class)) {
        return Err(throw_redefinition_refusal(
            ctx,
            "retransformClasses0",
            &refusal,
        ));
    }
    // GC-SAFETY. Every iteration of this loop runs Java — the transformer
    // chain is agent bytecode — and defines classes, so it allocates and can
    // safepoint. `arr` and `inst_receiver` are raw `ObjectRef`s captured
    // before that; a moving collector relocates the `Class[]` out from under
    // them and the next `get_array_element` reads whatever now occupies the
    // old address, which is a zero word. That is not a crash: the read yields
    // `Object(None)`, the `_ => continue` arm swallows it, and the REMAINING
    // CLASSES OF THE HIERARCHY ARE NEVER RETRANSFORMED — silently, with no
    // error for the agent to see.
    //
    // Measured on Mockito's inline mock maker, whose `mock()` retransforms the
    // whole superclass chain in one call. A relocation during the FIRST
    // class's transformation left `[java/net/URLConnection]` woven and
    // `[java/lang/Object, …/NestedUrlConnection]` untouched, so the mock did
    // not intercept and the first call on it ran the real body:
    // Spring Boot's `NestedUrlConnectionTests` NPE, ~12% of processes here and
    // ~2.4% through the suite runner.
    let arr_pin = ctx.pin_native_root(arr);
    let mut arr = arr;
    let inst_pin = inst_receiver.map(|r| ctx.pin_native_root(r));
    let mut inst_receiver = inst_receiver;
    // One loop-exit handshake for the whole call, not one per redefinition
    // (interpreter round i1 wave 25, lane L6; `begin_redefinition_batch`).
    let batch = ctx.begin_redefinition_batch();
    // Every class's chain output, collected before any class changes: JVMTI
    // `RetransformClasses` runs the transformers over every class, then
    // checks them all, then redefines them (`install_redefinitions`).
    // gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    // finding 3): an unwind out of a transformer chain or the install (the
    // native funnel's OOM scope catches it and throws `OutOfMemoryError`)
    // still closes the batch and releases the array pin: left open,
    // `thread.redefinition_batch` stayed `Some` and every later batch on
    // this thread skipped its loop-exit handshake. Re-raised unchanged.
    let installed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut definitions: Vec<PendingRedefinition> = Vec::with_capacity(n);
        for i in 0..n {
            // The refresh that makes the rest of this loop safe. On the
            // measured workload the array moved before EVERY i=1, so this is the
            // common path, not a corner.
            arr = ctx.read_native_pin(arr_pin, arr);
            if let (Some(pin), Some(r)) = (inst_pin, inst_receiver) {
                inst_receiver = Some(ctx.read_native_pin(pin, r));
            }
            let mirror = match ctx.get_array_element(arr, i) {
                Value::Object(Some(o)) => o,
                // Reported, not swallowed. This arm is how the GC-safety bug
                // above stayed invisible: the agent asked for a class, the read
                // came back as a zero word, `continue` dropped it, and Mockito
                // was handed a half-instrumented hierarchy with no error. If it
                // ever fires again the next reader gets a line instead of a
                // mystery.
                other => {
                    eprintln!(
                        "[instrument] retransformClasses0: element {i} of {n} is not a Class \
                         ({other:?}); that class is NOT retransformed and the agent is not told"
                    );
                    continue;
                }
            };
            let mirror_pin = ctx.pin_native_root(mirror);
            let class_id = match ctx.class_id_from_mirror(mirror) {
                Some(cid) => cid,
                None => {
                    eprintln!(
                        "[instrument] retransformClasses0: element {i} of {n} has no class id; \
                         that class is NOT retransformed and the agent is not told"
                    );
                    ctx.unpin_native_roots(mirror_pin);
                    continue;
                }
            };
            if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
                let nm = ctx.class_name_of_id(class_id).unwrap_or_default();
                let ob = original_class_bytes(ctx, class_id);
                eprintln!("[RETRANSFORM]   [{i}] {nm} original_bytes={}", ob.len());
            }
            // The JVMTI retransformation base (`retransformation_base`).
            let original = retransformation_base(ctx, class_id, mirror, mirror_pin);
            // The defining loader may have run Java: refresh what this iteration
            // holds.
            if let (Some(pin), Some(r)) = (inst_pin, inst_receiver) {
                inst_receiver = Some(ctx.read_native_pin(pin, r));
            }
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
                ctx.unpin_native_roots(mirror_pin);
                tracing::warn!(
                    "retransformClasses0: no retransformation base for `{}`; skipping \
                     (see the preceding diagnostic for which of the three reasons applied)",
                    ctx.class_name_of_id(class_id).unwrap_or_default(),
                );
                continue;
            }
            // The transformers are handed the retransformation base, but the class
            // keeps its CURRENT version while they run: JVMTI `RetransformClasses`
            // redefines each class once, with the chain's output, and
            // `classBeingRedefined` reflects (and any thread runs) the previous
            // retransform's result until then. HotSpot does exactly this.
            //
            // Until interpreter round i1 wave 26 this point first redefined the
            // class back to `original` (commit c02c20e57, 2026-07-28), so every
            // retransform was two redefinitions: twice the history steps, twice
            // the pauses, a native `ClassFileLoadHook` fired for both, and the base
            // body running meanwhile (`L3W25RetransformRunsTheBase`). Its comment
            // named Byte Buddy "combining the supplied bytes with reflection over
            // `classBeingRedefined`" when "a later temporary test loader installs
            // a second transformer"; that case is the second Mockito inline maker,
            // which the same commit stopped registering at all
            // (`add_instrumentation_transformer`), and reflection over a woven
            // class is what Byte Buddy sees on HotSpot too. See
            // `interpreter-L3-retransform-redefines-each-class-twice-FIXED-20260928.md`.
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            // The second answer (a new base) is a redefinition's: a
            // retransformation keeps the class's base.
            let (final_bytes, _) = run_transformer_chain(
                ctx,
                class_id,
                Some(mirror),
                &original,
                /* retransform_only = */ true,
                inst_receiver,
            );
            ctx.unpin_native_roots(mirror_pin);
            if final_bytes.is_empty() {
                continue;
            }
            // retransform (not redefine): `install_redefinitions` preserves the
            // class's original cached bytes so a subsequent retransform re-runs
            // the chain from the original, avoiding double-instrumentation
            // (mockStatic+mock). The base is handed along so the class keeps it
            // once it is woven (pinned unless it already is; interpreter round
            // i1 wave 29, lane L3): a base the FIFO evicts after this could
            // otherwise only be reconstituted from the woven class, which
            // `instrument_reconstitute` refuses, so the next retransform of a
            // generated class was skipped.
            definitions.push(PendingRedefinition {
                class_id,
                bytes: final_bytes,
                retransform_base: Some(original),
            });
        }
        install_redefinitions(ctx, &definitions, /* retransform = */ true)
    }));
    ctx.end_redefinition_batch(batch);
    ctx.unpin_native_roots(arr_pin);
    let refused = match installed {
        Ok(refused) => refused,
        Err(payload) => std::panic::resume_unwind(payload),
    };
    match refused {
        None => Ok(None),
        Some(refusal) => Err(throw_redefinition_refusal(
            ctx,
            "retransformClasses0",
            &refusal,
        )),
    }
}

/// The JVMTI retransformation base of `class_id`, whose mirror `mirror` is
/// pinned at `mirror_pin`: the class-bytes cache first (the only source that
/// knows dynamically-defined, hidden and agent-redefined classes), then
/// `<name>.class` off the classpath if the cache's soft cap evicted it, then
/// the class's own loader's `<name>.class` (`defining_loader_class_bytes`,
/// interpreter round i1 wave 28, lane L3), and last a class file written
/// back from the live class, as HotSpot reconstitutes one: a generated class
/// no loader can serve again (wave 29, lane L3; `instrument_reconstitute`),
/// refused for a class the live version of which may not be its base. A base
/// the class path cannot give back -- a file an agent transformed at load or
/// `redefineClasses` installed -- is pinned in the cache and never reaches
/// either fallback (`ClassManager::insert_class_bytes_pinned`). Empty when
/// there is no trustworthy base. The defining loader's lookup runs Java.
/// Shared by `retransformClasses0` and the C table's `RetransformClasses`
/// (interpreter round i2 wave 54, lane L1).
fn retransformation_base(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    mirror: ObjectRef,
    mirror_pin: usize,
) -> Vec<u8> {
    match try_original_class_bytes(ctx, class_id) {
        Ok(bytes) => bytes,
        Err(None) => Vec::new(),
        Err(Some(miss)) => {
            let live_mirror = ctx.read_native_pin(mirror_pin, mirror);
            let found = defining_loader_class_bytes(ctx, class_id, live_mirror, &miss.resource);
            match found.or_else(|| ctx.reconstituted_class_bytes(class_id)) {
                Some(bytes) => bytes,
                None => {
                    miss.log();
                    Vec::new()
                }
            }
        }
    }
}

/// JVMTI `RetransformClasses` for a C agent (the C table's slot 152,
/// interpreter round i2 wave 54, lane L1): every class's retransformation
/// base ([`retransformation_base`]) through the retransform-capable half of
/// the chain -- the C envs that hold `can_retransform_classes` and the Java
/// transformers registered with `canRetransform`, as HotSpot posts
/// `ClassFileLoadHook` to every retransform-capable env whichever env asked
/// -- then every class checked and redefined at once
/// ([`install_redefinitions`]), as `retransformClasses0` does. A class
/// without a trustworthy base is left as it is. The caller has refused the
/// unmodifiable classes. The first refusal, if any.
#[cfg(feature = "experimental-debug")]
pub(crate) fn retransform_classes_for_jvmti(
    ctx: &mut dyn NativeContext,
    class_ids: &[ClassId],
) -> Option<cratonvm_native_api::InstrumentRefusal> {
    let batch = ctx.begin_redefinition_batch();
    // gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    // finding 1): as in `redefine_classes_for_jvmti`, the batch is closed on
    // a heap-exhaustion unwind too (the JVMTI context runs under
    // `native_oom::catch_alloc_oom_on` since gci i18/na); the payload is
    // re-raised unchanged.
    let retransformed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut definitions: Vec<PendingRedefinition> = Vec::with_capacity(class_ids.len());
        for &class_id in class_ids {
            let mirror = ctx.get_class_mirror(class_id);
            let mirror_pin = ctx.pin_native_root(mirror);
            let original = retransformation_base(ctx, class_id, mirror, mirror_pin);
            if original.is_empty() {
                ctx.unpin_native_roots(mirror_pin);
                tracing::warn!(
                    "RetransformClasses: no retransformation base for `{}`; skipping",
                    ctx.class_name_of_id(class_id).unwrap_or_default(),
                );
                continue;
            }
            let mirror = ctx.read_native_pin(mirror_pin, mirror);
            let (final_bytes, _) = run_transformer_chain(
                ctx,
                class_id,
                Some(mirror),
                &original,
                /* retransform_only = */ true,
                None,
            );
            ctx.unpin_native_roots(mirror_pin);
            if final_bytes.is_empty() {
                continue;
            }
            definitions.push(PendingRedefinition {
                class_id,
                bytes: final_bytes,
                retransform_base: Some(original),
            });
        }
        install_redefinitions(ctx, &definitions, /* retransform = */ true)
    }));
    ctx.end_redefinition_batch(batch);
    match retransformed {
        Ok(refused) => refused,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// JVMTI `RedefineClasses` for a C agent (the C table's slot 87, interpreter
/// round i2 wave 57, lane L1): each class's new class file through the
/// whole transformer chain -- every C env's `ClassFileLoadHook` and every
/// Java transformer, as `redefineClasses0` runs it -- then every class
/// checked and redefined at once ([`install_redefinitions`]). The caller
/// has refused the unmodifiable classes. The first refusal, if any.
#[cfg(feature = "experimental-debug")]
pub(crate) fn redefine_classes_for_jvmti(
    ctx: &mut dyn NativeContext,
    classes: &[(ClassId, Vec<u8>)],
) -> Option<cratonvm_native_api::InstrumentRefusal> {
    let batch = ctx.begin_redefinition_batch();
    // gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    // finding 1): gci i18/na runs this under `native_oom::catch_alloc_oom_on`
    // (JVMTI `on_calling_thread`, `replay_early_class_hooks`), so a refused
    // allocation can unwind out of the chain or the install. The batch is
    // closed on that way out too: left open, `thread.redefinition_batch`
    // stayed `Some` and every later batch on this thread skipped its
    // loop-exit handshake. The payload is re-raised unchanged.
    let redefined = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut definitions: Vec<PendingRedefinition> = Vec::with_capacity(classes.len());
        for (class_id, bytes) in classes {
            let mirror = ctx.get_class_mirror(*class_id);
            let mirror_pin = ctx.pin_native_root(mirror);
            let (final_bytes, retransform_base) = run_transformer_chain(
                ctx,
                *class_id,
                Some(mirror),
                bytes,
                /* retransform_only = */ false,
                None,
            );
            ctx.unpin_native_roots(mirror_pin);
            definitions.push(PendingRedefinition {
                class_id: *class_id,
                bytes: final_bytes,
                retransform_base,
            });
        }
        install_redefinitions(ctx, &definitions, /* retransform = */ false)
    }));
    ctx.end_redefinition_batch(batch);
    match redefined {
        Ok(refused) => refused,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// The classes `loader` (`None`: the bootstrap loader) is an initiating
/// loader of, as `Instrumentation.getInitiatedClasses` and JVMTI
/// `GetClassLoaderClasses` list them (libinstrument's
/// `getInitiatedClasses0` IS `GetClassLoaderClasses`): the loader's
/// namespace in `ClassLoaderId::to_native_id`'s wire form (interpreter round
/// i1 wave 41, lane L3), a loader object mapped to the namespace its defines
/// record (`classloader::loader_namespace_id`), whose `0` for "some other
/// built-in loader" keeps the application namespace. Since wave 42 (lane L3)
/// the list is HotSpot's shape: the primitive array classes, the array
/// classes of the listed classes, and the direct supertypes of the classes
/// the loader defined (`NativeContextImpl::list_initiated_class_ids`;
/// `tools/probes/interp/L3/L3W42InitiatedArrays.java`). Shared with the C
/// table's slot 79 since interpreter round i2 wave 54 (lane L1).
pub(crate) fn initiated_class_ids(
    ctx: &mut dyn NativeContext,
    loader: Option<ObjectRef>,
) -> Vec<ClassId> {
    let loader_id = match loader {
        Some(loader) => {
            match cratonvm_native_builtins::classloader::loader_namespace_id(ctx, loader) {
                cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP => {
                    cratonvm_types::ClassLoaderId::NATIVE_APPLICATION
                }
                id => id,
            }
        }
        None => cratonvm_types::ClassLoaderId::NATIVE_BOOTSTRAP,
    };
    ctx.list_initiated_class_ids(loader_id)
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

/// Publish an agent JAR to both VM class search and the JDK system loader's
/// resource search. Updating only ClassManager left isolated agent loaders
/// unable to read their own class bytes (JaCoCo AgentModule, i56 residual).
pub(crate) fn append_system_search(ctx: &mut dyn NativeContext, path: &str) -> MethodCallResult {
    ctx.register_dynamic_classpath(&[path.to_string()]);
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let loader = match scope.invoke_static(
        "java/lang/ClassLoader",
        "getSystemClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    )? {
        Some(Value::Object(Some(loader))) => loader,
        _ => return Ok(None),
    };
    // The compatibility loader's resource natives already read ClassManager.
    // Only an adopted JDK loader owns a real URLClassPath to append to.
    if !cratonvm_native_builtins::classloader::builtin_loaders_adopted(
        cratonvm_native_builtins::classloader::loader_tables(&*scope),
    ) {
        return Ok(None);
    }
    let hook = "appendToClassPathForInstrumentation";
    let descriptor = "(Ljava/lang/String;)V";
    let mut class = Some(scope.class_id_of_object(loader));
    let mut has_hook = false;
    while let Some(id) = class {
        if scope.class_declares_method(id, hook, descriptor) {
            has_hook = true;
            break;
        }
        class = scope.superclass_of(id);
    }
    // A synthetic class library has no JDK resource search to update.
    if !has_hook {
        return Ok(None);
    }
    let loader = scope.root(loader);
    let string = scope.create_string(path);
    let loader = scope.get(&loader);
    scope.invoke_virtual(loader, hook, descriptor, &[Value::Object(Some(string))])
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
    append_system_search(ctx, &path)
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
        return append_system_search(ctx, &path);
    }
    Ok(None)
}

/// `void setNativeMethodPrefix0(ClassFileTransformer transformer, String prefix)`.
/// We record the prefix on the matching chain entry, where a failed native
/// link reads it ([`native_method_prefixes`]; wave 44).
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
                return;
            }
        }
    });
    Ok(None)
}

/// `boolean isRetransformClassesSupported0()`. Args: [this].
///
/// The agent's `Can-Retransform-Classes` since interpreter round i1 wave 42
/// (lane L3; `agent_capability`); `true` for every agent before.
fn native_is_retransform_supported0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let supported = agent_capability(&*ctx, instrumentation_receiver(args), CAN_RETRANSFORM_FIELD);
    Ok(Some(Value::Int(i32::from(supported))))
}

/// `boolean isRedefineClassesSupported0()`. Args: [this].
///
/// The agent's `Can-Redefine-Classes` since interpreter round i1 wave 42
/// (lane L3; `agent_capability`); `true` for every agent before.
fn native_is_redefine_supported0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let supported = agent_capability(&*ctx, instrumentation_receiver(args), CAN_REDEFINE_FIELD);
    Ok(Some(Value::Int(i32::from(supported))))
}

/// `boolean isNativeMethodPrefixSupported()`. Args: [this].
///
/// The agent's `Can-Set-Native-Method-Prefix` since interpreter round i1
/// wave 44 (lane L3; [`agent_can_set_native_method_prefix`]), `false` for an
/// `InstrumentationImpl` the VM did not stamp. From 2026-07-27 to wave 44 it
/// answered `false` for every agent, because the recorded prefixes were
/// consulted nowhere at native linking: an agent told "supported" would have
/// registered a wrapper whose native never bound. Since wave 44 a native
/// method whose implementation is not found under its own name is linked by
/// the name with the agents' prefixes removed, as JVMTI's
/// `SetNativeMethodPrefix` specifies (`vm_exec::prefixed_native_link`, at the
/// one arm every unlinked native call reaches, `invoke_on_class_shared_inner`'s
/// missing-native arm; `native_method_prefixes`).
fn native_is_prefix_supported0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let supported = agent_can_set_native_method_prefix(&*ctx, instrumentation_receiver(args));
    Ok(Some(Value::Int(i32::from(supported))))
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
    Ok(Some(Value::Long(object_size_of(ctx, obj))))
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
    let (final_bytes, retransform_base) = run_transformer_chain(
        ctx,
        class_id,
        Some(mirror),
        &bytes,
        /* retransform_only = */ false,
        /* inst_receiver = */ None,
    );
    let ok = ctx
        .redefine_class_as_instrument(class_id, &final_bytes, false, retransform_base.as_deref())
        .is_ok();
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
/// Returns the chain's output and, as [`run_chain_over_bytes`] does, the
/// class file as it was before the first retransform-capable transformer
/// changed it (`None` when none did), which a redefinition records as the
/// class's retransformation base.
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
) -> (Vec<u8>, Option<Vec<u8>>) {
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
    // Redefine/retransform: the loader argument is the class's defining
    // loader, `null` for the bootstrap loader, as HotSpot passes it
    // (`JvmtiClassFileLoadHookPoster`: `the_class->class_loader()`). Until
    // interpreter round i1 wave 28 (lane L3) this path passed `null` for every
    // class, so an agent that builds a type pool or class-file locator from the
    // loader it is handed (Byte Buddy's `AgentBuilder`) resolved an
    // application class's types through the bootstrap loader
    // (`L3/L3W28RetransformLoader`). Asked only when a transformer will run.
    let (class_mirror, loader) = match class_mirror {
        Some(mirror)
            if transformers_armed(ctx.vm_identity())
                || class_file_load_hook_vm(ctx.vm_identity()).is_some() =>
        {
            let (mirror, loader) = defining_loader_of_mirror(ctx, mirror);
            (Some(mirror), loader)
        }
        other => (other, None),
    };
    run_chain_over_bytes(
        ctx,
        &class_name_str,
        class_mirror,
        loader,
        /* domain = */ None,
        initial_bytes,
        retransform_only,
    )
}

/// `mirror.getClassLoader()`, with `mirror` read back after the call (which
/// runs Java and may move it): `(mirror, loader)`, the loader `None` for the
/// bootstrap loader or a failed call. Nothing allocates between the return
/// and the caller's use of both, so the caller pins them before its next
/// allocation (`run_chain_over_bytes` does, first thing).
fn defining_loader_of_mirror(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
) -> (ObjectRef, Option<ObjectRef>) {
    let pin = ctx.pin_native_root(mirror);
    let got = ctx.invoke_virtual(mirror, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]);
    let mirror = ctx.read_native_pin(pin, mirror);
    ctx.unpin_native_roots(pin);
    match got {
        Ok(Some(Value::Object(Some(loader)))) => (mirror, Some(loader)),
        _ => (mirror, None),
    }
}

/// The transformer-chain walk itself, over a class identified by NAME rather
/// than by `ClassId`.
///
/// Split out from [`run_transformer_chain`] because the load-time hook has no
/// `ClassId` to name the class with: the whole point is that the transformer
/// runs *before* the class is defined, exactly as `java.lang.instrument`
/// specifies. `class_mirror` is `None` and `loader` is the defining loader on
/// that path; on the redefine/retransform path the mirror is the live class and
/// `loader` its defining loader (`None` for the bootstrap loader; since
/// interpreter round i1 wave 28, before which it was always `None`).
///
/// Order and the retransformation base (interpreter round i1 wave 26, lane
/// L3). JVMTI offers a class file to every NON-retransformable transformer
/// first and to the retransform-capable ones after them, whatever order they
/// were registered in (`libinstrument` keeps them in two environments, and
/// `JvmtiClassFileLoadHookPoster::post_all_envs` walks the non-retransformable
/// ones first), and it caches the class file as it was just BEFORE the first
/// retransform-capable transformer changed it: that cached file is what a
/// later `retransformClasses` hands the retransform-capable transformers. The
/// walk below runs the two groups in that order and returns that cached file
/// as the second element, `None` when no retransform-capable transformer
/// returned bytes (the class file as defined is then the base). Probe:
/// `tools/probes/interp/L3/L3W26LoadHookFirstTouch.java`.
fn run_chain_over_bytes(
    ctx: &mut dyn NativeContext,
    class_name_str: &str,
    class_mirror: Option<ObjectRef>,
    loader: Option<ObjectRef>,
    domain: Option<ObjectRef>,
    initial_bytes: &[u8],
    retransform_only: bool,
) -> (Vec<u8>, Option<Vec<u8>>) {
    let rust_chain = snapshot_transformer_chain(ctx.vm_identity());
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[RETRANSFORM]   run_transformer_chain: rust_chain={} entries, initial_bytes={}, retransform_only={retransform_only}",
            rust_chain.len(),
            initial_bytes.len()
        );
    }
    // Interpreter round i2 wave 53, lane L1: a C agent's `ClassFileLoadHook`
    // runs in this walk too. The envs without `can_retransform_classes` ahead
    // of the non-retransformable Java transformers, at load and at a
    // redefinition; since wave 54 the envs with it ahead of the
    // retransform-capable Java transformers, at a retransformation too
    // (`post_c_hook_group`). `None` unless a C env of this VM takes it.
    let native_hook = class_file_load_hook_vm(ctx.vm_identity());
    if rust_chain.is_empty() && native_hook.is_none() {
        return (initial_bytes.to_vec(), None);
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
        return (Vec::new(), None);
    }
    // The SNAPSHOT's own transformer refs, pinned before anything allocates.
    //
    // HARDENING, NOT A FIXED CRASH — read the last paragraph before believing
    // this closed something that was firing.
    //
    // `scan_transformer_roots` + `remap_transformer_refs` keep the CANONICAL
    // chain correct across a moving collection, and their comment names the
    // exact failure they exist to stop: a surviving `transformer_ref` left
    // "pointing at the object's *old* address after compaction ... a dispatch
    // onto a relocated object the next time `run_transformer_chain` invokes
    // `transform`".
    //
    // That guard does not reach this walk. It iterates
    // `snapshot_transformer_chain`, which is `chain.to_vec()` — a COPY the
    // collector has never heard of, so the remap does not rewrite it. Every
    // entry after the first is therefore held across the PREVIOUS
    // transformer's `transform` call, which runs Java and allocates. If a
    // collection relocates a transformer object in that window, the canonical
    // chain follows it and this snapshot does not.
    // `a_snapshot_taken_before_a_relocation_keeps_the_old_address` is that
    // half, asserted.
    //
    // The snapshot is kept — it is what makes the walk stable against a
    // transformer that registers or removes another mid-chain — but its refs
    // are now read back through pins rather than used raw. Pinned first so a
    // single `unpin_native_roots` at the bottom releases these and the three
    // below with them.
    //
    // NOT DEMONSTRATED FIRING (2026-09-06). A probe that puts an allocating
    // transformer ahead of Mockito's in the chain, churns ~6 GB through it and
    // calls `System.gc()` twice inside the `transform` call, was clean 20/20
    // on a pristine-dev control as well as here. The transformer object is
    // registered once and long-lived, so reaching it needs an OLD-GEN
    // compaction inside a transform call, which that probe never produced.
    // What is established is the shape: the identical unpinned-across-Java
    // hazard one level up — the `Class[]` in the two loops below — moved
    // before `i=1` in 60 of 60 processes and cost ~12% of them a
    // half-instrumented mock. This is the same reference discipline applied
    // one level down, at the cost of one pin per registered transformer.
    let transformer_pins: Vec<usize> = rust_chain
        .iter()
        .map(|entry| ctx.pin_native_root(entry.transformer_ref))
        .collect();

    // Resolve constants used on every iteration once.
    //
    // All three live across `alloc_byte_array` and the `transform` call below,
    // both of which allocate and can therefore move a young object. Pin them:
    // an unpinned `ObjectRef` held across an allocating call is the native
    // stale-local family, and here it would hand the transformer a relocated
    // (i.e. garbage) class name. The mirror and the loader are pinned BEFORE
    // the string is created: `create_string` allocates, and they arrive as
    // raw references (interpreter round i1 wave 28, lane L3).
    let mirror_pin = class_mirror.map(|m| ctx.pin_native_root(m));
    let loader_pin = loader.map(|l| ctx.pin_native_root(l));
    // The `protectionDomain` argument (interpreter round i1 wave 28,
    // `docs/internal/fixed-bugs/interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011.md`):
    // on the load-time path the domain the `defineClass*` native received;
    // on a redefinition or retransformation the class's own, as HotSpot
    // passes `the_class->protection_domain()` -- what `getProtectionDomain0`
    // answers, `null` for a class the bootstrap loader defined. Every mode
    // since wave 29 (`--compatible` passed `null`). Asked after the mirror and
    // the loader are pinned, since minting a domain allocates.
    let domain = match (domain, class_mirror, mirror_pin) {
        (Some(domain), _, _) => Some(domain),
        (None, Some(mirror), Some(pin)) => {
            let mirror = ctx.read_native_pin(pin, mirror);
            cratonvm_native_builtins::lang_class::protection_domain_of_mirror(ctx, mirror)
        }
        _ => None,
    };
    let domain_pin = domain.map(|d| ctx.pin_native_root(d));
    // The lowest pin index this function takes: the first transformer's, or
    // (wave 53, a walk for a C hook alone) the first of the three objects';
    // `None` when nothing was pinned.
    let unpin_from = transformer_pins
        .first()
        .copied()
        .or(mirror_pin)
        .or(loader_pin)
        .or(domain_pin);
    let mut bytes_vec = initial_bytes.to_vec();
    let mut retransform_base: Option<Vec<u8>> = None;
    // The C envs' `ClassFileLoadHook` of one group (wave 53; the
    // retransform-capable group since wave 54), handed the three objects as
    // they are now (each callback's JNI may move them, which the hook's own
    // global references follow). A retransform-capable env's answer moves
    // the retransformation base as a retransform-capable transformer's does.
    let post_c_hook_group = |ctx: &mut dyn NativeContext,
                             bytes_vec: &mut Vec<u8>,
                             retransform_base: &mut Option<Vec<u8>>,
                             retransformable: bool| {
        let Some(shared) = native_hook.as_deref() else {
            return;
        };
        let read = |ctx: &mut dyn NativeContext, obj: Option<ObjectRef>, pin: Option<usize>| match (
            obj, pin,
        ) {
            (Some(obj), Some(pin)) => Some(ctx.read_native_pin(pin, obj)),
            _ => None,
        };
        let objects = [
            read(ctx, class_mirror, mirror_pin),
            read(ctx, loader, loader_pin),
            read(ctx, domain, domain_pin),
        ];
        let tid = ctx.thread_id();
        // gci i12/ev (`gci-i10al-jvmti-event-callbacks-borrow-the-registry-jvmthread-under-the-posters-borrow-FIXED-20261017`
        // item 1c): the C hooks run with this thread's JNI binding installed
        // from the context's own borrow (`run_with_jni_binding`), not the
        // registry's address of a thread whose `&mut` the context holds.
        let mut answer = None;
        ctx.run_with_jni_binding(&mut || {
            answer = post_native_class_file_load_hook(
                shared,
                tid,
                class_name_str,
                objects,
                &bytes_vec[..],
                retransformable,
            );
        });
        if let Some(hooked) = answer {
            if retransformable && retransform_base.is_none() {
                *retransform_base = Some(std::mem::replace(bytes_vec, hooked));
            } else {
                *bytes_vec = hooked;
            }
        }
    };
    // 1. The C envs without `can_retransform_classes` (not at a
    //    retransformation, which offers the class to the others only).
    if !retransform_only {
        post_c_hook_group(ctx, &mut bytes_vec, &mut retransform_base, false);
    }
    if rust_chain.is_empty() {
        // 3. below, with no Java transformer around it.
        post_c_hook_group(ctx, &mut bytes_vec, &mut retransform_base, true);
        if let Some(from) = unpin_from {
            ctx.unpin_native_roots(from);
        }
        return (bytes_vec, retransform_base);
    }
    let class_name_obj = ctx.create_string(class_name_str);
    let name_pin = ctx.pin_native_root(class_name_obj);
    // The `Module` argument (interpreter round i1 wave 42, lane L3): HotSpot
    // calls every transformer through `TransformerManager.transform`, which
    // calls the SIX-argument `transform(Module, ...)`; its default body calls
    // the five-argument one. This walk called the five-argument form
    // directly, so a transformer that overrides only the `Module` form
    // (the JDK 9+ API) was never run (`L3W42TransformerModuleForm`).
    let module = transform_module(
        ctx,
        class_name_str,
        class_mirror,
        mirror_pin,
        loader,
        loader_pin,
    );
    let module_pin = module.map(|m| ctx.pin_native_root(m));
    if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[RETRANSFORM]   transform(Module, ...) for {class_name_str}: module={}",
            if module.is_some() { "present" } else { "null" }
        );
    }

    // 2. Rust-side chain: every registered transformer (whether the
    //    agent registered it via the public Java `addTransformer` (now
    //    a native, see [`register_instrumentation_natives`]), or via
    //    the internal `addTransformer0`, or via the in-process
    //    [`cratonvm.Instrument.addTransformer`] bridge).
    //
    //    Every non-retransformable entry first, then every retransform-capable
    //    one, each group in registration order; a retransform offers the class
    //    to the second group only (see the function comment). `slot` stays the
    //    entry's index in the snapshot, which is what `transformer_pins` is
    //    indexed by.
    //
    //    3. Between the two groups (wave 54): the C envs with
    //    `can_retransform_classes`, once, before the first retransform-capable
    //    transformer or after the last entry when there is none.
    let order = rust_chain
        .iter()
        .enumerate()
        .filter(|(_, entry)| !retransform_only && !entry.can_retransform)
        .chain(
            rust_chain
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.can_retransform),
        );
    let mut c_retransformable_posted = false;
    for (slot, entry) in order {
        if entry.can_retransform && !c_retransformable_posted {
            c_retransformable_posted = true;
            post_c_hook_group(ctx, &mut bytes_vec, &mut retransform_base, true);
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
        let domain_arg = match (domain, domain_pin) {
            (Some(d), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, d))),
            _ => Value::Object(None),
        };
        let module_arg = match (module, module_pin) {
            (Some(m), Some(h)) => Value::Object(Some(ctx.read_native_pin(h, m))),
            _ => Value::Object(None),
        };
        // NOTE: invoke_virtual prepends the receiver itself, so args
        // here must NOT include the receiver. Pass only the 6 user args.
        let args = [
            // module: see `module` above.
            module_arg,
            // loader: the class's defining loader on the load-time path, and
            // `null` (the bootstrap loader) when we have none to surface.
            loader_arg,
            Value::Object(Some(class_name_obj)),
            mirror_arg,
            // protectionDomain: see `domain` above (`null` when there is none).
            domain_arg,
            Value::Object(Some(bytes_obj)),
        ];
        let descriptor = "(Ljava/lang/Module;Ljava/lang/ClassLoader;Ljava/lang/String;\
                          Ljava/lang/Class;Ljava/security/ProtectionDomain;[B)[B";
        // Read back through the pin, not out of the snapshot: by this point
        // every earlier transformer in this chain has run Java.
        let transformer = ctx.read_native_pin(transformer_pins[slot], entry.transformer_ref);
        // A transformer overriding only the five-argument form is reached
        // through `ClassFileTransformer`'s default six-argument body, as on
        // HotSpot (`invoke_virtual` selects an interface's default method the
        // way `Collection.stream()` is selected for the natives that call it).
        //
        // Interpreter round i6 wave 119, lane L3 (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`):
        // an `InstrumentationImpl` entry is a whole group of a JDK-managed
        // agent (`note_jdk_transformer_group`), handed to the JDK's own private
        // `transform(.., boolean isRetransformer)`, which runs that group's
        // `TransformerManager` as libinstrument's JVMTI hook does.
        let result = if is_jdk_transformer_group(&*ctx, transformer) {
            let mut jdk_args = Vec::with_capacity(args.len() + 2);
            jdk_args.push(Value::Object(Some(transformer)));
            jdk_args.extend_from_slice(&args);
            jdk_args.push(Value::Int(i32::from(entry.can_retransform)));
            ctx.invoke_special(
                INSTRUMENTATION_IMPL,
                "transform",
                "(Ljava/lang/Module;Ljava/lang/ClassLoader;Ljava/lang/String;Ljava/lang/Class;Ljava/security/ProtectionDomain;[BZ)[B",
                &jdk_args,
            )
        } else {
            ctx.invoke_virtual(transformer, "transform", descriptor, &args)
        };
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
                    if entry.can_retransform && retransform_base.is_none() {
                        // The first retransform-capable transformer to answer:
                        // the bytes it was handed are the retransformation base.
                        retransform_base = Some(std::mem::replace(&mut bytes_vec, out_bytes));
                    } else {
                        bytes_vec = out_bytes;
                    }
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
    if !c_retransformable_posted {
        post_c_hook_group(ctx, &mut bytes_vec, &mut retransform_base, true);
    }
    if let Some(from) = unpin_from {
        ctx.unpin_native_roots(from);
    }
    (bytes_vec, retransform_base)
}

/// The `Module` a transformer's `transform(Module, ...)` is handed, as
/// HotSpot computes it (`InstrumentationImpl.transform`, JDK 25): the
/// class's own module on a redefinition or retransformation
/// (`classBeingRedefined.getModule()`); at load, the named module that holds
/// the class's package when there is one, else the defining loader's unnamed
/// module (`BootLoader.getUnnamedModule()` for the bootstrap loader). `None`
/// when a step fails -- the default `transform(Module, ...)` ignores its
/// module, so only a transformer overriding that form sees the `null`.
///
/// The mirror and the loader arrive pinned (`mirror_pin`, `loader_pin`) and
/// are read back through the pins before each use: every step runs Java.
fn transform_module(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    class_mirror: Option<ObjectRef>,
    mirror_pin: Option<usize>,
    loader: Option<ObjectRef>,
    loader_pin: Option<usize>,
) -> Option<ObjectRef> {
    const MODULE_GETTER: &str = "()Ljava/lang/Module;";
    fn object(result: MethodCallResult) -> Option<ObjectRef> {
        match result {
            Ok(Some(Value::Object(Some(o)))) => Some(o),
            _ => None,
        }
    }
    if let (Some(mirror), Some(pin)) = (class_mirror, mirror_pin) {
        let mirror = ctx.read_native_pin(pin, mirror);
        return object(ctx.invoke_virtual(mirror, "getModule", MODULE_GETTER, &[]));
    }
    let package = class_name
        .rsplit_once('/')
        .map_or("", |(package, _)| package);
    if !package.is_empty() {
        if let Some(module_name) = ctx.module_for_package(package) {
            if let Some(named) = boot_layer_module(ctx, &module_name) {
                return Some(named);
            }
        }
    }
    match (loader, loader_pin) {
        (Some(loader), Some(pin)) => {
            let loader = ctx.read_native_pin(pin, loader);
            object(ctx.invoke_virtual(loader, "getUnnamedModule", MODULE_GETTER, &[]))
        }
        _ => object(ctx.invoke_static(
            "jdk/internal/loader/BootLoader",
            "getUnnamedModule",
            MODULE_GETTER,
            &[],
        )),
    }
}

/// `ModuleLayer.boot().findModule(name).orElse(null)`: the boot layer's
/// module `name`, `None` when it has none (a modular jar on the class path
/// is in the registry but not in the layer) or a step fails.
fn boot_layer_module(ctx: &mut dyn NativeContext, name: &str) -> Option<ObjectRef> {
    let layer = match ctx.invoke_static(
        "java/lang/ModuleLayer",
        "boot",
        "()Ljava/lang/ModuleLayer;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(layer)))) => layer,
        _ => return None,
    };
    let layer_pin = ctx.pin_native_root(layer);
    let name = ctx.create_string(name);
    let layer = ctx.read_native_pin(layer_pin, layer);
    ctx.unpin_native_roots(layer_pin);
    let found = match ctx.invoke_virtual(
        layer,
        "findModule",
        "(Ljava/lang/String;)Ljava/util/Optional;",
        &[Value::Object(Some(name))],
    ) {
        Ok(Some(Value::Object(Some(found)))) => found,
        _ => return None,
    };
    match ctx.invoke_virtual(
        found,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(None)],
    ) {
        Ok(Some(Value::Object(Some(module)))) => Some(module),
        _ => None,
    }
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
/// returns `initial_bytes` unchanged. The second element is the class file a
/// later `retransformClasses` must hand the retransform-capable transformers
/// when it differs from the first (see [`run_chain_over_bytes`]): the definer
/// records it as the class's cached bytes after the define.
pub fn run_load_time_transform_chain(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    loader_id: cratonvm_types::ClassLoaderId,
    domain: Option<ObjectRef>,
    initial_bytes: &[u8],
) -> (Vec<u8>, Option<Vec<u8>>) {
    use cratonvm_types::ClassLoaderId;
    // Wave 54: a C agent's hook in JVMTI's start phase sees the bootstrap
    // loader's classes only (`start_phase_withholds`; the define-from-bytes
    // path reaches here without `pre_transform_for_load`'s check).
    if let Some(shared) = class_file_load_hook_vm(ctx.vm_identity()) {
        if start_phase_withholds(&shared, loader_id) {
            return (initial_bytes.to_vec(), None);
        }
    }
    // Re-entrancy guard — see [`TRANSFORM_IN_FLIGHT`]. A transformer that,
    // while rewriting `X`, causes `X` itself to be loaded (ByteBuddy's type
    // pool does exactly this) would otherwise transform `X` to transform `X`
    // forever. Declining the nested offer costs coverage of one already-covered
    // class and is what makes the first `-javaagent:` run terminate.
    let already = TRANSFORM_IN_FLIGHT.with(|s| s.borrow().iter().any(|n| n == class_name));
    if already {
        return (initial_bytes.to_vec(), None);
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

    // The domain arrives as a raw reference and the loader lookup below may
    // allocate (the system loader's first use): pinned across it.
    let domain_pin = domain.map(|d| ctx.pin_native_root(d));
    let loader = match loader_id {
        // The bootstrap loader IS `null` in the Java API — not "unknown".
        ClassLoaderId::Bootstrap => None,
        // Extension/platform and application classes are reached through the
        // system class loader, which is what HotSpot passes for both.
        ClassLoaderId::Extension | ClassLoaderId::Application => {
            Some(cratonvm_native_builtins::classloader::get_or_create_app_loader(ctx))
        }
        // A user-defined loader defines its classes through
        // `ClassLoader.defineClass` (`define_class_full`), which names it by its
        // namespace id: the transformer gets that loader object, as HotSpot
        // passes the defining loader (interpreter round i2 wave 53, lane L1;
        // `L1W53JvmtiClassHooks`' `Defined` row -- until then every such class
        // was handed the system loader). The system loader only when no loader
        // object holds the id.
        ClassLoaderId::UserDefined(id) => {
            match cratonvm_native_builtins::classloader::user_loader_object(ctx, id) {
                Some(loader) => Some(Ok(loader)),
                None => Some(cratonvm_native_builtins::classloader::get_or_create_app_loader(ctx)),
            }
        }
    }
    // This transformer entry point returns the bytes and has no error channel
    // to carry a refusal. If `--jdk-only` refuses the app loader, fall back to
    // the bootstrap loader (`None`) — the same answer this already gave for a
    // bootstrap class — and let the recorded violation stand.
    .and_then(|loader| loader.ok());
    // Nothing allocates between this read and `run_chain_over_bytes`, which
    // pins it again before its first allocation, as it does the loader.
    let domain = match (domain, domain_pin) {
        (Some(d), Some(pin)) => {
            let d = ctx.read_native_pin(pin, d);
            ctx.unpin_native_roots(pin);
            Some(d)
        }
        _ => None,
    };
    run_chain_over_bytes(
        ctx,
        class_name,
        /* class_mirror = */ None,
        loader,
        domain,
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
    match try_original_class_bytes(ctx, class_id) {
        Ok(bytes) => bytes,
        Err(Some(miss)) => {
            miss.log();
            Vec::new()
        }
        Err(None) => Vec::new(),
    }
}

/// Why [`try_original_class_bytes`] found no base for a named class, kept for
/// the caller to log once it has tried the defining loader too
/// ([`defining_loader_class_bytes`]).
struct RetransformBaseMiss {
    refusal: RetransformBaseRefusal,
    name: String,
    resource: String,
}

impl RetransformBaseMiss {
    fn log(&self) {
        self.refusal.log(&self.name, &self.resource);
    }
}

/// [`original_class_bytes`] with the refusal handed back instead of logged.
/// `Err(None)`: the class has no name (logged here).
fn try_original_class_bytes(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Result<Vec<u8>, Option<RetransformBaseMiss>> {
    // Prefer the ClassManager's class_bytes_cache (populated by every
    // define_class_with_options call). This is the only path that finds
    // dynamically-defined / hidden / agent-redefined classes; the
    // classpath find_resource path only sees on-disk class files.
    let cached = ctx.class_bytes(class_id).filter(|b| !b.is_empty());
    let name = match ctx.class_name_of_id(class_id) {
        Some(n) => n,
        None => {
            tracing::debug!("retransform: class id {class_id:?} has no name; skipping");
            return Err(None);
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

    adjudicate_retransform_base(&name, cached, fallback, fallback_matches_base).map_err(|refusal| {
        Some(RetransformBaseMiss {
            refusal,
            name,
            resource,
        })
    })
}

/// The retransformation base of `class_id` read back through its DEFINING
/// loader: `mirror.getClassLoader().getResourceAsStream("<name>.class")`,
/// accepted only when it is byte for byte the class file the class was
/// defined from (`class_bytes_match_base` answers `Some(true)`).
///
/// For a class whose base the class-bytes FIFO evicted and the class path does
/// not hold (interpreter round i1 wave 28, lane L3): a class a user loader
/// defined from its own jar or directory -- a Spring Boot nested jar, a test's
/// `URLClassLoader` over another build -- which `find_resource`'s
/// bootstrap/extension/application walk never sees. `retransformClasses`
/// skipped such a class for the rest of the run. HotSpot reconstitutes the
/// class file from the live class instead (`JvmtiClassFileReconstituter`); a
/// class with no resource anywhere (a generated class) is still skipped here.
///
/// Runs Java (the loader's own lookup), so `mirror` is pinned across it, and
/// the caller must re-read every reference it holds afterwards. `None` for the
/// bootstrap loader (whose classes `find_resource` already covers), a missing
/// resource, any exception, or bytes that are not the class's base.
#[cold]
#[inline(never)]
fn defining_loader_class_bytes(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    mirror: ObjectRef,
    resource: &str,
) -> Option<Vec<u8>> {
    let base_pin = ctx.pin_native_root(mirror);
    let found = defining_loader_resource(ctx, mirror, base_pin, resource);
    ctx.unpin_native_roots(base_pin);
    let bytes = found.filter(|b| !b.is_empty())?;
    if ctx.class_bytes_match_base(class_id, &bytes) == Some(true) {
        tracing::debug!("retransform: `{resource}` read back through the class's defining loader");
        Some(bytes)
    } else {
        None
    }
}

/// The Java half of [`defining_loader_class_bytes`]. `mirror_pin` holds
/// `mirror`; every pin taken here is above it and released with it.
fn defining_loader_resource(
    ctx: &mut dyn NativeContext,
    mirror: ObjectRef,
    mirror_pin: usize,
    resource: &str,
) -> Option<Vec<u8>> {
    let mirror = ctx.read_native_pin(mirror_pin, mirror);
    let got = ctx.invoke_virtual(mirror, "getClassLoader", "()Ljava/lang/ClassLoader;", &[]);
    let loader = match got {
        Ok(Some(Value::Object(Some(loader)))) => loader,
        // The bootstrap loader (`null`), or the call failed.
        _ => return None,
    };
    let loader_pin = ctx.pin_native_root(loader);
    let resource_name = ctx.create_string(resource);
    let loader = ctx.read_native_pin(loader_pin, loader);
    let stream = match ctx.invoke_virtual(
        loader,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        &[Value::Object(Some(resource_name))],
    ) {
        Ok(Some(Value::Object(Some(stream)))) => stream,
        _ => return None,
    };
    let stream_pin = ctx.pin_native_root(stream);
    let stream = ctx.read_native_pin(stream_pin, stream);
    let read = ctx.invoke_virtual(stream, "readAllBytes", "()[B", &[]);
    let bytes = match read {
        Ok(Some(Value::Object(Some(array)))) => Some(read_byte_array(ctx, array)),
        _ => None,
    };
    let stream = ctx.read_native_pin(stream_pin, stream);
    let _ = ctx.invoke_virtual(stream, "close", "()V", &[]);
    bytes
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

/// Give back the load-time offer of `name` in `vm` ([`claim_load_time_offer`]),
/// so its next load offers the class again: the staged answer does not
/// define the class, so the load consuming it fails and defines nothing
/// (interpreter round i1 wave 43, lane L3).
fn release_load_time_offer(vm: usize, name: &str) {
    let mut offered = load_time_offered()
        .write()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(set) = offered.get_mut(&vm) {
        set.remove(name);
    }
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

/// Does the load path offer class files to `shared`'s hooks: a registered
/// transformer ([`transformers_armed`]) or (interpreter round i2 wave 53,
/// lane L1) a C agent's `ClassFileLoadHook`
/// (`DebuggerGates::native_class_file_load_hook_armed`)? The callers of the
/// load path ask this; one more relaxed load when no transformer is
/// registered.
#[inline]
pub fn load_hook_armed(shared: &crate::vm::SharedVm) -> bool {
    transformers_armed(shared.vm_identity) || native_load_hook_armed(shared)
}

/// The C half of [`load_hook_armed`]; always `false` without
/// `experimental-debug`, which has no C JVMTI table.
#[inline]
fn native_load_hook_armed(shared: &crate::vm::SharedVm) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        shared
            .debug
            .debugger_gates
            .native_class_file_load_hook_armed()
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = shared;
        false
    }
}

/// Does JVMTI's start phase keep the class file of a class `loader_id`
/// defines from the hook (interpreter round i2 wave 54, lane L1)? Only a C
/// agent's `ClassFileLoadHook` can be armed before `VMInit` (with
/// `DebuggerGates::c_start_phase_hook`); HotSpot posts it from `VMStart`,
/// after `System.initPhase1` (init level 2), not in the primordial phase
/// before it (without `can_generate_early_class_hook_events`, which this
/// table does not grant). Offered then: a class the bootstrap loader defines,
/// whose loader and domain are NULL, so the walk runs no Java. A class of
/// another loader in the start phase is not offered (the loader object may
/// not exist yet); HotSpot offers it. Asked only while the hook is armed.
#[cold]
fn start_phase_withholds(
    shared: &crate::vm::SharedVm,
    loader_id: cratonvm_types::ClassLoaderId,
) -> bool {
    #[cfg(feature = "experimental-debug")]
    {
        shared.debug.debugger_gates.jvmti_start_phase()
            && ((shared.get_init_level() < 2
                // Interpreter round i2 wave 63, lane L1: not with the early
                // class hook (`can_generate_early_class_hook_events`), which
                // offers the primordial phase's bootstrap classes.
                && !shared.debug.jvmti_env.lock().early_class_hooks)
                || loader_id != cratonvm_types::ClassLoaderId::Bootstrap)
    }
    #[cfg(not(feature = "experimental-debug"))]
    {
        let _ = (shared, loader_id);
        false
    }
}

/// The VM whose C agents take `ClassFileLoadHook`, for a walk of `vm`'s
/// chain (wave 53); `None` without `experimental-debug`.
#[cfg(feature = "experimental-debug")]
fn class_file_load_hook_vm(vm: usize) -> Option<std::sync::Arc<crate::vm::SharedVm>> {
    crate::jvmti::native_env::class_file_load_hook_vm(vm)
}

#[cfg(not(feature = "experimental-debug"))]
fn class_file_load_hook_vm(_vm: usize) -> Option<std::sync::Arc<crate::vm::SharedVm>> {
    None
}

/// Offer `bytes` to the C agents' `ClassFileLoadHook` (wave 53), of the envs
/// with `can_retransform_classes` (`retransformable`, wave 54) or of the
/// others: their last answer, or `None` when none replaced the class file.
#[cfg(feature = "experimental-debug")]
fn post_native_class_file_load_hook(
    shared: &crate::vm::SharedVm,
    tid: u64,
    name: &str,
    objects: [Option<ObjectRef>; 3],
    bytes: &[u8],
    retransformable: bool,
) -> Option<Vec<u8>> {
    crate::jvmti::native_env::post_class_file_load_hook(
        shared,
        tid,
        name,
        objects,
        bytes,
        retransformable,
    )
}

#[cfg(not(feature = "experimental-debug"))]
fn post_native_class_file_load_hook(
    _shared: &crate::vm::SharedVm,
    _tid: u64,
    _name: &str,
    _objects: [Option<ObjectRef>; 3],
    _bytes: &[u8],
    _retransformable: bool,
) -> Option<Vec<u8>> {
    None
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
    if !load_hook_armed(shared) {
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
            .map(|(bytes, loader_id)| {
                // The code source the definer will record for the class (its
                // `define_class_with_options` asks the same lookup), which the
                // application loader's domain is built from below.
                let code_source = match loader_id {
                    cratonvm_types::ClassLoaderId::Application => cm.find_class_code_source(name),
                    _ => None,
                };
                (bytes, loader_id, code_source)
            })
    };
    let (bytes, loader_id, code_source) = match found {
        Ok(found) => found,
        // Not on the built-in delegation chain (a user loader will supply it,
        // or it does not exist). The `ClassLoader.defineClass` hook covers the
        // former; either way there is nothing to transform here.
        Err(_) => return,
    };
    if start_phase_withholds(shared, loader_id) {
        return;
    }

    for supertype in class_file_supertypes(&bytes) {
        pre_transform_for_load(shared, thread, &supertype, depth + 1);
    }

    // gc-common w6-c (`handoff-w6c-rerunning-contexts-take-the-unwinding-arm`):
    // the chain runs agents' transformers, so an allocation in it cannot be
    // retried; it takes the unwinding arm instead of the infallible one. On
    // exhaustion the class loads untransformed -- this function's answer for
    // every other transformer failure -- and the `OutOfMemoryError` surfaces at
    // the next allocation the ordinary ladder cannot serve.
    let (transformed, retransform_base) = match crate::runtime::native_oom::catch_alloc_oom(|| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        // A class-path class gets the domain the application loader defines it
        // with, as HotSpot passes it (`null` otherwise; every mode since wave
        // 29). JaCoCo's default filter skips a class
        // whose domain has no location, which was every class of a plain `-cp`
        // application.
        let domain = code_source.and_then(|cs| {
            let url = cs.url?;
            cratonvm_native_builtins::lang_class::app_class_path_domain(
                &mut ctx,
                &url,
                cs.certificates,
            )
        });
        run_load_time_transform_chain(&mut ctx, name, loader_id, domain, &bytes)
    }) {
        Ok(t) => t,
        Err(_) => {
            tracing::warn!(
                "load-time transform of `{name}` ran out of heap; keeping the original class file"
            );
            return;
        }
    };
    if transformed == bytes {
        // Every transformer declined. Staging identical bytes would only make
        // the load take a different route to the same definition.
        return;
    }
    // A transformer's answer is the class file the load defines, whatever it
    // is (interpreter round i1 wave 43, lane L3;
    // `tools/probes/interp/L3/L3W42TransformerBadBytes.java`): bytes that are
    // not a class file fail the load with `ClassFormatError`, another class's
    // class file with `NoClassDefFoundError: <name> (wrong name: <other>)`, as
    // on HotSpot -- the definer (`ClassManager::load_class`, which consumes
    // the staged entry either way) refuses both, so no wrong class body is
    // installed under the name. Until wave 43 such an answer was dropped and
    // the class loaded untransformed, the agent's instrumentation silently
    // missing. The offer is given back, so the next load of the name (the
    // failed one defined nothing) offers it again, as HotSpot's next load
    // runs the hook again.
    if !matches!(class_file_this_class(&transformed), Some(found) if found == name) {
        if cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
            eprintln!(
                "[redefine] load-time transform of {name} staged a class file that does not \
                 define it: {:?}",
                class_file_this_class(&transformed)
            );
        }
        release_load_time_offer(shared.vm_identity, name);
    }
    // Interpreter round i1 wave 45, lane L3: the woven class runs its woven
    // bytecode, not a native the VM registers for one of its methods (the
    // wrapper a `setNativeMethodPrefix` agent puts around a native is Java;
    // `L3W44NativeMethodPrefix`, row `wrapper-ran`). Only a class the
    // registry may name is marked.
    let cede = shared.natives.native_methods.class_may_have_natives(name);
    if cede && cratonvm_types::flags::runtime_var("CRATONVM_DBG_RETRANSFORM").is_ok() {
        eprintln!(
            "[instrument] load-time transform of {name}: its registered natives cede to the \
             woven bytecode"
        );
    }
    let mut cm = shared.classes.class_manager_write();
    cm.stage_transformed_class(name, transformed, loader_id, retransform_base);
    if cede {
        cm.stage_native_shadow_cede(name);
    }
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
/// May JVMTI change `class_id` (`IsModifiableClass`, `RetransformClasses`'
/// `JVMTI_ERROR_UNMODIFIABLE_CLASS`): not a primitive, array or hidden class,
/// as [`mirror_classification`] judges a mirror (interpreter round i2 wave
/// 54, lane L1: the C table's `RetransformClasses`, which has a class id
/// where a primitive class is one of the class store's, by its name).
#[cfg(feature = "experimental-debug")]
pub(crate) fn class_id_is_modifiable(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let is_primitive = matches!(
        &*name,
        "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
    );
    let is_hidden = ctx.is_class_hidden(class_id)
        || (class_id.as_u32() >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE
            && ctx.lambda_proxy_host(class_id).is_some());
    is_modifiable_class(is_primitive, name.starts_with('['), is_hidden)
}

fn mirror_classification(ctx: &dyn NativeContext, mirror: ObjectRef) -> (bool, bool, bool) {
    // `class_id_from_mirror` returns None for primitive mirrors; we use
    // that as the primitive marker.
    let class_id = ctx.class_id_from_mirror(mirror);
    let is_primitive = class_id.is_none();
    if let Some(cid) = class_id {
        let name = ctx.class_name_of_id(cid).unwrap_or_default();
        let is_array = name.starts_with('[');
        // A lambda proxy the VM minted (an id at or above
        // `LAMBDA_PROXY_ID_BASE`, not in the class store) is a hidden class
        // in HotSpot, as `Class.isHidden` already answers for it; the class
        // store's flag alone called it modifiable (interpreter round i1 wave
        // 40, lane L3; `tools/probes/interp/L3/L3W40ModifiableLambdaClass.java`).
        let is_hidden = ctx.is_class_hidden(cid)
            || (cid.as_u32() >= crate::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE
                && ctx.lambda_proxy_host(cid).is_some());
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
// to the target VM's attach listener. CratonVM's own listener (Unix only,
// `runtime::serviceability`) serves OTHER processes; for an attach from
// inside the VM the target is always *this* process, so we intercept those three methods with
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
///
/// Interpreter round i2 wave 56, lane L1 (behind `VmConfig::attach_checks`,
/// `CRATONVM_ATTACH_CHECKS`;
/// `docs/internal/fixed-bugs/interpreter-L1-in-process-attach-answers-any-pid-and-ignores-allowattachself-FIXED-20261020.md`):
/// the id is checked as `HotSpotVirtualMachine`'s constructor checks it
/// ([`attach_refusal`]), so an id that names no process, this process
/// without `jdk.attach.allowAttachSelf`, or another process (which the
/// in-process handle cannot act on) is refused with HotSpot's exception
/// instead of a handle that loads the agent into THIS VM.
fn native_vm_attach(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let disabled = attach_disabled(ctx);
    if disabled || attach_checks(ctx) {
        refuse_attach(ctx, args, disabled)?;
    }
    ctx.new_object(VM_ATTACH_CLASS)
}

/// `VmConfig::attach_checks` of the calling native's VM (wave 56).
fn attach_checks(ctx: &dyn NativeContext) -> bool {
    crate::native::jni::vm_for_identity(ctx.vm_identity())
        .is_some_and(|shared| shared.config.attach_checks)
}

/// `VmConfig::disable_attach_mechanism` (`-XX:+DisableAttachMechanism`) of
/// the calling native's VM (interpreter round i2 wave 59, lane L1).
fn attach_disabled(ctx: &dyn NativeContext) -> bool {
    crate::native::jni::vm_for_identity(ctx.vm_identity())
        .is_some_and(|shared| shared.config.disable_attach_mechanism)
}

/// [`native_vm_attach`]'s checks: `Ok` when the handle may be made, else
/// the exception HotSpot throws for `args[0]` (`disabled`: the VM runs with
/// `-XX:+DisableAttachMechanism`).
#[cold]
#[inline(never)]
fn refuse_attach(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    disabled: bool,
) -> Result<(), MethodCallFailed> {
    let id = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("id cannot be null"),
            ))
        }
    };
    let allow_self = ctx.get_system_property("jdk.attach.allowAttachSelf");
    let Some((class, message)) =
        attach_refusal(&id, std::process::id(), allow_self.as_deref(), disabled)
    else {
        return Ok(());
    };
    let message = ctx.create_string_uninterned(&message);
    Err(attach_failure(ctx.new_object_initialized(
        class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    )))
}

/// What `VirtualMachine.attach(id)` throws on HotSpot 25 for `id` in the
/// process `own_pid`, whose saved `jdk.attach.allowAttachSelf` is
/// `allow_self` (measured, `L1W56AttachRefusals`; `HotSpotVirtualMachine`'s
/// constructor, then the Linux `VirtualMachineImpl`'s): `None` when the
/// in-process handle may be made (this process, attach-self allowed).
///
/// * an id `Integer.parseInt` refuses: `AttachNotSupportedException("Invalid
///   process identifier: <id>")`;
/// * this process or pid 0 in a VM run with `-XX:+DisableAttachMechanism`
///   (`disabled`), whatever the property: `AttachNotSupportedException("The
///   VM does not support the attach mechanism")` -- the provider's
///   `testAttachable` answers before `HotSpotVirtualMachine`'s constructor
///   looks at the property (interpreter round i2 wave 59, lane L1; measured,
///   `L1W59DisableAttachMechanism`);
/// * this process or pid 0 while attach-self is not allowed (the property is
///   neither `""` nor `Boolean.parseBoolean`'s `true`):
///   `IOException("Can not attach to current VM")`;
/// * pid 0 or a negative pid otherwise: `AttachNotSupportedException("Invalid
///   process identifier")` (Linux; Windows answers an `IOException`);
/// * any other process: `AttachNotSupportedException`. HotSpot attaches to
///   another JVM through its attach listener, which the in-process handle
///   cannot reach; one with no JVM behind it is `AttachNotSupportedException`
///   on HotSpot too (after a wait, with a platform text).
pub(crate) fn attach_refusal(
    id: &str,
    own_pid: u32,
    allow_self: Option<&str>,
    disabled: bool,
) -> Option<(&'static str, String)> {
    const NOT_SUPPORTED: &str = "com/sun/tools/attach/AttachNotSupportedException";
    let Some(pid) = parse_java_int(id) else {
        return Some((NOT_SUPPORTED, format!("Invalid process identifier: {id}")));
    };
    let own = i64::from(pid) == i64::from(own_pid);
    if disabled && (pid == 0 || own) {
        return Some((
            NOT_SUPPORTED,
            "The VM does not support the attach mechanism".to_string(),
        ));
    }
    let allowed = allow_self.is_some_and(|v| v.is_empty() || v.eq_ignore_ascii_case("true"));
    if !allowed && (pid == 0 || own) {
        return Some((
            "java/io/IOException",
            "Can not attach to current VM".to_string(),
        ));
    }
    if own {
        return None;
    }
    if pid < 1 {
        return Some((NOT_SUPPORTED, "Invalid process identifier".to_string()));
    }
    Some((
        NOT_SUPPORTED,
        format!("Unable to attach to process {pid}: this VM attaches only to its own process"),
    ))
}

/// `Integer.parseInt(text)`: an optional sign and decimal digits that fit an
/// `int`; `None` for anything else.
fn parse_java_int(text: &str) -> Option<i32> {
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse::<i32>().ok()
}

/// `void loadAgent(String agentJar, String options)` /
/// `void loadAgent(String agentJar)`. Args: `[this, agentJar, options?]`.
///
/// With `VmConfig::attach_checks` (interpreter round i2 wave 56, lane L1;
/// measured, `tools/probes/interp/L1/L1W56AttachLoadAgentErrors.java`), as
/// `HotSpotVirtualMachine.loadAgent` and JPLIS answer: a null path is
/// `NullPointerException("agent cannot be null")`; a JAR that cannot be read
/// or has no `Agent-Class` is `AgentLoadException("Agent JAR not found or no
/// Agent-Class attribute")`; an agent class that does not load, has no
/// `agentmain` or whose `agentmain` throws is
/// `AgentInitializationException("Agent JAR loaded but agent failed to
/// initialize")`; and `loadAgent(jar)` hands `agentmain` a NULL argument
/// string (an empty one before). Without it each of those returned quietly
/// (the throw propagated as itself).
fn native_vm_load_agent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let checks = attach_checks(ctx);
    let agent_jar = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let agent_jar = match agent_jar {
        Some(p) if checks || !p.is_empty() => p,
        None if checks => {
            return Err(throw_instrument_misuse(
                ctx,
                NPE_CLASS,
                Some("agent cannot be null"),
            ));
        }
        _ => {
            tracing::warn!("self-attach loadAgent: null/empty agent JAR path");
            return Ok(None);
        }
    };
    // `-XX:-EnableDynamicAgentLoading` (interpreter round i3 wave 81, lane
    // L1): HotSpot's listener refuses the `load` operation before it looks
    // at the JAR (MEASURED, HotSpot 25.0.4, an in-process self-attach).
    if dynamic_agent_loading_disabled(ctx) {
        return refuse_dynamic_agent_load(ctx);
    }
    let options = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let options = if checks {
        options
    } else {
        Some(options.unwrap_or_default())
    };
    if checks && attach_agentmain_jdk(ctx) {
        return load_agent_on_attach_listener(ctx, &agent_jar, options.as_deref());
    }
    run_self_attach(ctx, &agent_jar, options.as_deref(), checks)
}

/// HotSpot's attach listener's answer to a `load` operation under
/// `-XX:-EnableDynamicAgentLoading`, written after a non-zero completion
/// status (`AttachListener`'s dispatch, `attachListener.cpp`). The client
/// (`HotSpotVirtualMachine.loadAgentLibrary`) throws
/// `AgentLoadException("Failed to load agent library: " + <this>)`
/// (MEASURED, HotSpot 25.0.4: `VirtualMachine.loadAgent` of a self-attach,
/// for a good JAR and for one whose `agentmain` throws alike; `agentmain`
/// never runs). Interpreter round i3 wave 81, lane L1.
pub(crate) const DYNAMIC_AGENT_LOADING_DISABLED: &str =
    "Dynamic agent loading is not enabled. Use -XX:+EnableDynamicAgentLoading to launch target VM.";

/// `-XX:-EnableDynamicAgentLoading` on the calling native's VM's command
/// line (`VmConfig::enable_dynamic_agent_loading == Some(false)`).
fn dynamic_agent_loading_disabled(ctx: &dyn NativeContext) -> bool {
    crate::native::jni::vm_for_identity(ctx.vm_identity())
        .is_some_and(|shared| shared.config.enable_dynamic_agent_loading == Some(false))
}

/// `VmConfig::attach_agentmain_jdk` of the calling native's VM
/// (`CRATONVM_ATTACH_AGENTMAIN_JDK`; interpreter round i3 wave 81, lane L1).
fn attach_agentmain_jdk(ctx: &dyn NativeContext) -> bool {
    crate::native::jni::vm_for_identity(ctx.vm_identity())
        .is_some_and(|shared| shared.config.attach_agentmain_jdk)
}

/// What an in-process `VirtualMachine.loadAgent*` throws under
/// `-XX:-EnableDynamicAgentLoading`: the client's exception for the
/// listener's refusal ([`DYNAMIC_AGENT_LOADING_DISABLED`]).
#[cold]
#[inline(never)]
fn refuse_dynamic_agent_load(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let text = format!("Failed to load agent library: {DYNAMIC_AGENT_LOADING_DISABLED}");
    let message = ctx.create_string_uninterned(&text);
    Err(attach_failure(ctx.new_object_initialized(
        "com/sun/tools/attach/AgentLoadException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    )))
}

/// JPLIS's `Agent_OnAttach` option string for `agent_jar` and `options`, as
/// `HotSpotVirtualMachine.loadAgent` builds it: `<jar>` for a null
/// `options`, else `<jar>=<options>`.
fn jplis_attach_options(agent_jar: &str, options: Option<&str>) -> String {
    match options {
        Some(options) => format!("{agent_jar}={options}"),
        None => agent_jar.to_string(),
    }
}

/// How `HotSpotVirtualMachine.loadAgent` (JDK 25) reads the body of a `load`
/// reply sent with completion status 0 (`readMessage`: the lines joined by a
/// space): `None` when the agent loaded, else the exception it throws, as
/// `(class, message)`. `return code: <n>` is JPLIS's `Agent_OnAttach` code,
/// translated as `loadAgent` translates it; any other text is the reason
/// the library did not load.
pub(crate) fn jar_load_reply_exception(body: &str) -> Option<(&'static str, String)> {
    const LOAD: &str = "com/sun/tools/attach/AgentLoadException";
    const INIT: &str = "com/sun/tools/attach/AgentInitializationException";
    let result = body.lines().collect::<Vec<_>>().join(" ");
    if result.is_empty() {
        return Some((LOAD, "Target VM did not respond".to_string()));
    }
    let Some(code) = result.strip_prefix("return code: ") else {
        return Some((LOAD, format!("Failed to load agent library: {result}")));
    };
    // `Integer.parseInt` failing is a `NumberFormatException` on HotSpot's
    // client; this VM's listener never writes one, so it is a load failure.
    let Ok(code) = code.parse::<i32>() else {
        return Some((LOAD, format!("Failed to load agent library: {result}")));
    };
    match code {
        0 => None,
        -4 => Some((LOAD, "Insuffient memory".to_string())),
        100 => Some((
            LOAD,
            "Agent JAR not found or no Agent-Class attribute".to_string(),
        )),
        101 => Some((
            LOAD,
            "Unable to add JAR file to system class path".to_string(),
        )),
        102 => Some((
            INIT,
            "Agent JAR loaded but agent failed to initialize".to_string(),
        )),
        rc => Some((LOAD, format!("Failed to load agent - unknown reason: {rc}"))),
    }
}

/// An in-process `VirtualMachine.loadAgent` as HotSpot serves it
/// (`VmConfig::attach_agentmain_jdk`; interpreter round i3 wave 81, lane
/// L1): the client sends the `load` operation to the VM's own listener,
/// whose "Attach Listener" thread runs JPLIS's `Agent_OnAttach` while the
/// caller waits for the reply. Here the listener's load
/// ([`attach_listener_load`]) runs on a new thread attached as "Attach
/// Listener" while the caller waits in a blocking region, and its reply is
/// read as `HotSpotVirtualMachine.loadAgent` reads it. So the agent's
/// `agentmain` runs on that thread, and a failed start's trace
/// ([`print_jplis_start_failure`]) holds only the frames HotSpot's does.
#[cold]
#[inline(never)]
fn load_agent_on_attach_listener(
    ctx: &mut dyn NativeContext,
    agent_jar: &str,
    options: Option<&str>,
) -> MethodCallResult {
    let Some(shared) = crate::native::jni::vm_for_identity(ctx.vm_identity()) else {
        return run_self_attach(ctx, agent_jar, options, true);
    };
    let jplis = jplis_attach_options(agent_jar, options);
    // On HotSpot the caller is `VirtualMachineImpl` reading the listener's
    // reply from its socket: `RUNNABLE`, in native.
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let body = std::thread::Builder::new()
        .name("Attach Listener".to_string())
        .spawn(move || attach_listener_load(shared, "instrument", false, &jplis))
        .ok()
        .and_then(|handle| handle.join().ok());
    ctx.end_blocking_region();
    let Some(body) = body else {
        return self_attach_failed(ctx, true, false);
    };
    let Some((class, text)) = jar_load_reply_exception(&body) else {
        return Ok(None);
    };
    let message = ctx.create_string_uninterned(&text);
    Err(attach_failure(ctx.new_object_initialized(
        class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    )))
}

/// `static native void loadAgent0(String path)` of
/// `sun.instrument.InstrumentationImpl` (args: `[path]`): start the
/// executable JAR's `Launcher-Agent-Class` in the live phase, as JPLIS's
/// `loadAgent` does for the launcher -- `agentmain("", inst)`. The agent's
/// own throwable propagates (`LauncherHelper` aborts with
/// `java.launcher.jar.error4`); an agent class that cannot be loaded or has
/// no `agentmain` is the self-attach path's quiet answer (not measured
/// against JPLIS, which fails the launch).
/// Interpreter round i2 wave 60, lane L5.
fn native_instrumentation_load_agent0(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let path = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(path) = path else {
        return Err(throw_instrument_misuse(ctx, NPE_CLASS, None));
    };
    run_self_attach(ctx, &path, Some(""), false)
}

/// What a self-attach that cannot start its agent throws with
/// `VmConfig::attach_checks` (HotSpot's, through JPLIS's return codes):
/// `AgentLoadException` for a JAR it cannot use (`load`), else
/// `AgentInitializationException`. Without the switch, the quiet `Ok`.
#[cold]
#[inline(never)]
fn self_attach_failed(ctx: &mut dyn NativeContext, checks: bool, load: bool) -> MethodCallResult {
    if !checks {
        return Ok(None);
    }
    let (class, text) = if load {
        (
            "com/sun/tools/attach/AgentLoadException",
            "Agent JAR not found or no Agent-Class attribute",
        )
    } else {
        (
            "com/sun/tools/attach/AgentInitializationException",
            "Agent JAR loaded but agent failed to initialize",
        )
    };
    let message = ctx.create_string_uninterned(text);
    Err(attach_failure(ctx.new_object_initialized(
        class,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(message))],
    )))
}

/// `void loadAgentPath(String agentPath, String options)` (`absolute`) and
/// `void loadAgentLibrary(String agentLibrary, String options)`: load a
/// native agent into this VM and call its `Agent_OnAttach` in the live phase,
/// as HotSpot's attach listener does for the `load` command (interpreter
/// round i2 wave 55, lane L1;
/// `docs/internal/fixed-bugs/interpreter-L1-self-attach-cannot-load-a-native-agent-FIXED-20261019.md`).
/// Args: `[this, name, options]`. HotSpot's answers, measured
/// (`tools/probes/interp/L1/L1W55AttachNativeAgent.java`): a NULL `options`
/// reaches the agent as an empty string; a library that does not load is
/// `AgentLoadException("Failed to load agent library: <detail>")`; a non-zero
/// `Agent_OnAttach` is `AgentInitializationException("Agent_OnAttach
/// failed", <code>)`; the same library loaded again runs its
/// `Agent_OnAttach` again. Until wave 55 both ran the JDK's
/// `HotSpotVirtualMachine` code on the bare handle `attach` returns, with no
/// attach listener behind it.
fn native_vm_load_native_agent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    absolute: bool,
) -> MethodCallResult {
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(name) = name else {
        return Err(throw_instrument_misuse(ctx, NPE_CLASS, None));
    };
    // The same `load` operation as a JAR's, so refused the same way under
    // `-XX:-EnableDynamicAgentLoading` (interpreter round i3 wave 81, lane L1).
    if dynamic_agent_loading_disabled(ctx) {
        return refuse_dynamic_agent_load(ctx);
    }
    let options = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    match attach_native_agent(ctx.vm_identity(), &name, &options, absolute) {
        Ok(()) => Ok(None),
        Err(NativeAttachFailure::Init(code)) => {
            let message = ctx.create_string_uninterned("Agent_OnAttach failed");
            Err(attach_failure(ctx.new_object_initialized(
                "com/sun/tools/attach/AgentInitializationException",
                "(Ljava/lang/String;I)V",
                &[Value::Object(Some(message)), Value::Int(code)],
            )))
        }
        Err(NativeAttachFailure::Load(detail)) => {
            let text = format!("Failed to load agent library: {detail}");
            let message = ctx.create_string_uninterned(&text);
            Err(attach_failure(ctx.new_object_initialized(
                "com/sun/tools/attach/AgentLoadException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(message))],
            )))
        }
    }
}

/// The exception a failed native-agent load throws, built by
/// `new_object_initialized`; an internal error when none was built.
#[cold]
fn attach_failure(built: MethodCallResult) -> MethodCallFailed {
    match built {
        Ok(Some(Value::Object(Some(thrown)))) => MethodCallFailed::ExceptionThrown(thrown),
        Err(failed) => failed,
        Ok(_) => MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::UnsupportedOperationException {
                message: "native agent load failed".to_string(),
            },
        )),
    }
}

/// Why a self-attach could not load a native agent (wave 55).
enum NativeAttachFailure {
    /// `Agent_OnAttach` answered this non-zero code.
    Init(i32),
    /// The library or its `Agent_OnAttach` was not found: the detail.
    Load(String),
}

/// Load the native agent `name` (a path when `absolute`, else an
/// `-agentlib:`-style name, looked for in the JDK's library directory first)
/// into the VM of identity `vm` and run its `Agent_OnAttach` with
/// `options`, on the calling thread with that VM's JNI context installed, so
/// the env the agent obtains acts on it in the live phase. The VM's agent
/// registry is taken out of its JVMTI lock for the call (the agent's
/// `GetEnv` takes that lock) and the agent recorded in it after.
#[cfg(feature = "experimental-debug")]
fn attach_native_agent(
    vm: usize,
    name: &str,
    options: &str,
    absolute: bool,
) -> Result<(), NativeAttachFailure> {
    use crate::jvmti::JvmtiError;
    let Some(shared) = crate::native::jni::vm_for_identity(vm) else {
        return Err(NativeAttachFailure::Load("no VM".to_string()));
    };
    let mut registry = std::mem::take(&mut shared.debug.jvmti_env.lock().c_table.agent_registry);
    let java_home = shared.config.java_home.clone();
    let prev = crate::native::jni::replace_jni_context(&shared);
    let loaded = registry.load_attach_agent(name, options, absolute, java_home.as_deref());
    crate::native::jni::restore_jni_context(prev);
    shared
        .debug
        .jvmti_env
        .lock()
        .c_table
        .agent_registry
        .absorb(registry);
    match loaded {
        Ok(()) => Ok(()),
        Err(JvmtiError::AgentEntryPointFailed { code, .. }) => Err(NativeAttachFailure::Init(code)),
        Err(other) => Err(NativeAttachFailure::Load(other.to_string())),
    }
}

/// Without `experimental-debug` there is no C JVMTI table for an agent to
/// use: every load fails.
#[cfg(not(feature = "experimental-debug"))]
fn attach_native_agent(
    _vm: usize,
    name: &str,
    _options: &str,
    _absolute: bool,
) -> Result<(), NativeAttachFailure> {
    Err(NativeAttachFailure::Load(format!(
        "{name}: JVMTI support is not compiled in"
    )))
}

/// The attach listener's `load <lib> <absolute> <options>` operation
/// (interpreter round i2 wave 58, lane L1; `VmConfig::attach_listener_load`,
/// stage 1 of
/// `docs/internal/fixed-bugs/i57-L1-proposal-an-attach-listener-serves-external-attach-20261021.md`):
/// what another process's `VirtualMachine.loadAgent` / `loadAgentPath` /
/// `loadAgentLibrary` sends. Runs on the calling (connection) thread,
/// attached to `shared` as the daemon "Attach Listener" for the one load (as
/// HotSpot runs it on its "Attach Listener" thread), through the same
/// in-process loads the self-attach natives use. Answers the response body
/// HotSpot's listener writes after completion status 0
/// (`JvmtiAgentList::load_agent`): `return code: <n>` once an
/// `Agent_OnAttach` ran -- for a JAR agent (`instrument` with
/// `<jar>[=<options>]`), JPLIS's codes, 0, 100 (`ATTACH_ERROR_BADJAR`) or 102
/// (`ATTACH_ERROR_STARTFAIL`), which `HotSpotVirtualMachine.loadAgent` turns
/// into the exceptions the in-process path throws -- else the reason the
/// library did not load, which the client reports as
/// `AgentLoadException("Failed to load agent library: <reason>")`.
pub(crate) fn attach_listener_load(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    lib: &str,
    absolute: bool,
    options: &str,
) -> String {
    let (lib, options) = (lib.to_string(), options.to_string());
    on_attach_listener_thread(shared, move |ctx| {
        attach_listener_load_on(ctx, &lib, absolute, &options)
    })
    .unwrap_or_else(|| "the attach listener could not run the load".to_string())
}

/// Run `job` on the calling (connection) thread, attached to `shared` as the
/// daemon "Attach Listener" for the one job, as HotSpot runs its attach
/// operations on its "Attach Listener" thread (wave 58's `load`, factored out
/// in wave 59 for the property operations). `None` when the job did not run.
fn on_attach_listener_thread<R: Send + 'static>(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    job: impl FnOnce(&mut dyn NativeContext) -> R + Send + 'static,
) -> Option<R> {
    let (started_tx, _started_rx) = std::sync::mpsc::sync_channel(1);
    let (job_tx, job_rx) = std::sync::mpsc::channel::<crate::native::jni::AttachedServiceJob>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<R>();
    let boxed: crate::native::jni::AttachedServiceJob = Box::new(move |shared, thread| {
        let mut ctx = crate::vm::NativeContextImpl { shared, thread };
        let _ = out_tx.send(job(&mut ctx));
    });
    let _ = job_tx.send(boxed);
    drop(job_tx);
    crate::native::jni::run_attached_service(shared, "Attach Listener", started_tx, job_rx);
    out_rx.recv().ok()
}

/// The attach listener's `properties` (`agent` false) and `agentProperties`
/// (`agent` true) operations (interpreter round i2 wave 59, lane L1;
/// `VmConfig::attach_listener_props`, stage 2 of
/// `docs/internal/fixed-bugs/i57-L1-proposal-an-attach-listener-serves-external-attach-20261021.md`):
/// what another process's `VirtualMachine.getSystemProperties` /
/// `getAgentProperties` send. HotSpot's listener (`get_properties` in
/// `attachListener.cpp`) calls `jdk.internal.vm.VMSupport`'s
/// `serializePropertiesToByteArray()` / `serializeAgentPropertiesToByteArray()`
/// on its "Attach Listener" thread and writes the bytes (`Properties.store`
/// output) after completion status 0; this runs the same JDK methods on an
/// attached thread. `Err` with the exception's class name (HotSpot answers a
/// non-zero status with the exception) or the reason it did not run.
pub(crate) fn attach_listener_properties(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    agent: bool,
) -> Result<String, String> {
    on_attach_listener_thread(shared, move |ctx| attach_listener_properties_on(ctx, agent))
        .unwrap_or_else(
            || Err("the attach listener could not serialize the properties".to_string()),
        )
}

/// [`attach_listener_properties`]' call, on the attached thread.
fn attach_listener_properties_on(
    ctx: &mut dyn NativeContext,
    agent: bool,
) -> Result<String, String> {
    let method = if agent {
        "serializeAgentPropertiesToByteArray"
    } else {
        "serializePropertiesToByteArray"
    };
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[ATTACH_LISTENER_PROPS] {method}");
    }
    match ctx.invoke("jdk/internal/vm/VMSupport", method, "()[B", &[]) {
        Ok(Some(Value::Object(Some(bytes)))) => {
            let mut out = vec![0u8; ctx.array_length(bytes)];
            let copied = ctx.read_byte_array_into(bytes, 0, &mut out);
            out.truncate(copied);
            // `Properties.store(OutputStream)` writes ISO 8859-1 with every
            // character above `~` escaped, so this is ASCII.
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Ok(_) => Err(format!("VMSupport.{method} returned no array")),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            let class = ctx.class_id_of_object(thrown);
            let name = ctx
                .class_name_of_id(class)
                .unwrap_or_default()
                .replace('/', ".");
            Err(format!("{name}\n"))
        }
        Err(other) => Err(format!("VMSupport.{method} failed: {other:?}")),
    }
}

/// `jcmd <pid> Thread.dump_to_file [-overwrite] [-format=json] <file>`
/// (gci i12/td; page `gci-i11vt-virtual-thread-continuation-roots-review-findings`
/// item 13). HotSpot 25's `ThreadDumpToFileDCmd::dumpToFile` calls
/// `jdk.internal.vm.ThreadDumper.dumpThreads(String, boolean)` -- or
/// `dumpThreadsToJson` -- on its "Attach Listener" thread and prints the
/// returned bytes (`Created <abs path>`, `<abs path> exists, use -overwrite to
/// overwrite`, `Failed: <exception>`); a thrown exception is printed with
/// `java_lang_Throwable::print` (`<class>: <message>`) and a newline, still
/// completion status 0. This runs the same JDK method on an attached thread,
/// so the dump is the JDK's own: every platform and virtual thread of the
/// thread containers (`ThreadSnapshot.create` serves each one).
pub(crate) fn attach_listener_thread_dump(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    path: &str,
    overwrite: bool,
    json: bool,
) -> Result<String, String> {
    let path = path.to_string();
    on_attach_listener_thread(shared, move |ctx| {
        attach_listener_thread_dump_on(ctx, &path, overwrite, json)
    })
    .unwrap_or_else(|| Err("the attach listener could not run the thread dump".to_string()))
}

/// The `ThreadDumper` method HotSpot's `ThreadDumpToFileDCmd` calls
/// (`vmSymbols::dumpThreads_name` / `dumpThreadsToJson_name`).
fn thread_dumper_method(json: bool) -> &'static str {
    if json {
        "dumpThreadsToJson"
    } else {
        "dumpThreads"
    }
}

/// [`attach_listener_thread_dump`]'s call, on the attached thread.
fn attach_listener_thread_dump_on(
    ctx: &mut dyn NativeContext,
    path: &str,
    overwrite: bool,
    json: bool,
) -> Result<String, String> {
    let method = thread_dumper_method(json);
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[ATTACH_LISTENER_THREAD_DUMP] {method} {path} overwrite={overwrite}");
    }
    // Computed text: not interned (a jcmd path has no identity to keep).
    let file = ctx.create_string_uninterned(path);
    let args = [Value::Object(Some(file)), Value::Int(i32::from(overwrite))];
    match ctx.invoke("jdk/internal/vm/ThreadDumper", method, "(Ljava/lang/String;Z)[B", &args) {
        Ok(Some(Value::Object(Some(bytes)))) => {
            let mut out = vec![0u8; ctx.array_length(bytes)];
            let copied = ctx.read_byte_array_into(bytes, 0, &mut out);
            out.truncate(copied);
            // `ThreadDumper` encodes its reply as UTF-8.
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Ok(_) => Err(format!("ThreadDumper.{method} returned no array")),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            // `java_lang_Throwable::print`: the external class name, then
            // `: <detailMessage>` when there is one; the DCmd adds the newline.
            let class = ctx.class_id_of_object(thrown);
            let name = ctx.class_name_of_id(class).unwrap_or_default().replace('/', ".");
            let message = match ctx.get_field_by_name(thrown, "detailMessage") {
                Value::Object(Some(text)) => ctx.read_string(text),
                _ => None,
            };
            Ok(match message {
                Some(message) => format!("{name}: {message}\n"),
                None => format!("{name}\n"),
            })
        }
        Err(other) => Err(format!("ThreadDumper.{method} failed: {other:?}")),
    }
}

/// `jcmd <pid> help JFR.<x>` (gci i33/jf; page
/// `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016` item 1).
/// HotSpot 25's `JfrDCmd::print_help` constructs the command's
/// `jdk.jfr.internal.dcmd` class (`JfrJavaSupport::new_object`, `()V`), calls
/// `getHelp()[Ljava/lang/String;` on it and prints every line with
/// `print_cr`; a thrown exception prints its message alone
/// (`handle_pending_exception`, nothing for a null message). This runs the
/// same JDK code on an attached "Attach Listener" thread. `Err` only when it
/// could not run at all.
pub(crate) fn attach_listener_jfr_help(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    class: &str,
) -> Result<String, String> {
    let class = class.to_string();
    on_attach_listener_thread(shared, move |ctx| attach_listener_jfr_help_on(ctx, &class))
        .unwrap_or_else(|| Err("the attach listener could not run the JFR help".to_string()))
}

/// [`attach_listener_jfr_help`]'s calls, on the attached thread.
fn attach_listener_jfr_help_on(ctx: &mut dyn NativeContext, class: &str) -> Result<String, String> {
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[ATTACH_LISTENER_JFR_HELP] {class}");
    }
    let lines = match ctx.new_object_initialized(class, "()V", &[]) {
        Ok(Some(Value::Object(Some(dcmd)))) => {
            ctx.invoke_virtual(dcmd, "getHelp", "()[Ljava/lang/String;", &[])
        }
        Ok(_) => return Err(format!("{class} could not be constructed")),
        Err(failed) => Err(failed),
    };
    match lines {
        Ok(Some(Value::Object(Some(array)))) => {
            let mut out = String::new();
            for i in 0..ctx.array_length(array) {
                // gci i39/rw: HotSpot's `handle_dcmd_result` stops at the
                // first null line (`JfrJavaSupport::c_str` gives nullptr ->
                // `break`); it printed an empty line and went on.
                let Value::Object(Some(line)) = ctx.get_array_element(array, i) else {
                    break;
                };
                out.push_str(&ctx.read_string(line).unwrap_or_default());
                out.push('\n');
            }
            Ok(out)
        }
        Ok(_) => Err(format!("{class}.getHelp returned no array")),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            Ok(match ctx.get_field_by_name(thrown, "detailMessage") {
                Value::Object(Some(text)) => match ctx.read_string(text) {
                    Some(message) => format!("{message}\n"),
                    None => String::new(),
                },
                _ => String::new(),
            })
        }
        Err(other) => Err(format!("{class}.getHelp failed: {other:?}")),
    }
}

/// `jcmd <pid> GC.finalizer_info` (gci i33/jf; page
/// `gci-i32jh-jfr-dcmd-help-is-not-the-jdks-printhelp-FIXED-20261016` item 2).
/// HotSpot 25's `FinalizerInfoDCmd` calls
/// `java.lang.ref.FinalizerHistogram.getFinalizerHistogram()`, which walks the
/// JDK `Finalizer` queue; in this VM `Finalizer.register` is intercepted and
/// that queue stays empty (`native-builtins` `deprecated_lang.rs`, T8.1.6):
/// the objects waiting for `finalize()` are this VM's own queue
/// (`SharedVm::mem.finalizer_thread`). So this reads that queue's class names,
/// head first as `ReferenceQueue.forEach` walks (the queue is FIFO here, the
/// JDK's LIFO: newest first), on an attached thread -- a counted mutator, so
/// no pause moves an object while its class is read -- and
/// [`crate::runtime::serviceability::finalizer_info_text`] gives HotSpot's
/// histogram text.
pub(crate) fn attach_listener_finalizer_info(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    let queue_owner = std::sync::Arc::clone(&shared);
    on_attach_listener_thread(shared, move |ctx| {
        // addr-keyed: a transient snapshot of the finalizer queue's object
        // addresses, read and dropped on this counted mutator (no pause runs
        // in between); nothing keeps an address past this job.
        let addresses = queue_owner.mem.finalizer_thread.pending_addresses();
        let names: Vec<String> = addresses
            .iter()
            .rev()
            .filter_map(|&addr| {
                // SAFETY: a queued finalizable address is a finalizable root
                // (`pending_addresses`), so it names a live object, and no
                // pause relocates it while this counted mutator runs.
                let obj = unsafe { ObjectRef::from_raw(addr as *mut u8) };
                let class = ctx.class_id_of_object(obj);
                // gci i39/rw: `getClass().getName()` keeps a hidden class's
                // `/0x<hex>` suffix (HotSpot's `external_name`); a blanket
                // `/` -> `.` turned it into `.0x<hex>`.
                ctx.class_name_of_id(class)
                    .map(|name| crate::runtime::serviceability::external_class_name(&name))
            })
            .collect();
        crate::runtime::serviceability::finalizer_info_text(&names)
    })
    .ok_or_else(|| "the attach listener could not read the finalization queue".to_string())
}

/// `jcmd <pid> GC.run_finalization` (gci i33/jf): HotSpot 25's
/// `RunFinalizationDCmd` calls `java.lang.System.runFinalization()` and prints
/// nothing; a thrown exception is the `jcmd` operation's failure,
/// `java_lang_Throwable::print` (`<class>: <message>`) and a newline. Runs on
/// an attached "Attach Listener" thread, through the VM's own dispatch (which
/// drains this VM's finalizer queue).
pub(crate) fn attach_listener_run_finalization(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    on_attach_listener_thread(shared, |ctx| {
        match ctx.invoke("java/lang/System", "runFinalization", "()V", &[]) {
            Ok(_) => Ok(String::new()),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                let class = ctx.class_id_of_object(thrown);
                // gci i39/rw: `Klass::external_name` (a hidden class keeps
                // its `/0x<hex>` suffix).
                let name = crate::runtime::serviceability::external_class_name(
                    &ctx.class_name_of_id(class).unwrap_or_default(),
                );
                let message = match ctx.get_field_by_name(thrown, "detailMessage") {
                    Value::Object(Some(text)) => ctx.read_string(text),
                    _ => None,
                };
                Err(match message {
                    Some(message) => format!("{name}: {message}\n"),
                    None => format!("{name}\n"),
                })
            }
            Err(other) => Err(format!("System.runFinalization failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| Err("the attach listener could not run the finalization".to_string()))
}

/// Format a thrown Java exception as HotSpot's `java_lang_Throwable::print`
/// (`<external_class_name>: <detailMessage>\n` or `<external_class_name>\n`).
fn format_thrown_for_dcmd(ctx: &mut dyn NativeContext, thrown: ObjectRef) -> String {
    let class = ctx.class_id_of_object(thrown);
    let name = crate::runtime::serviceability::external_class_name(
        &ctx.class_name_of_id(class).unwrap_or_default(),
    );
    let message = match ctx.get_field_by_name(thrown, "detailMessage") {
        Value::Object(Some(text)) => ctx.read_string(text),
        _ => None,
    };
    match message {
        Some(message) => format!("{name}: {message}\n"),
        None => format!("{name}\n"),
    }
}

/// Call a zero-arg static method on `jdk/internal/vm/JcmdVThreadCommands`
/// returning `[B` (UTF-8 text) on an attached "Attach Listener" thread.
fn attach_listener_vthread_dcmd(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    method: &'static str,
) -> Result<String, String> {
    on_attach_listener_thread(shared, move |ctx| {
        match ctx.invoke("jdk/internal/vm/JcmdVThreadCommands", method, "()[B", &[]) {
            Ok(Some(Value::Object(Some(bytes)))) => {
                let mut out = vec![0u8; ctx.array_length(bytes)];
                let copied = ctx.read_byte_array_into(bytes, 0, &mut out);
                out.truncate(copied);
                Ok(String::from_utf8_lossy(&out).into_owned())
            }
            Ok(_) => Err(format!("JcmdVThreadCommands.{method} returned no byte array")),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Ok(format_thrown_for_dcmd(ctx, thrown))
            }
            Err(other) => Err(format!("JcmdVThreadCommands.{method} failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| Err(format!("the attach listener could not run {method}")))
}

/// `jcmd <pid> Thread.vthread_scheduler`: HotSpot 25's
/// `VThreadSchedulerDCmd::execute` calls
/// `jdk.internal.vm.JcmdVThreadCommands.printScheduler()`.
pub(crate) fn attach_listener_vthread_scheduler(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    attach_listener_vthread_dcmd(shared, "printScheduler")
}

/// `jcmd <pid> Thread.vthread_pollers`: HotSpot 25's
/// `VThreadPollersDCmd::execute` calls
/// `jdk.internal.vm.JcmdVThreadCommands.printPollers()`.
pub(crate) fn attach_listener_vthread_pollers(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    attach_listener_vthread_dcmd(shared, "printPollers")
}

/// `jcmd <pid> ManagementAgent.start`: HotSpot 25's
/// `ManagementAgentDCmd` calls
/// `jdk.internal.agent.Agent.startRemoteManagementAgent(String)`.
pub(crate) fn attach_listener_management_agent_start(
    shared: std::sync::Arc<crate::vm::SharedVm>,
    args: &[String],
) -> Result<String, String> {
    let mut props = Vec::with_capacity(args.len());
    for token in args {
        let token = token.trim().trim_start_matches('-');
        if token.is_empty() {
            continue;
        }
        if token.starts_with("com.sun.management.") {
            props.push(token.to_string());
        } else {
            props.push(format!("com.sun.management.{token}"));
        }
    }
    let joined = props.join(",");
    on_attach_listener_thread(shared, move |ctx| {
        let s = ctx.create_string_uninterned(&joined);
        match ctx.invoke(
            "jdk/internal/agent/Agent",
            "startRemoteManagementAgent",
            "(Ljava/lang/String;)V",
            &[Value::Object(Some(s))],
        ) {
            Ok(_) => Ok(String::new()),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Err(format_thrown_for_dcmd(ctx, thrown))
            }
            Err(other) => Err(format!("Agent.startRemoteManagementAgent failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| Err("the attach listener could not start the management agent".to_string()))
}

/// `jcmd <pid> ManagementAgent.start_local`: HotSpot 25's
/// `jdk.internal.agent.Agent.startLocalManagementAgent()`.
pub(crate) fn attach_listener_management_agent_start_local(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    on_attach_listener_thread(shared, |ctx| {
        match ctx.invoke("jdk/internal/agent/Agent", "startLocalManagementAgent", "()V", &[]) {
            Ok(_) => Ok(String::new()),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Err(format_thrown_for_dcmd(ctx, thrown))
            }
            Err(other) => Err(format!("Agent.startLocalManagementAgent failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| {
        Err("the attach listener could not start the local management agent".to_string())
    })
}

/// `jcmd <pid> ManagementAgent.stop`: HotSpot 25's
/// `jdk.internal.agent.Agent.stopRemoteManagementAgent()`.
pub(crate) fn attach_listener_management_agent_stop(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    on_attach_listener_thread(shared, |ctx| {
        match ctx.invoke("jdk/internal/agent/Agent", "stopRemoteManagementAgent", "()V", &[]) {
            Ok(_) => Ok(String::new()),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Err(format_thrown_for_dcmd(ctx, thrown))
            }
            Err(other) => Err(format!("Agent.stopRemoteManagementAgent failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| Err("the attach listener could not stop the management agent".to_string()))
}

/// `jcmd <pid> ManagementAgent.status`: HotSpot 25's
/// `jdk.internal.agent.Agent.getManagementAgentStatus()`.
pub(crate) fn attach_listener_management_agent_status(
    shared: std::sync::Arc<crate::vm::SharedVm>,
) -> Result<String, String> {
    on_attach_listener_thread(shared, |ctx| {
        match ctx.invoke(
            "jdk/internal/agent/Agent",
            "getManagementAgentStatus",
            "()Ljava/lang/String;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(s)))) => Ok(ctx.read_string(s).unwrap_or_default()),
            Ok(_) => Ok("Agent: disabled\n".to_string()),
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                Err(format_thrown_for_dcmd(ctx, thrown))
            }
            Err(other) => Err(format!("Agent.getManagementAgentStatus failed: {other:?}")),
        }
    })
    .unwrap_or_else(|| Err("the attach listener could not query the management agent".to_string()))
}

/// [`attach_listener_load`]'s load, on the attached thread.
fn attach_listener_load_on(
    ctx: &mut dyn NativeContext,
    lib: &str,
    absolute: bool,
    options: &str,
) -> String {
    if crate::runtime::env_cache::frame_trace() {
        eprintln!("[ATTACH_LISTENER_LOAD] lib={lib} absolute={absolute}");
    }
    if !absolute && lib == "instrument" {
        // JPLIS's `Agent_OnAttach` option string: `<jar>[=<options>]`.
        let (jar, agent_options) = match options.split_once('=') {
            Some((jar, rest)) => (jar, Some(rest)),
            None => (options, None),
        };
        const BADJAR: i32 = 100;
        const STARTFAIL: i32 = 102;
        let code = match run_self_attach(ctx, jar, agent_options, true) {
            Ok(_) => 0,
            Err(MethodCallFailed::ExceptionThrown(thrown)) => {
                let class = ctx.class_id_of_object(thrown);
                if ctx
                    .class_name_of_id(class)
                    .is_some_and(|name| name == "com/sun/tools/attach/AgentLoadException")
                {
                    BADJAR
                } else {
                    STARTFAIL
                }
            }
            Err(_) => STARTFAIL,
        };
        return format!("return code: {code}");
    }
    match attach_native_agent(ctx.vm_identity(), lib, options, absolute) {
        Ok(()) => "return code: 0".to_string(),
        Err(NativeAttachFailure::Init(code)) => format!("return code: {code}"),
        Err(NativeAttachFailure::Load(detail)) => detail,
    }
}

/// `void detach()`. No out-of-process connection to tear down.
fn native_vm_detach(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// What the calling native's VM prints for a dynamic agent load
/// (`VmConfig::dynamic_agent_warning`; interpreter round i2 wave 60, lane
/// L1): `None` with the switch off (nothing), else whether HotSpot's
/// dynamic-agent warning is due (`-XX:±EnableDynamicAgentLoading` not on
/// the command line). JPLIS's refusal lines are printed either way.
fn dynamic_agent_warning(ctx: &dyn NativeContext) -> Option<bool> {
    let shared = crate::native::jni::vm_for_identity(ctx.vm_identity())?;
    shared
        .config
        .dynamic_agent_warning
        .then(|| shared.config.enable_dynamic_agent_loading.is_none())
}

/// `path` as an absolute path, as HotSpot names the JAR in its lines
/// (MEASURED, `L1W60DynamicAgentWarning`: a relative path is printed
/// absolute); `path` itself when that cannot be told.
fn absolute_jar_path(path: &str) -> String {
    std::path::absolute(path).map_or_else(|_| path.to_string(), |p| p.display().to_string())
}

/// HotSpot's warning for a Java agent loaded dynamically, on stderr, before
/// its `agentmain` runs (interpreter round i2 wave 60, lane L1; MEASURED,
/// `tools/probes/interp/L1/L1W60DynamicAgentWarning.java`: every load of a
/// JAR that names an `Agent-Class`, again for each load of the same JAR,
/// and for an agent whose `agentmain` throws).
#[cold]
#[inline(never)]
fn print_dynamic_agent_warning(agent_jar: &str) {
    use std::io::Write;
    let path = absolute_jar_path(agent_jar);
    let mut err = std::io::stderr().lock();
    let _ = writeln!(
        err,
        "WARNING: A Java agent has been loaded dynamically ({path})"
    );
    let _ = writeln!(
        err,
        "WARNING: If a serviceability tool is in use, please run with -XX:+EnableDynamicAgentLoading to hide this warning"
    );
    let _ = writeln!(
        err,
        "WARNING: If a serviceability tool is not in use, please run with -Djdk.instrument.traceUsage for more information"
    );
    let _ = writeln!(
        err,
        "WARNING: Dynamic loading of agents will be disallowed by default in a future release"
    );
}

/// JPLIS's line on stderr for an agent JAR it cannot use (wave 60, lane
/// L1): one whose manifest names no `Agent-Class` (MEASURED,
/// `L1W60DynamicAgentWarning`), or one it cannot open (`has_manifest`
/// false; the text wave 59 measured for a missing JAR, its path's spelling
/// not measured).
#[cold]
#[inline(never)]
fn print_agent_jar_refusal(agent_jar: &str, has_manifest: bool) {
    let path = absolute_jar_path(agent_jar);
    if has_manifest {
        eprintln!("Failed to find Agent-Class manifest attribute from {path}");
    } else {
        eprintln!("Error opening zip file or JAR manifest missing: {path}");
    }
}

/// Load the agent JAR at `agent_jar` into the running VM and invoke its
/// `agentmain`, mirroring the JVM's dynamic-attach agent-load sequence.
fn run_self_attach(
    ctx: &mut dyn NativeContext,
    agent_jar: &str,
    options: Option<&str>,
    checks: bool,
) -> MethodCallResult {
    // The agent class is the SYSTEM loader's, whichever loader the attaching
    // `ByteBuddyAgent` came from (a `ModifiedClassPathClassLoader` fork's, under
    // Spring Boot's `@ClassPathExclusions`): `ByteBuddyAgent` reads the
    // `Installer` back through the system loader, so a fork copy of it stored the
    // `Instrumentation` where nothing looks ("The Byte Buddy agent is not
    // initialized or unavailable").
    cratonvm_native_api::caller_loader::suppress(|| {
        run_self_attach_inner(ctx, agent_jar, options, checks)
    })
}

fn run_self_attach_inner(
    ctx: &mut dyn NativeContext,
    agent_jar: &str,
    options: Option<&str>,
    checks: bool,
) -> MethodCallResult {
    use std::path::Path;

    // 1. Parse the agent JAR manifest. Dynamic attach resolves the agent
    //    entry point from `Launcher-Agent-Class` (JEP 330 style) or, more
    //    commonly, `Agent-Class`. (With `checks`, `Agent-Class` only, as
    //    JPLIS's `Agent_OnAttach` reads it.)
    let manifest = cratonvm_classloading::ClassPath::read_jar_manifest(Path::new(agent_jar));
    let (agent_class, can_redefine, can_retransform, can_set_native_method_prefix) = match &manifest
    {
        Some(m) => {
            let launcher = if checks {
                None
            } else {
                m.attributes.get("Launcher-Agent-Class")
            };
            let class = launcher
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
                bool_attr("Can-Set-Native-Method-Prefix"),
            )
        }
        None => (None, false, false, false),
    };
    let warning = dynamic_agent_warning(ctx);
    let agent_class = match agent_class {
        Some(c) => c,
        None => {
            // With the switch, JPLIS's own line says it (and the VM's log
            // line would be a stderr line HotSpot does not print).
            if warning.is_some() {
                print_agent_jar_refusal(agent_jar, manifest.is_some());
            } else {
                tracing::warn!(
                    "self-attach loadAgent: `{agent_jar}` has no Agent-Class/Launcher-Agent-Class \
                     manifest attribute"
                );
            }
            return self_attach_failed(ctx, checks, true);
        }
    };
    if warning == Some(true) {
        print_dynamic_agent_warning(agent_jar);
    }

    // 2. Make the agent's classes loadable. The agent JAR is frequently the
    //    library's own JAR (already on the app classpath) but may be a temp
    //    JAR that ByteBuddy extracted; add it either way (idempotent).
    if let Err(error) = append_system_search(ctx, agent_jar) {
        tracing::warn!("attach: cannot publish agent resource search: {error:?}");
        return self_attach_failed(ctx, checks, false);
    }

    // `CRATONVM_ATTACH_AGENTMAIN_JDK` (interpreter round i3 wave 81, lane
    // L1): the agent starts through the JDK's `loadClassAndCallAgentmain`,
    // which loads the class itself (a class that does not load is its
    // `ClassNotFoundException`, printed as JPLIS prints it).
    let jdk_start = checks && attach_agentmain_jdk(ctx);
    let agent_internal = agent_class.replace('.', "/");
    let agent_class_id = match ctx.load_class(&agent_internal) {
        // What HotSpot's `loadClassAndStartAgent` (`getDeclaredMethod` of
        // `agentmain`) and the JPLIS library resolve through loaders, for
        // `Instrumentation.getInitiatedClasses` (interpreter round i1 wave 47,
        // lane L5; a no-op while no record is armed).
        Ok(loaded) => {
            let agent_class_id = match loaded {
                Some(Value::Object(Some(mirror))) => ctx.class_id_from_mirror(mirror),
                _ => None,
            };
            ctx.note_agent_start(agent_class_id);
            agent_class_id
        }
        Err(_) if jdk_start => None,
        Err(e) => {
            tracing::warn!("self-attach loadAgent: cannot load Agent-Class `{agent_class}`: {e:?}");
            return self_attach_failed(ctx, checks, false);
        }
    };

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
        let args_str = ctx.create_string(options.unwrap_or_default());
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
        // The manifest's capabilities, which the public methods answer from
        // (`agent_capability`; wave 42, lane L3). The registered `<init>`
        // above is a no-op, so they are recorded here.
        let inst_now = ctx.read_native_pin(pin, inst);
        stamp_capabilities(
            &*ctx,
            inst_now,
            can_redefine,
            can_retransform,
            can_set_native_method_prefix,
        );
    }

    if jdk_start {
        return start_agent_through_jdk(ctx, &agent_class, options, pin, inst);
    }

    // 4. Invoke agentmain(String, Instrumentation), falling back to the
    //    single-arg agentmain(String) form per the instrument spec.
    let two_arg = "(Ljava/lang/String;Ljava/lang/instrument/Instrumentation;)V";
    let one_arg = "(Ljava/lang/String;)V";
    // With `checks` (interpreter round i2 wave 59, lane L1; MEASURED,
    // `tools/probes/interp/L1/L1W59AgentmainShapes.java`), JPLIS's lookup
    // (`InstrumentationImpl.loadClassAndStartAgent`): the agent class's OWN
    // declared `agentmain(String, Instrumentation)`, else its declared
    // `agentmain(String)` -- an inherited one is not found -- and the one
    // found must be public (else `IllegalAccessException`) and static (it is
    // invoked with a null receiver): any of those fails the load. Without
    // `checks` (or with no class id), the hierarchy walk as before.
    let (has_two, has_one) = match agent_class_id.filter(|_| checks) {
        Some(cid) => {
            let declared = |desc: &str| ctx.declared_method_access_flags(cid, "agentmain", desc);
            let (found, two) = match declared(two_arg) {
                Some(flags) => (Some(flags), true),
                None => (declared(one_arg), false),
            };
            const ACC_PUBLIC: u16 = 0x0001;
            const ACC_STATIC: u16 = 0x0008;
            match found {
                Some(flags) if flags & ACC_PUBLIC != 0 && flags & ACC_STATIC != 0 => (two, !two),
                _ => (false, false),
            }
        }
        None => {
            let has_two = ctx.method_exists(&agent_internal, "agentmain", two_arg);
            (
                has_two,
                !has_two && ctx.method_exists(&agent_internal, "agentmain", one_arg),
            )
        }
    };

    let opts_obj = options.map(|o| ctx.create_string(o));
    let inst_now = ctx.read_native_pin(pin, inst);
    let result = if has_two {
        ctx.invoke(
            &agent_internal,
            "agentmain",
            two_arg,
            &[Value::Object(opts_obj), Value::Object(Some(inst_now))],
        )
    } else if has_one {
        ctx.invoke(
            &agent_internal,
            "agentmain",
            one_arg,
            &[Value::Object(opts_obj)],
        )
    } else {
        tracing::warn!(
            "self-attach loadAgent: Agent-Class `{agent_class}` has no agentmain(String[,Instrumentation])"
        );
        ctx.unpin_native_roots(pin);
        return self_attach_failed(ctx, checks, false);
    };
    ctx.unpin_native_roots(pin);

    // Without `checks`, propagate a thrown agent exception as itself; with
    // them, HotSpot's `AgentInitializationException` (JPLIS answers
    // `ATTACH_ERROR_STARTFAIL`). A normal Installer.agentmain just stores the
    // Instrumentation and returns void.
    match result {
        Err(MethodCallFailed::ExceptionThrown(_)) if checks => {
            self_attach_failed(ctx, checks, false)
        }
        other => other.map(|_| None),
    }
}

/// JPLIS's stderr lines after the agent's start threw, under the throwable's
/// trace: its three `jplis_assert` failures (with the JDK 25.0.4 source
/// lines they name) and `Agent_OnAttach`'s `Agent failed to start!`
/// (MEASURED, HotSpot 25.0.4, an `agentmain` that throws). Interpreter
/// round i3 wave 81, lane L1.
pub(crate) const JPLIS_START_FAILURE_LINES: [&str; 4] = [
    "*** java.lang.instrument ASSERTION FAILED ***: \"!errorOutstanding\" with message Outstanding error when calling method in invokeJavaAgentMainMethod at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 627",
    "*** java.lang.instrument ASSERTION FAILED ***: \"success\" with message invokeJavaAgentMainMethod failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 466",
    "*** java.lang.instrument ASSERTION FAILED ***: \"success\" with message startJavaAgent failed at src/java.instrument/share/native/libinstrument/InvocationAdapter.c line: 444",
    "Agent failed to start!",
];

/// Step 4 of [`run_self_attach_inner`] as JPLIS does it
/// (`VmConfig::attach_agentmain_jdk`; interpreter round i3 wave 81, lane
/// L1): `inst.loadClassAndCallAgentmain(agentClass, options)`, the JDK's own
/// `loadClassAndStartAgent` (the system loader's `loadClass`, the declared
/// `agentmain(String, Instrumentation)` else `agentmain(String)`, the
/// public check, `Method.invoke`). A throwable out of it is what JPLIS's
/// `invokeJavaAgentMainMethod` describes and asserts on
/// ([`print_jplis_start_failure`]), and the load answers
/// `ATTACH_ERROR_STARTFAIL` (`AgentInitializationException`). Releases the
/// native pins from `pin` on.
#[cold]
#[inline(never)]
fn start_agent_through_jdk(
    ctx: &mut dyn NativeContext,
    agent_class: &str,
    options: Option<&str>,
    pin: usize,
    inst: ObjectRef,
) -> MethodCallResult {
    let class_name = ctx.create_string(agent_class);
    let class_pin = ctx.pin_native_root(class_name);
    let opts_obj = options.map(|o| ctx.create_string(o));
    let class_name = ctx.read_native_pin(class_pin, class_name);
    let inst_now = ctx.read_native_pin(pin, inst);
    let result = ctx.invoke(
        "sun/instrument/InstrumentationImpl",
        "loadClassAndCallAgentmain",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[
            Value::Object(Some(inst_now)),
            Value::Object(Some(class_name)),
            Value::Object(opts_obj),
        ],
    );
    ctx.unpin_native_roots(pin);
    match result {
        Ok(_) => Ok(None),
        Err(MethodCallFailed::ExceptionThrown(thrown)) => {
            print_jplis_start_failure(ctx, thrown);
            self_attach_failed(ctx, true, false)
        }
        Err(other) => Err(other),
    }
}

/// What JPLIS prints on stderr when the agent's start threw `thrown`
/// (interpreter round i3 wave 81, lane L1; MEASURED, HotSpot 25.0.4): JNI's
/// `ExceptionDescribe` -- `Exception in thread "<current thread's name>" `
/// and then `thrown.printStackTrace()` -- and [`JPLIS_START_FAILURE_LINES`].
#[cold]
#[inline(never)]
fn print_jplis_start_failure(ctx: &mut dyn NativeContext, thrown: ObjectRef) {
    describe_with_jplis_lines(ctx, thrown, "Attach Listener", &JPLIS_START_FAILURE_LINES);
}

/// JPLIS's stderr lines after a startup agent's `premain` start threw,
/// under the throwable's trace (interpreter round i4 wave 84, lane L1):
/// `invokeJavaAgentMainMethod`'s and `startJavaAgent`'s asserts (the two
/// JDK 25.0.4 source lines MEASURED for the attach path, the same C
/// functions), then `processJavaStart`'s `agent load/premain call failed`
/// (its line DERIVED: JDK 17-25 `JPLISAgent.c` keep it at 422 next to the
/// unchanged 466 and 627; refresh from `$J/java` with
/// `tools/probes/interp/L1/L1W84PremainFailure.java`). `eventHandlerVMInit`
/// then calls `abortJVM` -> JNI `FatalError` with
/// [`JPLIS_PREMAIN_FATAL_ERROR`].
pub(crate) const JPLIS_PREMAIN_FAILURE_LINES: [&str; 3] = [
    "*** java.lang.instrument ASSERTION FAILED ***: \"!errorOutstanding\" with message Outstanding error when calling method in invokeJavaAgentMainMethod at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 627",
    "*** java.lang.instrument ASSERTION FAILED ***: \"success\" with message invokeJavaAgentMainMethod failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 466",
    "*** java.lang.instrument ASSERTION FAILED ***: \"result\" with message agent load/premain call failed at src/java.instrument/share/native/libinstrument/JPLISAgent.c line: 422",
];

/// The line JNI `FatalError` prints on `tty` (stdout) for JPLIS's
/// `abortJVM(jnienv, JPLIS_ERRORMESSAGE_CANNOTSTART ", processJavaStart
/// failed")` before `os::abort` (interpreter round i4 wave 84, lane L1).
pub const JPLIS_PREMAIN_FATAL_ERROR: &str =
    "FATAL ERROR in native method: processing of -javaagent failed, processJavaStart failed";

/// What JPLIS prints on stderr when a startup agent's `premain` start threw
/// `thrown` on the main thread (interpreter round i4 wave 84, lane L1): JNI
/// `ExceptionDescribe` and [`JPLIS_PREMAIN_FAILURE_LINES`]. The caller
/// prints [`JPLIS_PREMAIN_FATAL_ERROR`] and aborts.
#[cold]
#[inline(never)]
pub(crate) fn print_jplis_premain_failure(ctx: &mut dyn NativeContext, thrown: ObjectRef) {
    describe_with_jplis_lines(ctx, thrown, "main", &JPLIS_PREMAIN_FAILURE_LINES);
}

/// `ExceptionDescribe` (`Exception in thread "<name>" ` and
/// `thrown.printStackTrace()`, `fallback_name` when the current thread's
/// name cannot be read), then `lines`, on stderr.
fn describe_with_jplis_lines(
    ctx: &mut dyn NativeContext,
    thrown: ObjectRef,
    fallback_name: &str,
    lines: &[&str],
) {
    use std::io::Write;
    let thrown_pin = ctx.pin_native_root(thrown);
    let name = current_thread_name(ctx).unwrap_or_else(|| fallback_name.to_string());
    {
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "Exception in thread \"{name}\" ");
        let _ = err.flush();
    }
    let thrown = ctx.read_native_pin(thrown_pin, thrown);
    let _ = ctx.invoke_virtual(thrown, "printStackTrace", "()V", &[]);
    ctx.unpin_native_roots(thrown_pin);
    let mut err = std::io::stderr().lock();
    for line in lines {
        let _ = writeln!(err, "{line}");
    }
    let _ = err.flush();
}

/// `Thread.currentThread().getName()` on the calling thread; `None` when it
/// cannot be read.
fn current_thread_name(ctx: &mut dyn NativeContext) -> Option<String> {
    let thread = ctx.current_thread_object();
    match ctx.invoke_virtual(thread, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(name)))) => ctx.read_string(name),
        _ => None,
    }
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
    // Interpreter round i2 wave 60, lane L5: JPLIS's
    // `InstrumentationImpl.loadAgent0(String jar)`, which
    // `LauncherHelper.getMainClassFromJar` reaches (through `loadAgent`) for
    // an executable JAR's `Launcher-Agent-Class` once the launcher passes
    // `LM_JAR` (`CRATONVM_LAUNCHER_HELPER_JAR`; probe
    // `L5W60LauncherJarAttributes`). A bridge, stated: the self-attach path
    // above with `Launcher-Agent-Class` preferred and an EMPTY argument
    // string, as JPLIS's `loadAgent` starts the agent (measured: HotSpot's
    // `agentmain` sees `""`). The JDK body is a bare `native` declaration.
    r.register_with_kind(
        "sun/instrument/InstrumentationImpl",
        "loadAgent0",
        "(Ljava/lang/String;)V",
        native_instrumentation_load_agent0 as NativeCallback,
        NativeKind::Bridge,
    );
    // Interpreter round i2 wave 55, lane L1: a native agent. A bridge, as
    // the JAR path above: the VM's own attach listener in process, which
    // the JDK's `HotSpotVirtualMachine` reaches through a socket the VM
    // does not open.
    r.register_with_kind(
        VM_ATTACH_CLASS,
        "loadAgentPath",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| native_vm_load_native_agent(ctx, args, true))
            as NativeCallback,
        NativeKind::Bridge,
    );
    r.register_with_kind(
        VM_ATTACH_CLASS,
        "loadAgentLibrary",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            native_vm_load_native_agent(ctx, args, false)
        }) as NativeCallback,
        NativeKind::Bridge,
    );
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
    //
    // JIT round 20, lane walkcompat
    // (`docs/internal/fixed-bugs/jr20-javaagent-retransform-weave-does-not-take-effect-FIXED-20261012.md`):
    // these public-method rows stand in front of concrete JDK bytecode, so
    // they are stated `Intrinsic`s. Since dev's bytecode-first dispatch in
    // both modes (`0b4278e051` / `d43fa8054b`, "the selected Java body
    // remains authoritative in both launch policies") a `Bridge` here LOST to
    // the JDK body: `addTransformer` stored the transformer in the JDK's
    // `TransformerManager`, `retransformClasses0` then walked this VM's empty
    // chain and re-installed the class's own bytes -- "weave ok", old bodies,
    // both modes, `--nojit` too (`R16RedefineWoven`). The chain is the VM's
    // state (the JDK's `transform` path is not run, see
    // `run_transformer_chain`), and the bare `InstrumentationImpl` the
    // `-javaagent` path builds never ran its constructor, so the JDK bodies
    // that read `mEnvironmentSupports*` / the managers answer wrongly. The
    // `0`-suffixed `ACC_NATIVE` rows stay `Bridge`s; the category is restored
    // before the synthetic `cratonvm/Instrument` probe bridge.
    //
    // Intrinsic, stated: the JDK bodies of these three (and of
    // `setNativeMethodPrefix` below) are concrete bytecode that writes only
    // the Java `TransformerManager`s, and since the bytecode-first dispatch a
    // `Bridge` yields to that body in BOTH modes. The Rust chain then stayed
    // empty, `retransformClasses0` ran no transformer
    // (`run_transformer_chain: rust_chain=0 entries`), and Mockito's inline
    // mock maker -- which weaves through `retransformClasses` -- never
    // intercepted: the first stubbed call ran the real body
    // (`InvalidUseOfMatchersException` / NPE in ~40 Spring Boot test classes).
    // The chain is the VM's own model of state the JDK body cannot reach, a
    // carrier; see `docs/internal/springboot/instrumentation-transformers-lost-to-bytecode-first-FIXED-20261013.md`.
    let prev_category = r.current_category();
    r.set_category(NativeKind::Intrinsic);
    r.register_with_kind(
        impl_class,
        "addTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;Z)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| {
            let can_retransform = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
            native_add_transformer(ctx, args, can_retransform)
        }) as NativeCallback,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        impl_class,
        "addTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;)V",
        (|ctx: &mut dyn NativeContext, args: &[Value]| native_add_transformer(ctx, args, false))
            as NativeCallback,
        NativeKind::Intrinsic,
    );
    r.register_with_kind(
        impl_class,
        "removeTransformer",
        "(Ljava/lang/instrument/ClassFileTransformer;)Z",
        native_remove_transformer,
        NativeKind::Intrinsic,
    );
    // The `ACC_NATIVE` rows below state their own `Bridge`; the ambient
    // category goes back for the rows registered without a kind until the
    // public-name aliases further down.
    r.set_category(prev_category);
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
    // retransformability matches the flag. A failed native link reads the
    // recorded prefixes (`native_method_prefixes`; interpreter round i1 wave
    // 44, lane L3), and `isNativeMethodPrefixSupported` answers the agent's
    // `Can-Set-Native-Method-Prefix` since then. The public
    // `setNativeMethodPrefix` is itself a registered native (the JDK body
    // reads `TransformerManager`s the VM's `InstrumentationImpl` lacks), so
    // this is reached only by a caller of the JDK's private entry.
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
    // Interpreter round i6 wave 119, lane L3 (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`):
    // for an `InstrumentationImpl` whose transformers the JDK's own bodies
    // keep, the flag files (or drops) the object's group in this VM's chain,
    // which is then the chain walk's entry for those transformers
    // (`note_jdk_transformer_group`). Every other receiver: nothing, as above.
    r.register_with_kind(
        impl_class,
        "setHasRetransformableTransformers",
        "(JZ)V",
        |ctx, args| {
            if let Some(inst) = jdk_managed_instrumentation(&*ctx, instrumentation_receiver(args)) {
                let has = args.get(2).and_then(Value::as_int).unwrap_or(0) != 0;
                note_jdk_transformer_group(&*ctx, inst, true, has);
            }
            Ok(None)
        },
        NativeKind::Bridge,
    );
    // The non-retransformable group's leaf (`(long, boolean)`, `ACC_NATIVE` in
    // JDK 21 and 25). Before wave 119 nothing registered it: the JDK body that
    // calls it never ran (`addTransformer` is an `Intrinsic`). Stated `Bridge`,
    // over a native method with no JDK body. Every receiver but a JDK-managed
    // one: nothing (the chain already holds what the flag summarises).
    r.register_with_kind(
        impl_class,
        "setHasTransformers",
        "(JZ)V",
        |ctx, args| {
            if let Some(inst) = jdk_managed_instrumentation(&*ctx, instrumentation_receiver(args)) {
                let has = args.get(2).and_then(Value::as_int).unwrap_or(0) != 0;
                note_jdk_transformer_group(&*ctx, inst, false, has);
            }
            Ok(None)
        },
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
    //
    // JIT round 20, lane walkcompat: stated `Intrinsic`s over the JDK's
    // concrete bodies (see `addTransformer` above), through `retransformClasses`
    // / `redefineClasses`; `<init>` keeps its force-native-override entry.
    //
    // Interpreter round i6 wave 120, lane L3 (`CRATONVM_INSTRUMENT_JDK_TRANSFORMERS`):
    // for a JDK-managed agent these eight run the JDK's own bodies over the
    // leaves (`jdk_body_or`); `setNativeMethodPrefix` keeps the native (its
    // leaf replays prefixes positionally over this VM's chain entries, and a
    // JDK-managed group is one entry).
    r.set_category(NativeKind::Intrinsic);
    r.register(
        impl_class,
        "isRetransformClassesSupported",
        "()Z",
        public_is_retransform_supported,
    );
    r.register(
        impl_class,
        "isRedefineClassesSupported",
        "()Z",
        public_is_redefine_supported,
    );
    r.register(
        impl_class,
        "isNativeMethodPrefixSupported",
        "()Z",
        public_is_prefix_supported,
    );
    // Interpreter round i1 wave 44, lane L3: the JDK body reads the
    // `TransformerManager`s the bare `InstrumentationImpl` lacks; answer from
    // this VM's chain instead, with the JDK method's checks. Intrinsic, as
    // `addTransformer` above: the chain is the state, the JDK body cannot see it.
    r.register_with_kind(
        impl_class,
        "setNativeMethodPrefix",
        "(Ljava/lang/instrument/ClassFileTransformer;Ljava/lang/String;)V",
        native_set_native_method_prefix,
        NativeKind::Intrinsic,
    );
    r.register(
        impl_class,
        "isModifiableClass",
        "(Ljava/lang/Class;)Z",
        public_is_modifiable_class,
    );
    r.register(
        impl_class,
        "getObjectSize",
        "(Ljava/lang/Object;)J",
        public_get_object_size,
    );
    r.register(
        impl_class,
        "getAllLoadedClasses",
        "()[Ljava/lang/Class;",
        public_get_all_loaded_classes,
    );
    r.register(
        impl_class,
        "retransformClasses",
        "([Ljava/lang/Class;)V",
        public_retransform_classes,
    );
    r.register(
        impl_class,
        "redefineClasses",
        "([Ljava/lang/instrument/ClassDefinition;)V",
        public_redefine_classes,
    );
    r.set_category(prev_category);
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

    /// gci i12/td: `jcmd Thread.dump_to_file` calls the `ThreadDumper` method
    /// HotSpot 25's `ThreadDumpToFileDCmd` calls, with its descriptor
    /// (`vmSymbols::string_bool_byte_array_signature`).
    #[test]
    fn gci_i12td_thread_dump_calls_hotspots_thread_dumper_method() {
        assert_eq!(thread_dumper_method(false), "dumpThreads");
        assert_eq!(thread_dumper_method(true), "dumpThreadsToJson");
        let src = include_str!("instrument.rs").replace("\r\n", "\n");
        assert!(src.contains(
            "ctx.invoke(\"jdk/internal/vm/ThreadDumper\", method, \"(Ljava/lang/String;Z)[B\", &args)"
        ));
    }

    /// Interpreter round i2 wave 62, lane L1: the modelled HotSpot instance
    /// sizes of `L1W62GetObjectSizeShapes`' classes, the numbers HotSpot
    /// 25.0.3 answers (MEASURED). Each class's field tags, `Object` first.
    #[test]
    fn hotspot_instance_sizes_match_the_measured_shapes() {
        let size = |chain: &[&[u8]]| {
            let chain: Vec<Vec<u8>> = chain
                .iter()
                .map(|tags| tags.iter().map(|&t| hotspot_field_size(t)).collect())
                .collect();
            hotspot_instance_size(&chain)
        };
        let object: &[u8] = b"";
        assert_eq!(size(&[object]), 16, "Object");
        assert_eq!(size(&[object, b""]), 16, "Empty");
        assert_eq!(size(&[object, b"L"]), 16, "OneRef");
        assert_eq!(size(&[object, b"LL"]), 24, "TwoRefs");
        assert_eq!(size(&[object, b"LLL"]), 24, "ThreeRefs");
        assert_eq!(size(&[object, b"I"]), 16, "OneInt");
        assert_eq!(size(&[object, b"J"]), 24, "OneLong");
        assert_eq!(size(&[object, b"JL"]), 24, "LongAndRef");
        assert_eq!(size(&[object, b"BL"]), 24, "ByteAndRef");
        assert_eq!(size(&[object, b"I", b"I"]), 24, "IntSub");
        assert_eq!(size(&[object, b"L", b"L"]), 24, "RefSub");
        assert_eq!(size(&[object, b"ZBCSIFJDL"]), 48, "Mixed");
        // AbstractMap (keySet, values), HashMap (table, entrySet, size,
        // modCount, threshold, loadFactor).
        assert_eq!(size(&[object, b"LL", b"LLIIIF"]), 48, "HashMap");
        // String (value, coder, hash, hashIsZero) and its injected `flags`.
        assert_eq!(size(&[object, b"LBIZB"]), 24, "String");
        // Throwable (backtrace, detailMessage, cause, stackTrace, depth,
        // suppressedExceptions); InternalError's injected boolean.
        assert_eq!(size(&[object, b"LLLLIL"]), 40, "Throwable");
        assert_eq!(
            size(&[object, b"LLLLIL", b"", b"", b"Z"]),
            40,
            "InternalError"
        );
        // Thread: eetop, tid, name, interrupted, contextClassLoader, holder,
        // threadLocals, inheritableThreadLocals, scopedValueBindings,
        // interruptLock, parkBlocker, nioBlocker, cont,
        // uncaughtExceptionHandler, threadLocalRandomSeed, two ints,
        // container, headStackableScopes; and the four injected fields.
        let thread: &[u8] = b"JJLZLLLLLLLLLLJIILLJIZS";
        assert_eq!(size(&[object, thread]), 112, "Thread");
        // Mirrors: 120 bytes, then the mirrored class's static fields.
        let mirror = |tags: &[u8]| {
            let statics: Vec<u8> = tags.iter().map(|&t| hotspot_field_size(t)).collect();
            hotspot_layout_size(CLASS_MIRROR_STATICS_OFFSET, &[statics])
        };
        assert_eq!(mirror(b""), 120, "Object.class");
        assert_eq!(mirror(b"I"), 128, "StaticInt.class");
        assert_eq!(mirror(b"L"), 128, "StaticRef.class");
        assert_eq!(mirror(b"J"), 128, "StaticLong.class");
        assert_eq!(mirror(b"BJ"), 136, "StaticByteAndLong.class");
        assert_eq!(mirror(b"LILSD"), 144, "StaticMixed.class");
        assert_eq!(align_to_object(16 + 3 * 4), 32, "Object[3]");
        assert_eq!(align_to_object(16), 16, "Object[0]");
    }

    /// Interpreter round i2 wave 56, lane L1: `VirtualMachine.attach`'s id
    /// checks, HotSpot 25's answers (measured, `L1W56AttachRefusals`).
    #[test]
    fn attach_refuses_what_hotspot_refuses() {
        const NS: &str = "com/sun/tools/attach/AttachNotSupportedException";
        let own = 4242u32;
        let refusal = |id: &str, allow: Option<&str>| attach_refusal(id, own, allow, false);
        let current = Some((
            "java/io/IOException",
            "Can not attach to current VM".to_string(),
        ));
        for allow in [None, Some("false"), Some("no")] {
            assert_eq!(refusal("4242", allow), current, "{allow:?}");
            assert_eq!(refusal("0", allow), current, "{allow:?}");
        }
        for allow in [Some("true"), Some("TRUE"), Some("")] {
            assert_eq!(refusal("4242", allow), None, "{allow:?}");
            assert_eq!(refusal("+4242", allow), None, "{allow:?}");
            assert_eq!(
                refusal("0", allow),
                Some((NS, "Invalid process identifier".to_string()))
            );
        }
        assert_eq!(
            refusal("abc", Some("true")),
            Some((NS, "Invalid process identifier: abc".to_string()))
        );
        assert_eq!(
            refusal("", None),
            Some((NS, "Invalid process identifier: ".to_string()))
        );
        assert_eq!(refusal("-7", Some("true")).map(|(c, _)| c), Some(NS));
        assert_eq!(
            refusal("99999999999", Some("true")).map(|(c, _)| c),
            Some(NS),
            "not an int"
        );
        assert_eq!(
            refusal("4243", Some("true")).map(|(c, _)| c),
            Some(NS),
            "another process"
        );
        assert_eq!(
            refusal("4243", None).map(|(c, _)| c),
            Some(NS),
            "another process"
        );
    }

    /// Interpreter round i2 wave 59, lane L1: under
    /// `-XX:+DisableAttachMechanism` this process and pid 0 answer HotSpot's
    /// `AttachNotSupportedException` whatever `jdk.attach.allowAttachSelf`
    /// says, after the id's own check (measured, `L1W59DisableAttachMechanism`).
    #[test]
    fn a_disabled_attach_mechanism_refuses_as_hotspot_does() {
        const NS: &str = "com/sun/tools/attach/AttachNotSupportedException";
        let disabled = Some((
            NS,
            "The VM does not support the attach mechanism".to_string(),
        ));
        for allow in [None, Some("true"), Some("false")] {
            assert_eq!(
                attach_refusal("4242", 4242, allow, true),
                disabled,
                "{allow:?}"
            );
            assert_eq!(
                attach_refusal("0", 4242, allow, true),
                disabled,
                "{allow:?}"
            );
            assert_eq!(
                attach_refusal("abc", 4242, allow, true),
                Some((NS, "Invalid process identifier: abc".to_string()))
            );
        }
        assert_eq!(attach_refusal("4242", 4242, Some("true"), false), None);
    }

    /// Interpreter round i3 wave 81, lane L1: a `load` reply body read as
    /// JDK 25's `HotSpotVirtualMachine.loadAgent` reads it, and the JPLIS
    /// option string it sends.
    #[test]
    fn a_jar_load_reply_maps_to_hotspots_client_exceptions() {
        const LOAD: &str = "com/sun/tools/attach/AgentLoadException";
        const INIT: &str = "com/sun/tools/attach/AgentInitializationException";
        assert_eq!(jar_load_reply_exception("return code: 0\n"), None);
        assert_eq!(jar_load_reply_exception("return code: 0"), None);
        assert_eq!(
            jar_load_reply_exception("return code: 100\n"),
            Some((
                LOAD,
                "Agent JAR not found or no Agent-Class attribute".to_string()
            ))
        );
        assert_eq!(
            jar_load_reply_exception("return code: 102\n"),
            Some((
                INIT,
                "Agent JAR loaded but agent failed to initialize".to_string()
            ))
        );
        assert_eq!(
            jar_load_reply_exception("return code: 7\n"),
            Some((LOAD, "Failed to load agent - unknown reason: 7".to_string()))
        );
        assert_eq!(
            jar_load_reply_exception(""),
            Some((LOAD, "Target VM did not respond".to_string()))
        );
        assert_eq!(
            jar_load_reply_exception("libx was not loaded.\nno such file\n"),
            Some((
                LOAD,
                "Failed to load agent library: libx was not loaded. no such file".to_string()
            ))
        );
        assert_eq!(jplis_attach_options("/d/a.jar", None), "/d/a.jar");
        assert_eq!(jplis_attach_options("/d/a.jar", Some("")), "/d/a.jar=");
        assert_eq!(
            jplis_attach_options("/d/a.jar", Some("x=y")),
            "/d/a.jar=x=y"
        );
        assert!(DYNAMIC_AGENT_LOADING_DISABLED.starts_with("Dynamic agent loading is not enabled."));
        assert_eq!(JPLIS_START_FAILURE_LINES[3], "Agent failed to start!");
    }

    fn fake_objref(addr: usize) -> ObjectRef {
        // Build a dummy ObjectRef from a raw addr. Only used inside
        // unit tests where we never dereference it.
        unsafe { std::mem::transmute::<usize, ObjectRef>(addr) }
    }

    // ---- GC-safety of the two agent-array loops (2026-09-06) ----

    /// The body of `fn <name>(` in THIS file, from the declaration to the
    /// first `\n}` in column 0 after it.
    ///
    /// Line endings are normalised first: `include_str!` embeds the file's raw
    /// bytes and this repository is checked out with CRLF on Windows, where
    /// every `contains` below would still work but the byte offsets used to
    /// order two needles would not.
    fn fn_body(name: &str) -> String {
        let src = include_str!("instrument.rs").replace("\r\n", "\n");
        let start = src
            .find(&format!("fn {name}("))
            .unwrap_or_else(|| panic!("{name} not found in instrument.rs"));
        let end = src[start..]
            .find("\n}")
            .map_or(src.len(), |i| start + i + 2);
        src[start..end].to_string()
    }

    /// Both loops walk an agent-supplied array while running agent bytecode
    /// and defining classes — that allocates, so a moving collector relocates
    /// the array mid-loop. Measured 2026-09-06: it moved before `i=1` in
    /// **60 of 60** processes, and roughly one in eight read a zero word out
    /// of the vacated address, silently dropped the rest of the hierarchy, and
    /// handed Mockito a mock that did not intercept.
    ///
    /// A source witness rather than a behavioural test because reproducing it
    /// needs a real agent, a real GC and luck: every in-process instrument
    /// that tried to observe it changed the allocation pattern and hid it. So
    /// this asserts the SHAPE that makes it impossible — pin, then re-read
    /// before every element access — and it fails if an edit drops either.
    #[test]
    fn the_agent_array_loops_pin_and_refresh_before_every_element_read() {
        for name in ["native_retransform_classes0", "native_redefine_classes0"] {
            let body = fn_body(name);
            assert!(
                body.contains("pin_native_root(arr)"),
                "{name} must pin the agent array before the loop"
            );
            assert!(
                body.contains("unpin_native_roots(arr_pin)"),
                "{name} must release the array pin"
            );
            let refresh = body
                .find("read_native_pin(arr_pin")
                .unwrap_or_else(|| panic!("{name} never re-reads the array from its pin"));
            let read = body
                .find("get_array_element(arr, i)")
                .unwrap_or_else(|| panic!("{name} no longer reads the array by index"));
            assert!(
                refresh < read,
                "{name} reads element i BEFORE refreshing `arr` from its pin — that is \
                 exactly the stale read this pin exists to stop"
            );
        }
    }

    /// The class reference read out of that array is itself carried across
    /// the transformer chain, so it needs the same treatment. Asserted
    /// separately because the array pin alone does not cover it: a relocation
    /// between the element read and the chain call moves the mirror, not the
    /// array.
    ///
    /// Both loops, because they are twins and the twin is where a fix stops
    /// being applied — `native_redefine_classes0` reaches the same chain with
    /// the same shape and had the same hazard.
    #[test]
    fn both_loops_refresh_the_class_reference_before_running_the_chain() {
        for (name, pin_call, refresh_call) in [
            (
                "native_retransform_classes0",
                "pin_native_root(mirror)",
                "read_native_pin(mirror_pin, mirror)",
            ),
            (
                "native_redefine_classes0",
                "pin_native_root(target_class)",
                "read_native_pin(class_pin, target_class)",
            ),
        ] {
            let body = fn_body(name);
            let pin = body
                .find(pin_call)
                .unwrap_or_else(|| panic!("{name}: the class reference must be pinned"));
            let refresh = body.find(refresh_call).unwrap_or_else(|| {
                panic!("{name}: the class reference must be re-read from its pin")
            });
            let chain = body
                .find("run_transformer_chain(")
                .unwrap_or_else(|| panic!("{name}: the transformer chain call is gone"));
            assert!(
                pin < refresh && refresh < chain,
                "{name}: pin, then refresh, then hand to the chain \
                 (pin={pin} refresh={refresh} chain={chain})"
            );
        }
    }

    /// The chain walk holds a SNAPSHOT of the transformer list across every
    /// `transform` call in it, and each of those runs Java. The canonical
    /// chain is a GC root and is remapped; the snapshot is not — see
    /// `a_snapshot_taken_before_a_relocation_keeps_the_old_address`. So each
    /// ref must be pinned and read back, and the `invoke_virtual` must use the
    /// value that came out of the pin rather than the one in the snapshot.
    ///
    /// One registered transformer hides this completely, which is why it has
    /// to be a witness: the common case (Mockito alone) never has a second
    /// entry to dispatch on.
    #[test]
    fn the_chain_walk_dispatches_on_a_pinned_transformer_not_the_snapshot() {
        let body = fn_body("run_chain_over_bytes");
        let pin = body
            .find("pin_native_root(entry.transformer_ref)")
            .expect("every transformer ref in the snapshot must be pinned");
        let read = body
            .find("read_native_pin(transformer_pins[slot], entry.transformer_ref)")
            .expect("each transformer ref must be read back through its pin");
        let call = body
            .find("invoke_virtual(transformer,")
            .expect("the dispatch must use the value read out of the pin");
        assert!(
            pin < read && read < call,
            "pin, then read back, then dispatch (pin={pin} read={read} call={call})"
        );
        assert!(
            !body.contains("invoke_virtual(entry.transformer_ref"),
            "the dispatch must not use the snapshot's raw ref"
        );
    }

    /// JVMTI `RetransformClasses` redefines each class ONCE, with the chain's
    /// output; the class keeps its current version while the transformers run
    /// on the base bytes. The swap back to the base that preceded the chain
    /// until interpreter round i1 wave 26 made every retransform two
    /// redefinitions (probe `L3/L3W25RetransformRunsTheBase`; see
    /// `interpreter-L3-retransform-redefines-each-class-twice-FIXED-20260928.md`
    /// before bringing it back).
    #[test]
    fn a_retransform_redefines_each_class_once() {
        let body = fn_body("native_retransform_classes0");
        // Since wave 27 the classes are installed together, after every
        // chain ran and every class was checked (`install_redefinitions`).
        assert!(
            !body.contains("retransform_class(") && !body.contains("redefine_class("),
            "the native redefines only through install_redefinitions"
        );
        assert_eq!(
            body.matches("install_redefinitions(").count(),
            1,
            "one install per retransform"
        );
        let chain = body
            .find("run_transformer_chain(")
            .expect("the transformer chain call is gone");
        let install = body
            .find("install_redefinitions(")
            .expect("the install call is gone");
        assert!(chain < install, "the chain runs before the only install");
        let install_body = fn_body("install_redefinitions");
        assert_eq!(
            install_body
                .matches("redefine_class_as_instrument(")
                .count(),
            1,
            "one redefinition per class"
        );
    }

    /// A refused redefinition throws what HotSpot throws (interpreter round
    /// i1 wave 27, lane L3; `L3/L3W27RedefinitionRefusals`): every class is
    /// checked before the first is redefined, and the throwable is built only
    /// after the batch closed and the array pin was released, so nothing the
    /// batch's handshake could move is held across it.
    #[test]
    fn both_natives_check_every_class_first_and_throw_the_refusal() {
        let install = fn_body("install_redefinitions");
        let check = install
            .find("check_class_redefinition(")
            .expect("install_redefinitions must check every class first");
        let redefine = install
            .find("redefine_class_as_instrument(")
            .expect("install_redefinitions must redefine");
        assert!(check < redefine, "check every class, then redefine");
        for name in ["native_retransform_classes0", "native_redefine_classes0"] {
            let body = fn_body(name);
            let end = body
                .find("end_redefinition_batch(batch)")
                .unwrap_or_else(|| panic!("{name}: the batch is never closed"));
            let unpin = body
                .find("unpin_native_roots(arr_pin)")
                .unwrap_or_else(|| panic!("{name}: the array pin is never released"));
            // The LAST throw: the first is the unmodifiable-class refusal,
            // taken before anything is pinned or opened.
            let throw = body
                .rfind("throw_redefinition_refusal(")
                .unwrap_or_else(|| panic!("{name}: a refusal is not thrown"));
            let unmodifiable = body
                .find("first_unmodifiable_class(")
                .unwrap_or_else(|| panic!("{name}: unmodifiable classes are not refused"));
            let pin = body
                .find("pin_native_root(arr)")
                .unwrap_or_else(|| panic!("{name}: the array is never pinned"));
            assert!(
                unmodifiable < pin,
                "{name}: refuse an unmodifiable class before pinning or running any chain"
            );
            assert!(
                end < throw && unpin < throw,
                "{name}: build the throwable after closing the batch and unpinning"
            );
            assert!(
                !body.contains("tracing::warn!(\"retransformClasses0: {msg}\")")
                    && !body.contains("tracing::warn!(\"redefineClasses0: {msg}\")"),
                "{name}: a refusal is thrown, not only logged"
            );
        }
    }

    /// gci i19/rv (`gci-i19rv-i18-default-path-review-findings-FIXED-20261014`
    /// finding 1): a heap-exhaustion unwind out of the install loop (the JVMTI
    /// contexts run under `native_oom::catch_alloc_oom_on` since gci i18/na,
    /// the natives under the funnel's scope) still lowers the install fence
    /// and releases the publication hold, and the JVMTI redefinition still
    /// closes its batch; the payload is re-raised afterwards.
    #[test]
    fn gci_i19rv_the_install_fence_and_the_batch_close_on_an_unwind() {
        let install = fn_body("install_redefinitions");
        let raised = install
            .find("let held = ctx.begin_redefinition_install(&classes);")
            .expect("the fence is raised once");
        let caught = install
            .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
            .expect("the install loop runs under a catch");
        let redefine = install
            .find("redefine_class_as_instrument(")
            .expect("the install loop redefines");
        let lowered = install
            .find("ctx.end_redefinition_install(held);")
            .expect("the fence is lowered");
        let resumed = install
            .find("std::panic::resume_unwind(payload)")
            .expect("the unwind is re-raised");
        assert!(raised < caught && caught < redefine && redefine < lowered && lowered < resumed);
        assert_eq!(install.matches("end_redefinition_install(").count(), 1);

        for name in ["redefine_classes_for_jvmti", "retransform_classes_for_jvmti"] {
            let jvmti = fn_body(name);
            let opened = jvmti
                .find("let batch = ctx.begin_redefinition_batch();")
                .unwrap_or_else(|| panic!("{name}: the batch is opened"));
            let caught = jvmti
                .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
                .unwrap_or_else(|| panic!("{name}: the chains and the install run under a catch"));
            let chain = jvmti
                .find("run_transformer_chain(")
                .unwrap_or_else(|| panic!("{name}: the chain runs"));
            let installs = jvmti
                .find("install_redefinitions(")
                .unwrap_or_else(|| panic!("{name}: the install runs"));
            let closed = jvmti
                .find("ctx.end_redefinition_batch(batch);")
                .unwrap_or_else(|| panic!("{name}: the batch is closed"));
            let resumed = jvmti
                .find("std::panic::resume_unwind(payload)")
                .unwrap_or_else(|| panic!("{name}: the unwind is re-raised"));
            assert!(opened < caught && caught < chain && chain < installs, "{name}");
            assert!(installs < closed && closed < resumed, "{name}");
        }
    }

    /// gci i19/rv finding 3: the two Java natives close their batch and
    /// release the array pin on an unwind out of a chain or the install too,
    /// then re-raise it.
    #[test]
    fn gci_i19rv_the_java_natives_close_their_batch_on_an_unwind() {
        for name in ["native_redefine_classes0", "native_retransform_classes0"] {
            let body = fn_body(name);
            let opened = body
                .find("let batch = ctx.begin_redefinition_batch();")
                .unwrap_or_else(|| panic!("{name}: the batch is opened"));
            let caught = body
                .find("std::panic::catch_unwind(std::panic::AssertUnwindSafe(||")
                .unwrap_or_else(|| panic!("{name}: the chains and the install run under a catch"));
            let chain = body
                .find("run_transformer_chain(")
                .unwrap_or_else(|| panic!("{name}: the chain runs"));
            let installs = body
                .find("install_redefinitions(")
                .unwrap_or_else(|| panic!("{name}: the install runs"));
            let closed = body
                .find("ctx.end_redefinition_batch(batch);")
                .unwrap_or_else(|| panic!("{name}: the batch is closed"));
            let unpinned = body
                .find("ctx.unpin_native_roots(arr_pin);")
                .unwrap_or_else(|| panic!("{name}: the array pin is released"));
            let resumed = body
                .find("std::panic::resume_unwind(payload)")
                .unwrap_or_else(|| panic!("{name}: the unwind is re-raised"));
            assert!(opened < caught && caught < chain && chain < installs, "{name}");
            assert!(installs < closed && closed < unpinned && unpinned < resumed, "{name}");
        }
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

    /// A `vm_identity` no other test and no real VM can collide with.
    ///
    /// Real identities come from `NEXT_VM_IDENTITY` (`vm/src/vm/vm_init.rs`),
    /// a counter starting at 1 -- not `SharedVm` addresses. This counter used
    /// to start at 1 too, so in this binary, where other test modules build
    /// and drop real `SharedVm`s in parallel, test identity N WAS real VM N:
    /// that VM's teardown (`release_vm_native_state` ->
    /// `forget_vm_transformers(N)`) wiped this test's chain mid-assertion, and
    /// while both lived that VM's class loads and root scans saw this test's
    /// fake `0x1000`-style transformer refs (gc-common w25-c). The base is
    /// far above any plausible allocation and distinct from the jvmti
    /// modules' bases (`0x7000_0000`, `0x7100_0000`, `0x7200_0000`). gcg g2/k:
    /// it now comes from the real counter itself (`vm_init::test_vm_identity`),
    /// which no real VM is ever handed again, so no base is needed.
    fn unique_vm() -> usize {
        crate::vm::vm_init::test_vm_identity()
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

    /// A snapshot taken BEFORE a collection is not repaired by it — which is
    /// the whole reason `run_chain_over_bytes` pins the refs it walks.
    ///
    /// `remap_transformer_refs` rewrites the CANONICAL chain; a
    /// `snapshot_transformer_chain` result is a `to_vec()` copy the collector
    /// has never heard of. The chain walk holds such a copy across every
    /// transformer's `transform` call, each of which runs Java and allocates,
    /// so without a pin the second and later entries would be dispatched on a
    /// dead address. That is the failure `scan_transformer_roots`'s own doc
    /// comment describes and that the canonical remap alone does not prevent.
    ///
    /// This asserts the staleness rather than the fix, deliberately: if the
    /// snapshot ever DOES get remapped this test fails and whoever made that
    /// true can retire the pins with evidence.
    #[test]
    fn a_snapshot_taken_before_a_relocation_keeps_the_old_address() {
        let vm = unique_vm();
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0x4000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        let stale = snapshot_transformer_chain(vm);

        let mut map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        map.insert(0x4000, 0xA000);
        remap_transformer_refs(vm, &map);

        assert_eq!(
            snapshot_transformer_chain(vm)[0].transformer_ref.as_ptr() as usize,
            0xA000,
            "the canonical chain must follow the relocation"
        );
        assert_eq!(
            stale[0].transformer_ref.as_ptr() as usize,
            0x4000,
            "a snapshot taken earlier is NOT rewritten — so a walk over one must \
             read its refs back through a pin, not use them raw"
        );
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

    /// gc-common w25-c: a test identity never names a real VM, so a real
    /// VM's teardown -- which every other test module in this binary runs,
    /// in parallel, on `NEXT_VM_IDENTITY` values counting from 1 -- cannot
    /// reach a test's chain. `unique_vm()` used to count from 1 as well.
    #[test]
    fn a_real_vms_teardown_never_reaches_a_test_identitys_chain() {
        let real = crate::vm::SharedVm::new(crate::config::VmConfig::default());
        let vm = unique_vm();
        assert_ne!(vm, real.vm_identity);
        assert!(
            vm > real.vm_identity,
            "test identities ({vm:#x}) come from the real counter, after {:#x}",
            real.vm_identity
        );
        add_transformer_entry(
            vm,
            TransformerEntry {
                transformer_ref: fake_objref(0xD000),
                can_retransform: true,
                native_method_prefix: None,
            },
        );
        // `Drop for SharedVm` runs `release_vm_native_state`, which calls
        // `forget_vm_transformers` for the real VM's identity only.
        drop(real);
        assert_eq!(transformer_count(vm), 1, "the test's row survives");
        forget_vm_transformers(vm);
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

    /// gc-common w5-e (`common-w4e-object-size-reports-legacy-footprint`): a
    /// footprint read from the object's header wins over the shape estimate.
    /// A compact class of eight `boolean`s is a 16-byte header plus an
    /// 8-byte packed body, not `16 + 8 * 16`.
    #[test]
    fn object_size_prefers_the_heap_footprint() {
        let compact = cratonvm_types::HEADER_SIZE + 8;
        assert_eq!(size_from_footprint(Some(compact)), Some(compact as i64));
        assert!(
            (compact as i64)
                < approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 8),
            "the legacy formula over-sizes the compact object"
        );
        assert_eq!(size_from_footprint(Some(0)), Some(1), "never zero");
        assert_eq!(size_from_footprint(Some(usize::MAX)), Some(i64::MAX));
        assert_eq!(size_from_footprint(None), None);
    }

    /// A context that cannot read a header (the mock's default `None`) keeps
    /// the legacy shape estimate, byte for byte.
    #[test]
    fn object_size_falls_back_to_the_shape_without_a_header() {
        let mut ctx = cratonvm_native_api::test_mock::MockNativeContext::new();
        let obj = cratonvm_native_api::NativeHeapAccess::alloc_object(&mut ctx, ClassId::new(7), 5);
        assert_eq!(
            object_size_of(&ctx, obj),
            approximate_object_size(ObjectKind::Object, ArrayElementType::Reference, 0, 5)
        );
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
            // Interpreter round i6 wave 119, lane L3.
            ("setHasTransformers", "(JZ)V"),
            ("setNativeMethodPrefixes", "(J[Ljava/lang/String;Z)V"),
        ] {
            assert!(
                r.find(impl_class, name, descriptor).is_some(),
                "{impl_class}.{name}{descriptor} is declared native by JDK 21 \
                 and 25 and must be registered"
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
                "{impl_class}.{name}{descriptor} has no leading `long jvmtienv` \
                 and is declared by no supported JDK image; registering it \
                 binds nothing and hides the arity that does"
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
                "{impl_class}.{name}{descriptor} is CratonVM's own entry point \
                 and must stay registered"
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
    // Identities come from the real counter (`unique_vm`, gcg g2/k) so these
    // never collide with a real VM identity or with each other under the test
    // harness's shared process; they were literals (`0x10ad_0001..5`).

    #[test]
    fn load_time_offer_is_claimed_exactly_once_per_name() {
        let vm = unique_vm();
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
        let a = unique_vm();
        let b = unique_vm();
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
        let vm = unique_vm();
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
        let vm = unique_vm();
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
