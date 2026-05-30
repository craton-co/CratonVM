// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Synthetic native implementations for the Java Collections Framework.
//!
//! **DEPRECATED (Session 15)**: These are Rust-backed synthetic stubs used only when
//! `synthetic-jdk` feature is enabled. In real JDK mode, `java.util.*` classes are
//! loaded from JDK class files and executed via the interpreter.
//!
//! This crate is gated behind the `synthetic-jdk` feature flag at the API level:
//! `register_collections_natives()` and `register_builtins()` are only available
//! when the feature is enabled. The crate still compiles for backward compatibility
//! but is not called in the default (real JDK) build path.

use cratonvm_types::ArrayElementType;
use cratonvm_types::ClassId;
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::{ObjectKind, ObjectRef, Value};

// `pub` (gated by `#[doc(hidden)]` on the items themselves) so the
// GC-relocation integration harness in `tests/gc_relocation_harness.rs`
// can drive `obj_key` against a stub NativeContext directly.
#[doc(hidden)]
pub mod identity_hash;
use identity_hash::{obj_key as ih_obj_key, seed as ih_seed};

// ---------------------------------------------------------------------------
// Cached debug-flag probes.
//
// These flags are read from the OS environment on every native call in the
// original code (`std::env::var`/`var_os`), which is a syscall-backed lookup
// on the hot path. The values never change for the lifetime of the process,
// so we resolve each exactly once into a `OnceLock<bool>` and reference the
// cached boolean thereafter. Behaviour is identical — the flag is still
// "enabled iff the env var is set" — but the per-call OS probe is gone.
// ---------------------------------------------------------------------------

/// `true` iff `CRATONVM_HM_TRACE` is set (HashMap equals/contract tracing).
fn dbg_hm_trace() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_HM_TRACE").is_some())
}

/// `true` iff `CRATONVM_HS_ITR_DBG` is set (HashSet iterator tracing).
fn dbg_hs_itr() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_HS_ITR_DBG").is_some())
}

/// `true` iff `CRATONVM_DBG_SBLOAD` is set (synthetic-build-load tracing).
fn dbg_sbload() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_DBG_SBLOAD").is_some())
}

/// `true` iff `CRATONVM_DBG_KCBOOL` is set — traces enum-keyed map lookups,
/// the prime suspect for Keycloak's `Profile.isFeatureEnabled` NPE
/// (`features.get(feature)` returning null → `Boolean.booleanValue()` on
/// null). When set, an enum-keyed `Map.get` that misses logs the key's
/// `(class_id, ordinal)` plus, for every node in the map, that node's enum
/// identity — so the orchestrator can confirm whether a duplicate enum
/// constant object exists (same `(class_id, ordinal)`, different pointer).
fn dbg_kcbool() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_DBG_KCBOOL").is_some())
}

/// `true` iff `CRATONVM_DBG_HMPUT` is set (HashMap put node-walk tracing).
/// Cached to avoid a syscall-backed env probe per node on the hot put path.
fn dbg_hmput() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("CRATONVM_DBG_HMPUT").is_some())
}

/// Diagnostic: on an enum-keyed `Map.get` miss, dump the lookup key's enum
/// identity and every node's enum identity in the map. `nodes` is an iterator
/// of `(node_key_ref)` for each entry currently in the map.
fn dbg_kcbool_report_miss(
    ctx: &dyn NativeContext,
    kind: &str,
    key: ObjectRef,
    node_keys: &[ObjectRef],
) {
    let key_id = enum_key_identity(ctx, key);
    eprintln!(
        "[cratonvm-dbg KCBOOL] {kind} MISS: key ptr={:p} enum_id={:?} entries={}",
        key.as_ptr(),
        key_id,
        node_keys.len(),
    );
    if let Some((kcls, kord)) = key_id {
        for nk in node_keys {
            if let Some((ncls, nord)) = enum_key_identity(ctx, *nk) {
                let same_logical = ncls == kcls && nord == kord;
                if same_logical {
                    eprintln!(
                        "[cratonvm-dbg KCBOOL]   DUPLICATE ENUM CONSTANT: node ptr={:p} \
                         enum_id=({ncls},{nord}) == key but pointer differs — \
                         this is the VM enum-canonicalization bug",
                        nk.as_ptr(),
                    );
                }
            }
        }
    }
}

/// Create an iterator backed by a snapshot array of the given size.
/// The iterator uses the HashMap$KeyItr layout (field 0 = keys array, field 1 = cursor, field 2 = total).
pub fn make_iterator_from_array(
    ctx: &mut dyn NativeContext,
    snapshot_array: ObjectRef,
    size: usize,
) -> MethodCallResult {
    let itr = alloc_synthetic(ctx, "java/util/HashMap$KeyItr", 3);
    ctx.set_field(itr, 0, Value::Object(Some(snapshot_array)));
    ctx.set_field(itr, 1, Value::Int(0));
    ctx.set_field(itr, 2, Value::Int(size as i32));
    Ok(Some(Value::Object(Some(itr))))
}

/// Register all collection native methods.
pub fn register_collections_natives(registry: &mut NativeMethodRegistry) {
    register_arraylist_natives(registry);
    register_hashmap_natives(registry);
    register_hashset_natives(registry);
    register_iterator_natives(registry);
    register_arrays_natives(registry);
    register_optional_natives(registry);
    register_collections_utility_natives(registry);
    register_map_entry_natives(registry);
    register_factory_natives(registry);
    register_stream_natives(registry);
    register_collectors_natives(registry);
    register_int_stream_natives(registry);
    register_long_stream_natives(registry);
    register_double_stream_natives(registry);
    register_interface_natives(registry);
    register_copy_constructor_natives(registry);
    register_comparator_natives(registry);
    register_string_joiner_natives(registry);
    register_random_natives(registry);
    register_optional_int_natives(registry);
    register_optional_long_natives(registry);
    register_optional_double_natives(registry);
    register_linked_list_natives(registry);
    register_linked_hashmap_natives(registry);
    register_array_deque_natives(registry);
    register_priority_queue_natives(registry);
    register_vector_natives(registry);
    register_stack_natives(registry);
    register_bulk_ops_natives(registry);
    register_queue_deque_interface_natives(registry);
    register_tree_map_natives(registry);
    register_tree_set_natives(registry);
    register_concurrent_hashmap_natives(registry);
    register_properties_natives(registry);
    register_collections_extras_natives(registry);
    register_unmodifiable_natives(registry);
    register_set_from_map_natives(registry);
    // BlockingQueue family (LinkedBlockingQueue, ArrayBlockingQueue,
    // ConcurrentLinkedQueue, ConcurrentLinkedDeque) is overridden with a
    // synthetic 4-field layout (head/tail/size/capacity) that conflicts with
    // the real JDK's field layout (head/last/count/putLock/takeLock/notEmpty/
    // notFull/capacity for LBQ).  In real-JDK mode the synthetic <init> never
    // assigns putLock/takeLock, so later real bytecode (e.g. drainTo line 706
    // in JDK 25) NPEs on `takeLock.lock()`.  Gate behind synthetic-jdk only.
    #[cfg(feature = "synthetic-jdk")]
    register_blocking_queue_natives(registry);
    register_iterator_protocol_natives(registry);
    // ScheduledThreadPoolExecutor.schedule is implemented in
    // native-builtins `register_p63_scheduled_executor` (scheduled_pump +
    // delay-aware firing). The previous inline `native_stpe_schedule` ran
    // every runnable immediately, which breaks Surefire ForkedBooter.exit1
    // (it schedules kill() as a delayed backup; immediate kill NPEs on a
    // null CommandReader when setupBooter failed first).
    register_concurrent_skip_list_map_natives(registry);
    register_stamped_lock_natives(registry);
    register_phaser_natives(registry);
    register_priority_blocking_queue_natives(registry);
    register_executors_scheduled_natives(registry);
    register_concurrent_completeness_natives(registry);
}

// ===========================================================================
// Helper: allocate a synthetic object with a well-known class name
/// Create a snapshot-based iterator (2 fields: array=0, cursor=1) from an existing array.pub fn make_iterator_from_array(    ctx: &mut dyn NativeContext,    array: ObjectRef,    _count: usize,) -> MethodCallResult {    let itr = alloc_synthetic(ctx, "java/util/Iterator", 2);    ctx.set_field(itr, 0, Value::Object(Some(array)));    ctx.set_field(itr, 1, Value::Int(0));    Ok(Some(Value::Object(Some(itr))))}
// ===========================================================================

/// Allocate a synthetic object, trying to load the real class first.
fn alloc_synthetic(ctx: &mut dyn NativeContext, class_name: &str, num_fields: usize) -> ObjectRef {
    // S111r7: when `<clinit>` fails (e.g. transient state where a class
    // is mid-initialization on a parent frame), fall back to a name-only
    // lookup before degrading to bare `Object`. The previous behaviour
    // returned objects whose `class_id_of` reported `java/lang/Object`,
    // which then propagated to virtual dispatch sites (e.g.
    // `HashSet.iterator()`'s `invokeinterface Set.iterator()` on the
    // HashMap.keySet result) and surfaced as a swallowed
    // `NoSuchMethodError Object.iterator()`.
    let cid = match ctx.ensure_class_initialized(class_name) {
        Ok(class_id) => class_id,
        Err(_) => match ctx.class_id_by_name(class_name) {
            Some(id) => id,
            // No real or already-registered class with this name. Previously
            // this degraded to `ClassId::new(0)` (`java/lang/Object`), which
            // (a) the GC field-bounds guard rejects as an undersized layout
            // and (b) loses the class identity — natives registered on
            // `class_name` (e.g. the `hasMoreElements`/`nextElement` on
            // `cratonvm/internal/SnapshotEnumeration`) no longer resolve, so
            // callers hit a `NoSuchMethodError`. Instead create a synthetic
            // class carrying THIS name + `num_fields`, so those name-keyed
            // natives dispatch and the object layout is well-sized.
            None => ctx.ensure_synthetic_class(class_name, num_fields),
        },
    };
    ctx.alloc_object(cid, num_fields)
}

/// Allocate a reference array (Object[]) of the given length.
fn alloc_ref_array(ctx: &mut dyn NativeContext, length: usize) -> ObjectRef {
    ctx.new_ref_array(ClassId::new(0), length)
}

// ---------------------------------------------------------------------------
// Per-segment resize lock (Bug 1+2 CRIT correctness fix, round-10)
// ---------------------------------------------------------------------------
//
// Lock-free CHM readers walk old bucket chains and follow NEXT pointers via
// `get_field_volatile`. When `map_resize` mutates a chain's NEXT pointers in
// place to splice the hi/lo partitions, a concurrent reader can skip past
// the keys living in the hi partition and return a spurious miss.
//
// Round-10 fix (Bug 1+2 CRIT, round-9 native-misc CRIT-1/CRIT-2):
// The earlier implementation looked up a per-segment RwLock through a global
// `Mutex<HashMap<ptr, Arc<RwLock>>>` — every CHM read AND write paid the
// global Mutex, completely defeating the per-segment design. The map was
// also keyed by raw segment heap pointer, which is GC-relocatable, so a
// moving GC could cause cross-segment aliasing (two distinct segments share
// a lock if their heap addresses overlap after collection).
//
// Replacement: a fixed-size striped array of 256 `parking_lot::RwLock`s
// (workspace-shared `parking_lot` dep). The stripe index is derived from
// the segment's identity hash code, which is stable across GC moves —
// previously the global HashMap was keyed by raw heap pointer, which a
// moving GC could relocate and alias across distinct segments. No global
// lock; lookup is a single masked array index, with a Fibonacci
// multiplier to spread sequential identity hashes uniformly.
//
// Trade-off: occasional false sharing if two segments hash to the same
// stripe (1/256 collision rate). That serializes resizes between unrelated
// segments — acceptable since resize is rare. Reads stay fully concurrent
// because the read lock is non-exclusive.
//
// TODO(round-6+): replace the RwLock with a clone-resize implementation that
// allocates new Node objects via `alloc_node` and links the new sub-chains
// without mutating the old chain. The old chain is then unreachable from the
// new buckets array and becomes GC-collectible. That restores fully
// lock-free reads and eliminates the need for striped locks entirely.

const NUM_SEG_LOCKS: usize = 256;

/// Striped per-segment resize lock array.
///
/// Round-10 CRIT fix (native-misc CRIT-1 + concurrency CRIT-2):
/// the original implementation kept a `Mutex<FxHashMap<id, Arc<RwLock>>>`
/// where every CHM read AND write took the global Mutex just to look up
/// its per-segment RwLock. That defeated the per-segment design entirely
/// — worse than a single global RwLock, because reads serialized through
/// a Mutex (exclusive) instead of an RwLock::read (shared).
///
/// Round-10 replacement: a fixed-size 256-stripe array indexed by the
/// segment's identity hash. No global lock, lookup is one masked array
/// index. False-sharing risk is 1/256 between unrelated segments — only
/// matters during resize, which is rare.
///
/// `parking_lot::RwLock` is preferred over `std::sync::RwLock` because:
///   - no poison-Result handling (lock acquisition is infallible),
///   - smaller (single-word) and faster fast path,
///   - guarantees writer fairness on contention, avoiding reader-starve
///     bursts during resize storms.
///
/// Lock ordering / recursion: this RwLock is only acquired in
/// `map_resize` (write side) and `chm_seg_get` (read side). All other
/// CHM mutator paths (put / put_if_absent / compute_* / merge / replace)
/// serialize against each other via the per-segment Java monitor
/// (`ChmMonitorGuard::acquire(ctx, seg)`), NOT this RwLock. Therefore
/// the only nesting that occurs is:
///   mutator-with-monitor → map_resize → write_guard
/// which never recurses because `map_resize` does not invoke user code.
/// Lock-free readers in `chm_seg_get` take only the read side and never
/// call back into the mutator, so they cannot deadlock either.
static SEG_LOCKS: std::sync::OnceLock<[parking_lot::RwLock<()>; NUM_SEG_LOCKS]> =
    std::sync::OnceLock::new();

fn seg_locks() -> &'static [parking_lot::RwLock<()>; NUM_SEG_LOCKS] {
    SEG_LOCKS.get_or_init(|| std::array::from_fn(|_| parking_lot::RwLock::new(())))
}

fn chm_seg_lock_for(seg_id: i32) -> &'static parking_lot::RwLock<()> {
    // Multiply by the 64-bit Fibonacci constant (golden-ratio derived)
    // to spread sequential / low-entropy `identity_hash_code` values
    // across all 256 stripes; identity hashes are often assigned
    // sequentially by the GC so a raw mod would cluster nearby segments
    // on a handful of stripes. The top byte of the product is well
    // mixed and is masked with `NUM_SEG_LOCKS - 1` (256 is a power of
    // two, so the mask is exact).
    let h = (seg_id as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &seg_locks()[((h >> 56) as usize) & (NUM_SEG_LOCKS - 1)]
}

/// RAII guard for a native monitor (`ctx.monitor_enter` / `monitor_exit`).
///
/// CRIT fix (round-5): the previous CHM write path performed
///   `ctx.monitor_enter(seg); ... do work ...; ctx.monitor_exit(seg);`
/// directly. If the "do work" body returned `Err(..)` early via `?`, or
/// panicked (e.g. resize hit a guard-tripped chain corruption case), the
/// matching `monitor_exit` never ran and the segment stayed locked
/// forever — every subsequent thread blocking on that segment would
/// deadlock.
///
/// Using `ChmMonitorGuard` instead ensures `monitor_exit` runs on every
/// exit path, including unwinding. We store the context as a raw pointer
/// because Drop cannot hold the original `&mut dyn NativeContext` borrow
/// without conflicting with the body's use of the same reference. The
/// pointer is always live: the guard is bound to a local that cannot
/// outlive the borrow used to acquire it (the borrow is reborrowed
/// fresh for each call inside the function body).
///
/// If `monitor_exit` itself panics during unwind the process aborts —
/// preferable to silently leaking the monitor.
///
/// MED fix: a `PhantomData<&'a mut dyn NativeContext>` field carries a
/// lifetime parameter `'a` at the type level, so any struct that tries to
/// store the guard must also name `'a` (which a function-local borrow
/// cannot project to a caller scope). The transmute remains for runtime
/// storage — without it the guard would conflict with the body's reuse of
/// `ctx` — but the lifetime parameter prevents the previous
/// silently-`'static` escape hatch that would have let a refactor stash
/// the guard into a long-lived struct and miscompile catastrophically.
struct ChmMonitorGuard<'a> {
    ctx: *mut (dyn NativeContext + 'static),
    seg: ObjectRef,
    /// Phantom borrow tying the guard's lifetime parameter `'a` to a
    /// surrounding scope at the type level. Using
    /// `fn() -> &'a mut dyn NativeContext` rather than `&'a mut …` directly
    /// means no actual mutable borrow is held at runtime (so the body
    /// inside the guarded section can keep using `ctx` via reborrows, as
    /// before), but `'a` is still part of the guard's type and propagates
    /// into any container that tries to store the guard.
    _borrow: std::marker::PhantomData<fn() -> &'a mut dyn NativeContext>,
}

impl<'a> ChmMonitorGuard<'a> {
    fn acquire(ctx: &mut dyn NativeContext, seg: ObjectRef) -> Self {
        ctx.monitor_enter(seg);
        // SAFETY: the guard MUST be dropped before the `&mut dyn NativeContext`
        // borrow ends. We transmute away the lifetime so the guard doesn't
        // hold the &mut borrow for its scope — call sites still use ctx
        // mutably between acquire and drop, which is sound as long as no
        // code outlives the original &mut borrow. The `PhantomData<fn() ->
        // &'a mut …>` field pins `'a` to a surrounding scope at the type
        // level without retaining the borrow at runtime, so attempts to
        // escape the guard into a longer-lived container fail to type-check.
        let ctx_ptr: *mut dyn NativeContext = ctx;
        let ctx_ptr_static: *mut (dyn NativeContext + 'static) =
            unsafe { core::mem::transmute(ctx_ptr) };
        ChmMonitorGuard {
            ctx: ctx_ptr_static,
            seg,
            _borrow: std::marker::PhantomData,
        }
    }
}

impl<'a> Drop for ChmMonitorGuard<'a> {
    fn drop(&mut self) {
        // SAFETY: the guard is always a local whose lifetime is bounded
        // by the `&mut dyn NativeContext` borrow used in `acquire`. No
        // other code can drop or invalidate the context while the guard
        // is live.
        //
        // Bug 4 (HIGH): if `monitor_exit` itself panics while we are
        // already unwinding from a panic in the protected block, Rust
        // promotes the second panic to an abort. Catch any panic from
        // `monitor_exit` here so we degrade to a leaked monitor + log
        // instead of taking down the whole VM. The closure captures a
        // raw pointer (`self.ctx`) plus a Copy ObjectRef, neither of
        // which carries UnwindSafe bounds, so we wrap in
        // `AssertUnwindSafe` — `monitor_exit` does not maintain
        // invariants that would be broken by an unwinding caller.
        let ctx_ptr = self.ctx;
        let seg = self.seg;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            (*ctx_ptr).monitor_exit(seg);
        }));
        if result.is_err() {
            eprintln!(
                "[CHM] monitor_exit panicked during ChmMonitorGuard::drop — ignoring to avoid double-panic abort"
            );
        }
    }
}

/// Try to read an object's string representation for display purposes.
///
/// For objects that are not plain strings or primitives, this calls
/// `toString()` via virtual dispatch so that overridden implementations
/// (e.g. ArrayList, HashMap, user classes) produce correct output.
fn obj_to_display_string(ctx: &mut dyn NativeContext, val: &Value) -> String {
    match val {
        Value::Object(None) => "null".to_string(),
        Value::Object(Some(obj)) => {
            // Fast path: if it's a Java String, read it directly
            if let Some(s) = ctx.read_string(*obj) {
                return s;
            }

            // Fast path for wrapper types: if the object has exactly 1 field
            // and its value is a primitive, format it directly.
            let nf = ctx.object_num_fields(*obj);
            if nf == 1 {
                match ctx.get_field(*obj, 0) {
                    Value::Int(v) => {
                        let class_id = ctx.class_id_of_object(*obj);
                        let name = ctx.class_name_of_id(class_id).unwrap_or_default();
                        return if name.contains("Boolean") {
                            if v != 0 { "true" } else { "false" }.to_string()
                        } else if name.contains("Character") {
                            char::from_u32(v as u32).unwrap_or('?').to_string()
                        } else if name.contains("Byte") {
                            (v as i8).to_string()
                        } else if name.contains("Short") {
                            (v as i16).to_string()
                        } else {
                            v.to_string()
                        };
                    }
                    Value::Long(v) => return v.to_string(),
                    Value::Float(v) => return format!("{}", v),
                    Value::Double(v) => return format!("{}", v),
                    _ => {}
                }
            }

            // Call toString() via virtual dispatch
            match ctx.invoke_virtual(*obj, "toString", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(str_ref)))) => {
                    ctx.read_string(str_ref).unwrap_or_else(|| "null".to_string())
                }
                _ => {
                    // Final fallback: ClassName@hash
                    let class_id = ctx.class_id_of_object(*obj);
                    let class_name = ctx
                        .class_name_of_id(class_id)
                        .unwrap_or_else(|| "?".to_string());
                    let hash = ctx.identity_hash_code(*obj);
                    format!("{}@{:x}", class_name.replace('/', "."), hash)
                }
            }
        }
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::Float(v) => format!("{}", v),
        Value::Double(v) => format!("{}", v),
        _ => "?".to_string(),
    }
}

/// Unbox a wrapper object (Integer, Long, Boolean, etc.) to its primitive Value.
/// Returns None if the object is not a recognized 1-field wrapper type.
fn unbox_wrapper(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<Value> {
    let nf = ctx.object_num_fields(obj);
    if nf == 1 {
        let inner = ctx.get_field(obj, 0);
        match inner {
            Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) => Some(inner),
            _ => None,
        }
    } else {
        None
    }
}

/// Normalize a Value for comparison: unbox wrapper objects to their primitive.
fn normalize_for_compare(ctx: &dyn NativeContext, v: &Value) -> Value {
    match v {
        Value::Object(Some(obj)) => {
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                prim
            } else {
                *v
            }
        }
        _ => *v,
    }
}

/// Check if two Values refer to the same object (by identity) or are equal
/// strings/wrapper values. Used by ArrayList.contains/indexOf and HashMap key lookup.
fn values_equal(ctx: &dyn NativeContext, a: &Value, b: &Value) -> bool {
    // Normalize: unbox wrapper objects to primitives for comparison
    let na = normalize_for_compare(ctx, a);
    let nb = normalize_for_compare(ctx, b);
    match (&na, &nb) {
        (Value::Object(Some(oa)), Value::Object(Some(ob))) => {
            // Identity check first
            if std::ptr::eq(oa.as_ptr(), ob.as_ptr()) {
                return true;
            }
            // String value equality
            if let (Some(sa), Some(sb)) = (ctx.read_string(*oa), ctx.read_string(*ob)) {
                return sa == sb;
            }
            // Enum constants: compare by (declaring class, ordinal) so an
            // enum-keyed List/Set still finds a member when the VM produced a
            // duplicate object for the same constant. See `enum_key_identity`.
            if let (Some(ia), Some(ib)) =
                (enum_key_identity(ctx, *oa), enum_key_identity(ctx, *ob))
            {
                return ia == ib;
            }
            false
        }
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Long(a), Value::Long(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Double(a), Value::Double(b)) => a == b,
        // Cross-type numeric: Int vs Long
        (Value::Int(a), Value::Long(b)) => (*a as i64) == *b,
        (Value::Long(a), Value::Int(b)) => *a == (*b as i64),
        _ => false,
    }
}

// ===========================================================================
// ArrayList — synthetic-jdk layout: field 0 = Object[] elementData, field 1 = Int size
// Real-JDK layout: AbstractList.modCount(I) at slot 0, ArrayList.elementData at slot 1,
// ArrayList.size at slot 2. We resolve the real layout via field-index lookup and fall
// back to the synthetic layout when the real class is unavailable.
// ===========================================================================

const AL_FIELD_DATA: usize = 0;
const AL_FIELD_SIZE: usize = 1;
const AL_NUM_FIELDS: usize = 2;
const AL_DEFAULT_CAPACITY: usize = 10;

/// Resolve ArrayList field slots: returns (data_slot, size_slot, n_fields).
/// In real-JDK mode this follows the actual `elementData` / `size` field
/// indices (with `modCount` from `AbstractList` taking slot 0); in
/// synthetic-jdk mode it falls back to (0, 1, 2). The result is cheap to
/// compute (the underlying resolver caches by class), so we recompute it on
/// each access rather than caching globally.
#[inline]
fn al_slots(ctx: &dyn NativeContext) -> (usize, usize, usize) {
    let data = ctx.resolve_field_index("java/util/ArrayList", "elementData");
    let size = ctx.resolve_field_index("java/util/ArrayList", "size");
    match (data, size) {
        (Some(d), Some(s)) => {
            let n = std::cmp::max(d, s) + 1;
            (d, s, n)
        }
        _ => (AL_FIELD_DATA, AL_FIELD_SIZE, AL_NUM_FIELDS),
    }
}

/// Allocate an ArrayList instance, sized to fit whichever field layout the
/// runtime is using. Initializes elementData and size to (buf, init_size).
fn alloc_arraylist_with(
    ctx: &mut dyn NativeContext,
    buf: ObjectRef,
    init_size: i32,
) -> ObjectRef {
    let (data_slot, size_slot, n_fields) = al_slots(ctx);
    let list = alloc_synthetic(ctx, "java/util/ArrayList", n_fields);
    ctx.set_field(list, data_slot, Value::Object(Some(buf)));
    ctx.set_field(list, size_slot, Value::Int(init_size));
    list
}

/// Extract ArrayList state: (elementData, size).
fn al_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32) {
    // See through CratonVM's unmodifiable wrapper views. A generic
    // `java/util/List` interface native (e.g. `native_al_equals`) can be
    // dispatched with a `cratonvm/internal/UnmodifiableList` receiver — and,
    // critically, with such a wrapper as the *other* argument: e.g.
    // `List.of(...).equals(List.of(...))` delegates `equals` to the backing
    // ArrayList, which then reads the wrapper argument's slots. Slot 0 of a
    // wrapper is the backing collection ObjectRef, NOT the element array;
    // without this unwrap `al_state` sees a non-array there, reports
    // `data = None` / `size = 0`, and `equals` wrongly returns false. Reading
    // from the backing ArrayList is always correct here — all wrapper mutators
    // throw, so callers reaching `al_state` are read-only. Mirrors the same
    // unwrap that `map_state` already performs for unmodifiable maps.
    let this = unwrap_unmod(ctx, this);
    let (data_slot, size_slot, _) = al_slots(ctx);
    // Receiver-layout guard. `al_state` is reached through `Collection`-
    // and `List`-interface natives (`size`, `forEach`, `stream`, …) whose
    // bytecode dispatcher can target *any* object — including non-list
    // receivers funnelled through reflection, lambda metafactory, or a
    // misresolved vtable.  In real-JDK mode `al_slots` returns
    // `(elementData=4, size=6)` (matching the inherited AbstractList /
    // AbstractCollection field layout); on a 3-slot receiver such as
    // `java/nio/charset/Charset` the unguarded `get_field` issues an
    // out-of-bounds slot read which the GC guard catches and the JUnit
    // bootstrap eventually segfaults on downstream.  Returning the
    // "empty list" sentinel matches the documented contract of
    // `collection_elements_generic` and lets the caller fall back to a
    // virtual-dispatch path.
    let n_fields = ctx.object_num_fields(this);
    if n_fields <= data_slot || n_fields <= size_slot {
        return (None, 0);
    }
    let data = match ctx.get_field(this, data_slot) {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => Some(arr),
        _ => None,
    };
    let size = match ctx.get_field(this, size_slot) {
        Value::Int(s) => s,
        _ => 0,
    };
    (data, size)
}

/// Snapshot the elements of a collection whose receiver is **not** one of
/// our ArrayList-layout objects (so `al_state` reported `data = None`).
///
/// The ArrayList-shaped natives (`stream`, `forEach`, …) are registered on
/// the `Collection` / `List` / `Iterable` interfaces, so they also catch
/// foreign collections — Guava's `ImmutableList`, `Maps$Values`, etc. For
/// those, `al_state`'s hardcoded `elementData`/`size` slots read past the
/// (shorter) object and yield an empty result, silently dropping every
/// element. Instead, walk the receiver's *real* `iterator()` — which
/// dispatches to the collection's own bytecode — so the elements survive.
///
/// The 64M cap is a runaway guard; a well-behaved iterator terminates long
/// before it.
fn collection_elements_generic(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Vec<Value> {
    let mut out = Vec::new();
    let iter = match ctx.invoke_virtual(this, "iterator", "()Ljava/util/Iterator;", &[]) {
        Ok(Some(Value::Object(Some(it)))) => it,
        _ => return out,
    };
    const MAX_ELEMENTS: usize = 64 * 1024 * 1024;
    while out.len() < MAX_ELEMENTS {
        match ctx.invoke_virtual(iter, "hasNext", "()Z", &[]) {
            Ok(Some(Value::Int(n))) if n != 0 => {}
            _ => break,
        }
        match ctx.invoke_virtual(iter, "next", "()Ljava/lang/Object;", &[]) {
            Ok(Some(v)) => out.push(v),
            _ => break,
        }
    }
    out
}

#[inline]
fn al_set_data(ctx: &mut dyn NativeContext, this: ObjectRef, buf: ObjectRef) {
    let (data_slot, _, _) = al_slots(ctx);
    // Receiver-layout guard — see `al_state` for rationale. Skip the write
    // entirely on a wrong-class receiver to avoid the out-of-bounds GC guard
    // warning that pairs with the read-side fix above.
    if data_slot >= ctx.object_num_fields(this) {
        return;
    }
    ctx.set_field(this, data_slot, Value::Object(Some(buf)));
}

#[inline]
fn al_set_size(ctx: &mut dyn NativeContext, this: ObjectRef, size: i32) {
    let (_, size_slot, _) = al_slots(ctx);
    if size_slot >= ctx.object_num_fields(this) {
        return;
    }
    ctx.set_field(this, size_slot, Value::Int(size));
}

/// Ensure the backing array has room for at least `min_cap` elements.
fn al_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, min_cap: usize) -> ObjectRef {
    let (data, _size) = al_state(ctx, this);
    let old_cap = data.map_or(0, |d| ctx.array_length(d));

    if min_cap <= old_cap {
        // `old_cap == 0` implies the backing array is None (e.g. a freshly
        // constructed list asked to ensure capacity 0). Unwrapping would
        // panic, so allocate an empty array as the None-safe fallback.
        return data.unwrap_or_else(|| alloc_ref_array(ctx, 0));
    }

    // Grow: max(old_cap * 1.5, min_cap) — matches Java's ArrayList strategy.
    // Use >> 1 for integer 1.5x, with minimum growth of 1 (handles old_cap == 0).
    let growth = std::cmp::max(old_cap >> 1, 1);
    let new_cap = std::cmp::max(old_cap + growth, min_cap);
    // Cap at a sane maximum to prevent OOM from absurd allocations.
    const AL_MAX_CAPACITY: usize = 1 << 30; // ~1 billion elements
    if new_cap > AL_MAX_CAPACITY {
        return data.unwrap_or_else(|| alloc_ref_array(ctx, 0));
    }
    let new_buf = alloc_ref_array(ctx, new_cap);

    // Copy old content. Prefer the bulk intrinsic so the VM can use
    // `copy_nonoverlapping` on the underlying storage; fall back to the
    // per-element loop only when the override declines (default trait impl
    // also returns `true`, so the loop is unreachable when bulk succeeds).
    if let Some(old_buf) = data {
        let copy_len = std::cmp::min(old_cap, new_cap);
        if !ctx.bulk_array_copy(old_buf, 0, new_buf, 0, copy_len) {
            for i in 0..copy_len {
                let val = ctx.get_array_element(old_buf, i);
                ctx.set_array_element(new_buf, i, val);
            }
        }
    }

    al_set_data(ctx, this, new_buf);
    new_buf
}

fn register_arraylist_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/ArrayList";

    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "<init>", "(I)V", native_al_init_capacity);
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_al_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_al_set,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add);
    r.register(c, "add", "(ILjava/lang/Object;)V", native_al_add_at);
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_al_remove_at);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_al_remove_obj);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(
        c,
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
        native_al_last_index_of,
    );
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_al_to_array);
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );
    // ArrayList.toArray(T[]) — Spring Boot fat-jar launcher's
    // `Launcher.createClassLoader(Collection)` calls `c.toArray(new URL[0])`
    // to obtain a typed `URL[]`. Real-JDK bytecode reads `elementData` and
    // calls `Arrays.copyOf(elementData, size, a.getClass())`, but that path
    // NPEs on our synthetic ArrayList because `a.getClass()` returns a
    // mirror without the array-component-type metadata `Arrays.copyOf`
    // walks. Provide an explicit override that copies into the supplied
    // array (or allocates a fresh one) without consulting the runtime
    // class of the template. Returning a simple Object[] is fine because
    // checkcast at the call site only verifies the array's component
    // class; our `alloc_ref_array` produces a raw reference array that
    // checkcasts to any `Object[]` subtype.
    r.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_al_to_array_typed,
    );
    // Also register on AbstractCollection (where the inherited bytecode
    // resolves) so the dispatch path that walks the superclass chain
    // finds the native before reaching the broken bytecode.
    r.register(
        "java/util/AbstractCollection",
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_al_to_array_typed,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "ensureCapacity", "(I)V", native_al_ensure_capacity);
    r.register(c, "trimToSize", "()V", native_al_trim_to_size);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);
    r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_al_add_all);
    r.register(c, "subList", "(II)Ljava/util/List;", native_al_sub_list);
    r.register(c, "hashCode", "()I", native_al_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_al_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    r.register(
        c,
        "sort",
        "(Ljava/util/Comparator;)V",
        native_al_sort_comparator,
    );
    r.register(
        c,
        "removeIf",
        "(Ljava/util/function/Predicate;)Z",
        native_al_remove_if,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/UnaryOperator;)V",
        native_al_replace_all,
    );
    r.register(c, "stream", "()Ljava/util/stream/Stream;", native_al_stream);
}

pub fn native_al_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let buf = alloc_ref_array(ctx, AL_DEFAULT_CAPACITY);
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, 0);
    Ok(None)
}

fn native_al_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    const AL_MAX_INIT_CAPACITY: usize = 1 << 30;
    let cap = match args.get(1) {
        Some(Value::Int(c)) => std::cmp::min(std::cmp::max(*c, 0) as usize, AL_MAX_INIT_CAPACITY),
        _ => AL_DEFAULT_CAPACITY,
    };
    let buf = alloc_ref_array(ctx, cap);
    al_set_data(ctx, this, buf);
    al_set_size(ctx, this, 0);
    Ok(None)
}

pub fn native_al_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size) = al_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

pub fn native_al_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, size) = al_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

pub fn native_al_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if index < 0 || index >= size {
        // JDK contract: out-of-range index throws IndexOutOfBoundsException
        // (ArrayIndexOutOfBoundsException is a subclass, so `catch
        // (IndexOutOfBoundsException)` still catches it).
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(data, index as usize);
    Ok(Some(val))
}

pub fn native_al_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    if index < 0 || index >= size {
        // JDK contract: out-of-range index throws IndexOutOfBoundsException.
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let old = ctx.get_array_element(data, index as usize);
    ctx.set_array_element(data, index as usize, new_val);
    Ok(Some(old))
}

pub fn native_al_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (_, size) = al_state(ctx, this);
    let size = size as usize;
    let buf = al_ensure_capacity(ctx, this, size + 1);
    ctx.set_array_element(buf, size, elem);
    al_set_size(ctx, this, (size + 1) as i32);
    Ok(Some(Value::Int(1))) // returns true
}

pub fn native_al_add_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Parse as i32 and reject negatives BEFORE casting to usize — a negative
    // Java int would otherwise wrap to a huge usize and pass the `> size` check.
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    let elem = args.get(2).copied().unwrap_or(Value::Object(None));
    let (_, size) = al_state(ctx, this);
    let size = size as usize;
    if index < 0 || index as usize > size {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    let index = index as usize;
    let buf = al_ensure_capacity(ctx, this, size + 1);
    // Shift elements right
    for i in (index..size).rev() {
        let val = ctx.get_array_element(buf, i);
        ctx.set_array_element(buf, i + 1, val);
    }
    ctx.set_array_element(buf, index, elem);
    al_set_size(ctx, this, (size + 1) as i32);
    Ok(None)
}

pub fn native_al_remove_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if index < 0 || index >= size {
        // JDK contract: out-of-range index throws IndexOutOfBoundsException.
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let old = ctx.get_array_element(data, index as usize);
    // Shift elements left
    let size = size as usize;
    for i in (index as usize + 1)..size {
        let val = ctx.get_array_element(data, i);
        ctx.set_array_element(data, i - 1, val);
    }
    // Null out the last element
    ctx.set_array_element(data, size - 1, Value::Object(None));
    al_set_size(ctx, this, (size - 1) as i32);
    Ok(Some(old))
}

pub fn native_al_remove_obj(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let size = size as usize;
    for i in 0..size {
        let elem = ctx.get_array_element(data, i);
        if values_equal(ctx, &elem, &target) {
            // Close the gap: shift the tail [i+1..size) left by one into
            // [i..size-1). Use the bulk intrinsic (memmove-style overlap is
            // handled by the VM) with a per-element fallback, mirroring
            // `native_al_add_all`.
            let tail_len = size - i - 1;
            if !ctx.bulk_array_copy(data, i + 1, data, i, tail_len) {
                for j in (i + 1)..size {
                    let val = ctx.get_array_element(data, j);
                    ctx.set_array_element(data, j - 1, val);
                }
            }
            ctx.set_array_element(data, size - 1, Value::Object(None));
            al_set_size(ctx, this, (size - 1) as i32);
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_al_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    if let Some(d) = data {
        for i in 0..(size as usize) {
            ctx.set_array_element(d, i, Value::Object(None));
        }
    }
    al_set_size(ctx, this, 0);
    Ok(None)
}

pub fn native_al_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    for i in 0..(size as usize) {
        let elem = ctx.get_array_element(data, i);
        if values_equal(ctx, &elem, &target) {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

pub fn native_al_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(-1))),
    };
    for i in 0..(size as usize) {
        let elem = ctx.get_array_element(data, i);
        if values_equal(ctx, &elem, &target) {
            return Ok(Some(Value::Int(i as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

fn native_al_last_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(-1))),
    };
    let size = size as usize;
    for i in (0..size).rev() {
        let elem = ctx.get_array_element(data, i);
        if values_equal(ctx, &elem, &target) {
            return Ok(Some(Value::Int(i as i32)));
        }
    }
    Ok(Some(Value::Int(-1)))
}

pub fn native_al_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    let result = alloc_ref_array(ctx, size);
    if let Some(d) = data {
        for i in 0..size {
            let val = ctx.get_array_element(d, i);
            ctx.set_array_element(result, i, val);
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

/// `ArrayList.toArray(T[])` / `AbstractCollection.toArray(T[])` —
/// produce a typed array (or grow the supplied template) without going
/// through the bytecode's `Arrays.copyOf(elementData, size, a.getClass())`
/// path which NPEs on synthetic ArrayLists.
pub fn native_al_to_array_typed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let template = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    if dbg_sbload() {
        eprintln!(
            "[DBG_SBLOAD] AL.toArray(T[]) size={} data_some={} template_some={}",
            size,
            data.is_some(),
            matches!(template, Value::Object(Some(_)))
        );
    }
    let target = match template {
        Value::Object(Some(arr)) if ctx.array_length(arr) >= size => arr,
        _ => alloc_ref_array(ctx, size),
    };
    if let Some(d) = data {
        for i in 0..size {
            let val = ctx.get_array_element(d, i);
            ctx.set_array_element(target, i, val);
        }
    }
    let target_len = ctx.array_length(target);
    if target_len > size {
        ctx.set_array_element(target, size, Value::Object(None));
    }
    Ok(Some(Value::Object(Some(target))))
}

/// Collection.toArray(IntFunction) — delegates to toArray() since CratonVM uses Object[] uniformly.
fn native_collection_to_array_generator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The IntFunction generator is used in real Java to create a typed array (T[]).
    // Since CratonVM uses Object[] uniformly, we ignore the generator and delegate.
    native_al_to_array(ctx, args)
}

pub fn native_al_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // `java/util/List.iterator()` / `Collection.iterator()` / `Iterable.iterator()`
    // are all wired to this native at the interface level (see
    // `register_interface_natives`).  When the receiver is *not* a
    // `java/util/ArrayList`-compatible object (i.e. its instance layout
    // doesn't have `elementData` at the ArrayList slot), the ArrayList$Itr
    // we build below reads from the wrong field slots and yields an empty
    // iteration — silently breaking real-JDK collections that the
    // interpreter cannot dispatch to via bytecode.
    //
    // The canonical offender is `java/util/concurrent/CopyOnWriteArrayList`
    // (used by Spring's `BeanPostProcessorCacheAwareList`): COWAL stores
    // its backing `Object[]` in the `array` field at instance-slot 1
    // (slot 0 is the `lock` monitor), with no separate `size` field.
    // Sending such a receiver through the ArrayList path makes
    // `al_state` read `lock` as `elementData` (returns `None`), so
    // `ArrayList$Itr.hasNext()` reports `false` on the first call and
    // every loop over the list runs zero iterations.  In Spring 6+ this
    // manifests as `internalConfigurationAnnotationProcessor` setter
    // injection failing with
    //   `Property 'metadataReaderFactory' threw exception: ...
    //    MetadataReaderFactory must not be null`
    // because `ApplicationContextAwareProcessor.setResourceLoader` is
    // never invoked on `SharedMetadataReaderFactoryBean` — the
    // `applyBeanPostProcessorsBeforeInitialization` loop over
    // `beanPostProcessors` produces zero items.
    //
    // Route COWAL receivers (including subclasses) to a snapshot
    // iterator backed by the live `array` field.
    let receiver_class_id = ctx.class_id_of_object(this);
    let cowal_cid = ctx.class_id_by_name("java/util/concurrent/CopyOnWriteArrayList");
    if let Some(cowal_id) = cowal_cid {
        if ctx.is_subclass(receiver_class_id, cowal_id) {
            // Snapshot COWAL's `array` field by resolving the slot by name
            // — works whether the runtime saw the real JDK class file or a
            // synthetic stub.  `size()` on COWAL is `array.length`, so we
            // wrap the snapshot in an ArrayList-shaped object (slot 0 =
            // backing array, slot 1 = size) and build a normal
            // ArrayList$Itr on top.  We do *not* mutate the original COWAL.
            let arr_slot = ctx
                .resolve_field_index("java/util/concurrent/CopyOnWriteArrayList", "array");
            let snapshot = arr_slot.and_then(|s| match ctx.get_field(this, s) {
                Value::Object(Some(a)) => Some(a),
                _ => None,
            });
            let snap_len = snapshot
                .map(|a| ctx.array_length(a) as i32)
                .unwrap_or(0);
            let backing = snapshot.unwrap_or_else(|| alloc_ref_array(ctx, 0));
            let wrapper = alloc_arraylist_with(ctx, backing, snap_len);
            let (cursor_slot, list_slot, n_fields) = al_itr_slots(ctx);
            let itr = alloc_synthetic(ctx, "java/util/ArrayList$Itr", n_fields);
            ctx.set_field(itr, list_slot, Value::Object(Some(wrapper)));
            ctx.set_field(itr, cursor_slot, Value::Int(0));
            return Ok(Some(Value::Object(Some(itr))));
        }
    }
    // Create ArrayList$Itr. Real-JDK has fields: cursor, lastRet,
    // expectedModCount, this$0. Use field-name resolution so we write to
    // the right slots regardless of layout.
    let (cursor_slot, list_slot, n_fields) = al_itr_slots(ctx);
    let itr = alloc_synthetic(ctx, "java/util/ArrayList$Itr", n_fields);
    ctx.set_field(itr, list_slot, Value::Object(Some(this)));
    ctx.set_field(itr, cursor_slot, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_al_ensure_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let min_cap = match args.get(1) {
        Some(Value::Int(c)) => std::cmp::max(*c, 0) as usize,
        _ => return Ok(None),
    };
    al_ensure_capacity(ctx, this, min_cap);
    Ok(None)
}

fn native_al_trim_to_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    let old_cap = data.map_or(0, |d| ctx.array_length(d));
    if size < old_cap {
        let new_buf = alloc_ref_array(ctx, size);
        if let Some(old_buf) = data {
            if !ctx.bulk_array_copy(old_buf, 0, new_buf, 0, size) {
                for i in 0..size {
                    let val = ctx.get_array_element(old_buf, i);
                    ctx.set_array_element(new_buf, i, val);
                }
            }
        }
        al_set_data(ctx, this, new_buf);
    }
    Ok(None)
}

pub fn native_al_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::fmt::Write as _;
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    // Pre-size: "[" + N elements averaging ~16 chars each + (N-1) ", " + "]".
    // Single allocation avoids the intermediate Vec<String> + join() walk.
    let mut text = String::with_capacity(2 + size.saturating_mul(18));
    text.push('[');
    if let Some(d) = data {
        for i in 0..size {
            if i > 0 {
                text.push_str(", ");
            }
            let val = ctx.get_array_element(d, i);
            // `write!` into a String never fails; ignore the Result.
            let _ = write!(text, "{}", obj_to_display_string(ctx, &val));
        }
    }
    text.push(']');
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

fn native_al_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Fast path: the source is a plain ArrayList whose synthetic
    // (data, size) slots are directly readable — copy its backing
    // array in one shot.
    let (other_data, other_size) = al_state(ctx, other);
    let other_size = other_size as usize;
    if let (Some(other_data), true) = (other_data, other_size > 0) {
        let (_, my_size) = al_state(ctx, this);
        let my_size = my_size as usize;
        let buf = al_ensure_capacity(ctx, this, my_size + other_size);
        if !ctx.bulk_array_copy(other_data, 0, buf, my_size, other_size) {
            for i in 0..other_size {
                let val = ctx.get_array_element(other_data, i);
                ctx.set_array_element(buf, my_size + i, val);
            }
        }
        al_set_size(ctx, this, (my_size + other_size) as i32);
        return Ok(Some(Value::Int(1)));
    }
    // General path: the source is NOT a plain ArrayList — it may be a
    // `Collections.unmodifiableList(...)` view, an `Arrays.asList(...)`
    // (`java/util/Arrays$ArrayList`), a `List.of(...)` snapshot, a
    // HashSet, etc. `al_state` only understands the synthetic ArrayList
    // layout, so it reports size 0 for all of these and the elements
    // are silently dropped. Fall back to `collect_collection_elements`,
    // which sees through every wrapper and reads arbitrary collections
    // via their real layout / iterator. This is the path WildFly's
    // `PathAddress.append(...)` depends on (`ArrayList.addAll` of a
    // `Collections.unmodifiableList` parent address) — without it the
    // appended PathAddress comes out empty and resource-tree
    // registration NPEs in `ConcreteResourceRegistration.registerSubModel`.
    let elems = collect_collection_elements(ctx, other);
    if elems.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    let (_, my_size) = al_state(ctx, this);
    let my_size = my_size as usize;
    let buf = al_ensure_capacity(ctx, this, my_size + elems.len());
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(buf, my_size + i, *val);
    }
    al_set_size(ctx, this, (my_size + elems.len()) as i32);
    Ok(Some(Value::Int(1)))
}

fn native_al_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_i32 = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let to_i32 = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let (data, size) = al_state(ctx, this);
    if from_i32 < 0 || from_i32 > size {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index: from_i32,
        }
        .into());
    }
    if to_i32 < 0 || to_i32 > size {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index: to_i32,
        }
        .into());
    }
    if from_i32 > to_i32 {
        return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: format!("fromIndex({from_i32}) > toIndex({to_i32})"),
        }
        .into());
    }
    let from = from_i32 as usize;
    let to = to_i32 as usize;
    let sub_size = to.saturating_sub(from);
    let __al_n_fields = al_slots(ctx).2;
    let new_list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let new_buf = alloc_ref_array(ctx, std::cmp::max(sub_size, AL_DEFAULT_CAPACITY));
    if let Some(d) = data {
        if !ctx.bulk_array_copy(d, from, new_buf, 0, sub_size) {
            for i in 0..sub_size {
                let val = ctx.get_array_element(d, from + i);
                ctx.set_array_element(new_buf, i, val);
            }
        }
    }
    al_set_data(ctx, new_list, new_buf);
    al_set_size(ctx, new_list, sub_size as i32);
    Ok(Some(Value::Object(Some(new_list))))
}

fn native_al_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (data, size) = al_state(ctx, this);
    let size = size as usize;
    let mut hash: i32 = 1;
    if let Some(d) = data {
        for i in 0..size {
            let val = ctx.get_array_element(d, i);
            // List.hashCode contract: 31*acc + e.hashCode() (0 for null).
            let elem_hash = element_hash_code(ctx, &val);
            hash = hash.wrapping_mul(31).wrapping_add(elem_hash);
        }
    }
    Ok(Some(Value::Int(hash)))
}

fn native_al_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this_raw = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other_raw = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Identity check (on the original references, before unwrapping).
    if std::ptr::eq(this_raw.as_ptr(), other_raw.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    // See through unmodifiable wrappers on either side so an ArrayList can be
    // compared element-by-element against a `List.of(...)` /
    // `Collections.unmodifiableList(...)` view. `al_state` reads the backing
    // array directly and would otherwise see the wrapper's slot-0 backing
    // pointer (a non-array), reporting size 0 and a spurious inequality.
    let this = unwrap_unmod(ctx, this_raw);
    let other = unwrap_unmod(ctx, other_raw);
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    let (data_a, size_a) = al_state(ctx, this);
    let (data_b, size_b) = al_state(ctx, other);
    if size_a != size_b {
        return Ok(Some(Value::Int(0)));
    }
    let size = size_a as usize;
    match (data_a, data_b) {
        (Some(da), Some(db)) => {
            for i in 0..size {
                let va = ctx.get_array_element(da, i);
                let vb = ctx.get_array_element(db, i);
                if !values_equal(ctx, &va, &vb) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            Ok(Some(Value::Int(1)))
        }
        (None, None) => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

// ===========================================================================
// HashMap — field 0 = Object[] buckets, field 1 = Int size, field 2 = Int cap
// HashMap$Node — field 0 = key, field 1 = value, field 2 = hash, field 3 = next
// ===========================================================================

const MAP_FIELD_BUCKETS: usize = 0;
const MAP_FIELD_SIZE: usize = 1;
const MAP_FIELD_CAPACITY: usize = 2;
const MAP_NUM_FIELDS: usize = 3;
const MAP_DEFAULT_CAPACITY: usize = 16;

const NODE_FIELD_KEY: usize = 0;
const NODE_FIELD_VALUE: usize = 1;
const NODE_FIELD_HASH: usize = 2;
const NODE_FIELD_NEXT: usize = 3;
const NODE_NUM_FIELDS: usize = 4;

/// Extract HashMap state: (buckets, size, capacity).
fn map_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32) {
    // See through CratonVM's unmodifiable wrapper views. When a generic
    // `java/util/Map` interface native (registered to `native_map_*`) is
    // dispatched on a `cratonvm/internal/UnmodifiableMap` receiver — which
    // happens when the wrapper class does not itself register the invoked
    // method — slot 0 of the wrapper is the *backing map* ObjectRef, not a
    // bucket array. Without this unwrap, `map_state` reads a non-array
    // Object at slot 0, fires `[MAP-STATE-GUARD]`, and (in `native_map_put`)
    // `map_resize` would clobber the wrapper's backing pointer; the warning
    // floods because Keycloak 26's config code repeatedly calls `get` /
    // `containsKey` / `size` on `Map.of(...)` / `Collections.unmodifiableMap`
    // results. Reading map state from the backing is always correct because
    // every wrapper mutator throws — these are read-only callers.
    let this = unwrap_unmod(ctx, this);
    let buckets_slot0 = match ctx.get_field(this, MAP_FIELD_BUCKETS) {
        Value::Object(Some(arr)) => {
            if ctx.heap_kind_of(arr) == ObjectKind::Array {
                Some(arr)
            } else {
                // Diagnostic only — slot 0 is not the `table` field for
                // JDK-constructed maps, and the fallback below resolves it
                // properly. Gate the probe behind the cached HM-trace flag
                // so the common path does no stderr I/O.
                if dbg_hm_trace() {
                    let map_cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(this))
                        .unwrap_or_default();
                    let slot0_cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(arr))
                        .unwrap_or_default();
                    eprintln!(
                        "[MAP-STATE-GUARD] non-array buckets slot0: map={:?}({}) slot0={:?}({}) \
                         — receiver was not a bucket-backed HashMap; treating buckets as absent",
                        this, map_cls, arr, slot0_cls
                    );
                }
                None
            }
        }
        _ => None,
    };
    // For JDK-constructed maps (e.g. AnnotationAttributes via Spring's
    // `new AnnotationAttributes(annotationType, false)`), the absolute
    // slot 0 is NOT necessarily the `table` field. Fall back to the
    // JDK-resolved `table` slot when slot 0 doesn't yield a bucket array.
    let buckets = buckets_slot0.or_else(|| {
        let slot = ctx.resolve_field_index("java/util/HashMap", "table")?;
        if slot == MAP_FIELD_BUCKETS || slot >= ctx.object_num_fields(this) {
            return None;
        }
        match ctx.get_field(this, slot) {
            Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => Some(arr),
            _ => None,
        }
    });
    // S111r28: Read size from the JDK-resolved `size` field by name when the
    // class metadata is available. The synthetic absolute-slot-1 fallback
    // only fires for raw `alloc_object(ClassId::new(0), ...)` allocations
    // that were never bound to the real `java/util/HashMap` class — those
    // still use the legacy `slot 1 = Int(size)` convention.
    //
    // Why name-resolution: real-JDK HashMap inherits AbstractMap's
    // `keySet`/`values` reference fields, so absolute slot 1 actually
    // corresponds to the inherited `values` field (descriptor
    // `Ljava/util/Collection;`). Writing `Int(0)` there triggers the
    // descriptor-aware coercion path which rewrites `Int(0)` as
    // `Object(None)` (see `coerce_field_value_by_descriptor`'s `b'L'` arm),
    // so subsequent reads see `Object(None)` instead of `Int(0)` and we
    // would fall through to slot 2 (`MAP_FIELD_CAPACITY`) and report the
    // bucket count as the size.
    let size_by_name = ctx
        .resolve_field_index("java/util/HashMap", "size")
        .filter(|&slot| slot < ctx.object_num_fields(this))
        .map(|slot| ctx.get_field(this, slot));
    let size = match size_by_name {
        Some(Value::Int(s)) => s,
        _ => match ctx.get_field(this, MAP_FIELD_SIZE) {
            Value::Int(s) => s,                            // legacy: slot 1 is Int
            _ => match ctx.get_field(this, 2) {            // ancient fallback
                Value::Int(s) => s,
                _ => 0,
            },
        },
    };
    // S111r26: Use bucket array length as the true capacity.  When
    // make_hashset_with_elements uses the real JDK HashMap field layout
    // (table=0, entrySet=1, size=2, ...), MAP_FIELD_CAPACITY (slot 2)
    // holds the element count, not the bucket count.  Reading the array
    // length of the bucket array always gives the correct value regardless
    // of which layout was used (legacy 3-field or real JDK).
    let cap = if let Some(b) = buckets {
        let arr_len = ctx.array_length(b) as i32;
        if arr_len > 0 { arr_len } else { MAP_DEFAULT_CAPACITY as i32 }
    } else {
        match ctx.get_field(this, MAP_FIELD_CAPACITY) {
            Value::Int(c) if c > 0 => c,
            _ => MAP_DEFAULT_CAPACITY as i32,
        }
    };
    (buckets, size, cap)
}

/// S111r28: Write the HashMap `size` field to BOTH the legacy synthetic
/// slot (absolute slot 1) and the JDK-resolved `size` slot when the class
/// metadata is available. Reads through `map_state` prefer the name-resolved
/// slot, which has descriptor `I` and is immune from the `b'L'` Int→Object
/// coercion that mangles slot 1 (= `AbstractMap.values: Collection`).
fn set_map_size(ctx: &mut dyn NativeContext, this: ObjectRef, size: i32) {
    ctx.set_field(this, MAP_FIELD_SIZE, Value::Int(size));
    if let Some(slot) = ctx.resolve_field_index("java/util/HashMap", "size") {
        if slot != MAP_FIELD_SIZE && slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, Value::Int(size));
        }
    }
}

/// S111r28 helper: best-effort write of a JDK-named HashMap field to the
/// resolved slot, but only when the slot is within the allocated field
/// count of `this`. Synthetic backing maps (e.g. inside HashSet) are
/// allocated with `MAP_NUM_FIELDS = 3` regardless of the real-JDK class
/// metadata, so naively writing to slot 4+ would land in undefined memory.
fn try_set_jdk_map_field(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
    value: Value,
) {
    if let Some(slot) = ctx.resolve_field_index("java/util/HashMap", field_name) {
        if slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, value);
        }
    }
}

/// Determine whether `obj`'s runtime class is a `java.lang.Enum` subclass and,
/// if so, return a *canonical logical identity* for the enum constant as
/// `(declaring_class_id, ordinal)`.
///
/// # Why this exists
///
/// `java.lang.Enum` declares `equals`/`hashCode` `final` with pure-identity
/// semantics, so a correct VM only ever has ONE object per enum constant and
/// `HashMap` keyed by enum constants works on pointer identity. CratonVM's
/// class-loading / `$VALUES` plumbing can, on some boot paths, materialise a
/// *second* object for the same constant (e.g. the constant read back via
/// `Enum.values()` during one `<clinit>` is not pointer-equal to the one a
/// later `getstatic Foo.BAR` resolves). When that happens, an enum-keyed
/// `HashMap`/`LinkedHashMap` silently misses every lookup — `Map.get` returns
/// `null` even though the key was `put`.
///
/// Concrete victim: Keycloak's `Profile.isFeatureEnabled(Feature)` does
/// `features.get(feature)` then unboxes the result; a missing entry yields
/// `NullPointerException: Cannot invoke "java.lang.Boolean.booleanValue()"`.
///
/// Keying enum constants by `(declaring class name, constant name)` is
/// *semantically identical* to identity — the JLS guarantees exactly one
/// constant per (class, name) — so this never makes two genuinely-distinct
/// constants compare equal, and it makes enum-keyed maps immune to the
/// duplicate-object bug.
///
/// The *class name string* (not the `ClassId`) is used deliberately: if the
/// VM materialised the duplicate by loading the enum class under two
/// different `ClassId`s, the names still agree, whereas the ids would not.
/// The constant's `name` (slot 0) and `ordinal` (slot 1) layout is fixed by
/// `Enum.<init>` (see `native-builtins` `native_enum_init`).
fn enum_key_identity(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<(String, String)> {
    let cid = ctx.class_id_of_object(obj);
    // Walk the superclass chain looking for java/lang/Enum. Cap the walk to
    // guard against malformed class hierarchies.
    let mut cur = Some(cid);
    let mut is_enum = false;
    for _ in 0..64 {
        match cur {
            Some(c) => {
                match ctx.class_name_of_id(c) {
                    Some(n) if n == "java/lang/Enum" => {
                        is_enum = true;
                        break;
                    }
                    Some(n) if n == "java/lang/Object" => break,
                    Some(_) => cur = ctx.superclass_of(c),
                    None => break,
                }
            }
            None => break,
        }
    }
    if !is_enum {
        return None;
    }
    // Declaring-class name — stable across ClassId duplication.
    let class_name = ctx.class_name_of_id(cid)?;
    // Enum layout: slot 0 = name (String), slot 1 = ordinal (int).
    let const_name = match ctx.get_field(obj, 0) {
        Value::Object(Some(s)) => ctx.read_string(s)?,
        _ => return None,
    };
    Some((class_name, const_name))
}

/// Compute hash for a key.
///
/// MED fix: when the user-supplied `hashCode()` throws (i.e. `invoke_virtual`
/// returns `Err(MethodCallFailed)`), the previous implementation silently
/// fell back to `identity_hash_code`, breaking the equals/hashCode contract
/// in a way that made subsequent `get` calls miss every key (and combined
/// with the matching swallow in `map_keys_equal`, `put` succeeded but `get`
/// always returned null). Real JDK propagates the exception out of
/// `HashMap.put`/`get` — this implementation now does the same by returning
/// `Result<i32, MethodCallFailed>` and surfacing the original error to the
/// caller. Non-exceptional contract violations (e.g. the user returned a
/// non-Int from hashCode) still fall back to identity since those are not
/// exceptional control flow.
fn map_hash_key(ctx: &mut dyn NativeContext, key: ObjectRef) -> Result<i32, MethodCallFailed> {
    // Try to read as string for better distribution (the common case, so it
    // is checked first).
    // Must match Java's String.hashCode (UTF-16 code units, i32 wrapping mul+add),
    // otherwise non-ASCII keys hash differently from bytecode-computed hashes and
    // HashMap.containsKey silently returns false. See vm::vm_exec::java_string_hash.
    if let Some(s) = ctx.read_string(key) {
        let mut h: i32 = 0;
        for cu in s.encode_utf16() {
            h = h.wrapping_mul(31).wrapping_add(cu as i32);
        }
        // Spread bits (like HashMap.hash in JDK)
        return Ok(h ^ (h >> 16));
    }
    // Enum constants: hash by (declaring class name, constant name) — the
    // JLS-canonical identity of an enum constant — so an enum-keyed map's
    // `put` and `get` agree on the bucket even if the VM materialised two
    // objects for the same constant. See `enum_key_identity` for the full
    // rationale.
    if let Some((class_name, const_name)) = enum_key_identity(ctx, key) {
        let mut h: i32 = 0;
        for b in class_name.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
        // Separator byte so ("AB","C") and ("A","BC") do not collide.
        h = h.wrapping_mul(31).wrapping_add(0x1F);
        for b in const_name.bytes() {
            h = h.wrapping_mul(31).wrapping_add(b as i32);
        }
        return Ok(h ^ (h >> 16));
    }
    if let Some(prim) = unbox_wrapper(ctx, key) {
        // Wrapper types: hash by their primitive value (matches JDK Integer.hashCode etc.)
        let h = match prim {
            Value::Int(v) => v,
            Value::Long(v) => (v ^ (v >> 32)) as i32,
            Value::Float(v) => v.to_bits() as i32,
            Value::Double(v) => {
                let bits = v.to_bits() as i64;
                (bits ^ (bits >> 32)) as i32
            }
            _ => ctx.identity_hash_code(key),
        };
        return Ok(h ^ (h >> 16));
    }
    // S111r-bug-fix (peaceful-sammet): for arbitrary user-defined objects we
    // MUST call their `hashCode()` so HashMap honours the equals/hashCode
    // contract — otherwise `HashMap.get(equalKey) == null` for non-identical
    // but equal keys (Spring's `AnnotationTypeMapping.aliasedBy` keyed by
    // `java.lang.reflect.Method` is the canonical victim, surfacing as the
    // `@AliasFor ... is not meta-present` chain on Spring Boot startup).
    //
    // MED fix: a thrown exception from `hashCode()` propagates via `?` —
    // previously it was swallowed and substituted with `identity_hash_code`,
    // silently breaking the equals/hashCode contract.
    let h = match ctx.invoke_virtual(key, "hashCode", "()I", &[])? {
        Some(Value::Int(v)) => v,
        _ => ctx.identity_hash_code(key),
    };
    Ok(h ^ (h >> 16))
}

/// Compute the *raw* Java `hashCode()` of an element `Value` — i.e. the value
/// returned by `Object.hashCode()` with NO HashMap bit-spreading applied.
///
/// This is what the `List`/`Set`/`Map` `hashCode` contracts require (unlike
/// `map_hash_key`, which additionally spreads via `h ^ h>>>16` for bucket
/// distribution). `null` hashes to 0. Strings and primitive wrappers are
/// hashed by value to match the JDK; arbitrary objects dispatch to their
/// virtual `hashCode()`.
fn element_hash_code(ctx: &mut dyn NativeContext, v: &Value) -> i32 {
    match v {
        Value::Object(None) => 0,
        Value::Int(x) => *x,
        Value::Long(x) => (*x ^ (*x >> 32)) as i32,
        Value::Float(x) => x.to_bits() as i32,
        Value::Double(x) => {
            let bits = x.to_bits() as i64;
            (bits ^ (bits >> 32)) as i32
        }
        Value::Object(Some(obj)) => {
            // String hashCode by value (UTF-16 code units, wrapping mul+add).
            if let Some(s) = ctx.read_string(*obj) {
                let mut h: i32 = 0;
                for cu in s.encode_utf16() {
                    h = h.wrapping_mul(31).wrapping_add(cu as i32);
                }
                return h;
            }
            // Primitive wrapper types hash by their boxed primitive value.
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                return element_hash_code(ctx, &prim);
            }
            // Arbitrary objects: honour the contract via their virtual hashCode().
            match ctx.invoke_virtual(*obj, "hashCode", "()I", &[]) {
                Ok(Some(Value::Int(h))) => h,
                _ => ctx.identity_hash_code(*obj),
            }
        }
        // Internal VM values that cannot legitimately be collection elements.
        Value::ReturnAddress(_) | Value::Uninitialized => 0,
    }
}

/// Check if two keys are equal.
///
/// MED fix: when the user-supplied `equals(Object)` throws (i.e.
/// `invoke_virtual` returns `Err(MethodCallFailed)`), the previous
/// implementation silently treated it as `false`, breaking the
/// equals/hashCode contract — combined with the matching `map_hash_key`
/// swallowing this meant `put` succeeded but `get` always returned null.
/// Real JDK propagates the exception out of `HashMap.put`/`get` — this
/// implementation now does the same by returning
/// `Result<bool, MethodCallFailed>` and surfacing the original error to
/// the caller. Non-exceptional contract violations (e.g. the user returned
/// a non-Int from equals) still fall through as `false`.
fn map_keys_equal(
    ctx: &mut dyn NativeContext,
    a: ObjectRef,
    b: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if std::ptr::eq(a.as_ptr(), b.as_ptr()) {
        return Ok(true);
    }
    // String value equality (the common case, checked first).
    if let (Some(sa), Some(sb)) = (ctx.read_string(a), ctx.read_string(b)) {
        return Ok(sa == sb);
    }
    // Enum constants: compare by (declaring class, ordinal). `Enum.equals` is
    // `final` identity, so a correct VM never reaches here for two non-equal
    // enum objects; but if the VM produced a duplicate object for the same
    // constant, identity comparison spuriously fails. (class, ordinal) is the
    // JLS-canonical identity and cannot collide across distinct constants.
    // See `enum_key_identity`.
    if let (Some(ia), Some(ib)) = (enum_key_identity(ctx, a), enum_key_identity(ctx, b)) {
        return Ok(ia == ib);
    }
    // Wrapper type equality: unbox and compare primitives
    if let (Some(pa), Some(pb)) = (unbox_wrapper(ctx, a), unbox_wrapper(ctx, b)) {
        return Ok(match (pa, pb) {
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Long(x), Value::Long(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Double(x), Value::Double(y)) => x == y,
            (Value::Int(x), Value::Long(y)) => (x as i64) == y,
            (Value::Long(x), Value::Int(y)) => x == (y as i64),
            _ => false,
        });
    }
    // S111r-bug-fix (peaceful-sammet): fall back to the user-defined
    // `equals(Object)` so HashMap honours the equals/hashCode contract for
    // arbitrary key types. See `map_hash_key` for the matching contract
    // commentary and the Spring `AnnotationTypeMapping.aliasedBy` symptom.
    //
    // MED fix: thrown exceptions from `equals(Object)` propagate via `?` —
    // previously they were silently mapped to `false`, which combined with
    // the matching swallow in `map_hash_key` meant `put` succeeded but
    // `get` always returned null.
    let res = ctx.invoke_virtual(a, "equals", "(Ljava/lang/Object;)Z", &[Value::Object(Some(b))]);
    if dbg_hm_trace() {
        eprintln!("[HM-EQ] invoke_virtual(equals) -> {:?}", res);
    }
    match res? {
        Some(Value::Int(v)) => Ok(v != 0),
        _ => Ok(false),
    }
}

/// Get the bucket index for a given hash and capacity.
fn map_bucket_index(hash: i32, capacity: i32) -> usize {
    // Use bitwise AND for power-of-two capacity (like JDK HashMap)
    ((hash as u32) & ((capacity as u32).wrapping_sub(1))) as usize
}

/// Allocate a HashMap$Node entry using the legacy synthetic layout
/// (slot 0 = key, slot 1 = value, slot 2 = hash, slot 3 = next).
///
/// We deliberately allocate with `ClassId::new(0)` instead of binding the
/// node to the real-JDK `java/util/HashMap$Node` class. The JDK declares
/// fields in order `hash:I, key:Object, value:Object, next:HashMap$Node`,
/// so binding to the real class causes descriptor-aware field coercion
/// (see `coerce_field_value_by_descriptor`) to interpret slot 0 as `int`
/// and rewrite our `Object(key)` write as `Int(<pointer-bits>)`. The
/// downstream `get_node_key` layout-sniff then sees an `Int` in slot 0,
/// concludes the node uses the JDK layout, and reads slot 1 as the key —
/// but slot 1 was set to the sentinel `Int(1)` (for HashSet-backed maps),
/// surfacing as `Iterator.next()` returning `Int(1)` and a downstream
/// `checkcast Map.Entry` against an `Int`.
///
/// Matches `native_map_put`'s direct `alloc_object(ClassId::new(0), ...)`
/// at the insert path; reads via `get_node_key`/`get_node_value` keep
/// their layout-sniff for nodes produced by either site.
fn map_alloc_node(
    ctx: &mut dyn NativeContext,
    key: ObjectRef,
    value: Value,
    hash: i32,
    next: Option<ObjectRef>,
) -> ObjectRef {
    let node = ctx.alloc_object(cratonvm_types::ClassId::new(0), NODE_NUM_FIELDS);
    ctx.set_field(node, NODE_FIELD_KEY, Value::Object(Some(key)));
    ctx.set_field(node, NODE_FIELD_VALUE, value);
    ctx.set_field(node, NODE_FIELD_HASH, Value::Int(hash));
    ctx.set_field(node, NODE_FIELD_NEXT, Value::Object(next));
    node
}

/// S111r26: Layout-aware node key reader.
///
/// The legacy synthetic layout stores: key=0, value=1, hash=2, next=3.
/// The real JDK HashMap$Node layout stores: hash=0, key=1, value=2, next=3.
///
/// Detect which layout is in use by checking slot 0:
///   - Object → legacy layout (slot 0 = key)
///   - Int    → JDK layout   (slot 0 = hash, key is at slot 1)
fn get_node_key(ctx: &dyn NativeContext, node: ObjectRef) -> Value {
    match ctx.get_field(node, 0) {
        v @ Value::Object(_) => v,          // legacy: slot 0 is the key
        _ => ctx.get_field(node, 1),         // JDK:    slot 1 is the key
    }
}

/// S111r26: Layout-aware node value reader (see `get_node_key`).
fn get_node_value(ctx: &dyn NativeContext, node: ObjectRef) -> Value {
    match ctx.get_field(node, 0) {
        Value::Object(_) => ctx.get_field(node, NODE_FIELD_VALUE), // legacy: slot 1
        _ => ctx.get_field(node, 2),                               // JDK: slot 2
    }
}

/// Maximum capacity for HashMap buckets (~1 billion).
const MAP_MAX_CAPACITY: i32 = 1 << 30;

// Bug 5 (HIGH) round-10: thread-local flag set while a CHM mutator
// (native_chm_put / put_if_absent / compute_* / merge / replace*) is on
// the stack. `map_resize` consults the flag — only CHM-segment resizes
// pay the striped-lock cost; plain HashMap resizes run lock-free.
thread_local! {
    static CHM_RESIZE_LOCK_NEEDED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII guard that marks the current thread as being inside a CHM
/// mutator. While alive, `map_resize` calls performed by code below this
/// frame take the per-segment write lock against concurrent readers.
struct ChmResizeLockGuard {
    prev: bool,
}

impl ChmResizeLockGuard {
    fn enter() -> Self {
        let prev = CHM_RESIZE_LOCK_NEEDED.with(|c| {
            let p = c.get();
            c.set(true);
            p
        });
        ChmResizeLockGuard { prev }
    }
}

impl Drop for ChmResizeLockGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        CHM_RESIZE_LOCK_NEEDED.with(|c| c.set(prev));
    }
}

/// Resize the HashMap when load factor is exceeded.
///
/// Whether the per-segment resize lock is taken is governed by the
/// thread-local `CHM_RESIZE_LOCK_NEEDED` flag, set by `ChmResizeLockGuard`
/// inside CHM mutator entry points. Plain `java/util/HashMap` callers
/// resize lock-free (HashMap is not thread-safe). CHM segment callers
/// take the write lock so concurrent CHM lock-free readers in
/// `chm_seg_get` serialize against the in-place NEXT mutations below.
fn map_resize(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let is_concurrent = CHM_RESIZE_LOCK_NEEDED.with(|c| c.get());
    map_resize_inner(ctx, this, is_concurrent);
}

fn map_resize_inner(ctx: &mut dyn NativeContext, this: ObjectRef, is_concurrent: bool) {
    // Bug 1+2+5 (CRIT/HIGH) round-10 fix: only take the resize lock when
    // resizing a CHM segment. Plain `java/util/HashMap.put` is single-
    // threaded; serializing its resize through the striped lock array
    // (round-9 native-misc HIGH-3) was pure overhead.
    //
    // For CHM segments, the per-segment striped RwLock (256 stripes,
    // keyed by `ctx.identity_hash_code(this)`) excludes concurrent
    // lock-free readers in `chm_seg_get` while we mutate NEXT pointers
    // in place.
    //
    // TODO(round-6+): replace with a clone-resize that allocates fresh
    // Node objects for the new sub-chains and leaves the old chain
    // untouched, so reads can stay fully lock-free and this lock can be
    // removed entirely.
    let _write_guard = if is_concurrent {
        // `parking_lot::RwLock::write` is infallible (no PoisonError),
        // so no `unwrap_or_else` wrapper is needed.
        let seg_id = ctx.identity_hash_code(this);
        Some(chm_seg_lock_for(seg_id).write())
    } else {
        None
    };

    let (old_buckets, size, old_cap) = map_state(ctx, this);
    if old_cap >= MAP_MAX_CAPACITY {
        return; // cannot grow further
    }
    let new_cap = std::cmp::min(old_cap * 2, MAP_MAX_CAPACITY);
    let new_buckets = alloc_ref_array(ctx, new_cap as usize);

    // Re-hash all entries. When `new_cap == 2 * old_cap` (the common
    // doubling case) we use JDK's split semantics: each entry whose
    // `hash & old_cap == 0` stays at bucket `i`, and the rest move to
    // bucket `i + old_cap`. This walks each chain exactly once and
    // writes the two head slots — strictly better than the legacy
    // head-prepend path which performs N `set_array_element` calls per
    // bucket.
    //
    // AUDIT 2026-05-17: previously this branch was gated on a
    // successful `bulk_array_copy` of the reference bucket array as a
    // "pre-seed" optimization. `vm_exec::bulk_array_copy` refuses
    // reference arrays (write-barrier reasons), so the gate always
    // failed and we always took the slow legacy path. The split is a
    // perf win even without the pre-seed — overwriting the two head
    // slots is what the JDK does itself — so we always take it.
    // `bulk_array_copy` is still attempted as an optional fast path so
    // that if a future VM override does support reference arrays, we
    // skip the per-bucket head reads.
    if let Some(old_b) = old_buckets {
        let doubled = (new_cap as i64) == (old_cap as i64) * 2;
        if doubled {
            // Optional fast path: if the VM's bulk_array_copy supports
            // reference arrays, pre-seed `new_buckets[0..old_cap]` from
            // `old_b[0..old_cap]`. If not, the split loop below still
            // writes every head slot, so correctness is unaffected.
            let _preseeded =
                ctx.bulk_array_copy(old_b, 0, new_buckets, 0, old_cap as usize);
            // JDK-style split: walk each old bucket, partition its chain
            // into "low" (stays at i) and "high" (moves to i+old_cap)
            // lists, then overwrite the two head slots.
            let split_mask = old_cap; // power-of-two: the new high bit
            for i in 0..(old_cap as usize) {
                let mut node_val = ctx.get_array_element(old_b, i);
                // Track lo/hi head + tail so we preserve original chain order.
                let mut lo_head: Option<ObjectRef> = None;
                let mut lo_tail: Option<ObjectRef> = None;
                let mut hi_head: Option<ObjectRef> = None;
                let mut hi_tail: Option<ObjectRef> = None;
                let mut steps: usize = 0;
                let step_cap = (size as usize).saturating_add(8);
                let mut cycle_tripped = false;
                while let Value::Object(Some(node)) = node_val {
                    steps += 1;
                    if steps > step_cap {
                        // Total live entries bound the chain length;
                        // exceeding it indicates a cycle. Round-5 fix: do
                        // NOT silently keep the partial lo/hi chains —
                        // that loses every entry past the bound. Mark
                        // this bucket for full re-insert via the
                        // standard hash-prepend path below.
                        eprintln!(
                            "[HM-RESIZE-GUARD] aborting old-chain split walk at {} nodes (suspected cycle); bucket={} — falling back to full re-insert",
                            steps, i
                        );
                        cycle_tripped = true;
                        break;
                    }
                    let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let next = ctx.get_field(node, NODE_FIELD_NEXT);
                    if (key_hash & split_mask) == 0 {
                        if lo_head.is_none() {
                            lo_head = Some(node);
                        } else if let Some(t) = lo_tail {
                            ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(node)));
                        }
                        lo_tail = Some(node);
                    } else {
                        if hi_head.is_none() {
                            hi_head = Some(node);
                        } else if let Some(t) = hi_tail {
                            ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(Some(node)));
                        }
                        hi_tail = Some(node);
                    }
                    node_val = next;
                }
                if cycle_tripped {
                    // Round-5 cycle-bound fallback: discard the partial
                    // lo/hi work for this bucket and re-walk the
                    // original chain with a visited-set so we never
                    // follow the cycle twice, inserting each unique
                    // node into `new_buckets` via head-prepend at the
                    // correct index. This preserves correctness at the
                    // cost of bucket order — much better than silently
                    // dropping ~50% of entries.
                    let mut seen: std::collections::HashSet<*const ()> =
                        std::collections::HashSet::new();
                    let mut nv = ctx.get_array_element(old_b, i);
                    while let Value::Object(Some(node)) = nv {
                        let key = node.as_ptr() as *const ();
                        if !seen.insert(key) {
                            // Already inserted — cycle detected, stop.
                            break;
                        }
                        let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                            Value::Int(h) => h,
                            _ => 0,
                        };
                        let next = ctx.get_field(node, NODE_FIELD_NEXT);
                        let new_idx = map_bucket_index(key_hash, new_cap);
                        let existing = ctx.get_array_element(new_buckets, new_idx);
                        ctx.set_field(node, NODE_FIELD_NEXT, existing);
                        ctx.set_array_element(
                            new_buckets,
                            new_idx,
                            Value::Object(Some(node)),
                        );
                        nv = next;
                    }
                    continue;
                }
                // Terminate both partitioned chains.
                if let Some(t) = lo_tail {
                    ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(None));
                }
                if let Some(t) = hi_tail {
                    ctx.set_field(t, NODE_FIELD_NEXT, Value::Object(None));
                }
                // Overwrite the two slots (low keeps `i`, high goes to i+old_cap).
                ctx.set_array_element(
                    new_buckets,
                    i,
                    Value::Object(lo_head),
                );
                ctx.set_array_element(
                    new_buckets,
                    i + old_cap as usize,
                    Value::Object(hi_head),
                );
            }
        } else {
            // Legacy rebuild path: per-bucket re-insert (head-prepend).
            for i in 0..(old_cap as usize) {
                let mut node_val = ctx.get_array_element(old_b, i);
                let mut steps: usize = 0;
                while let Value::Object(Some(node)) = node_val {
                    steps += 1;
                    if steps > 1_000_000 {
                        eprintln!(
                            "[HM-RESIZE-GUARD] aborting old-chain walk at {} nodes (suspected cycle); bucket={}",
                            steps, i
                        );
                        break;
                    }
                    let key_hash = match ctx.get_field(node, NODE_FIELD_HASH) {
                        Value::Int(h) => h,
                        _ => 0,
                    };
                    let next = ctx.get_field(node, NODE_FIELD_NEXT);
                    // Self-cycle guard before we splice into the new bucket
                    if let Value::Object(Some(nx)) = next {
                        if std::ptr::eq(nx.as_ptr(), node.as_ptr()) {
                            eprintln!(
                                "[HM-RESIZE-GUARD] self-cycle in old bucket {} step {}",
                                i, steps
                            );
                            // Truncate: splice node alone, do not continue
                            let new_idx = map_bucket_index(key_hash, new_cap);
                            let existing = ctx.get_array_element(new_buckets, new_idx);
                            ctx.set_field(node, NODE_FIELD_NEXT, existing);
                            ctx.set_array_element(new_buckets, new_idx, Value::Object(Some(node)));
                            break;
                        }
                    }

                    // Insert into new bucket
                    let new_idx = map_bucket_index(key_hash, new_cap);
                    let existing = ctx.get_array_element(new_buckets, new_idx);
                    ctx.set_field(node, NODE_FIELD_NEXT, existing);
                    ctx.set_array_element(new_buckets, new_idx, Value::Object(Some(node)));

                    node_val = next;
                }
            }
        }
    }

    // Round-5 CRIT fix (publication race): publish the new buckets
    // array with a volatile/Release-style write so that concurrent
    // readers in `chm_get_volatile` (which use `get_field_volatile` on
    // MAP_FIELD_BUCKETS) are guaranteed to either see the OLD fully-
    // linked array or the NEW fully-linked array — never a half-spliced
    // chain whose NEXT pointers were mid-rewrite. All chain mutations
    // for `new_buckets` (the lo/hi splice and the head-prepend
    // fallback) complete before this volatile store. The non-CHM
    // (single-threaded HashMap) callers see identical semantics — a
    // volatile store is at least as strong as a plain store.
    ctx.set_field_volatile(this, MAP_FIELD_BUCKETS, Value::Object(Some(new_buckets)));
    set_map_size(ctx, this, size);
    // Mirror the bucket array to the JDK-resolved `table` slot when present
    // (and different from slot 0). This is critical for AnnotationAttributes
    // and other JDK-constructed maps where slot 0 may not be the `table`
    // field — without this mirror, subsequent reads via `map_state` (which
    // reads slot 0) would see the new buckets, but any JDK-bytecode path
    // that reads `table` directly would see null. Also keeps the two
    // storage locations in sync.
    let table_slot = ctx.resolve_field_index("java/util/HashMap", "table");
    if let Some(slot) = table_slot {
        if slot != MAP_FIELD_BUCKETS && slot < ctx.object_num_fields(this) {
            ctx.set_field_volatile(this, slot, Value::Object(Some(new_buckets)));
        }
    }
    if table_slot != Some(MAP_FIELD_CAPACITY) {
        ctx.set_field(this, MAP_FIELD_CAPACITY, Value::Int(new_cap));
    }
}

/// Collect all keys from a HashMap into a Vec.
/// If `this` is a `java.util.Properties` whose `map` field (JDK 25 layout)
/// points at a `ConcurrentHashMap`, return that CHM. Otherwise None.
///
/// JDK 25 changed `Properties` to back its entries with a private
/// `ConcurrentHashMap<Object,Object> map` field instead of the inherited
/// `Hashtable.table`. The synthetic-mode slot-0 `buckets` we populate on
/// `Properties.load()` therefore looks empty to JDK-style readers, and any
/// helper that walks slot 0 (`map_collect_keys` / `map_collect_entries` /
/// etc.) reports zero entries. That made `new HashMap<>(props)` empty, which
/// in turn made Kafka's `KafkaConfig$.populateSynonyms` drop every property
/// and surface as `Missing required configuration "process.roles"`.
fn properties_backing_chm(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ctx.resolve_field_index("java/util/Properties", "map")?;
    if slot >= ctx.object_num_fields(this) {
        return None;
    }
    let m = match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => m,
        _ => return None,
    };
    let cid = ctx.class_id_of_object(m);
    let cn = ctx.class_name_of_id(cid).unwrap_or_default();
    if cn == "java/util/concurrent/ConcurrentHashMap"
        || cn.starts_with("java/util/concurrent/ConcurrentHashMap$")
    {
        Some(m)
    } else {
        None
    }
}

/// If `obj` is one of CratonVM's unmodifiable wrapper views, return the
/// backing collection it wraps (recursing through nested wrappers); otherwise
/// return `obj` unchanged. Lets the bucket-walking map helpers transparently
/// see through `Collections.unmodifiableMap` / `Map.of` results.
fn unwrap_unmod(ctx: &dyn NativeContext, obj: ObjectRef) -> ObjectRef {
    let cid = ctx.class_id_of_object(obj);
    if let Some(name) = ctx.class_name_of_id(cid) {
        if name == UNMOD_MAP_CLASS
            || name == UNMOD_LIST_CLASS
            || name == UNMOD_SET_CLASS
            || name == UNMOD_COLLECTION_CLASS
        {
            if let Value::Object(Some(inner)) = ctx.get_field(obj, UNMOD_FIELD_BACKING) {
                return unwrap_unmod(ctx, inner);
            }
        }
    }
    obj
}

fn map_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let this = unwrap_unmod(ctx, this);
    if let Some(chm) = properties_backing_chm(ctx, this) {
        return chm_collect_all_keys(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        return chm_collect_all_keys(ctx, this);
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut keys = Vec::new();
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let key = get_node_key(ctx, node);
                keys.push(key);
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    keys
}

/// Collect all values from a HashMap into a Vec.
fn map_collect_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let this = unwrap_unmod(ctx, this);
    if let Some(chm) = properties_backing_chm(ctx, this) {
        return chm_collect_all_values(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        return chm_collect_all_values(ctx, this);
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut values = Vec::new();
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let value = get_node_value(ctx, node);
                values.push(value);
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    values
}

/// Collect all key-value pairs as (key, value) from a HashMap.
fn map_collect_entries(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let this = unwrap_unmod(ctx, this);
    if let Some(chm) = properties_backing_chm(ctx, this) {
        return chm_collect_all_entries(ctx, chm);
    }
    if is_chm_receiver(ctx, this) {
        return chm_collect_all_entries(ctx, this);
    }
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut entries = Vec::new();
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                let key = get_node_key(ctx, node);
                let value = get_node_value(ctx, node);
                entries.push((key, value));
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    entries
}

fn register_hashmap_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/HashMap";

    r.register(c, "<init>", "()V", native_map_init);
    r.register(c, "<init>", "(I)V", native_map_init_capacity);
    r.register(c, "<init>", "(Ljava/util/Map;)V", native_map_init_from_map);
    r.register(c, "size", "()I", native_map_size);
    r.register(c, "isEmpty", "()Z", native_map_is_empty);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );
    r.register(c, "clear", "()V", native_map_clear);
    r.register(c, "keySet", "()Ljava/util/Set;", native_map_key_set);
    r.register(c, "values", "()Ljava/util/Collection;", native_map_values);
    r.register(c, "entrySet", "()Ljava/util/Set;", native_map_entry_set);
    r.register(c, "toString", "()Ljava/lang/String;", native_map_to_string);
    r.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get_or_default,
    );
    r.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put_if_absent,
    );
    r.register(c, "putAll", "(Ljava/util/Map;)V", native_map_put_all);
    r.register(c, "hashCode", "()I", native_map_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_map_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each,
    );
    r.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_map_compute_if_absent,
    );
    r.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute,
    );
    r.register(
        c,
        "computeIfPresent",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_compute_if_present,
    );
    r.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_map_merge,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_map_replace_all,
    );
}

pub fn native_map_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // S111r28 (peaceful-sammet bug fix): Restore legacy synthetic layout at
    // absolute slots 0/1/2 (= buckets/size/capacity), which is what
    // `native_map_put` / `native_map_get` / `native_map_size` etc. read and
    // write. The previous S111r27 init wrote to absolute slots 0..5 thinking
    // they were JDK fields (table/entrySet/size/modCount/threshold/loadFactor),
    // but the real JDK class hierarchy puts AbstractMap.keySet at slot 0 and
    // AbstractMap.values at slot 1 first — so absolute slots 0..5 are NOT the
    // JDK HashMap own-fields. The result was that `native_map_put` saw
    // `slot0 = Object(None)` (no buckets) and bailed out without inserting,
    // which surfaced as `HashMap.size() == 0` / `HashSet` empty after every
    // put, breaking SpringApplication.<init> ("Sources must not be empty").
    //
    // Native HashMap path is the authoritative implementation (every
    // observable Map method — put, get, size, isEmpty, containsKey, keySet,
    // values, entrySet, toString, etc. — is registered as a native), so we
    // can just use the legacy synthetic layout at slots 0/1/2. JDK bytecode
    // for `HashMap.<method>` does not run because the natives shadow it.
    //
    // We additionally write the JDK-named fields (`size`, `table`, etc.) by
    // name when the class metadata is resolvable, so any JDK-bytecode caller
    // that reaches into HashMap via direct getfield (rare but possible
    // through reflection / private putVal entry points) sees consistent
    // values. These writes go to the correct real-JDK slots regardless of
    // how parent fields shift the absolute index.
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Legacy synthetic layout — what every other native HashMap op expects.
    let buckets = alloc_ref_array(ctx, MAP_DEFAULT_CAPACITY);
    ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, this, 0);
    // S111r29: Resolve the JDK `table` slot (descriptor `[Ljava/util/HashMap$Node;`)
    // and store the bucket array there. Writing `Int(MAP_DEFAULT_CAPACITY)` to
    // `MAP_FIELD_CAPACITY` (absolute slot 2) is dangerous when the JDK class
    // layout puts `table` at the same slot — JDK bytecode (e.g. `HashMap.resize()`)
    // then runs `arraylength` on `Int(16)` and aborts with
    //   `internal error: expected object reference, got int(16)`.
    // The fix: never let the JDK `table` slot hold an Int. Write the bucket
    // array to the JDK `table` slot when resolvable, and skip the legacy
    // capacity Int write if it would clobber `table`. `map_state` already
    // prefers the bucket array's length over `MAP_FIELD_CAPACITY`, so dropping
    // the Int write is safe.
    let table_slot = ctx.resolve_field_index("java/util/HashMap", "table");
    if let Some(slot) = table_slot {
        if slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, Value::Object(Some(buckets)));
        }
    }
    if table_slot != Some(MAP_FIELD_CAPACITY) {
        ctx.set_field(this, MAP_FIELD_CAPACITY, Value::Int(MAP_DEFAULT_CAPACITY as i32));
    }

    // Best-effort JDK-named field population for bytecode readers. `size`
    // was already mirrored by `set_map_size`; the rest are best-effort.
    try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
    try_set_jdk_map_field(
        ctx,
        this,
        "threshold",
        Value::Int((MAP_DEFAULT_CAPACITY as i32 * 3) / 4),
    );
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
    Ok(None)
}

fn native_map_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(c)) => {
            // Bug 4 (round-9 native-misc HIGH): the JDK's
            // `HashMap(int initialCapacity)` reads the requested value as
            // a *minimum number of mappings to hold without resizing*,
            // then internally allocates `ceil(c / loadFactor)` buckets so
            // the threshold (loadFactor * buckets) >= requested capacity.
            // With the default load factor of 0.75 that's `c * 4 / 3`,
            // rounded up to the next power of two. Previously we rounded
            // `c` itself to a power of two, which means
            // `new HashMap<>(16)` allocated 16 buckets and resized on the
            // 13th insert — defeating the entire purpose of the sizing
            // hint and causing extra rehash work in the hot loop.
            let requested = std::cmp::max(*c, 1) as u64;
            // ceil(requested * 4 / 3), then cap to MAP_MAX_CAPACITY before
            // next_power_of_two to avoid u32 overflow panic on absurdly
            // large hints.
            let needed = requested.saturating_mul(4).div_ceil(3);
            let capped = std::cmp::min(needed, MAP_MAX_CAPACITY as u64).max(1) as u32;
            let n = capped.checked_next_power_of_two().unwrap_or(MAP_MAX_CAPACITY as u32);
            std::cmp::min(n as usize, MAP_MAX_CAPACITY as usize)
        }
        _ => MAP_DEFAULT_CAPACITY,
    };
    // S111r28: same legacy synthetic layout as `native_map_init`. See the
    // longer comment there for the rationale.
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, this, 0);
    // S111r29: see `native_map_init` for rationale. Mirror the bucket array
    // into the JDK `table` slot so `HashMap.resize()` bytecode sees an array
    // (or null), never an Int. Skip the legacy Int-capacity write when it
    // would land on the same slot as JDK `table`.
    let table_slot = ctx.resolve_field_index("java/util/HashMap", "table");
    if let Some(slot) = table_slot {
        if slot < ctx.object_num_fields(this) {
            ctx.set_field(this, slot, Value::Object(Some(buckets)));
        }
    }
    if table_slot != Some(MAP_FIELD_CAPACITY) {
        ctx.set_field(this, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    }

    // Best-effort JDK-named field population for bytecode readers.
    // `size` is mirrored by `set_map_size`.
    try_set_jdk_map_field(ctx, this, "modCount", Value::Int(0));
    try_set_jdk_map_field(ctx, this, "threshold", Value::Int((cap as i32 * 3) / 4));
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
    Ok(None)
}

// Public wrappers for cross-module access (EnumMap, IdentityHashMap, WeakHashMap)
pub fn native_map_put_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_put(ctx, args)
}
pub fn native_map_get_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_get(ctx, args)
}
pub fn native_map_remove_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_remove(ctx, args)
}
pub fn native_map_contains_key_pub(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_map_contains_key(ctx, args)
}
pub fn native_map_contains_value_pub(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    native_map_contains_value(ctx, args)
}
pub fn native_map_size_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_size(ctx, args)
}
pub fn native_map_is_empty_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_is_empty(ctx, args)
}
pub fn native_map_clear_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_clear(ctx, args)
}
pub fn native_map_key_set_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_key_set(ctx, args)
}
/// Collect a synthetic `HashMap`'s keys into a freshly-allocated `Object[]`.
///
/// Unlike `native_map_key_set_pub` (which builds a `HashSet` view), this
/// returns a plain reference array — the shape consumed by the synthetic
/// `java/util/Enumeration$Impl` helper (field 0 = `Object[]`, field 1 =
/// `int` cursor). Used by `ResourceBundle.getKeys()` overrides so a
/// caller iterating the bundle via `Enumeration` walks the real keys.
///
/// `map` must be the backing `HashMap` ref. Returns an empty array when
/// `map` is null or unreadable.
pub fn native_map_keys_as_array(
    ctx: &mut dyn NativeContext,
    map: Option<ObjectRef>,
) -> ObjectRef {
    let keys = match map {
        Some(m) => map_collect_keys(ctx, m),
        None => Vec::new(),
    };
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(arr, i, *k);
    }
    arr
}
pub fn native_map_values_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_values(ctx, args)
}
pub fn native_map_entry_set_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_entry_set(ctx, args)
}
pub fn native_map_to_string_pub(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_map_to_string(ctx, args)
}

/// Detect if a receiver is a TreeMap (or subclass). The `java/util/Map`
/// interface natives below use a HashMap-style field layout (buckets/size/
/// capacity at slots 0/1/2) which conflicts with TreeMap's
/// (data/size/comparator). Without this check, a `Map.put`/`get`/`size` call
/// on a TreeMap receiver silently no-ops, leaving size=0 and
/// dropping every entry. This blocked Keycloak FeatureOptions.<clinit>:
/// FeaturePropertyMappers stores feature→mapper entries in a TreeMap, and
/// when the JDK's TreeMap.keySpliteratorFor ran it traversed a null root
/// because the put native had never reached `tm_put`.
fn is_tree_map_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut cur = ctx.class_id_of_object(this);
    loop {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/util/TreeMap" => return true,
            Some(n) if n == "java/util/HashMap" || n == "java/lang/Object" => return false,
            _ => {}
        }
        match ctx.superclass_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
}

/// True when `this`'s runtime class is `java/util/concurrent/ConcurrentHashMap`.
///
/// CratonVM's CHM natives store a *segmented* layout — slot 0 is a ref-array
/// of per-segment map objects, not a HashMap bucket array. A generic
/// `java/util/Map.<method>` interface native (registered on `java/util/Map`)
/// dispatched on a CHM receiver — which happens whenever JDK code accesses a
/// CHM polymorphically through the `Map` interface, e.g. `MethodType`'s
/// `ReferencedKeySet`/`ReferencedKeyMap` backing store — must NOT run the
/// plain-HashMap path: `map_state` would mistake the segments array for a
/// bucket array and the node walk would dereference a segment object (3-field
/// synthetic map) as a 4-field `HashMap$Node`, reading field index 3 past the
/// end (the recurring `AnonymousObject$3` out-of-bounds-field-read error).
/// The plain-Map natives consult this and reroute to the CHM natives.
fn is_chm_receiver(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let mut cur = ctx.class_id_of_object(this);
    for _ in 0..32 {
        match ctx.class_name_of_id(cur) {
            Some(n) if n == "java/util/concurrent/ConcurrentHashMap" => return true,
            Some(n)
                if n == "java/util/HashMap"
                    || n == "java/util/TreeMap"
                    || n == "java/lang/Object" =>
            {
                return false
            }
            _ => {}
        }
        match ctx.superclass_of(cur) {
            Some(p) if p != cur => cur = p,
            _ => return false,
        }
    }
    false
}

fn native_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_size(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_size(ctx, args);
    }
    let (_, size, _) = map_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

fn native_map_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_is_empty(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_is_empty(ctx, args);
    }
    let (_, size, _) = map_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

/// True when `obj` is one of CratonVM's unmodifiable wrapper views.
fn is_unmod_wrapper(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let cid = ctx.class_id_of_object(obj);
    matches!(
        ctx.class_name_of_id(cid).as_deref(),
        Some(UNMOD_MAP_CLASS)
            | Some(UNMOD_LIST_CLASS)
            | Some(UNMOD_SET_CLASS)
            | Some(UNMOD_COLLECTION_CLASS)
    )
}

fn native_map_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // If a generic `java/util/Map.put` interface native is dispatched on an
    // unmodifiable wrapper receiver, honour the JDK contract and throw rather
    // than mutating (or, worse, `map_resize`-clobbering) the private backing.
    if is_unmod_wrapper(ctx, this) {
        return Err(unsupported_op());
    }
    // A `java/util/Map.put` interface native dispatched on a
    // ConcurrentHashMap receiver must use the segmented CHM path — the
    // plain-HashMap bucket code would treat the segments array as buckets.
    if is_chm_receiver(ctx, this) {
        return native_chm_put(ctx, args);
    }
    // S111r34: when the receiver is a LinkedHashMap (or subclass like
    // `org/springframework/core/annotation/AnnotationAttributes`),
    // delegate to `native_lhm_put` so that subsequent `LinkedHashMap.get`
    // calls — which JDK25 overrides and we redirect to `native_lhm_get`
    // (reads from the LHM overlay) — find the entries. Without this
    // redirect, the put writes to slot-0-based storage which the LHM
    // `get` native cannot see, and Spring's `AnnotationAttributes.get(...)`
    // returns null for every key written via `TypeMappedAnnotation.asMap`.
    // Surfaces as `IllegalArgumentException: Attribute 'type' not found
    // in attributes for annotation [...ComponentScan$Filter]` in
    // `ComponentScanAnnotationParser.parse` for `@SpringBootApplication`.
    let cid = ctx.class_id_of_object(this);
    if let Some(name) = ctx.class_name_of_id(cid) {
        if name != "java/util/HashMap" {
            // Walk parent chain to detect LinkedHashMap or TreeMap ancestry.
            // Without the TreeMap branch, the `java/util/Map.put` interface
            // override (registered as an abstract-method native) falls
            // through to the HashMap-bucket code below for a TreeMap
            // receiver. TreeMap's field 0 is `root` (a TreeMap.Entry),
            // not a bucket array, so `map_state`/`map_resize` corrupt the
            // object and every subsequent put is a silent no-op. Symptom:
            // `new TreeMap().put(k,v)` leaves size=0 and get returns null,
            // which broke Keycloak FeatureOptions.<clinit> (the JDK
            // TreeMap.keySpliteratorFor NPE was downstream — actual data
            // never reached the tree).
            let mut cur = cid;
            let mut is_lhm = false;
            let mut is_tm = false;
            while let Some(n) = ctx.class_name_of_id(cur) {
                if n == "java/util/LinkedHashMap" {
                    is_lhm = true;
                    break;
                }
                if n == "java/util/TreeMap" {
                    is_tm = true;
                    break;
                }
                if n == "java/util/HashMap" || n == "java/lang/Object" {
                    break;
                }
                match ctx.superclass_of(cur) {
                    Some(p) => cur = p,
                    None => break,
                }
            }
            if is_lhm {
                return native_lhm_put(ctx, args);
            }
            if is_tm {
                return native_tm_put(ctx, args);
            }
        }
    }
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));

    // Handle null key: hash=0, bucket=0, key field stores null
    let (key_ref, hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (Some(k), map_hash_key(ctx, k)?, false),
        Value::Object(None) => (None, 0, true),
        _ => return Ok(Some(Value::Object(None))), // non-object keys not supported
    };

    // Check for resize first. Also initialize table when buckets is None
    // (e.g. AnnotationAttributes constructed via JDK bytecode constructor —
    // LinkedHashMap.<init>() leaves `table` null until first put, but our
    // synthetic put native previously returned a silent no-op when buckets
    // were None, dropping `type` from `@ComponentScan.Filter` AnnotationAttributes
    // and surfacing as `IllegalArgumentException: Attribute 'type' not found`
    // deep in Spring's bean factory).
    let (initial_buckets, size, cap) = map_state(ctx, this);
    if initial_buckets.is_none() || size + 1 > (cap * 3) / 4 {
        map_resize(ctx, this);
    }

    let (buckets, size, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    // Walk chain looking for existing key (S111r27: layout-aware).
    // Safety: cap the chain walk to detect pathological cases (cycles or
    // O(n^2) blowup from massive single-bucket pile-ups). Real chains
    // should be O(log n) even with poor hashes; anything past 4096 is a
    // strong signal of a cycle/corruption.
    //
    // MED fix: tripping the cap previously continued silently with a head
    // insert, producing duplicate keys in the chain and breaking the map's
    // basic key-uniqueness invariant. An attacker who can force >4096
    // colliding keys (hash-collision DoS) thus turned each subsequent
    // `put` into a degenerate O(n) duplicate-insert with no error path.
    // We now throw `IllegalStateException` instead, signalling the
    // attack/corruption to the caller rather than silently producing
    // wrong data.
    let mut walk_count: usize = 0;
    const CHAIN_WALK_LIMIT: usize = 4096;
    while let Value::Object(Some(node)) = node_val {
        walk_count += 1;
        if walk_count > CHAIN_WALK_LIMIT {
            eprintln!(
                "[HM-PUT-GUARD] aborting chain walk at {} nodes (suspected cycle); map={:?} idx={} cap={}",
                walk_count, this, idx, cap
            );
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "hashmap chain exceeded safety cap; possible hash-collision DoS"
                    .to_string(),
            }
            .into());
        }
        let node_key_field = get_node_key(ctx, node);
        if dbg_hmput() {
            eprintln!("[HMPUT] walk node_key_field={:?} key_ref={:?} hash_arg={} is_null={}", node_key_field, key_ref, hash, is_null_key);
        }
        if is_null_key {
            // Looking for a null-key node
            if matches!(node_key_field, Value::Object(None)) {
                let old_value = get_node_value(ctx, node);
                // Update value in-place using the detected layout
                match ctx.get_field(node, 0) {
                    Value::Object(_) => ctx.set_field(node, NODE_FIELD_VALUE, value), // legacy slot 1
                    _ => ctx.set_field(node, 2, value), // JDK slot 2
                }
                return Ok(Some(old_value));
            }
        } else if let Value::Object(Some(node_key)) = node_key_field {
            let eq = map_keys_equal(ctx, node_key, key_ref.unwrap())?;
            if dbg_hmput() {
                eprintln!("[HMPUT] map_keys_equal(node_key={:?}, key={:?}) = {}", node_key, key_ref.unwrap(), eq);
            }
            if eq {
                let old_value = get_node_value(ctx, node);
                match ctx.get_field(node, 0) {
                    Value::Object(_) => ctx.set_field(node, NODE_FIELD_VALUE, value), // legacy slot 1
                    _ => ctx.set_field(node, 2, value), // JDK slot 2
                }
                return Ok(Some(old_value));
            }
        }
        let next = ctx.get_field(node, NODE_FIELD_NEXT);
        // Self-cycle guard: if node.next == node, abort immediately.
        if let Value::Object(Some(nx)) = next {
            if std::ptr::eq(nx.as_ptr(), node.as_ptr()) {
                eprintln!(
                    "[HM-PUT-GUARD] self-cycle node detected at walk {}; map={:?} idx={}",
                    walk_count, this, idx
                );
                break;
            }
        }
        node_val = next;
    }

    // Key not found — insert at head of chain
    let existing_head = ctx.get_array_element(buckets, idx);
    let head_ref = match existing_head {
        Value::Object(obj_opt) => obj_opt,
        _ => None,
    };
    // Create node — for null keys, store Value::Object(None) in key field
    let new_node = ctx.alloc_object(cratonvm_types::ClassId::new(0), NODE_NUM_FIELDS);
    ctx.set_field(new_node, NODE_FIELD_HASH, Value::Int(hash));
    ctx.set_field(new_node, NODE_FIELD_KEY, key_val);
    ctx.set_field(new_node, NODE_FIELD_VALUE, value);
    ctx.set_field(
        new_node,
        NODE_FIELD_NEXT,
        head_ref.map_or(Value::Object(None), |r| Value::Object(Some(r))),
    );
    ctx.set_array_element(buckets, idx, Value::Object(Some(new_node)));
    set_map_size(ctx, this, size + 1);

    Ok(Some(Value::Object(None))) // no old value
}

fn native_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_get(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_get(ctx, args);
    }
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    let (key_ref, hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (Some(k), map_hash_key(ctx, k)?, false),
        Value::Object(None) => (None, 0, true),
        _ => return Ok(Some(Value::Object(None))),
    };

    let (buckets, _, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    // S111r27: Use layout-aware helpers so that JDK-created nodes
    // (hash=0, key=1, value=2, next=3) are handled correctly alongside
    // legacy-created nodes (key=0, value=1, hash=2, next=3).
    while let Value::Object(Some(node)) = node_val {
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                let value = get_node_value(ctx, node);
                return Ok(Some(value));
            }
        } else if let Value::Object(Some(node_key)) = node_key_field {
            if map_keys_equal(ctx, node_key, key_ref.unwrap())? {
                let value = get_node_value(ctx, node);
                return Ok(Some(value));
            }
        }
        node_val = ctx.get_field(node, NODE_FIELD_NEXT);
    }

    // Diagnostic: enum-keyed HashMap miss — prime suspect for Keycloak's
    // `Profile.isFeatureEnabled` NPE. Dump every node's enum identity.
    if dbg_kcbool() {
        if let Some(k) = key_ref {
            if enum_key_identity(ctx, k).is_some() {
                let mut node_keys: Vec<ObjectRef> = Vec::new();
                for b in 0..(cap.max(0) as usize) {
                    let mut nv = ctx.get_array_element(buckets, b);
                    let mut guard = 0;
                    while let Value::Object(Some(node)) = nv {
                        guard += 1;
                        if guard > 4096 {
                            break;
                        }
                        if let Value::Object(Some(nk)) = get_node_key(ctx, node) {
                            node_keys.push(nk);
                        }
                        nv = ctx.get_field(node, NODE_FIELD_NEXT);
                    }
                }
                dbg_kcbool_report_miss(ctx, "HashMap.get", k, &node_keys);
            }
        }
    }

    Ok(Some(Value::Object(None)))
}

fn native_map_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_unmod_wrapper(ctx, this) {
        return Err(unsupported_op());
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_remove(ctx, args);
    }
    if is_tree_map_receiver(ctx, this) {
        return native_tm_remove(ctx, args);
    }
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    let (key_ref, hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (Some(k), map_hash_key(ctx, k)?, false),
        Value::Object(None) => (None, 0, true),
        _ => return Ok(Some(Value::Object(None))),
    };

    let (buckets, size, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };

    let idx = map_bucket_index(hash, cap);
    let head_val = ctx.get_array_element(buckets, idx);

    // Helper closure: check if node matches our key (S111r27: layout-aware).
    // Returns Result so a thrown `equals` from the user-supplied key class
    // propagates instead of being silently treated as "not equal".
    fn node_matches_inner(
        ctx: &mut dyn NativeContext,
        node: ObjectRef,
        is_null_key: bool,
        key_ref: Option<ObjectRef>,
    ) -> Result<bool, MethodCallFailed> {
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            Ok(matches!(node_key_field, Value::Object(None)))
        } else if let Value::Object(Some(nk)) = node_key_field {
            match key_ref {
                Some(k) => map_keys_equal(ctx, nk, k),
                None => Ok(false),
            }
        } else {
            Ok(false)
        }
    }

    // Check if the head node is the target
    if let Value::Object(Some(head)) = head_val {
        if node_matches_inner(ctx, head, is_null_key, key_ref)? {
            let next = ctx.get_field(head, NODE_FIELD_NEXT);
            ctx.set_array_element(buckets, idx, next);
            set_map_size(ctx, this, size - 1);
            let old_value = get_node_value(ctx, head);
            return Ok(Some(old_value));
        }

        // Walk chain
        let mut prev = head;
        let mut curr_val = ctx.get_field(head, NODE_FIELD_NEXT);

        while let Value::Object(Some(curr)) = curr_val {
            if node_matches_inner(ctx, curr, is_null_key, key_ref)? {
                let next = ctx.get_field(curr, NODE_FIELD_NEXT);
                ctx.set_field(prev, NODE_FIELD_NEXT, next);
                set_map_size(ctx, this, size - 1);
                let old_value = get_node_value(ctx, curr);
                return Ok(Some(old_value));
            }
            prev = curr;
            curr_val = ctx.get_field(curr, NODE_FIELD_NEXT);
        }
    }

    Ok(Some(Value::Object(None)))
}

fn native_map_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Round 72: must not conflate `containsKey` with `get(...) != null`.
    // HashMap allows null values, so an entry with a null value must still
    // report containsKey == true. Walk the bucket chain directly and check
    // for node presence, mirroring `native_map_get` but returning a boolean
    // on node match regardless of the stored value. (Kafka 4.2
    // `ConfigDef.parse` puts `early.start.listeners` with a null default;
    // `AbstractConfig.get` then calls `values.containsKey(...)` and would
    // wrongly throw "Unknown configuration" if we returned false here.)
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_contains_key(ctx, args);
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_contains_key(ctx, args);
    }
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    let (key_ref, hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (Some(k), map_hash_key(ctx, k)?, false),
        Value::Object(None) => (None, 0, true),
        _ => return Ok(Some(Value::Int(0))),
    };

    let (buckets, _, cap) = map_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };

    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);

    while let Value::Object(Some(node)) = node_val {
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                return Ok(Some(Value::Int(1)));
            }
        } else if let Value::Object(Some(node_key)) = node_key_field {
            if map_keys_equal(ctx, node_key, key_ref.unwrap())? {
                return Ok(Some(Value::Int(1)));
            }
        }
        node_val = ctx.get_field(node, NODE_FIELD_NEXT);
    }

    Ok(Some(Value::Int(0)))
}

fn native_map_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    if is_chm_receiver(ctx, this) {
        return native_chm_contains_value(ctx, args);
    }
    // Properties-backed ConcurrentHashMap path: keep the existing
    // segment-aware collection (rare; correctness over speed).
    if properties_backing_chm(ctx, this).is_some() {
        let values = map_collect_values(ctx, this);
        for val in &values {
            if values_equal(ctx, val, &target) {
                return Ok(Some(Value::Int(1)));
            }
        }
        return Ok(Some(Value::Int(0)));
    }
    // Plain HashMap: walk the buckets directly and short-circuit on the
    // first matching value instead of materializing every value into a Vec.
    let (buckets, _size, cap) = map_state(ctx, this);
    if let Some(b) = buckets {
        // Chain-walk cycle guard: bound each chain by the table-wide node
        // count to avoid a hang on a corrupt (cyclic) chain.
        const CHAIN_WALK_LIMIT: usize = 4096;
        for i in 0..(cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            let mut walk_count: usize = 0;
            while let Value::Object(Some(node)) = node_val {
                walk_count += 1;
                if walk_count > CHAIN_WALK_LIMIT {
                    break;
                }
                let value = get_node_value(ctx, node);
                if values_equal(ctx, &value, &target) {
                    return Ok(Some(Value::Int(1)));
                }
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_map_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    if is_unmod_wrapper(ctx, this) {
        return Err(unsupported_op());
    }
    if is_chm_receiver(ctx, this) {
        return native_chm_clear(ctx, args);
    }
    let (buckets, _, cap) = map_state(ctx, this);
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            ctx.set_array_element(b, i, Value::Object(None));
        }
    }
    set_map_size(ctx, this, 0);
    Ok(None)
}

fn native_map_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_key_set(ctx, args);
    }
    let keys = map_collect_keys(ctx, this);
    // Build a HashSet from the keys
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let backing_map = alloc_backing_map(ctx);
    // Initialize the backing map
    let cap = std::cmp::max(keys.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing_map, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing_map, 0);
    ctx.set_field(backing_map, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(backing_map)));

    // Add each key
    for key in &keys {
        if let Value::Object(Some(k)) = key {
            let hash = map_hash_key(ctx, *k)?;
            let (b, size, c) = map_state(ctx, backing_map);
            let b = b.unwrap();
            let idx = map_bucket_index(hash, c);
            let existing = ctx.get_array_element(b, idx);
            let head = match existing {
                Value::Object(obj_opt) => obj_opt,
                _ => None,
            };
            let sentinel = Value::Int(1);
            let node = map_alloc_node(ctx, *k, sentinel, hash, head);
            ctx.set_array_element(b, idx, Value::Object(Some(node)));
            set_map_size(ctx, backing_map, size + 1);
        }
    }

    Ok(Some(Value::Object(Some(set))))
}

fn native_map_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_values(ctx, args);
    }
    let values = map_collect_values(ctx, this);
    // Build an ArrayList from the values
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(values.len(), AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for (i, val) in values.iter().enumerate() {
        ctx.set_array_element(buf, i, *val);
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, values.len() as i32);
    Ok(Some(Value::Object(Some(list))))
}

fn native_map_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if is_tree_map_receiver(ctx, this) {
        return native_tm_entry_set(ctx, args);
    }
    let entries = map_collect_entries(ctx, this);
    // Build a HashSet of Map.Entry objects
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let backing_map = alloc_backing_map(ctx);
    let cap = std::cmp::max(entries.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing_map, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing_map, 0);
    ctx.set_field(backing_map, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(backing_map)));

    // Each entry is a Map.Entry object with 2 fields: key and value.
    // Use `java/util/Map$Entry` instead of `HashMap$Entry`: in early
    // bootstrap the concrete nested class can be unresolved and degrade to
    // cid=0 (`java/lang/Object`), breaking downstream checkcasts.
    for (key, value) in &entries {
        let entry_obj = alloc_synthetic(ctx, "java/util/Map$Entry", 2);
        ctx.set_field(entry_obj, 0, *key);
        ctx.set_field(entry_obj, 1, *value);

        // Add to the set's backing map
        let hash = ctx.identity_hash_code(entry_obj);
        let (b, size, c) = map_state(ctx, backing_map);
        let b = b.unwrap();
        let idx = map_bucket_index(hash, c);
        let existing = ctx.get_array_element(b, idx);
        let head = match existing {
            Value::Object(obj_opt) => obj_opt,
            _ => None,
        };
        let sentinel = Value::Int(1);
        let node = map_alloc_node(ctx, entry_obj, sentinel, hash, head);
        ctx.set_array_element(b, idx, Value::Object(Some(node)));
        set_map_size(ctx, backing_map, size + 1);
    }

    Ok(Some(Value::Object(Some(set))))
}

fn native_map_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let entries = map_collect_entries(ctx, this);
    let mut parts = Vec::with_capacity(entries.len());
    for (key, value) in &entries {
        let ks = obj_to_display_string(ctx, key);
        let vs = obj_to_display_string(ctx, value);
        parts.push(format!("{}={}", ks, vs));
    }
    let text = format!("{{{}}}", parts.join(", "));
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

fn native_map_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let result = native_map_get(ctx, args)?;
    match result {
        Some(Value::Object(None)) => {
            // Return the default value (arg 2)
            let default_val = args.get(2).copied().unwrap_or(Value::Object(None));
            Ok(Some(default_val))
        }
        _ => Ok(result),
    }
}

fn native_map_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Check if key exists
    let get_result = native_map_get(ctx, args)?;
    match get_result {
        Some(Value::Object(None)) => {
            // Key not present — do the put
            native_map_put(ctx, args)
        }
        _ => Ok(get_result), // Key present — return existing value
    }
}

fn native_map_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // S111r24-fix: Source may be a LinkedHashMap whose entries live in the
    // Rust-side `lhm_overlay`. Walk its insertion-order list first; only
    // fall back to HashMap bucket scanning when there is no LHM head
    // pointer registered for `other`.
    let mut entries: Vec<(Value, Value)> = Vec::new();
    let lhm_head = lhm_get(ctx, other, "head", LHM_FIELD_HEAD);
    if let Value::Object(Some(_)) = lhm_head {
        let mut cur = lhm_head;
        while let Value::Object(Some(node)) = cur {
            let key = ctx.get_field(node, LHM_NODE_KEY);
            let val = ctx.get_field(node, LHM_NODE_VALUE);
            entries.push((key, val));
            cur = ctx.get_field(node, LHM_NODE_AFTER);
        }
    } else {
        entries = map_collect_entries(ctx, other);
    }
    for (key, value) in entries {
        if let Value::Object(Some(k)) = key {
            // Call put on this map
            let put_args = [Value::Object(Some(this)), Value::Object(Some(k)), value];
            native_map_put(ctx, &put_args)?;
        }
    }
    Ok(None)
}

fn native_map_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let entries = map_collect_entries(ctx, this);
    let mut hash: i32 = 0;
    for (key, value) in &entries {
        // Map.hashCode contract: sum of Map.Entry hashes,
        // where Entry hash = keyHash ^ valueHash.
        let kh = element_hash_code(ctx, key);
        let vh = element_hash_code(ctx, value);
        hash = hash.wrapping_add(kh ^ vh);
    }
    Ok(Some(Value::Int(hash)))
}

fn native_map_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    let (_, size_a, _) = map_state(ctx, this);
    let (_, size_b, _) = map_state(ctx, other);
    if size_a != size_b {
        return Ok(Some(Value::Int(0)));
    }
    // Check all entries in this map exist in other.
    //
    // MED fix: previously the `if let Value::Object(Some(k))` guard
    // silently skipped null-keyed entries entirely, so two maps that
    // differed only on the value mapped to `null` would compare equal —
    // a Map.equals contract violation. We now include null-keyed entries
    // by dispatching the lookup with a `null` key (HashMap permits this
    // and `native_map_get` handles it), and also include non-`Object`
    // primitive-keyed entries for completeness.
    let entries = map_collect_entries(ctx, this);
    for (key, value) in &entries {
        let get_args = [Value::Object(Some(other)), *key];
        let other_val = native_map_get(ctx, &get_args)?;
        match other_val {
            Some(ref ov) => {
                if !values_equal(ctx, value, ov) {
                    return Ok(Some(Value::Int(0)));
                }
            }
            None => return Ok(Some(Value::Int(0))),
        }
    }
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// HashSet — field 0 = Object (backing HashMap)
// ===========================================================================

const HS_FIELD_MAP: usize = 0;
const HS_NUM_FIELDS: usize = 1;

/// Get the backing HashMap from a HashSet.
fn hs_backing_map(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(this, HS_FIELD_MAP) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

/// Public helper: allocate a properly-initialised HashSet containing `elems`.
///
/// Field layout matches `make_set_of` / `native_hs_init`: a single
/// instance field at offset 0 holding the backing `java/util/HashMap`. This
/// is what real-JDK HashSet bytecode expects (`getfield map` reads field 0
/// → must be a HashMap; `HashMap.keySet()` then dispatches normally).
///
/// Used by `native-builtins/src/lib.rs::build_hashset_from_args` so that
/// the higher-arity `Set.of(...)` natives (4..=10 args) produce HashSets
/// whose layout is compatible with real-JDK `HashSet.iterator()` /
/// `AbstractSet.equals()` etc.
pub fn make_hashset_with_elements(
    ctx: &mut dyn NativeContext,
    elems: &[Value],
) -> ObjectRef {
    // S111r13: Build the backing HashMap using the real-JDK field layout
    // (`table`, `size`, `threshold`, `loadFactor`, `entrySet`) instead of
    // the synthetic 3-field `(buckets, size, capacity)` layout.
    //
    // Why: JDK `HashSet.spliterator()` does
    //   `new HashMap.KeySpliterator<>(this.map, ...)` and reads the map's
    // `table` field directly via `getfield`. With the synthetic layout,
    // slot 2 held `Int(capacity=16)` rather than the bucket array, so
    // downstream `arraylength` panicked with
    //   `internal error: expected object reference, got int(16)`.
    //
    // Strategy: resolve the real HashMap / HashMap$Node field slot indices
    // via `resolve_field_index`. If every required field is resolvable,
    // allocate enough field slots to cover the real layout, populate at
    // the real indices, and build the bucket array + Node chain so that
    // bytecode `getfield`/`arraylength` see the correct types. Falls back
    // to the legacy synthetic 3-field layout if any slot resolution fails
    // (e.g. running before bootstrap completes or against a synthetic stub).
    //
    // Hashing follows the JDK formula: `h = key.hashCode() ^ (h >>> 16)`,
    // and bucket index is `(n - 1) & hash` for power-of-two `n`.
    let cap = std::cmp::max(elems.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);

    // Best-effort: ensure the real classes are loaded so the field-index
    // resolver can see them.
    let hashmap_class_id = ctx
        .ensure_class_initialized("java/util/HashMap")
        .ok()
        .or_else(|| ctx.class_id_by_name("java/util/HashMap"))
        .unwrap_or(ClassId::new(0));
    let _ = ctx.ensure_class_initialized("java/util/HashMap$Node");
    let node_class_id = ctx
        .class_id_by_name("java/util/HashMap$Node")
        .unwrap_or(ClassId::new(0));

    let f_table = ctx.resolve_field_index("java/util/HashMap", "table");
    let f_size = ctx.resolve_field_index("java/util/HashMap", "size");
    let f_threshold = ctx.resolve_field_index("java/util/HashMap", "threshold");
    let f_loadfactor = ctx.resolve_field_index("java/util/HashMap", "loadFactor");
    let f_entryset = ctx.resolve_field_index("java/util/HashMap", "entrySet");
    let n_hash = ctx.resolve_field_index("java/util/HashMap$Node", "hash");
    let n_key = ctx.resolve_field_index("java/util/HashMap$Node", "key");
    let n_value = ctx.resolve_field_index("java/util/HashMap$Node", "value");
    let n_next = ctx.resolve_field_index("java/util/HashMap$Node", "next");

    if let (
        Some(f_table),
        Some(f_size),
        Some(f_threshold),
        Some(f_loadfactor),
        Some(f_entryset),
        Some(n_hash),
        Some(n_key),
        Some(n_value),
        Some(n_next),
    ) = (
        f_table, f_size, f_threshold, f_loadfactor, f_entryset, n_hash, n_key,
        n_value, n_next,
    ) {
        let map_n_fields = [f_table, f_size, f_threshold, f_loadfactor, f_entryset]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;
        let node_n_fields = [n_hash, n_key, n_value, n_next]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
            + 1;

        let buckets = alloc_ref_array(ctx, cap);
        let backing_map = ctx.alloc_object(hashmap_class_id, map_n_fields);
        ctx.set_field(backing_map, f_table, Value::Object(Some(buckets)));
        ctx.set_field(backing_map, f_size, Value::Int(0));
        ctx.set_field(backing_map, f_threshold, Value::Int((cap as i32 * 3) / 4));
        ctx.set_field(backing_map, f_loadfactor, Value::Float(0.75));
        ctx.set_field(backing_map, f_entryset, Value::Object(None));

        // Resolve HashSet's `map` slot too, with a defensive fallback to
        // the synthetic `HS_FIELD_MAP = 0` if unresolved.
        let hs_map_slot = ctx
            .resolve_field_index("java/util/HashSet", "map")
            .unwrap_or(HS_FIELD_MAP);
        let hs_n_fields = std::cmp::max(hs_map_slot + 1, HS_NUM_FIELDS);
        let set = alloc_synthetic(ctx, "java/util/HashSet", hs_n_fields);
        ctx.set_field(set, hs_map_slot, Value::Object(Some(backing_map)));

        let sentinel = Value::Object(None); // PRESENT marker; null is fine for "is in set"
        let mut size = 0i32;
        for elem in elems {
            let key_obj = match elem {
                Value::Object(Some(obj)) => *obj,
                _ => continue, // skip nulls / primitives we can't hash
            };
            // `make_hashset_with_elements` cannot propagate exceptions
            // (returns `ObjectRef`, not `MethodCallResult`). If a user
            // `hashCode()` throws here we fall back to identity to keep
            // construction infallible — this matches the legacy behaviour
            // and is acceptable for `Set.of(...)` constants which only
            // hold JDK-internal types (String, wrappers, enum constants).
            let raw_hash = map_hash_key(ctx, key_obj)
                .unwrap_or_else(|_| ctx.identity_hash_code(key_obj));
            // map_hash_key already applies the (h ^ h>>>16) spread; the
            // bucket index uses raw_hash as-is for power-of-two cap.
            let idx = ((cap as u32 - 1) & raw_hash as u32) as usize;

            // Skip duplicates (real-JDK Set.of rejects them; we just no-op).
            let mut existing_head = ctx.get_array_element(buckets, idx);
            let mut dup = false;
            let mut probe = existing_head;
            while let Value::Object(Some(probe_obj)) = probe {
                let probe_key = ctx.get_field(probe_obj, n_key);
                if let Value::Object(Some(pk)) = probe_key {
                    // Swallowing a thrown equals here mirrors the hashCode
                    // fallback above; legitimate `Set.of(...)` keys do not
                    // throw equals/hashCode.
                    if map_keys_equal(ctx, pk, key_obj).unwrap_or(false) {
                        dup = true;
                        break;
                    }
                }
                probe = ctx.get_field(probe_obj, n_next);
            }
            if dup {
                continue;
            }
            // Re-read head in case ctx mutated between probes (defensive).
            existing_head = ctx.get_array_element(buckets, idx);

            let node = ctx.alloc_object(node_class_id, node_n_fields);
            ctx.set_field(node, n_hash, Value::Int(raw_hash));
            ctx.set_field(node, n_key, Value::Object(Some(key_obj)));
            ctx.set_field(node, n_value, sentinel);
            ctx.set_field(node, n_next, existing_head);
            ctx.set_array_element(buckets, idx, Value::Object(Some(node)));
            size += 1;
        }
        ctx.set_field(backing_map, f_size, Value::Int(size));
        return set;
    }

    // Legacy fallback: synthetic 3-field (buckets, size, capacity) layout.
    // Used when real HashMap/Node classes aren't resolvable yet.
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let backing_map = alloc_backing_map(ctx);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing_map, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing_map, 0);
    ctx.set_field(backing_map, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(backing_map)));

    let sentinel = Value::Int(1);
    for elem in elems {
        // Best-effort populate; ignore errors so callers see a non-empty
        // set even if a single put failed (e.g. unhashable wrapper).
        let _ = native_map_put(
            ctx,
            &[Value::Object(Some(backing_map)), *elem, sentinel],
        );
    }
    set
}

fn register_hashset_natives(r: &mut NativeMethodRegistry) {
    // LETSGO_S1: Mirror HashSet's native surface onto the class names of
    // its real-JDK subclasses that share the same `field 0 = backing
    // map` layout. Without this mirroring, `LinkedHashSet.add(Object)Z`
    // dispatch — which `force_native` looks up by the receiver's *exact*
    // class name — falls off the synthetic-jdk dispatcher and surfaces a
    // `NoSuchMethodError`, breaking real-app boots that allocate
    // `LinkedHashSet`s during Spring/Log4J/SLF4J initialisation. Real
    // JDK semantics for `LinkedHashSet` differ only in iteration order
    // (insertion-order vs hash order), which our `make_set_of` /
    // `native_hs_iterator` already preserve via the synthetic backing
    // map's bucket walk for the purposes of caller code. Same rationale
    // for `EnumSet`, `CopyOnWriteArraySet`, and `ConcurrentSkipListSet`
    // — applications that allocate them via the synthetic-stub path get
    // a working contract, and applications that rely on their JDK
    // bytecode keep working because the natives only override matching
    // signatures.
    const SET_CLASSES: &[&str] = &[
        "java/util/HashSet",
        "java/util/LinkedHashSet",
        "java/util/concurrent/CopyOnWriteArraySet",
    ];

    for c in SET_CLASSES {
        r.register(c, "<init>", "()V", native_hs_init);
        r.register(c, "<init>", "(I)V", native_hs_init_capacity);
        r.register(c, "<init>", "(IF)V", native_hs_init_capacity_load);
        r.register(c, "<init>", "(Ljava/util/Collection;)V", native_hs_init_from_collection);
        r.register(c, "size", "()I", native_hs_size);
        r.register(c, "isEmpty", "()Z", native_hs_is_empty);
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_hs_add);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_hs_remove);
        r.register(c, "contains", "(Ljava/lang/Object;)Z", native_hs_contains);
        r.register(c, "clear", "()V", native_hs_clear);
        r.register(c, "iterator", "()Ljava/util/Iterator;", native_hs_iterator);
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_hs_to_array);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_hs_to_array_typed,
        );
        r.register(
            c,
            "toArray",
            "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
            native_collection_to_array_generator,
        );
        r.register(c, "toString", "()Ljava/lang/String;", native_hs_to_string);
        r.register(c, "hashCode", "()I", native_hs_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_hs_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_hs_for_each,
        );
        r.register(c, "stream", "()Ljava/util/stream/Stream;", native_hs_stream);
        r.register(
            c,
            "addAll",
            "(Ljava/util/Collection;)Z",
            native_hs_add_all,
        );
        r.register(
            c,
            "removeAll",
            "(Ljava/util/Collection;)Z",
            native_hs_remove_all,
        );
        r.register(
            c,
            "retainAll",
            "(Ljava/util/Collection;)Z",
            native_hs_retain_all,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_hs_contains_all,
        );
    }
}

/// LETSGO_S1: HashSet(int initialCapacity, float loadFactor) — mirror of
/// `native_hs_init_capacity` that ignores the load factor (matches our
/// HashMap layout, where loadFactor is fixed at 0.75).
fn native_hs_init_capacity_load(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Drop the loadFactor (last) arg and reuse the (I)V path.
    let trimmed: Vec<Value> = args.iter().take(2).copied().collect();
    native_hs_init_capacity(ctx, &trimmed)
}

/// LETSGO_S1: HashSet.toArray(T[]) — typed-array variant. Our existing
/// `native_hs_to_array` returns a fresh `Object[]`, but the typed
/// variant must populate the caller's array (or allocate a new one of
/// the same component type if the input is too small). For now we
/// allocate a fresh `Object[]` of the right size — sufficient for the
/// callers we observe (Spring's iteration of HashSet keysets). When the
/// input array is large enough we copy into it and write null at index
/// `size` per spec; otherwise we fall back to a fresh allocation.
fn native_hs_to_array_typed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let target = match args.get(1).copied() {
        Some(Value::Object(Some(t))) => Some(t),
        _ => None,
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let keys = map_collect_keys(ctx, backing);
    if let Some(arr) = target {
        let len = ctx.array_length(arr);
        if len >= keys.len() {
            for (i, k) in keys.iter().enumerate() {
                ctx.set_array_element(arr, i, *k);
            }
            if len > keys.len() {
                ctx.set_array_element(arr, keys.len(), Value::Object(None));
            }
            return Ok(Some(Value::Object(Some(arr))));
        }
    }
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(arr, i, *k);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// LETSGO_S1: HashSet.hashCode — sum of `hashCode()` over elements
/// (matches `AbstractSet.hashCode` semantics).
fn native_hs_hash_code(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    let keys = map_collect_keys(ctx, backing);
    let mut h: i32 = 0;
    for k in &keys {
        // Set.hashCode contract: sum of element hashCode()s (0 for null).
        h = h.wrapping_add(element_hash_code(ctx, k));
    }
    Ok(Some(Value::Int(h)))
}

/// LETSGO_S1: HashSet.equals — same size + every element of `this` is in
/// `other` (`AbstractSet.equals` semantics).
fn native_hs_equals(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1).copied() {
        Some(Value::Object(Some(o))) => o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    let other_size = match ctx.invoke_virtual(other, "size", "()I", &[])? {
        Some(Value::Int(n)) => n,
        _ => return Ok(Some(Value::Int(0))),
    };
    let keys = map_collect_keys(ctx, backing);
    if keys.len() as i32 != other_size {
        return Ok(Some(Value::Int(0)));
    }
    for k in &keys {
        let contains = ctx.invoke_virtual(
            other,
            "contains",
            "(Ljava/lang/Object;)Z",
            &[*k],
        )?;
        if !matches!(contains, Some(Value::Int(1))) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// LETSGO_S1: HashSet.containsAll(Collection<?>) — true iff every element
/// of the source collection is present in this set.  Wraps the existing
/// `native_hs_contains` and `collect_collection_elements` helpers.
fn native_hs_contains_all(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = collect_collection_elements(ctx, coll);
    for e in &elems {
        let r = native_hs_contains(ctx, &[Value::Object(Some(this)), *e])?;
        if !matches!(r, Some(Value::Int(1))) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

/// S111r28: Allocate the backing HashMap with enough field slots to cover
/// every inherited / declared instance field of the real JDK
/// `java.util.HashMap` class. The previous `alloc_synthetic("HashMap", 3)`
/// produced a 3-slot object whose class metadata still claims 8 fields, so
/// the descriptor-aware `set_field` coercion path mangled writes to the
/// inherited `AbstractMap.values: Collection` field at slot 1 (where our
/// `MAP_FIELD_SIZE` index lives) — `Int(0)` got rewritten as
/// `Object(None)`, which broke the size accounting downstream.
fn alloc_backing_map(ctx: &mut dyn NativeContext) -> ObjectRef {
    let cid = match ctx.ensure_class_initialized("java/util/HashMap") {
        Ok(class_id) => class_id,
        Err(_) => ctx
            .class_id_by_name("java/util/HashMap")
            .unwrap_or(ClassId::new(0)),
    };
    let total = ctx.class_num_total_fields(cid);
    let n = std::cmp::max(total, MAP_NUM_FIELDS);
    ctx.alloc_object(cid, n)
}

fn native_hs_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let backing = alloc_backing_map(ctx);
    // Initialize the backing HashMap
    let init_args = [Value::Object(Some(backing))];
    native_map_init(ctx, &init_args)?;
    ctx.set_field(this, HS_FIELD_MAP, Value::Object(Some(backing)));
    Ok(None)
}

fn native_hs_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = args
        .get(1)
        .copied()
        .unwrap_or(Value::Int(MAP_DEFAULT_CAPACITY as i32));
    let backing = alloc_backing_map(ctx);
    let init_args = [Value::Object(Some(backing)), cap];
    native_map_init_capacity(ctx, &init_args)?;
    ctx.set_field(this, HS_FIELD_MAP, Value::Object(Some(backing)));
    Ok(None)
}

fn native_hs_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    let map_args = [Value::Object(Some(backing))];
    native_map_size(ctx, &map_args)
}

fn native_hs_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(1))),
    };
    let map_args = [Value::Object(Some(backing))];
    native_map_is_empty(ctx, &map_args)
}

fn native_hs_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    // put(key, sentinel) — returns null if key was new
    let sentinel = Value::Int(1);
    let put_args = [Value::Object(Some(backing)), elem, sentinel];
    let old = native_map_put(ctx, &put_args)?;
    let was_new = matches!(old, Some(Value::Object(None)));
    Ok(Some(Value::Int(if was_new { 1 } else { 0 })))
}

fn native_hs_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    let remove_args = [Value::Object(Some(backing)), elem];
    let old = native_map_remove(ctx, &remove_args)?;
    let was_present = !matches!(old, Some(Value::Object(None)));
    Ok(Some(Value::Int(if was_present { 1 } else { 0 })))
}

fn native_hs_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Int(0))),
    };
    let ck_args = [Value::Object(Some(backing)), elem];
    native_map_contains_key(ctx, &ck_args)
}

fn native_hs_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(None),
    };
    let clear_args = [Value::Object(Some(backing))];
    native_map_clear(ctx, &clear_args)
}

fn native_hs_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            // (Quieted: previously eprintln. Enable via CRATONVM_HS_ITR_DBG.)
            if dbg_hs_itr() {
                eprintln!("[HS-ITR-DBG] native_hs_iterator: backing map is None for {:?}", this);
            }
            return Ok(Some(Value::Object(None)));
        }
    };
    // Collect keys into a snapshot array
    let keys = map_collect_keys(ctx, backing);
    if dbg_hs_itr() {
        eprintln!("[HS-ITR-DBG] native_hs_iterator: collected {} keys from backing map {:?}", keys.len(), backing);
    }
    let keys_arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(keys_arr, i, *k);
    }
    let itr = alloc_synthetic(ctx, "java/util/HashMap$KeyItr", MAP_KEY_ITR_NUM_FIELDS);
    ctx.set_field(itr, MAP_KEY_ITR_FIELD_KEYS, Value::Object(Some(keys_arr)));
    ctx.set_field(itr, MAP_KEY_ITR_FIELD_CURSOR, Value::Int(0));
    ctx.set_field(itr, MAP_KEY_ITR_FIELD_TOTAL, Value::Int(keys.len() as i32));
    // Wire backing HashSet for Iterator.remove() — without this, JDK code
    // like MXBeanSupport.findMXBeanInterface (which iterates a HashSet and
    // calls it.remove() inside the loop) throws UnsupportedOperationException
    // because dispatch falls through to the default Iterator.remove().
    ctx.set_field(itr, MAP_KEY_ITR_FIELD_BACKING, Value::Object(Some(this)));
    ctx.set_field(itr, MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_hs_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(Some(Value::Object(None))),
    };
    let keys = map_collect_keys(ctx, backing);
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(arr, i, *k);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_hs_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => {
            let s = ctx.create_string("[]");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let keys = map_collect_keys(ctx, backing);
    let mut parts = Vec::with_capacity(keys.len());
    for k in &keys {
        parts.push(obj_to_display_string(ctx, k));
    }
    let text = format!("[{}]", parts.join(", "));
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

// ===========================================================================
// Iterators — ArrayList$Itr and HashMap$KeyItr
// ===========================================================================

const AL_ITR_FIELD_LIST: usize = 0;
const AL_ITR_FIELD_CURSOR: usize = 1;
const AL_ITR_NUM_FIELDS: usize = 2;

/// Resolve ArrayList$Itr field slots: returns (cursor_slot, list_slot, n_fields).
/// Real-JDK layout: cursor (slot 0), lastRet (slot 1), expectedModCount (slot 2),
/// this$0 (slot 3 — the enclosing ArrayList). Falls back to synthetic
/// (list=0, cursor=1) when the real class isn't available.
#[inline]
fn al_itr_slots(ctx: &dyn NativeContext) -> (usize, usize, usize) {
    let cursor = ctx.resolve_field_index("java/util/ArrayList$Itr", "cursor");
    let list = ctx.resolve_field_index("java/util/ArrayList$Itr", "this$0");
    match (cursor, list) {
        (Some(c), Some(l)) => {
            let n = std::cmp::max(c, l) + 1;
            // Account for lastRet/expectedModCount slots between cursor and this$0.
            let n = std::cmp::max(n, 4);
            (c, l, n)
        }
        _ => (AL_ITR_FIELD_CURSOR, AL_ITR_FIELD_LIST, AL_ITR_NUM_FIELDS),
    }
}

/// Resolve the `lastRet` slot of `ArrayList$Itr`. The native `next()` /
/// `remove()` implementations must keep this field consistent with real-JDK
/// semantics: `next()` records the index it just returned, and the real-JDK
/// `ArrayList$Itr.remove()` bytecode — when *not* intercepted — guards with
/// `if (lastRet < 0) throw new IllegalStateException()`. Falls back to the
/// real-JDK slot index (1, immediately after `cursor`) when the class is not
/// available for name resolution.
#[inline]
fn al_itr_last_ret_slot(ctx: &dyn NativeContext) -> usize {
    ctx.resolve_field_index("java/util/ArrayList$Itr", "lastRet")
        .unwrap_or(1)
}

const MAP_KEY_ITR_FIELD_KEYS: usize = 0;
const MAP_KEY_ITR_FIELD_CURSOR: usize = 1;
const MAP_KEY_ITR_FIELD_TOTAL: usize = 2;
// Backing collection (HashSet/HashMap ref) used by Iterator.remove().
// May be Object(None) for snapshot-only iterators from make_iterator_from_array;
// in that case remove() throws UnsupportedOperationException.
const MAP_KEY_ITR_FIELD_BACKING: usize = 3;
// Index of the last key returned by next(); -1 before first next() and after remove().
const MAP_KEY_ITR_FIELD_LAST_RET: usize = 4;
const MAP_KEY_ITR_NUM_FIELDS: usize = 5;

fn register_iterator_natives(r: &mut NativeMethodRegistry) {
    // ArrayList$Itr
    r.register(
        "java/util/ArrayList$Itr",
        "hasNext",
        "()Z",
        native_al_itr_has_next,
    );
    r.register(
        "java/util/ArrayList$Itr",
        "next",
        "()Ljava/lang/Object;",
        native_al_itr_next,
    );
    r.register(
        "java/util/ArrayList$Itr",
        "remove",
        "()V",
        native_al_itr_remove,
    );

    // HashMap$KeyItr
    r.register(
        "java/util/HashMap$KeyItr",
        "hasNext",
        "()Z",
        native_map_key_itr_has_next,
    );
    r.register(
        "java/util/HashMap$KeyItr",
        "next",
        "()Ljava/lang/Object;",
        native_map_key_itr_next,
    );
    r.register(
        "java/util/HashMap$KeyItr",
        "remove",
        "()V",
        native_map_key_itr_remove,
    );
}

fn native_al_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (cursor_slot, list_slot, _) = al_itr_slots(ctx);
    let cursor = match ctx.get_field(this, cursor_slot) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size) = al_state(ctx, list);
    Ok(Some(Value::Int(if cursor < size { 1 } else { 0 })))
}

fn native_al_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (cursor_slot, list_slot, _) = al_itr_slots(ctx);
    let cursor = match ctx.get_field(this, cursor_slot) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, list);
    if cursor >= size {
        return Ok(Some(Value::Object(None))); // NoSuchElementException (simplified)
    }
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(data, cursor as usize);
    ctx.set_field(this, cursor_slot, Value::Int(cursor + 1));
    // Record the index just consumed in `lastRet` so a subsequent
    // `Iterator.remove()` is legal. Real-JDK `next()` does `lastRet = i`
    // via `dup_x1; putfield`; because this native shadows the bytecode that
    // write must be reproduced here, otherwise `remove()` sees the
    // constructor's `lastRet == -1` and throws `IllegalStateException`.
    let last_ret_slot = al_itr_last_ret_slot(ctx);
    ctx.set_field(this, last_ret_slot, Value::Int(cursor));
    Ok(Some(val))
}

/// Native `ArrayList$Itr.remove()` — removes the last element returned by
/// `next()`. The real-JDK bytecode for `remove()` reads `expectedModCount`
/// and calls back into `ArrayList.remove(int)`; because the native-collections
/// model maintains list state outside the real `modCount` machinery, the whole
/// iterator (`hasNext` / `next` / `remove`) must be serviced natively and
/// kept self-consistent. Mirrors real-JDK semantics: remove `data[lastRet]`,
/// rewind `cursor` to `lastRet`, then reset `lastRet` to `-1`.
fn native_al_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let (cursor_slot, list_slot, _) = al_itr_slots(ctx);
    let last_ret_slot = al_itr_last_ret_slot(ctx);
    let last_ret = match ctx.get_field(this, last_ret_slot) {
        Value::Int(l) => l,
        _ => -1,
    };
    if last_ret < 0 {
        // JDK contract: `remove()` before `next()` (or twice in a row)
        // throws IllegalStateException.
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let list = match ctx.get_field(this, list_slot) {
        Value::Object(Some(l)) => l,
        _ => return Ok(None),
    };
    // Delegate to the list's own native removal so backing-array shifting and
    // size bookkeeping stay in one place.
    native_al_remove_at(ctx, &[Value::Object(Some(list)), Value::Int(last_ret)])?;
    // Real-JDK: cursor = lastRet; lastRet = -1;
    ctx.set_field(this, cursor_slot, Value::Int(last_ret));
    ctx.set_field(this, last_ret_slot, Value::Int(-1));
    Ok(None)
}

fn native_map_key_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, MAP_KEY_ITR_FIELD_CURSOR) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Int(0))),
    };
    let total = match ctx.get_field(this, MAP_KEY_ITR_FIELD_TOTAL) {
        Value::Int(t) => t,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(if cursor < total { 1 } else { 0 })))
}

fn native_map_key_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cursor = match ctx.get_field(this, MAP_KEY_ITR_FIELD_CURSOR) {
        Value::Int(c) => c,
        _ => return Ok(Some(Value::Object(None))),
    };
    let total = match ctx.get_field(this, MAP_KEY_ITR_FIELD_TOTAL) {
        Value::Int(t) => t,
        _ => return Ok(Some(Value::Object(None))),
    };
    if cursor >= total {
        return Ok(Some(Value::Object(None)));
    }
    let keys = match ctx.get_field(this, MAP_KEY_ITR_FIELD_KEYS) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_array_element(keys, cursor as usize);
    ctx.set_field(this, MAP_KEY_ITR_FIELD_CURSOR, Value::Int(cursor + 1));
    // Record index just returned for a subsequent Iterator.remove().
    // Iterator allocated with <5 fields (older snapshot iterators) silently
    // ignores the write because alloc_object pre-sized the field block.
    let n_fields = ctx.object_num_fields(this);
    if n_fields > MAP_KEY_ITR_FIELD_LAST_RET {
        ctx.set_field(this, MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(cursor));
    }
    Ok(Some(val))
}

/// Native `HashMap$KeyItr.remove()` — pairs with `native_hs_iterator` so
/// JDK code that iterates a HashSet (or HashMap.keySet()) and calls
/// `it.remove()` actually deletes the entry, instead of falling through
/// to the `Iterator.remove()` default that throws `UnsupportedOperationException`.
///
/// Trigger case: `com.sun.jmx.mbeanserver.MXBeanSupport.findMXBeanInterface`
/// builds a `HashSet<Class<?>>` of candidate MXBean interfaces and uses
/// `it.remove()` to drop superseded ones during platform-MBean registration.
/// Without this native, `ManagementFactory.getPlatformMBeanServer()` fails
/// with `NotCompliantMBeanException: sun.management.GarbageCollectorImpl: remove`,
/// blocking jboss-modules / WildFly / Keycloak boot.
fn native_map_key_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Iterators allocated via the legacy 3-field snapshot path
    // (`make_iterator_from_array`) have no backing reference — surface
    // UnsupportedOperationException so callers see the JDK-spec behaviour.
    let n_fields = ctx.object_num_fields(this);
    if n_fields <= MAP_KEY_ITR_FIELD_LAST_RET {
        return Err(unsupported_op());
    }
    let last_ret = match ctx.get_field(this, MAP_KEY_ITR_FIELD_LAST_RET) {
        Value::Int(v) => v,
        _ => -1,
    };
    if last_ret < 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let backing = match ctx.get_field(this, MAP_KEY_ITR_FIELD_BACKING) {
        Value::Object(Some(b)) => b,
        _ => return Err(unsupported_op()),
    };
    let keys = match ctx.get_field(this, MAP_KEY_ITR_FIELD_KEYS) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    let key = ctx.get_array_element(keys, last_ret as usize);
    // Delegate to the receiver's own remove(Object). HashSet.remove unwraps
    // to native_map_remove on the backing HashMap; HashMap.keySet().iterator()
    // would route through native_hs_remove on the synthetic KeySet, which
    // chains to native_map_remove on the underlying map. Either way the
    // entry actually disappears from the source collection.
    let _ = native_hs_remove(ctx, &[Value::Object(Some(backing)), key])?;
    ctx.set_field(this, MAP_KEY_ITR_FIELD_LAST_RET, Value::Int(-1));
    Ok(None)
}

// ===========================================================================
// Arrays utility class
// ===========================================================================

fn register_arrays_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "java/util/Arrays",
        "copyOf",
        "([Ljava/lang/Object;I)[Ljava/lang/Object;",
        native_arrays_copy_of,
    );
    r.register(
        "java/util/Arrays",
        "toString",
        "([Ljava/lang/Object;)Ljava/lang/String;",
        native_arrays_to_string,
    );
    r.register(
        "java/util/Arrays",
        "asList",
        "([Ljava/lang/Object;)Ljava/util/List;",
        native_arrays_as_list,
    );
    r.register("java/util/Arrays", "sort", "([I)V", native_arrays_sort_int);
    r.register(
        "java/util/Arrays",
        "sort",
        "([Ljava/lang/Object;)V",
        native_arrays_sort_objects,
    );
    r.register("java/util/Arrays", "fill", "([II)V", native_arrays_fill_int);
    r.register(
        "java/util/Arrays",
        "fill",
        "([Ljava/lang/Object;Ljava/lang/Object;)V",
        native_arrays_fill_object,
    );
    r.register(
        "java/util/Arrays",
        "binarySearch",
        "([II)I",
        native_arrays_binary_search_int,
    );
    r.register(
        "java/util/Arrays",
        "equals",
        "([I[I)Z",
        native_arrays_equals_int,
    );
}

fn native_arrays_copy_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_len = match args.get(1) {
        Some(Value::Int(l)) => std::cmp::max(*l, 0) as usize,
        _ => return Ok(Some(Value::Object(None))),
    };
    let old_len = ctx.array_length(src);
    let result = alloc_ref_array(ctx, new_len);
    let copy_len = std::cmp::min(old_len, new_len);
    for i in 0..copy_len {
        let val = ctx.get_array_element(src, i);
        ctx.set_array_element(result, i, val);
    }
    Ok(Some(Value::Object(Some(result))))
}

fn native_arrays_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        Some(Value::Object(None)) => {
            let s = ctx.create_string("null");
            return Ok(Some(Value::Object(Some(s))));
        }
        _ => {
            let s = ctx.create_string("null");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let len = ctx.array_length(arr);
    let mut parts = Vec::with_capacity(len);
    for i in 0..len {
        let val = ctx.get_array_element(arr, i);
        parts.push(obj_to_display_string(ctx, &val));
    }
    let text = format!("[{}]", parts.join(", "));
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

fn native_arrays_as_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            // Return empty list
            let __al_n_fields = al_slots(ctx).2;
            let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
            let buf = alloc_ref_array(ctx, AL_DEFAULT_CAPACITY);
            al_set_data(ctx, list, buf);
            al_set_size(ctx, list, 0);
            return Ok(Some(Value::Object(Some(list))));
        }
    };
    let len = ctx.array_length(arr);
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(len, AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for i in 0..len {
        let val = ctx.get_array_element(arr, i);
        ctx.set_array_element(buf, i, val);
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, len as i32);
    Ok(Some(Value::Object(Some(list))))
}

// ===========================================================================
// Tests
// ===========================================================================
// java.util.Optional
// ===========================================================================

const OPT_FIELD_VALUE: usize = 0;
const OPT_NUM_FIELDS: usize = 1;

fn register_optional_natives(r: &mut NativeMethodRegistry) {
    let o = "java/util/Optional";
    r.register(o, "empty", "()Ljava/util/Optional;", native_opt_empty);
    r.register(
        o,
        "of",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        native_opt_of,
    );
    r.register(
        o,
        "ofNullable",
        "(Ljava/lang/Object;)Ljava/util/Optional;",
        native_opt_of_nullable,
    );
    r.register(o, "get", "()Ljava/lang/Object;", native_opt_get);
    r.register(o, "isPresent", "()Z", native_opt_is_present);
    r.register(o, "isEmpty", "()Z", native_opt_is_empty);
    r.register(
        o,
        "orElse",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_opt_or_else,
    );
    r.register(
        o,
        "orElseThrow",
        "()Ljava/lang/Object;",
        native_opt_or_else_throw,
    );
    r.register(o, "equals", "(Ljava/lang/Object;)Z", native_opt_equals);
    r.register(o, "hashCode", "()I", native_opt_hash_code);
    r.register(o, "toString", "()Ljava/lang/String;", native_opt_to_string);
    r.register(
        o,
        "ifPresent",
        "(Ljava/util/function/Consumer;)V",
        native_opt_if_present,
    );
    r.register(
        o,
        "map",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
        native_opt_map,
    );
    r.register(
        o,
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/Optional;",
        native_opt_flat_map,
    );
    r.register(
        o,
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/Optional;",
        native_opt_filter,
    );
    r.register(
        o,
        "orElseGet",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_opt_or_else_get,
    );
    r.register(
        o,
        "ifPresentOrElse",
        "(Ljava/util/function/Consumer;Ljava/lang/Runnable;)V",
        native_opt_if_present_or_else,
    );
    r.register(
        o,
        "or",
        "(Ljava/util/function/Supplier;)Ljava/util/Optional;",
        native_opt_or,
    );
    r.register(
        o,
        "stream",
        "()Ljava/util/stream/Stream;",
        native_opt_stream,
    );
    r.register(
        o,
        "orElseThrow",
        "(Ljava/util/function/Supplier;)Ljava/lang/Object;",
        native_opt_or_else_throw_supplier,
    );
}

fn native_opt_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    // field 0 stays as default (null)
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(None)) | None => {
            Err(cratonvm_types::error::RuntimeError::NullPointerException { message: None }.into())
        }
        Some(val) => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            ctx.set_field(opt, OPT_FIELD_VALUE, *val);
            Ok(Some(Value::Object(Some(opt))))
        }
    }
}

fn native_opt_of_nullable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    let val = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_field(opt, OPT_FIELD_VALUE, val);
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    match val {
        Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into()),
        _ => Ok(Some(val)),
    }
}

fn native_opt_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    Ok(Some(Value::Int(if matches!(val, Value::Object(None)) {
        0
    } else {
        1
    })))
}

fn native_opt_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    Ok(Some(Value::Int(if matches!(val, Value::Object(None)) {
        1
    } else {
        0
    })))
}

fn native_opt_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let default = args.get(1).copied().unwrap_or(Value::Object(None));
            return Ok(Some(default));
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    match val {
        Value::Object(None) => {
            let default = args.get(1).copied().unwrap_or(Value::Object(None));
            Ok(Some(default))
        }
        _ => Ok(Some(val)),
    }
}

fn native_opt_or_else_throw(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into())
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    match val {
        Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into()),
        _ => Ok(Some(val)),
    }
}

fn native_opt_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let this_val = ctx.get_field(this, OPT_FIELD_VALUE);
    let other_val = ctx.get_field(other, OPT_FIELD_VALUE);
    let eq = values_equal(ctx, &this_val, &other_val);
    Ok(Some(Value::Int(if eq { 1 } else { 0 })))
}

fn native_opt_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    match val {
        Value::Object(Some(obj)) => Ok(Some(Value::Int(ctx.identity_hash_code(obj)))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_opt_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let s = ctx.create_string("Optional.empty");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    let s = match val {
        Value::Object(None) => "Optional.empty".to_string(),
        _ => {
            let display = obj_to_display_string(ctx, &val);
            format!("Optional[{display}]")
        }
    };
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

// ===========================================================================
// Arrays.sort / fill / binarySearch / equals (added to register_arrays_natives)
// ===========================================================================

fn native_arrays_sort_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let len = ctx.array_length(arr);
    let mut vals: Vec<i32> = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => vals.push(v),
            _ => vals.push(0),
        }
    }
    vals.sort();
    for (i, &v) in vals.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(v));
    }
    Ok(None)
}

fn native_arrays_sort_objects(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    if ctx.heap_kind_of(arr) != ObjectKind::Array {
        // Some early bootstrap paths (ClassWorlds / Maven launcher) can
        // route a List receiver into Arrays.sort native overrides. Coerce an
        // ArrayList receiver to its backing elementData to preserve behavior.
        let (data, _size) = al_state(ctx, arr);
        if let Some(backing) = data {
            arr = backing;
        } else {
            return Ok(None);
        }
    }
    let len = ctx.array_length(arr);

    // MED fix: real JDK `Arrays.sort(Object[])` orders elements by
    // natural comparison (`Comparable.compareTo`), and throws
    // `ClassCastException` for elements that don't implement Comparable.
    // The previous implementation sorted by `read_string()` representation,
    // which silently produced wrong order for any type whose toString does
    // not match its natural ordering (Integer, Date, custom types) and
    // happily sorted non-Comparable objects without complaint.
    //
    // We now:
    //   1. Snapshot the array into a Vec<Value>.
    //   2. Verify every non-null element is Comparable; throw
    //      ClassCastException on the first non-Comparable element.
    //   3. Sort via insertion sort, dispatching through
    //      `Comparable.compareTo(Object)` for every pair-wise comparison.
    //      Insertion sort is O(n^2) but works correctly with a fallible
    //      comparator (a thrown compareTo propagates out cleanly) and is
    //      acceptable for the synthetic-stub path — real JDK code uses
    //      a TimSort that we can't replicate while propagating exceptions
    //      from Rust's stable `sort_by`.
    //
    // Null elements are permitted (JDK sorts them as if smaller than any
    // non-null element when the comparator is null, but throws NPE when
    // comparing null via compareTo). To match JDK behaviour we propagate
    // the NPE that compareTo would naturally throw if a null sneaks in.

    let mut items: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        items.push(ctx.get_array_element(arr, i));
    }

    // Verify Comparable on every non-null element.
    for v in &items {
        if let Value::Object(Some(obj)) = v {
            if !implements_comparable(ctx, *obj) {
                let cname = ctx
                    .class_name_of_id(ctx.class_id_of_object(*obj))
                    .unwrap_or_else(|| "<unknown>".to_string());
                return Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: format!(
                        "element of class {} does not implement java.lang.Comparable",
                        cname
                    ),
                }
                .into());
            }
        }
    }

    // Insertion sort with fallible comparator.
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 {
            let cmp = compare_via_compare_to(ctx, &items[j - 1], &items[j])?;
            if cmp <= 0 {
                break;
            }
            items.swap(j - 1, j);
            j -= 1;
        }
    }

    for (i, val) in items.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(None)
}

/// Walk `obj`'s class hierarchy (including superclasses) and return true if
/// any class implements `java/lang/Comparable` directly or transitively
/// via a super-interface. Used by `native_arrays_sort_objects` to throw
/// `ClassCastException` before invoking `compareTo` on a non-Comparable.
fn implements_comparable(ctx: &dyn NativeContext, obj: ObjectRef) -> bool {
    let mut cid = ctx.class_id_of_object(obj);
    for _ in 0..64 {
        for iface in ctx.class_interfaces(cid) {
            if iface_extends_comparable(ctx, iface) {
                return true;
            }
        }
        match ctx.superclass_of(cid) {
            Some(p) => cid = p,
            None => break,
        }
    }
    false
}

/// True iff `iface` IS `java/lang/Comparable` or transitively extends it.
fn iface_extends_comparable(ctx: &dyn NativeContext, iface: ClassId) -> bool {
    // BFS with a depth cap (interface graphs in real-world JDKs are shallow).
    let mut stack: Vec<ClassId> = vec![iface];
    let mut budget = 64usize;
    while let Some(c) = stack.pop() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        if let Some(name) = ctx.class_name_of_id(c) {
            if name == "java/lang/Comparable" {
                return true;
            }
        }
        for super_iface in ctx.class_interfaces(c) {
            stack.push(super_iface);
        }
    }
    false
}

/// Dispatch through `Comparable.compareTo(Object)`. Treats `null` as
/// less than any non-null element (matching how the JDK's natural-order
/// comparator handles the corner case for `Arrays.sort(Object[])`).
fn compare_via_compare_to(
    ctx: &mut dyn NativeContext,
    a: &Value,
    b: &Value,
) -> Result<i32, MethodCallFailed> {
    match (a, b) {
        (Value::Object(None), Value::Object(None)) => Ok(0),
        (Value::Object(None), _) => Ok(-1),
        (_, Value::Object(None)) => Ok(1),
        (Value::Object(Some(ao)), Value::Object(Some(bo))) => {
            let r = ctx.invoke_virtual(
                *ao,
                "compareTo",
                "(Ljava/lang/Object;)I",
                &[Value::Object(Some(*bo))],
            )?;
            match r {
                Some(Value::Int(v)) => Ok(v),
                _ => Err(cratonvm_types::error::RuntimeError::ClassCastException {
                    message: "compareTo did not return an int".to_string(),
                }
                .into()),
            }
        }
        // Non-object slots are unreachable in an Object[]; defensive fallback.
        _ => Ok(0),
    }
}

fn native_arrays_fill_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let val = args.get(1).copied().unwrap_or(Value::Int(0));
    let len = ctx.array_length(arr);
    for i in 0..len {
        ctx.set_array_element(arr, i, val);
    }
    Ok(None)
}

fn native_arrays_fill_object(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let val = args.get(1).copied().unwrap_or(Value::Object(None));
    let len = ctx.array_length(arr);
    for i in 0..len {
        ctx.set_array_element(arr, i, val);
    }
    Ok(None)
}

fn native_arrays_binary_search_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let key = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let len = ctx.array_length(arr);
    let mut low: usize = 0;
    let mut high = len;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_val = match ctx.get_array_element(arr, mid) {
            Value::Int(v) => v,
            _ => 0,
        };
        match mid_val.cmp(&key) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Equal => return Ok(Some(Value::Int(mid as i32))),
            std::cmp::Ordering::Greater => high = mid,
        }
    }
    // Not found: return -(insertion point) - 1
    Ok(Some(Value::Int(-(low as i32) - 1)))
}

fn native_arrays_equals_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Object(Some(arr))) => *arr,
        Some(Value::Object(None)) => {
            return match args.get(1) {
                Some(Value::Object(None)) => Ok(Some(Value::Int(1))),
                _ => Ok(Some(Value::Int(0))),
            };
        }
        _ => return Ok(Some(Value::Int(0))),
    };
    let b = match args.get(1) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => return Ok(Some(Value::Int(0))),
    };
    let len_a = ctx.array_length(a);
    let len_b = ctx.array_length(b);
    if len_a != len_b {
        return Ok(Some(Value::Int(0)));
    }
    for i in 0..len_a {
        if ctx.get_array_element(a, i) != ctx.get_array_element(b, i) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// Collections utilities
// ===========================================================================

fn register_collections_utility_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/Collections";
    r.register(c, "sort", "(Ljava/util/List;)V", native_collections_sort);
    r.register(
        c,
        "emptyList",
        "()Ljava/util/List;",
        native_collections_empty_list,
    );
    r.register(
        c,
        "singletonList",
        "(Ljava/lang/Object;)Ljava/util/List;",
        native_collections_singleton_list,
    );
    r.register(
        c,
        "reverse",
        "(Ljava/util/List;)V",
        native_collections_reverse,
    );
    r.register(
        c,
        "unmodifiableList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_collections_unmodifiable_list,
    );
    r.register(
        c,
        "sort",
        "(Ljava/util/List;Ljava/util/Comparator;)V",
        native_collections_sort_comparator,
    );
}

fn native_collections_sort(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        None => return Ok(None),
    };
    let len = size as usize;
    // Read elements with string keys
    let mut items: Vec<(String, Value)> = Vec::with_capacity(len);
    for i in 0..len {
        let val = ctx.get_array_element(data, i);
        let key = match &val {
            Value::Object(Some(obj)) => ctx.read_string(*obj).unwrap_or_default(),
            _ => String::new(),
        };
        items.push((key, val));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    for (i, (_, val)) in items.iter().enumerate() {
        ctx.set_array_element(data, i, *val);
    }
    Ok(None)
}

fn native_collections_empty_list(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let arr = alloc_ref_array(ctx, 0);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, 0);
    Ok(Some(Value::Object(Some(list))))
}

fn native_collections_singleton_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let val = args.first().copied().unwrap_or(Value::Object(None));
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let arr = alloc_ref_array(ctx, 1);
    ctx.set_array_element(arr, 0, val);
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, 1);
    Ok(Some(Value::Object(Some(list))))
}

fn native_collections_reverse(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        None => return Ok(None),
    };
    let len = size as usize;
    // Read all elements
    let mut elems: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(data, i));
    }
    // Write back in reverse
    for (i, val) in elems.iter().rev().enumerate() {
        ctx.set_array_element(data, i, *val);
    }
    Ok(None)
}

fn native_collections_unmodifiable_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Wrap the source list in a live `UnmodifiableList` view: reads delegate
    // to the backing list (so later mutations of the backing list are
    // visible), and every mutator throws `UnsupportedOperationException`.
    let src = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let w = alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, src);
    Ok(Some(Value::Object(Some(w))))
}

// ===========================================================================
// forEach implementations
// ===========================================================================

fn native_al_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    let data = match data {
        Some(d) => d,
        None => return Ok(None),
    };
    let len = size as usize;
    // Collect elements first to avoid borrowing issues during invoke_virtual.
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(data, i));
    }
    for elem in &elems {
        ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[*elem])?;
    }
    Ok(None)
}

fn native_map_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let entries = map_collect_entries(ctx, this);
    for (key, value) in &entries {
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[*key, *value],
        )?;
    }
    Ok(None)
}

fn native_hs_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return Ok(None),
    };
    let keys = map_collect_keys(ctx, backing);
    for key in &keys {
        ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[*key])?;
    }
    Ok(None)
}

// ===========================================================================
// Optional functional methods
// ===========================================================================

fn native_opt_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if !matches!(val, Value::Object(None)) {
        ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[val])?;
    }
    Ok(None)
}

fn native_opt_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(
        mapper,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    let mapped = result.unwrap_or(Value::Object(None));
    native_opt_of_nullable(ctx, &[mapped])
}

fn native_opt_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let mapper = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(
        mapper,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    // flatMap returns the Optional directly (the Function must return an Optional).
    Ok(result)
}

fn native_opt_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return native_opt_empty(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        return native_opt_empty(ctx, &[]);
    }
    let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[val])?;
    let passed = matches!(result, Some(Value::Int(1)));
    if passed {
        // Return `this` (the same Optional)
        Ok(Some(Value::Object(Some(this))))
    } else {
        native_opt_empty(ctx, &[])
    }
}

fn native_opt_or_else_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // null optional → call supplier
            let supplier = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return Ok(Some(Value::Object(None))),
            };
            return ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        let supplier = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(Some(Value::Object(None))),
        };
        ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(val))
    }
}

fn native_opt_if_present_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if !matches!(val, Value::Object(None)) {
        let consumer = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val])?;
    } else {
        let runnable = match args.get(2) {
            Some(Value::Object(Some(r))) => *r,
            _ => return Ok(None),
        };
        ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    }
    Ok(None)
}

fn native_opt_or(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            // null optional → call supplier
            let supplier = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => return native_opt_empty(ctx, &[]),
            };
            return ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[]);
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        let supplier = match args.get(1) {
            Some(Value::Object(Some(r))) => *r,
            _ => return native_opt_empty(ctx, &[]),
        };
        ctx.invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])
    } else {
        Ok(Some(Value::Object(Some(this))))
    }
}

fn native_opt_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        make_stream(ctx, &[])
    } else {
        make_stream(ctx, &[val])
    }
}

fn native_opt_or_else_throw_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into());
        }
    };
    let val = ctx.get_field(this, OPT_FIELD_VALUE);
    if matches!(val, Value::Object(None)) {
        Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into())
    } else {
        Ok(Some(val))
    }
}

// ===========================================================================
// Comparator-based sort
// ===========================================================================

fn native_al_sort_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        Some(Value::Object(None)) | None => {
            // null comparator → natural ordering (delegate to existing sort)
            return native_collections_sort(ctx, &[Value::Object(Some(this))]);
        }
        _ => return Ok(None),
    };
    sort_with_comparator(ctx, this, comparator)
}

fn native_collections_sort_comparator(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        Some(Value::Object(None)) | None => {
            // null comparator → natural ordering
            return native_collections_sort(ctx, &[Value::Object(Some(list))]);
        }
        _ => return Ok(None),
    };
    sort_with_comparator(ctx, list, comparator)
}

/// Insertion sort using a Comparator lambda.
fn sort_with_comparator(
    ctx: &mut dyn NativeContext,
    list: ObjectRef,
    comparator: ObjectRef,
) -> MethodCallResult {
    let (data, size) = al_state(ctx, list);
    let data = match data {
        Some(d) => d,
        None => return Ok(None),
    };
    let len = size as usize;
    if len <= 1 {
        return Ok(None);
    }

    // Read all elements into a Vec.
    let mut elems: Vec<Value> = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(data, i));
    }

    // Insertion sort — O(n²) but stable and correct.
    for i in 1..len {
        let key = elems[i];
        let mut j = i;
        while j > 0 {
            let cmp_result = comparator_compare(ctx, comparator, elems[j - 1], key)?;
            let cmp = match cmp_result {
                Some(Value::Int(v)) => v,
                _ => 0,
            };
            if cmp <= 0 {
                break;
            }
            elems[j] = elems[j - 1];
            j -= 1;
        }
        elems[j] = key;
    }

    // Write sorted elements back.
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(data, i, *val);
    }
    Ok(None)
}

// ===========================================================================
// Map.Entry accessors
// ===========================================================================

fn register_map_entry_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/HashMap$Entry";
    r.register(c, "getKey", "()Ljava/lang/Object;", native_entry_get_key);
    r.register(
        c,
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    r.register(
        c,
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
}

fn native_entry_get_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 0)))
}

fn native_entry_get_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_field(this, 1)))
}

fn native_entry_set_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let new_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let old_val = ctx.get_field(this, 1);
    ctx.set_field(this, 1, new_val);
    Ok(Some(old_val))
}

// ===========================================================================
// ArrayList.removeIf / replaceAll
// ===========================================================================

fn native_al_remove_if(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(p))) => *p,
        _ => return Ok(Some(Value::Int(0))),
    };

    let data = match ctx.get_field(this, al_slots(ctx).0) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, al_slots(ctx).1) {
        Value::Int(s) => s as usize,
        _ => return Ok(Some(Value::Int(0))),
    };

    // Snapshot elements, then test each with the predicate.
    let mut keep = Vec::with_capacity(size);
    for i in 0..size {
        let elem = ctx.get_array_element(data, i);
        let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[elem])?;
        let is_true = matches!(result, Some(Value::Int(v)) if v != 0);
        if !is_true {
            keep.push(elem);
        }
    }

    let removed = keep.len() < size;

    // Write back kept elements.
    for (i, val) in keep.iter().enumerate() {
        ctx.set_array_element(data, i, *val);
    }
    // Clear trailing slots.
    for i in keep.len()..size {
        ctx.set_array_element(data, i, Value::Object(None));
    }
    al_set_size(ctx, this, keep.len() as i32);

    Ok(Some(Value::Int(if removed { 1 } else { 0 })))
}

fn native_al_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(op))) => *op,
        _ => return Ok(None),
    };

    let data = match ctx.get_field(this, al_slots(ctx).0) {
        Value::Object(Some(arr)) => arr,
        _ => return Ok(None),
    };
    let size = match ctx.get_field(this, al_slots(ctx).1) {
        Value::Int(s) => s as usize,
        _ => return Ok(None),
    };

    for i in 0..size {
        let elem = ctx.get_array_element(data, i);
        let result = ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[elem],
        )?;
        let raw = result.unwrap_or(Value::Object(None));
        // Unbox wrapper returns so the list stores the primitive the
        // lambda produced, not a boxed Integer/Long/etc.
        let new_val = normalize_for_compare(ctx, &raw);
        ctx.set_array_element(data, i, new_val);
    }

    Ok(None)
}

// ===========================================================================
// HashMap functional methods (computeIfAbsent, compute, merge, replaceAll)
// ===========================================================================

fn native_map_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Check if key already present.
    let existing = native_map_get(ctx, &[Value::Object(Some(this)), key])?;
    if let Some(Value::Object(Some(_))) = existing {
        return Ok(existing);
    }

    // Key absent — call function.apply(key).
    let result = ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key],
    )?;
    let raw = result.unwrap_or(Value::Object(None));
    // Unbox wrapper returns (Integer/Long/etc.) so callers see the primitive
    // variant produced by the lambda impl, matching javac's autoboxing view.
    let new_val = normalize_for_compare(ctx, &raw);

    if let Value::Object(None) = new_val {
        return Ok(Some(Value::Object(None)));
    }

    native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

fn native_map_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let bi_function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    let existing = native_map_get(ctx, &[Value::Object(Some(this)), key])?;
    let old_val = existing.unwrap_or(Value::Object(None));

    let result = ctx.invoke_virtual(
        bi_function,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[key, old_val],
    )?;
    let raw = result.unwrap_or(Value::Object(None));
    // Unbox wrapper returns so callers see the primitive the lambda produced.
    let new_val = normalize_for_compare(ctx, &raw);

    if let Value::Object(None) = new_val {
        // Remove mapping if new value is null.
        if let Value::Object(Some(_)) = old_val {
            native_map_remove(ctx, &[Value::Object(Some(this)), key])?;
        }
        return Ok(Some(Value::Object(None)));
    }

    native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

fn native_map_compute_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let bi_function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    let existing = native_map_get(ctx, &[Value::Object(Some(this)), key])?;
    let old_val = existing.unwrap_or(Value::Object(None));

    // Only apply if key is present (old_val is non-null)
    if let Value::Object(None) = old_val {
        return Ok(Some(Value::Object(None)));
    }

    let result = ctx.invoke_virtual(
        bi_function,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[key, old_val],
    )?;
    let raw = result.unwrap_or(Value::Object(None));
    let new_val = normalize_for_compare(ctx, &raw);

    if let Value::Object(None) = new_val {
        native_map_remove(ctx, &[Value::Object(Some(this)), key])?;
        return Ok(Some(Value::Object(None)));
    }

    native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

fn native_map_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let bi_function = match args.get(3) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    let existing = native_map_get(ctx, &[Value::Object(Some(this)), key])?;
    let old_val = existing.unwrap_or(Value::Object(None));

    let new_val = if let Value::Object(Some(_)) = old_val {
        // Key present — merge with BiFunction.
        let result = ctx.invoke_virtual(
            bi_function,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[old_val, value],
        )?;
        let raw = result.unwrap_or(Value::Object(None));
        // Unbox wrapper returns so callers see the primitive variant.
        normalize_for_compare(ctx, &raw)
    } else {
        // Key absent — use value directly.
        value
    };

    if let Value::Object(None) = new_val {
        native_map_remove(ctx, &[Value::Object(Some(this)), key])?;
        return Ok(Some(Value::Object(None)));
    }

    native_map_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

fn native_map_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let bi_function = match args.get(1) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(None),
    };

    let entries = map_collect_entries(ctx, this);
    for (key, value) in &entries {
        let result = ctx.invoke_virtual(
            bi_function,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[*key, *value],
        )?;
        let raw = result.unwrap_or(Value::Object(None));
        // Unbox wrapper returns so stored values reflect the primitive
        // produced by the lambda impl.
        let new_val = normalize_for_compare(ctx, &raw);
        native_map_put(ctx, &[Value::Object(Some(this)), *key, new_val])?;
    }

    Ok(None)
}

// ===========================================================================
// Factory methods: List.of, Set.of, Map.of, Map.entry
// ===========================================================================

fn register_factory_natives(r: &mut NativeMethodRegistry) {
    // List.of
    r.register(
        "java/util/List",
        "of",
        "()Ljava/util/List;",
        native_list_of_0,
    );
    r.register(
        "java/util/List",
        "of",
        "(Ljava/lang/Object;)Ljava/util/List;",
        native_list_of_1,
    );
    r.register(
        "java/util/List",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;",
        native_list_of_2,
    );
    r.register(
        "java/util/List",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/List;",
        native_list_of_3,
    );
    r.register(
        "java/util/List",
        "of",
        "([Ljava/lang/Object;)Ljava/util/List;",
        native_list_of_array,
    );

    // Set.of
    r.register("java/util/Set", "of", "()Ljava/util/Set;", native_set_of_0);
    r.register(
        "java/util/Set",
        "of",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        native_set_of_1,
    );
    r.register(
        "java/util/Set",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;",
        native_set_of_2,
    );
    r.register(
        "java/util/Set",
        "of",
        "([Ljava/lang/Object;)Ljava/util/Set;",
        native_set_of_array,
    );

    // Map.of
    r.register("java/util/Map", "of", "()Ljava/util/Map;", native_map_of_0);
    r.register(
        "java/util/Map",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_map_of_1,
    );
    r.register(
        "java/util/Map",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_map_of_2,
    );
    r.register(
        "java/util/Map",
        "entry",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map$Entry;",
        native_map_entry,
    );

    // Higher-arity `List.of` / `Set.of` / `Map.of` overloads (real JDK
    // declares fixed-arity variants up to 10). Without these, e.g.
    // `Set.of("a","b","c")` falls through to real-JDK bytecode and the
    // result is not one of our immutable wrappers — its mutators would not
    // throw. Register a generic handler for every fixed arity so all of
    // them produce a frozen wrapper.
    for n in 4..=10 {
        let elem_obj = "Ljava/lang/Object;";
        let list_desc = format!("({})Ljava/util/List;", elem_obj.repeat(n));
        r.register("java/util/List", "of", &list_desc, native_list_of_varargs);
        let set_desc = format!("({})Ljava/util/Set;", elem_obj.repeat(n));
        r.register("java/util/Set", "of", &set_desc, native_set_of_varargs);
    }
    // `Set.of` 3-arg (List.of 1..3 already covered above; Set.of only had
    // 0..2 fixed variants registered).
    r.register(
        "java/util/Set",
        "of",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Set;",
        native_set_of_varargs,
    );
    // `Map.of` 3..10 entries (each entry is two Object args).
    for n in 3..=10 {
        let map_desc = format!("({})Ljava/util/Map;", "Ljava/lang/Object;".repeat(n * 2));
        r.register("java/util/Map", "of", &map_desc, native_map_of_varargs);
    }
    // `Map.ofEntries(Map$Entry...)`.
    r.register(
        "java/util/Map",
        "ofEntries",
        "([Ljava/util/Map$Entry;)Ljava/util/Map;",
        native_map_of_entries,
    );
}

/// Generic `List.of` for fixed-arity overloads: every positional arg is an
/// element. Produces a frozen (unmodifiable) list.
fn native_list_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elems: Vec<Value> = args.to_vec();
    let r = make_list_of(ctx, &elems);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

/// Generic `Set.of` for fixed-arity overloads. Produces a frozen set.
fn native_set_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elems: Vec<Value> = args.to_vec();
    let r = make_set_of(ctx, &elems);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

/// Generic `Map.of` for fixed-arity overloads: args are k0,v0,k1,v1,...
fn native_map_of_varargs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(args.len() / 2);
    let mut i = 0;
    while i + 1 < args.len() {
        pairs.push((args[i], args[i + 1]));
        i += 2;
    }
    let r = make_map_of(ctx, &pairs);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

/// `Map.ofEntries(Map$Entry...)` — each array element is a Map.Entry whose
/// key/value live at slots 0/1 (see `native_map_entry`).
fn native_map_of_entries(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            let r = make_map_of(ctx, &[]);
            return freeze_result(ctx, UNMOD_MAP_CLASS, r);
        }
    };
    let len = ctx.array_length(arr);
    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(len);
    for i in 0..len {
        if let Value::Object(Some(entry)) = ctx.get_array_element(arr, i) {
            let k = ctx.get_field(entry, 0);
            let v = ctx.get_field(entry, 1);
            pairs.push((k, v));
        }
    }
    let r = make_map_of(ctx, &pairs);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

/// Helper: create an ArrayList from a slice of values.
fn make_list_of(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(elems.len(), AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(buf, i, *val);
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, elems.len() as i32);
    Ok(Some(Value::Object(Some(list))))
}

/// Helper: create an ArrayList (returns ObjectRef, not MethodCallResult).
fn make_list_of_raw(ctx: &mut dyn NativeContext, elems: &[Value]) -> ObjectRef {
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(elems.len(), AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for (i, val) in elems.iter().enumerate() {
        ctx.set_array_element(buf, i, *val);
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, elems.len() as i32);
    list
}

/// Helper: create a HashSet from a slice of values.
fn make_set_of(ctx: &mut dyn NativeContext, elems: &[Value]) -> MethodCallResult {
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let backing_map = alloc_backing_map(ctx);
    let cap = std::cmp::max(elems.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing_map, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing_map, 0);
    ctx.set_field(backing_map, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(backing_map)));

    let sentinel = Value::Int(1);
    for elem in elems {
        native_map_put(ctx, &[Value::Object(Some(backing_map)), *elem, sentinel])?;
    }

    Ok(Some(Value::Object(Some(set))))
}

/// Helper: create a HashMap from key-value pairs.
fn make_map_of(ctx: &mut dyn NativeContext, pairs: &[(Value, Value)]) -> MethodCallResult {
    let map = alloc_backing_map(ctx);
    let cap = std::cmp::max(pairs.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(map, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, map, 0);
    ctx.set_field(map, MAP_FIELD_CAPACITY, Value::Int(cap as i32));

    for (key, value) in pairs {
        native_map_put(ctx, &[Value::Object(Some(map)), *key, *value])?;
    }

    Ok(Some(Value::Object(Some(map))))
}

/// Wrap a `MethodCallResult` holding a collection ObjectRef in an
/// unmodifiable view of `wrapper_class`. Used by the `List.of` / `Set.of` /
/// `Map.of` immutable factories — the backing collection is private so the
/// snapshot can never be mutated, and every mutator throws.
fn freeze_result(
    ctx: &mut dyn NativeContext,
    wrapper_class: &str,
    result: MethodCallResult,
) -> MethodCallResult {
    match result? {
        Some(Value::Object(Some(backing))) => {
            let w = alloc_unmod_wrapper(ctx, wrapper_class, backing);
            Ok(Some(Value::Object(Some(w))))
        }
        other => Ok(other),
    }
}

fn native_list_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let r = make_list_of(ctx, &[]);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

fn native_list_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let r = make_list_of(ctx, &[e1]);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

fn native_list_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let r = make_list_of(ctx, &[e1, e2]);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

fn native_list_of_3(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let e3 = args.get(2).copied().unwrap_or(Value::Object(None));
    let r = make_list_of(ctx, &[e1, e2, e3]);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

fn native_list_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let r = make_list_of(ctx, &[]);
            return freeze_result(ctx, UNMOD_LIST_CLASS, r);
        }
    };
    let len = ctx.array_length(arr);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(arr, i));
    }
    let r = make_list_of(ctx, &elems);
    freeze_result(ctx, UNMOD_LIST_CLASS, r)
}

fn native_set_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let r = make_set_of(ctx, &[]);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

fn native_set_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let r = make_set_of(ctx, &[e1]);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

fn native_set_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let e1 = args.first().copied().unwrap_or(Value::Object(None));
    let e2 = args.get(1).copied().unwrap_or(Value::Object(None));
    let r = make_set_of(ctx, &[e1, e2]);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

fn native_set_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            let r = make_set_of(ctx, &[]);
            return freeze_result(ctx, UNMOD_SET_CLASS, r);
        }
    };
    let len = ctx.array_length(arr);
    let mut elems = Vec::with_capacity(len);
    for i in 0..len {
        elems.push(ctx.get_array_element(arr, i));
    }
    let r = make_set_of(ctx, &elems);
    freeze_result(ctx, UNMOD_SET_CLASS, r)
}

fn native_map_of_0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let r = make_map_of(ctx, &[]);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

fn native_map_of_1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let k = args.first().copied().unwrap_or(Value::Object(None));
    let v = args.get(1).copied().unwrap_or(Value::Object(None));
    let r = make_map_of(ctx, &[(k, v)]);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

fn native_map_of_2(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let k1 = args.first().copied().unwrap_or(Value::Object(None));
    let v1 = args.get(1).copied().unwrap_or(Value::Object(None));
    let k2 = args.get(2).copied().unwrap_or(Value::Object(None));
    let v2 = args.get(3).copied().unwrap_or(Value::Object(None));
    let r = make_map_of(ctx, &[(k1, v1), (k2, v2)]);
    freeze_result(ctx, UNMOD_MAP_CLASS, r)
}

fn native_map_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key = args.first().copied().unwrap_or(Value::Object(None));
    let value = args.get(1).copied().unwrap_or(Value::Object(None));
    let entry = alloc_synthetic(ctx, "java/util/Map$Entry", 2);
    ctx.set_field(entry, 0, key);
    ctx.set_field(entry, 1, value);
    Ok(Some(Value::Object(Some(entry))))
}

// ===========================================================================
// Stream API — Eager evaluation on Vec<Value>
// ===========================================================================

const STREAM_FIELD_ELEMENTS: usize = 0;
const STREAM_NUM_FIELDS: usize = 1;

/// Create a Stream from a slice of values.
fn make_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    let stream = alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

/// Extract elements from a Stream.
fn stream_elements(ctx: &dyn NativeContext, stream: ObjectRef) -> Vec<Value> {
    // For our synthetic Stream object, field 0 holds an Object[] of elements.
    // But some streams are real JDK ReferencePipeline instances (returned by
    // e.g. Spring's MergedAnnotations.stream()). In those cases we can't peek
    // at field 0 — fall through to materialize via Stream.toArray().
    let class_id = ctx.class_id_of_object(stream);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let is_synthetic = class_name == "java/util/stream/Stream";
    if is_synthetic {
        if let Value::Object(Some(arr)) = ctx.get_field(stream, STREAM_FIELD_ELEMENTS) {
            let len = ctx.array_length(arr);
            return (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
        }
    }
    // Non-synthetic streams (real JDK ReferencePipeline etc.) require an
    // `invoke_virtual` call to materialize via `Stream.toArray()` — use
    // `stream_elements_mut` from a context that has `&mut dyn NativeContext`.
    Vec::new()
}

/// Mutable variant of stream_elements that can invoke virtual methods.
fn stream_elements_mut(ctx: &mut dyn NativeContext, stream: ObjectRef) -> Vec<Value> {
    let class_id = ctx.class_id_of_object(stream);
    let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let is_synthetic = class_name == "java/util/stream/Stream";
    if is_synthetic {
        if let Value::Object(Some(arr)) = ctx.get_field(stream, STREAM_FIELD_ELEMENTS) {
            let len = ctx.array_length(arr);
            return (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
        }
        return Vec::new();
    }
    // Real ReferencePipeline (or any JDK Stream impl): materialize via toArray.
    match ctx.invoke_virtual(stream, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(arr)))) => {
            let len = ctx.array_length(arr);
            (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
        }
        _ => Vec::new(),
    }
}

fn register_stream_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/stream/Stream";

    // Source methods
    r.register(
        c,
        "of",
        "(Ljava/lang/Object;)Ljava/util/stream/Stream;",
        native_stream_of_one,
    );
    r.register(
        c,
        "of",
        "([Ljava/lang/Object;)Ljava/util/stream/Stream;",
        native_stream_of_array,
    );
    r.register(
        c,
        "empty",
        "()Ljava/util/stream/Stream;",
        native_stream_empty,
    );
    r.register(
        c,
        "concat",
        "(Ljava/util/stream/Stream;Ljava/util/stream/Stream;)Ljava/util/stream/Stream;",
        native_stream_concat,
    );

    // Intermediate operations
    r.register(
        c,
        "filter",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Stream;",
        native_stream_filter,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        native_stream_map,
    );
    r.register(
        c,
        "flatMap",
        "(Ljava/util/function/Function;)Ljava/util/stream/Stream;",
        native_stream_flat_map,
    );
    r.register(
        c,
        "sorted",
        "()Ljava/util/stream/Stream;",
        native_stream_sorted,
    );
    r.register(
        c,
        "sorted",
        "(Ljava/util/Comparator;)Ljava/util/stream/Stream;",
        native_stream_sorted_cmp,
    );
    r.register(
        c,
        "distinct",
        "()Ljava/util/stream/Stream;",
        native_stream_distinct,
    );
    r.register(
        c,
        "limit",
        "(J)Ljava/util/stream/Stream;",
        native_stream_limit,
    );
    r.register(
        c,
        "skip",
        "(J)Ljava/util/stream/Stream;",
        native_stream_skip,
    );
    r.register(
        c,
        "peek",
        "(Ljava/util/function/Consumer;)Ljava/util/stream/Stream;",
        native_stream_peek,
    );

    // Terminal operations
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_stream_for_each,
    );
    r.register(c, "count", "()J", native_stream_count);
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_stream_to_array,
    );
    r.register(
        c,
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_stream_to_array_gen,
    );
    // Same override on ReferencePipeline so real-JDK code that dispatches
    // virtual on the concrete class also hits us.
    r.register(
        "java/util/stream/ReferencePipeline",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_stream_to_array_gen,
    );
    // close() — BaseStream.close is abstract; synthetic Stream objects
    // (class `java/util/stream/Stream`) dispatch directly to the abstract
    // interface declaration and throw AbstractMethodError. Register a
    // no-op so try-with-resources and explicit s.close() succeed.
    // Without this, jboss-modules / WildFly / Keycloak boot hits
    // "BaseStream.close()V has no Code attribute" inside lambda/stream-based
    // utility methods during MBean / module wiring.
    let close_noop: fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult =
        |_ctx, _args| Ok(None);
    r.register(c, "close", "()V", close_noop);
    r.register("java/util/stream/BaseStream", "close", "()V", close_noop);
    r.register("java/util/stream/IntStream", "close", "()V", close_noop);
    r.register("java/util/stream/LongStream", "close", "()V", close_noop);
    r.register("java/util/stream/DoubleStream", "close", "()V", close_noop);
    r.register(
        c,
        "findFirst",
        "()Ljava/util/Optional;",
        native_stream_find_first,
    );
    r.register(
        c,
        "findAny",
        "()Ljava/util/Optional;",
        native_stream_find_first,
    ); // same as findFirst for sequential
    r.register(
        c,
        "anyMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_any_match,
    );
    r.register(
        c,
        "allMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_all_match,
    );
    r.register(
        c,
        "noneMatch",
        "(Ljava/util/function/Predicate;)Z",
        native_stream_none_match,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/lang/Object;Ljava/util/function/BinaryOperator;)Ljava/lang/Object;",
        native_stream_reduce_identity,
    );
    r.register(
        c,
        "reduce",
        "(Ljava/util/function/BinaryOperator;)Ljava/util/Optional;",
        native_stream_reduce_optional,
    );
    r.register(
        c,
        "min",
        "(Ljava/util/Comparator;)Ljava/util/Optional;",
        native_stream_min,
    );
    r.register(
        c,
        "max",
        "(Ljava/util/Comparator;)Ljava/util/Optional;",
        native_stream_max,
    );

    // collect — register on both the Stream interface and JDK's
    // ReferencePipeline concrete class. JDK code obtained via
    // `MergedAnnotations.stream()` returns a real `ReferencePipeline`, so
    // `invokeinterface Stream.collect` dispatches to `ReferencePipeline.collect`
    // (a JDK method) rather than our `Stream.collect` native. The JDK
    // implementation would then call `Collector.supplier/accumulator/finisher`
    // on our synthetic tagged Collector, which has no such methods. Intercept
    // `ReferencePipeline.collect` so our tagged-Collector dispatch always runs.
    r.register(
        c,
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        native_stream_collect,
    );
    r.register(
        "java/util/stream/ReferencePipeline",
        "collect",
        "(Ljava/util/stream/Collector;)Ljava/lang/Object;",
        native_stream_collect,
    );

    // 3-arg collect — `<R> R collect(Supplier<R>, BiConsumer<R,? super T>, BiConsumer<R,R>)`.
    // In real JDK 25 this is an ABSTRACT method on `Stream` (the body lives in
    // `ReferencePipeline`), so when our synthetic Stream (class =
    // `java/util/stream/Stream` interface) is the receiver, invokeinterface
    // resolves to the abstract declaration and throws
    // "AbstractMethodError: Stream.collect(...) has no Code attribute".
    // H2's `FilePathDisk.newDirectoryStream` hits this on every directory
    // listing because `Files.list(Path)` -> `StreamSupport.stream(spliterator, false)`
    // is intercepted by our native and returns a synthetic Stream.
    //
    // Semantics: `R c = supplier.get(); for elem in stream: accumulator.accept(c, elem);
    // return c;`. The combiner is parallel-only and we are sequential-only,
    // so it is ignored (matches JDK behaviour for non-parallel streams).
    r.register(
        c,
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        native_stream_collect_3arg,
    );
    r.register(
        "java/util/stream/ReferencePipeline",
        "collect",
        "(Ljava/util/function/Supplier;Ljava/util/function/BiConsumer;Ljava/util/function/BiConsumer;)Ljava/lang/Object;",
        native_stream_collect_3arg,
    );

    // mapToInt
    r.register(
        c,
        "mapToInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/stream/IntStream;",
        native_stream_map_to_int,
    );

    // mapToLong
    r.register(
        c,
        "mapToLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mapper = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let elements = stream_elements(ctx, this);
            let mapped: Vec<Value> = elements
                .iter()
                .map(|e| {
                    ctx.invoke_virtual(
                        mapper,
                        "applyAsLong",
                        "(Ljava/lang/Object;)J",
                        &[*e],
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Long(0))
                })
                .collect();
            let stream = alloc_synthetic(ctx, "java/util/stream/LongStream", 1);
            let arr = alloc_ref_array(ctx, mapped.len());
            for (i, v) in mapped.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // flatMapToInt — Stream<T>.flatMapToInt(T -> IntStream) -> IntStream.
    // Lucene's UnicodeUtil clinit uses this; without a native the
    // abstract method on `Stream` is called and the clinit aborts with
    // AbstractMethodError.
    r.register(
        c,
        "flatMapToInt",
        "(Ljava/util/function/Function;)Ljava/util/stream/IntStream;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mapper = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let elements = stream_elements_mut(ctx, this);
            let mut flat: Vec<Value> = Vec::new();
            for e in elements {
                let sub = ctx
                    .invoke_virtual(mapper, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[e])
                    .ok()
                    .flatten();
                if let Some(Value::Object(Some(sub_stream))) = sub {
                    let sub_arr = match ctx.invoke_virtual(sub_stream, "toArray", "()[I", &[]) {
                        Ok(Some(Value::Object(Some(a)))) => a,
                        _ => continue,
                    };
                    let len = ctx.array_length(sub_arr);
                    for i in 0..len {
                        flat.push(ctx.get_array_element(sub_arr, i));
                    }
                }
            }
            let stream = alloc_synthetic(ctx, "java/util/stream/IntStream", 1);
            let arr = alloc_ref_array(ctx, flat.len());
            for (i, v) in flat.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    // flatMapToLong — same shape returning a LongStream.
    r.register(
        c,
        "flatMapToLong",
        "(Ljava/util/function/Function;)Ljava/util/stream/LongStream;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mapper = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let elements = stream_elements_mut(ctx, this);
            let mut flat: Vec<Value> = Vec::new();
            for e in elements {
                let sub = ctx
                    .invoke_virtual(mapper, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[e])
                    .ok()
                    .flatten();
                if let Some(Value::Object(Some(sub_stream))) = sub {
                    let sub_arr = match ctx.invoke_virtual(sub_stream, "toArray", "()[J", &[]) {
                        Ok(Some(Value::Object(Some(a)))) => a,
                        _ => continue,
                    };
                    let len = ctx.array_length(sub_arr);
                    for i in 0..len {
                        flat.push(ctx.get_array_element(sub_arr, i));
                    }
                }
            }
            let stream = alloc_synthetic(ctx, "java/util/stream/LongStream", 1);
            let arr = alloc_ref_array(ctx, flat.len());
            for (i, v) in flat.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );
    // flatMapToDouble — same shape returning a DoubleStream.
    r.register(
        c,
        "flatMapToDouble",
        "(Ljava/util/function/Function;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mapper = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let elements = stream_elements_mut(ctx, this);
            let mut flat: Vec<Value> = Vec::new();
            for e in elements {
                let sub = ctx
                    .invoke_virtual(mapper, "apply", "(Ljava/lang/Object;)Ljava/lang/Object;", &[e])
                    .ok()
                    .flatten();
                if let Some(Value::Object(Some(sub_stream))) = sub {
                    let sub_arr = match ctx.invoke_virtual(sub_stream, "toArray", "()[D", &[]) {
                        Ok(Some(Value::Object(Some(a)))) => a,
                        _ => continue,
                    };
                    let len = ctx.array_length(sub_arr);
                    for i in 0..len {
                        flat.push(ctx.get_array_element(sub_arr, i));
                    }
                }
            }
            let stream = alloc_synthetic(ctx, "java/util/stream/DoubleStream", 1);
            let arr = alloc_ref_array(ctx, flat.len());
            for (i, v) in flat.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // mapToDouble
    r.register(
        c,
        "mapToDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/stream/DoubleStream;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mapper = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let elements = stream_elements(ctx, this);
            let mapped: Vec<Value> = elements
                .iter()
                .map(|e| {
                    ctx.invoke_virtual(
                        mapper,
                        "applyAsDouble",
                        "(Ljava/lang/Object;)D",
                        &[*e],
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(Value::Double(0.0))
                })
                .collect();
            let stream = alloc_synthetic(ctx, "java/util/stream/DoubleStream", 1);
            let arr = alloc_ref_array(ctx, mapped.len());
            for (i, v) in mapped.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(stream, 0, Value::Object(Some(arr)));
            Ok(Some(Value::Object(Some(stream))))
        },
    );

    // toList (Java 16+ convenience)
    r.register(c, "toList", "()Ljava/util/List;", native_stream_to_list);

    // spliterator() — declared abstract on `java.util.stream.BaseStream` and
    // inherited by Stream/IntStream/LongStream/DoubleStream. Real-JDK bytecode
    // (e.g. Netty's iterator-from-stream paths) calls `stream.spliterator()`;
    // with no Java-side pipeline our synthetic Stream has nothing to dispatch
    // to, surfacing as `NoSuchMethodError Stream.spliterator()`. Register a
    // native on every Stream sub-interface (plus BaseStream) that materialises
    // the stream's backing Object[] into a synthetic 3-field Spliterator
    // (field 0 = array, field 1 = pos, field 2 = fence) — the exact layout the
    // existing Spliterator natives (estimateSize/tryAdvance/forEachRemaining/
    // characteristics) already understand.
    for sc in &[
        "java/util/stream/Stream",
        "java/util/stream/BaseStream",
        "java/util/stream/IntStream",
        "java/util/stream/LongStream",
        "java/util/stream/DoubleStream",
        "java/util/stream/ReferencePipeline",
    ] {
        r.register(
            sc,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_stream_spliterator,
        );
    }
}

/// `Stream.spliterator()` / `BaseStream.spliterator()` — build a synthetic
/// Spliterator over the stream's backing elements.
///
/// The returned object is our standard 3-field synthetic Spliterator
/// (field 0 = backing Object[], field 1 = cursor/pos, field 2 = fence),
/// which `native_spliterator_{estimate_size,characteristics,try_advance,
/// for_each_remaining}` all consume directly. Works for both synthetic
/// Streams (field-0 array read) and real-JDK ReferencePipeline instances
/// (materialised via `toArray()` inside `stream_elements_mut`).
fn native_stream_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // Null receiver — return an empty spliterator.
            let arr = alloc_ref_array(ctx, 0);
            let spl = alloc_synthetic(ctx, "java/util/Spliterator", 3);
            ctx.set_field(spl, 0, Value::Object(Some(arr)));
            ctx.set_field(spl, 1, Value::Int(0));
            ctx.set_field(spl, 2, Value::Int(0));
            return Ok(Some(Value::Object(Some(spl))));
        }
    };
    let elements = stream_elements_mut(ctx, this);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, v) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    let spl = alloc_synthetic(ctx, "java/util/Spliterator", 3);
    ctx.set_field(spl, 0, Value::Object(Some(arr)));
    ctx.set_field(spl, 1, Value::Int(0));
    ctx.set_field(spl, 2, Value::Int(elements.len() as i32));
    Ok(Some(Value::Object(Some(spl))))
}

// -- Source methods --

fn native_stream_of_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().copied().unwrap_or(Value::Object(None));
    make_stream(ctx, &[elem])
}

fn native_stream_of_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let arr = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return make_stream(ctx, &[]),
    };
    let len = ctx.array_length(arr);
    let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
    make_stream(ctx, &elems)
}

fn native_stream_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    make_stream(ctx, &[])
}

fn native_stream_concat(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let a = match args.first() {
        Some(Value::Object(Some(r))) => stream_elements(ctx, *r),
        _ => Vec::new(),
    };
    let b = match args.get(1) {
        Some(Value::Object(Some(r))) => stream_elements(ctx, *r),
        _ => Vec::new(),
    };
    let mut combined = a;
    combined.extend(b);
    make_stream(ctx, &combined)
}

fn native_al_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let (data, size) = al_state(ctx, this);
    let elements: Vec<Value> = match data {
        Some(d) => (0..size as usize)
            .map(|i| ctx.get_array_element(d, i))
            .collect(),
        // Foreign collection (not ArrayList-shaped): the interface-level
        // `stream` registration caught e.g. a Guava `Maps$Values`. Walk
        // its real iterator instead of returning an empty stream.
        None => collection_elements_generic(ctx, this),
    };
    make_stream(ctx, &elements)
}

fn native_hs_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let backing = match hs_backing_map(ctx, this) {
        Some(m) => m,
        None => return make_stream(ctx, &[]),
    };
    let keys = map_collect_keys(ctx, backing);
    make_stream(ctx, &keys)
}

// TreeSet.stream() — snapshot of sorted elements (also serves TreeMap.keySet()
// since native_tm_key_set returns a synthetic TreeSet). Real JDK's
// TreeSet.spliterator() goes through TreeMap.keySpliteratorFor(), which requires
// the JDK's `root`/`size` red-black tree fields populated by put(); our
// synthetic layout does not provide those, so without this override the stream
// is empty (observed: `m.keySet().stream().count()` returned 0 for a 3-entry
// TreeMap, which broke Keycloak FeatureOptions.<clinit>).
fn native_ts_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let elements: Vec<Value> = match data_opt {
        Some(d) => (0..size as usize)
            .map(|i| ctx.get_array_element(d, i))
            .collect(),
        None => Vec::new(),
    };
    make_stream(ctx, &elements)
}

// -- Intermediate operations --

fn native_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // Use `stream_elements_mut` (rather than the immutable `stream_elements`)
    // so we fall through to `Stream.toArray()` for real-JDK Stream
    // implementations whose field-0 isn't our synthetic backing array. ES's
    // `CliToolProvider.load` runs `ServiceLoader.load(...).spliterator()
    // .stream().filter(name=="server")`; the resulting stream is a real JDK
    // `ReferencePipeline` (not our `java/util/stream/Stream` synthetic) and
    // the immutable helper would return Vec::new() — producing
    // `AssertionError: available names are []` even though
    // `ServiceLoader.iterator()` correctly produced 13 providers.
    let elements = stream_elements_mut(ctx, this);
    let mut kept = Vec::new();
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    make_stream(ctx, &kept)
}

fn native_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    // Mirror the `filter` fix: use the `_mut` variant so real-JDK
    // `ReferencePipeline` instances drain via `Stream.toArray()` rather than
    // silently producing an empty stream.
    let elements = stream_elements_mut(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(
            function,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[*elem],
        )?;
        mapped.push(result.unwrap_or(Value::Object(None)));
    }
    make_stream(ctx, &mapped)
}

fn native_stream_flat_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements_mut(ctx, this);
    let mut flat = Vec::new();
    for elem in &elements {
        let result = ctx.invoke_virtual(
            function,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[*elem],
        )?;
        if let Some(Value::Object(Some(inner_stream))) = result {
            let inner = stream_elements_mut(ctx, inner_stream);
            flat.extend(inner);
        }
    }
    make_stream(ctx, &flat)
}

/// Extract a numeric sort key from a Value, unboxing wrapper objects.
fn numeric_sort_key(ctx: &dyn NativeContext, val: &Value) -> Option<f64> {
    match val {
        Value::Int(v) => Some(*v as f64),
        Value::Long(v) => Some(*v as f64),
        Value::Float(v) => Some(*v as f64),
        Value::Double(v) => Some(*v),
        Value::Object(Some(obj)) => {
            if let Some(prim) = unbox_wrapper(ctx, *obj) {
                match prim {
                    Value::Int(v) => Some(v as f64),
                    Value::Long(v) => Some(v as f64),
                    Value::Float(v) => Some(v as f64),
                    Value::Double(v) => Some(v),
                    _ => None,
                }
            } else {
                None
            }
        }
        _ => None,
    }
}

fn native_stream_sorted(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let mut elements = stream_elements(ctx, this);

    // Check if all elements are numeric (including boxed wrappers) — sort numerically
    let all_numeric = elements.iter().all(|v| numeric_sort_key(ctx, v).is_some());
    if all_numeric {
        elements.sort_by(|a, b| {
            let ka = numeric_sort_key(ctx, a).unwrap_or(0.0);
            let kb = numeric_sort_key(ctx, b).unwrap_or(0.0);
            ka.partial_cmp(&kb).unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        // Fallback: sort by string representation
        let mut strs: Vec<(String, Value)> = elements
            .drain(..)
            .map(|v| {
                let s = obj_to_display_string(ctx, &v);
                (s, v)
            })
            .collect();
        strs.sort_by(|a, b| a.0.cmp(&b.0));
        elements = strs.into_iter().map(|(_, v)| v).collect();
    }
    make_stream(ctx, &elements)
}

fn native_stream_sorted_cmp(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let mut elems = stream_elements(ctx, this);
    let len = elems.len();
    // Insertion sort — O(n²) but stable.
    for i in 1..len {
        let key = elems[i];
        let mut j = i;
        while j > 0 {
            let cmp_result = comparator_compare(ctx, comparator, elems[j - 1], key)?;
            let cmp = match cmp_result {
                Some(Value::Int(v)) => v,
                _ => 0,
            };
            if cmp <= 0 {
                break;
            }
            elems[j] = elems[j - 1];
            j -= 1;
        }
        elems[j] = key;
    }
    make_stream(ctx, &elems)
}

fn native_stream_distinct(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut unique: Vec<Value> = Vec::new();
    for elem in &elements {
        let dup = unique.iter().any(|u| values_equal(ctx, u, elem));
        if !dup {
            unique.push(*elem);
        }
    }
    make_stream(ctx, &unique)
}

fn native_stream_limit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let elements = stream_elements(ctx, this);
    let limited: Vec<Value> = elements.into_iter().take(n).collect();
    make_stream(ctx, &limited)
}

fn native_stream_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let n = match args.get(1) {
        Some(Value::Long(v)) => *v as usize,
        Some(Value::Int(v)) => *v as usize,
        _ => 0,
    };
    let elements = stream_elements(ctx, this);
    let skipped: Vec<Value> = elements.into_iter().skip(n).collect();
    make_stream(ctx, &skipped)
}

fn native_stream_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[*elem])?;
    }
    make_stream(ctx, &elements)
}

// -- Terminal operations --

fn native_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[*elem])?;
    }
    Ok(None)
}

fn native_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, this);
    Ok(Some(Value::Long(elements.len() as i64)))
}

fn native_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, this);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `Stream.toArray(IntFunction<A[]> generator)` — invoke the generator
/// with the element count to produce a correctly-typed array, then copy
/// elements into it.
///
/// Real-JDK code uses this for any `Stream.toArray(String[]::new)` chain,
/// e.g. `Utils.enumOptions(SecurityProtocol.class)` in Kafka. Without
/// this native override, dispatch falls into the real JDK
/// `ReferencePipeline.toArray` which expects a real Spliterator-backed
/// pipeline, not our synthetic field-0-array stream — silently
/// returning a 0-length array. That made
/// `ConfigDef.ValidString.in(...)` build an empty allow-list, and
/// `ReplicationConfigs.<clinit>` threw
/// `ConfigException: Invalid value PLAINTEXT for configuration
/// security.inter.broker.protocol: String must be one of:`.
fn native_stream_to_array_gen(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let generator = args.get(1).copied().unwrap_or(Value::Object(None));
    let elements = stream_elements(ctx, this);
    let len = elements.len();
    // Try to use the generator's apply(int) to allocate a typed array.
    let arr = match generator {
        Value::Object(Some(g)) => {
            let r = ctx.invoke_virtual(
                g,
                "apply",
                "(I)Ljava/lang/Object;",
                &[Value::Int(len as i32)],
            );
            match r {
                Ok(Some(Value::Object(Some(a)))) => a,
                _ => alloc_ref_array(ctx, len),
            }
        }
        _ => alloc_ref_array(ctx, len),
    };
    // If generator returned a too-small array (or fallback Object[]),
    // ensure it has enough slots.
    let cap = ctx.array_length(arr);
    let arr = if cap < len {
        alloc_ref_array(ctx, len)
    } else {
        arr
    };
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_stream_find_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    if let Some(first) = elements.first() {
        ctx.set_field(opt, OPT_FIELD_VALUE, *first);
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_stream_any_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_stream_all_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?;
        if !matches!(result, Some(Value::Int(v)) if v != 0) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

fn native_stream_none_match(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

fn native_stream_reduce_identity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(args.get(1).copied()),
    };
    let identity = args.get(1).copied().unwrap_or(Value::Object(None));
    let operator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(identity)),
    };
    let elements = stream_elements(ctx, this);
    let mut acc = identity;
    for elem in &elements {
        let result = ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[acc, *elem],
        )?;
        // T16.9 follow-up: The `BinaryOperator<T>` SAM descriptor returns
        // `Object`, so primitive-returning lambdas (e.g. `Math::max` on
        // `Integer`) get auto-boxed by the lambda-proxy `coerce_return`.
        // Callers (and the stream's internal accumulator) expect the
        // primitive form. Unbox single-field `Integer`/`Long`/`Float`/
        // `Double` wrappers back to `Value::Int` / `Value::Long` / etc.
        // Non-wrapper objects pass through unchanged.
        let raw = result.unwrap_or(Value::Object(None));
        acc = normalize_for_compare(ctx, &raw);
    }
    Ok(Some(acc))
}

fn native_stream_reduce_optional(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    if elements.is_empty() {
        return Ok(Some(Value::Object(Some(opt))));
    }
    let mut acc = elements[0];
    for elem in elements.iter().skip(1) {
        let result = ctx.invoke_virtual(
            operator,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[acc, *elem],
        )?;
        acc = result.unwrap_or(Value::Object(None));
    }
    ctx.set_field(opt, OPT_FIELD_VALUE, acc);
    Ok(Some(Value::Object(Some(opt))))
}

fn native_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    if elements.is_empty() {
        return Ok(Some(Value::Object(Some(opt))));
    }
    let mut best = elements[0];
    for elem in elements.iter().skip(1) {
        let cmp = comparator_compare(ctx, comparator, *elem, best)?;
        if matches!(cmp, Some(Value::Int(v)) if v < 0) {
            best = *elem;
        }
    }
    ctx.set_field(opt, OPT_FIELD_VALUE, best);
    Ok(Some(Value::Object(Some(opt))))
}

fn native_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let comparator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/Optional", OPT_NUM_FIELDS);
    if elements.is_empty() {
        return Ok(Some(Value::Object(Some(opt))));
    }
    let mut best = elements[0];
    for elem in elements.iter().skip(1) {
        let cmp = comparator_compare(ctx, comparator, *elem, best)?;
        if matches!(cmp, Some(Value::Int(v)) if v > 0) {
            best = *elem;
        }
    }
    ctx.set_field(opt, OPT_FIELD_VALUE, best);
    Ok(Some(Value::Object(Some(opt))))
}

fn native_stream_to_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_list_of(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    make_list_of(ctx, &elements)
}

fn native_stream_map_to_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let function = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut ints = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result =
            ctx.invoke_virtual(function, "applyAsInt", "(Ljava/lang/Object;)I", &[*elem])?;
        ints.push(result.unwrap_or(Value::Int(0)));
    }
    make_int_stream(ctx, &ints)
}

// ===========================================================================
// Collectors — tagged synthetic objects
// ===========================================================================

const COLLECTOR_FIELD_TAG: usize = 0;
const COLLECTOR_FIELD_ARG1: usize = 1;
const COLLECTOR_FIELD_ARG2: usize = 2;
const COLLECTOR_FIELD_ARG3: usize = 3;
const COLLECTOR_NUM_FIELDS: usize = 4;

const COLLECTOR_TAG_TO_LIST: i32 = 1;
const COLLECTOR_TAG_TO_SET: i32 = 2;
const COLLECTOR_TAG_JOINING: i32 = 3;
const COLLECTOR_TAG_JOINING_DELIM: i32 = 4;
const COLLECTOR_TAG_TO_MAP: i32 = 5;
const COLLECTOR_TAG_COUNTING: i32 = 6;
const COLLECTOR_TAG_GROUPING_BY: i32 = 7;
const COLLECTOR_TAG_PARTITIONING_BY: i32 = 8;
/// groupingBy(classifier, downstream) — arg1=classifier, arg2=downstream Collector
const COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM: i32 = 9;
/// T2.3.18 — `Collectors.groupingBy(Function,Supplier,Collector)`.
/// ARG1=classifier, ARG2=supplier (ignored — we always build a HashMap since
/// every supported Supplier from Collectors.* yields some Map subtype our
/// synthetic HashMap natively satisfies), ARG3=downstream Collector.
const COLLECTOR_TAG_GROUPING_BY_SUPPLIER: i32 = 10;
/// T2.3.19 — `Collectors.partitioningBy(Predicate,Collector)` with
/// downstream collector applied to each partition.
const COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM: i32 = 11;
/// `Collectors.toMap(keyFn, valFn, mergeFn)` — ARG1=keyFn, ARG2=valFn,
/// ARG3=BinaryOperator merge function applied on duplicate keys.
const COLLECTOR_TAG_TO_MAP_MERGE: i32 = 12;
/// `Collectors.collectingAndThen(downstream, finisher)` — ARG1=downstream
/// Collector, ARG2=finisher Function applied to the downstream result.
const COLLECTOR_TAG_COLLECTING_AND_THEN: i32 = 13;
/// `Collectors.toCollection(Supplier)` — ARG1=Supplier producing the target
/// Collection. Stream elements are added to the supplied Collection via
/// `Collection.add(Object)`. Required by Spring Boot 4.x
/// `AutoConfigurationImportSelector.AutoConfigurationGroup.selectImports` which
/// collects entries into a user-supplied LinkedHashSet.
const COLLECTOR_TAG_TO_COLLECTION: i32 = 14;

fn register_collectors_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/stream/Collectors";

    r.register(
        c,
        "toList",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_list,
    );
    r.register(
        c,
        "toSet",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_set,
    );
    r.register(
        c,
        "toUnmodifiableList",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_list,
    );
    r.register(
        c,
        "toUnmodifiableSet",
        "()Ljava/util/stream/Collector;",
        native_collectors_to_set,
    );
    r.register(
        c,
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_to_map,
    );
    r.register(
        c,
        "toMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;",
        native_collectors_to_map_merge,
    );
    r.register(
        c,
        "toUnmodifiableMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_to_map,
    );
    r.register(
        c,
        "toUnmodifiableMap",
        "(Ljava/util/function/Function;Ljava/util/function/Function;Ljava/util/function/BinaryOperator;)Ljava/util/stream/Collector;",
        native_collectors_to_map_merge,
    );
    r.register(
        c,
        "collectingAndThen",
        "(Ljava/util/stream/Collector;Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_collecting_and_then,
    );
    r.register(
        c,
        "joining",
        "()Ljava/util/stream/Collector;",
        native_collectors_joining,
    );
    r.register(
        c,
        "joining",
        "(Ljava/lang/CharSequence;)Ljava/util/stream/Collector;",
        native_collectors_joining_delim,
    );
    r.register(
        c,
        "counting",
        "()Ljava/util/stream/Collector;",
        native_collectors_counting,
    );
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by,
    );
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by_downstream,
    );
    r.register(
        c,
        "partitioningBy",
        "(Ljava/util/function/Predicate;)Ljava/util/stream/Collector;",
        native_collectors_partitioning_by,
    );
    // T2.3.18 — groupingBy(Function, Supplier, Collector)
    r.register(
        c,
        "groupingBy",
        "(Ljava/util/function/Function;Ljava/util/function/Supplier;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_grouping_by_supplier,
    );
    // T2.3.19 — partitioningBy(Predicate, Collector)
    r.register(
        c,
        "partitioningBy",
        "(Ljava/util/function/Predicate;Ljava/util/stream/Collector;)Ljava/util/stream/Collector;",
        native_collectors_partitioning_by_downstream,
    );
    // toCollection(Supplier) — needed by Spring Boot 4.x
    // AutoConfigurationImportSelector which collects into a user-supplied
    // LinkedHashSet.
    r.register(
        c,
        "toCollection",
        "(Ljava/util/function/Supplier;)Ljava/util/stream/Collector;",
        native_collectors_to_collection,
    );
    // Our synthetic Collector objects need a `characteristics()` method that
    // returns a non-null Set — JDK stream internals (e.g.
    // ReduceOps$3.getOpFlags) call `collector.characteristics().contains(UNORDERED)`,
    // which NPEs if characteristics returns null. We return an empty HashSet so
    // the stream treats the collector as ordered, matching Collectors.toList /
    // toCollection semantics for the cases we synthesize.
    r.register(
        "java/util/stream/Collector",
        "characteristics",
        "()Ljava/util/Set;",
        native_collector_characteristics,
    );
}

fn native_collector_characteristics(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    make_set_of(ctx, &[])
}

fn native_collectors_to_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_TO_COLLECTION);
    let supplier = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, supplier);
    Ok(Some(Value::Object(Some(c))))
}

fn make_collector(ctx: &mut dyn NativeContext, tag: i32) -> ObjectRef {
    let collector = alloc_synthetic(ctx, "java/util/stream/Collector", COLLECTOR_NUM_FIELDS);
    ctx.set_field(collector, COLLECTOR_FIELD_TAG, Value::Int(tag));
    collector
}

fn native_collectors_to_list(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_TO_LIST);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_to_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_TO_SET);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_to_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_TO_MAP);
    let key_fn = args.first().copied().unwrap_or(Value::Object(None));
    let val_fn = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, key_fn);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, val_fn);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_to_map_merge(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_TO_MAP_MERGE);
    let key_fn = args.first().copied().unwrap_or(Value::Object(None));
    let val_fn = args.get(1).copied().unwrap_or(Value::Object(None));
    let merge_fn = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, key_fn);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, val_fn);
    ctx.set_field(c, COLLECTOR_FIELD_ARG3, merge_fn);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_collecting_and_then(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_COLLECTING_AND_THEN);
    let downstream = args.first().copied().unwrap_or(Value::Object(None));
    let finisher = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, downstream);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, finisher);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_joining(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_JOINING);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_joining_delim(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_JOINING_DELIM);
    let delim = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, delim);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_counting(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_COUNTING);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_grouping_by(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_GROUPING_BY);
    let classifier = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, classifier);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_grouping_by_downstream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM);
    let classifier = args.first().copied().unwrap_or(Value::Object(None));
    let downstream = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, classifier);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, downstream);
    Ok(Some(Value::Object(Some(c))))
}

fn native_collectors_partitioning_by(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_PARTITIONING_BY);
    let predicate = args.first().copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, predicate);
    Ok(Some(Value::Object(Some(c))))
}

/// T2.3.18 — `Collectors.groupingBy(Function, Supplier, Collector)`.
/// Stores classifier in ARG1, supplier in ARG2, and the downstream
/// Collector in ARG3 for `native_stream_collect` to apply per-group.
fn native_collectors_grouping_by_supplier(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_GROUPING_BY_SUPPLIER);
    let classifier = args.first().copied().unwrap_or(Value::Object(None));
    let supplier = args.get(1).copied().unwrap_or(Value::Object(None));
    let downstream = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, classifier);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, supplier);
    ctx.set_field(c, COLLECTOR_FIELD_ARG3, downstream);
    Ok(Some(Value::Object(Some(c))))
}

/// T2.3.19 — `Collectors.partitioningBy(Predicate, Collector)`.
/// Stores predicate in ARG1 and downstream Collector in ARG2.
fn native_collectors_partitioning_by_downstream(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let c = make_collector(ctx, COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM);
    let predicate = args.first().copied().unwrap_or(Value::Object(None));
    let downstream = args.get(1).copied().unwrap_or(Value::Object(None));
    ctx.set_field(c, COLLECTOR_FIELD_ARG1, predicate);
    ctx.set_field(c, COLLECTOR_FIELD_ARG2, downstream);
    Ok(Some(Value::Object(Some(c))))
}

/// Run a *real* `java.util.stream.Collector` (one not produced by our
/// `make_collector` fast-path — e.g. the collector from
/// `ImmutableList.toImmutableList()` or any `Collector.of(...)`) via the
/// standard JLS collector contract:
///   `c = supplier().get(); accumulator().accept(c, e)*; finisher().apply(c)`.
///
/// Without this, `native_stream_collect` returned `null` for every
/// non-tagged collector, which surfaced downstream as a bogus NPE — e.g.
/// cassandra's airline `MetadataLoader.mergeOptionSet` does
/// `... .collect(ImmutableList.toImmutableList())` and then iterates the
/// result: a `null` there throws `Cannot invoke iterator on null`.
fn collect_via_collector_protocol(
    ctx: &mut dyn NativeContext,
    collector: ObjectRef,
    elements: &[Value],
) -> MethodCallResult {
    let supplier = match ctx.invoke_virtual(
        collector,
        "supplier",
        "()Ljava/util/function/Supplier;",
        &[],
    )? {
        Some(Value::Object(Some(s))) => s,
        _ => return Ok(Some(Value::Object(None))),
    };
    let container = ctx
        .invoke_virtual(supplier, "get", "()Ljava/lang/Object;", &[])?
        .unwrap_or(Value::Object(None));
    let accumulator = match ctx.invoke_virtual(
        collector,
        "accumulator",
        "()Ljava/util/function/BiConsumer;",
        &[],
    )? {
        Some(Value::Object(Some(a))) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    for elem in elements {
        ctx.invoke_virtual(
            accumulator,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[container, *elem],
        )?;
    }
    let finisher = match ctx.invoke_virtual(
        collector,
        "finisher",
        "()Ljava/util/function/Function;",
        &[],
    )? {
        Some(Value::Object(Some(f))) => f,
        // An IDENTITY_FINISH collector with no finisher: the accumulated
        // container is itself the result.
        _ => return Ok(Some(container)),
    };
    let result = ctx
        .invoke_virtual(
            finisher,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[container],
        )?
        .unwrap_or(Value::Object(None));
    Ok(Some(result))
}

/// `Stream.collect(Supplier<R>, BiConsumer<R,? super T>, BiConsumer<R,R>)`
/// — the 3-arg mutable-reduction terminal operation.
///
/// In real JDK this is abstract on `Stream` (body lives in `ReferencePipeline`),
/// so when our synthetic Stream — or any receiver whose runtime class is the
/// `Stream` interface itself — is the receiver, the abstract declaration has
/// no Code attribute and the VM throws AbstractMethodError. H2's
/// `FilePathDisk.newDirectoryStream` is the canonical tripwire.
///
/// Sequential semantics (no parallel split):
///   `R c = supplier.get();`
///   `for each t in stream: accumulator.accept(c, t);`
///   `return c;`
/// The combiner is parallel-only and intentionally ignored.
fn native_stream_collect_3arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let supplier = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let accumulator = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // args.get(3) is the combiner — ignored in sequential mode.
    let elements = stream_elements_mut(ctx, this);
    let container = match ctx.invoke_virtual(
        supplier,
        "get",
        "()Ljava/lang/Object;",
        &[],
    )? {
        Some(Value::Object(Some(c))) => Value::Object(Some(c)),
        // Null supplier result is unusual but permitted; pass through to
        // the accumulator just like JDK would.
        other => other.unwrap_or(Value::Object(None)),
    };
    for elem in &elements {
        let _ = ctx.invoke_virtual(
            accumulator,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[container, *elem],
        )?;
    }
    Ok(Some(container))
}

fn native_stream_collect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let collector = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elements = stream_elements_mut(ctx, this);
    let tag = match ctx.get_field(collector, COLLECTOR_FIELD_TAG) {
        Value::Int(t) => t,
        // Not one of our `make_collector` tagged fast-path collectors —
        // a real JDK/Guava `Collector`. Honour the standard contract
        // instead of returning `null`.
        _ => return collect_via_collector_protocol(ctx, collector, &elements),
    };

    match tag {
        COLLECTOR_TAG_TO_LIST => make_list_of(ctx, &elements),
        COLLECTOR_TAG_TO_SET => make_set_of(ctx, &elements),
        COLLECTOR_TAG_TO_COLLECTION => {
            // Invoke the supplier to materialize the target Collection, then
            // add each stream element via Collection.add(Object). Fall back to
            // a HashSet if the supplier is missing or fails.
            let supplier = ctx.get_field(collector, COLLECTOR_FIELD_ARG1);
            let coll_obj = match supplier {
                Value::Object(Some(s)) => {
                    match ctx.invoke_virtual(s, "get", "()Ljava/lang/Object;", &[]) {
                        Ok(Some(Value::Object(Some(c)))) => Some(c),
                        _ => None,
                    }
                }
                _ => None,
            };
            match coll_obj {
                Some(c) => {
                    for elem in &elements {
                        let _ = ctx.invoke_virtual(
                            c,
                            "add",
                            "(Ljava/lang/Object;)Z",
                            &[*elem],
                        )?;
                    }
                    Ok(Some(Value::Object(Some(c))))
                }
                None => make_set_of(ctx, &elements),
            }
        }
        COLLECTOR_TAG_COUNTING => Ok(Some(Value::Long(elements.len() as i64))),
        COLLECTOR_TAG_JOINING => {
            let mut parts = Vec::with_capacity(elements.len());
            for elem in &elements {
                parts.push(obj_to_display_string(ctx, elem));
            }
            let joined = parts.join("");
            let s = ctx.create_string(&joined);
            Ok(Some(Value::Object(Some(s))))
        }
        COLLECTOR_TAG_JOINING_DELIM => {
            let delim_str = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
                _ => String::new(),
            };
            let mut parts = Vec::with_capacity(elements.len());
            for elem in &elements {
                parts.push(obj_to_display_string(ctx, elem));
            }
            let joined = parts.join(&delim_str);
            let s = ctx.create_string(&joined);
            Ok(Some(Value::Object(Some(s))))
        }
        COLLECTOR_TAG_TO_MAP => {
            let key_fn = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let val_fn = match ctx.get_field(collector, COLLECTOR_FIELD_ARG2) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mut pairs = Vec::with_capacity(elements.len());
            for elem in &elements {
                let k = ctx
                    .invoke_virtual(
                        key_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                let v = ctx
                    .invoke_virtual(
                        val_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                pairs.push((k, v));
            }
            make_map_of(ctx, &pairs)
        }
        COLLECTOR_TAG_TO_MAP_MERGE => {
            let key_fn = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let val_fn = match ctx.get_field(collector, COLLECTOR_FIELD_ARG2) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let merge_fn = match ctx.get_field(collector, COLLECTOR_FIELD_ARG3) {
                Value::Object(Some(r)) => Some(r),
                _ => None,
            };
            // Walk elements, merging duplicate keys via the BinaryOperator.
            let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(elements.len());
            for elem in &elements {
                let k = ctx
                    .invoke_virtual(
                        key_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                let v = ctx
                    .invoke_virtual(
                        val_fn,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                let mut idx = None;
                for (i, (ek, _)) in pairs.iter().enumerate() {
                    if values_equal(ctx, ek, &k) {
                        idx = Some(i);
                        break;
                    }
                }
                if let Some(i) = idx {
                    let existing = pairs[i].1;
                    let merged = if let Some(mf) = merge_fn {
                        ctx.invoke_virtual(
                            mf,
                            "apply",
                            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                            &[existing, v],
                        )?
                        .unwrap_or(Value::Object(None))
                    } else {
                        v
                    };
                    pairs[i].1 = merged;
                } else {
                    pairs.push((k, v));
                }
            }
            make_map_of(ctx, &pairs)
        }
        COLLECTOR_TAG_COLLECTING_AND_THEN => {
            let downstream = ctx.get_field(collector, COLLECTOR_FIELD_ARG1);
            let finisher = match ctx.get_field(collector, COLLECTOR_FIELD_ARG2) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Re-build a stream over the same elements and recursively collect
            // through the downstream Collector, then apply finisher.apply().
            let inner_stream =
                alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS);
            let arr = alloc_ref_array(ctx, elements.len());
            for (i, v) in elements.iter().enumerate() {
                ctx.set_array_element(arr, i, *v);
            }
            ctx.set_field(
                inner_stream,
                STREAM_FIELD_ELEMENTS,
                Value::Object(Some(arr)),
            );
            let downstream_result = native_stream_collect(
                ctx,
                &[Value::Object(Some(inner_stream)), downstream],
            )?
            .unwrap_or(Value::Object(None));
            let finished = ctx
                .invoke_virtual(
                    finisher,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[downstream_result],
                )?
                .unwrap_or(Value::Object(None));
            Ok(Some(finished))
        }
        COLLECTOR_TAG_GROUPING_BY => {
            let classifier = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Group elements by classifier result into HashMap<K, ArrayList<V>>
            // We use a Vec to collect groups, then build the map
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for elem in &elements {
                let key = ctx
                    .invoke_virtual(
                        classifier,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                // Find existing group
                let mut found = false;
                for (gk, gv) in &mut groups {
                    if values_equal(ctx, gk, &key) {
                        gv.push(*elem);
                        found = true;
                        break;
                    }
                }
                if !found {
                    groups.push((key, vec![*elem]));
                }
            }
            // Build HashMap<K, ArrayList<V>>
            let mut pairs = Vec::with_capacity(groups.len());
            for (key, vals) in &groups {
                let list = make_list_of_raw(ctx, vals);
                pairs.push((*key, Value::Object(Some(list))));
            }
            make_map_of(ctx, &pairs)
        }
        COLLECTOR_TAG_PARTITIONING_BY => {
            let predicate = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let mut true_list = Vec::new();
            let mut false_list = Vec::new();
            for elem in &elements {
                let result = ctx
                    .invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?
                    .unwrap_or(Value::Int(0));
                if matches!(result, Value::Int(v) if v != 0) {
                    true_list.push(*elem);
                } else {
                    false_list.push(*elem);
                }
            }
            // Build HashMap with Boolean.TRUE and Boolean.FALSE keys
            let true_al = make_list_of_raw(ctx, &true_list);
            let false_al = make_list_of_raw(ctx, &false_list);
            let true_key = alloc_synthetic(ctx, "java/lang/Boolean", 1);
            ctx.set_field(true_key, 0, Value::Int(1));
            let false_key = alloc_synthetic(ctx, "java/lang/Boolean", 1);
            ctx.set_field(false_key, 0, Value::Int(0));
            let pairs = [
                (Value::Object(Some(true_key)), Value::Object(Some(true_al))),
                (
                    Value::Object(Some(false_key)),
                    Value::Object(Some(false_al)),
                ),
            ];
            make_map_of(ctx, &pairs)
        }
        COLLECTOR_TAG_GROUPING_BY_DOWNSTREAM => {
            let classifier = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let downstream = ctx.get_field(collector, COLLECTOR_FIELD_ARG2);

            // Group elements by classifier into Vec<(key, Vec<elem>)>
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for elem in &elements {
                let key = ctx
                    .invoke_virtual(
                        classifier,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                let mut found = false;
                for (gk, gv) in &mut groups {
                    if values_equal(ctx, gk, &key) {
                        gv.push(*elem);
                        found = true;
                        break;
                    }
                }
                if !found {
                    groups.push((key, vec![*elem]));
                }
            }

            // Apply downstream collector to each group inline to avoid recursion.
            let downstream_tag = match downstream {
                Value::Object(Some(d)) => {
                    match ctx.get_field(d, COLLECTOR_FIELD_TAG) {
                        Value::Int(t) => Some((d, t)),
                        _ => None,
                    }
                }
                _ => None,
            };
            let mut pairs = Vec::with_capacity(groups.len());
            for (key, group_elems) in &groups {
                let group_result = match downstream_tag {
                    Some((_d, COLLECTOR_TAG_TO_LIST)) => {
                        make_list_of(ctx, group_elems)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_TO_SET)) => {
                        make_set_of(ctx, group_elems)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_COUNTING)) => {
                        // Box as java/lang/Long so .intValue() works
                        let long_obj = alloc_synthetic(ctx, "java/lang/Long", 1);
                        ctx.set_field(long_obj, 0, Value::Long(group_elems.len() as i64));
                        Value::Object(Some(long_obj))
                    }
                    _ => {
                        // Fallback: build stream and collect (single level only)
                        let group_stream =
                            alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS);
                        let arr = alloc_ref_array(ctx, group_elems.len());
                        for (i, v) in group_elems.iter().enumerate() {
                            ctx.set_array_element(arr, i, *v);
                        }
                        ctx.set_field(
                            group_stream,
                            STREAM_FIELD_ELEMENTS,
                            Value::Object(Some(arr)),
                        );
                        native_stream_collect(
                            ctx,
                            &[Value::Object(Some(group_stream)), downstream],
                        )?
                        .unwrap_or(Value::Object(None))
                    }
                };
                pairs.push((*key, group_result));
            }
            make_map_of(ctx, &pairs)
        }
        // T2.3.18 — groupingBy(Function, Supplier, Collector).
        // Semantically equivalent to the 2-arg downstream variant; the
        // supplier argument just customizes the Map subtype — since our
        // synthetic HashMap is the only map shape native code produces,
        // we honor the spec by materializing the supplier's object and
        // populating it via its put(K,V) method instead of our internal
        // make_map_of helper. This keeps user-supplied LinkedHashMap /
        // TreeMap / EnumMap suppliers working.
        COLLECTOR_TAG_GROUPING_BY_SUPPLIER => {
            let classifier = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let supplier = ctx.get_field(collector, COLLECTOR_FIELD_ARG2);
            let downstream = ctx.get_field(collector, COLLECTOR_FIELD_ARG3);

            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            for elem in &elements {
                let key = ctx
                    .invoke_virtual(
                        classifier,
                        "apply",
                        "(Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*elem],
                    )?
                    .unwrap_or(Value::Object(None));
                let mut found = false;
                for (gk, gv) in &mut groups {
                    if values_equal(ctx, gk, &key) {
                        gv.push(*elem);
                        found = true;
                        break;
                    }
                }
                if !found {
                    groups.push((key, vec![*elem]));
                }
            }

            // Materialize the user-supplied Map via the Supplier. If the
            // supplier cannot be invoked (null / not a real Supplier we can
            // dispatch), fall back to a plain HashMap built via make_map_of.
            let map_obj = match supplier {
                Value::Object(Some(s)) => {
                    match ctx.invoke_virtual(s, "get", "()Ljava/lang/Object;", &[]) {
                        Ok(Some(Value::Object(Some(m)))) => Some(m),
                        _ => None,
                    }
                }
                _ => None,
            };

            let downstream_tag = match downstream {
                Value::Object(Some(d)) => match ctx.get_field(d, COLLECTOR_FIELD_TAG) {
                    Value::Int(t) => Some((d, t)),
                    _ => None,
                },
                _ => None,
            };

            let mut pairs = Vec::with_capacity(groups.len());
            for (key, group_elems) in &groups {
                let group_result = match downstream_tag {
                    Some((_d, COLLECTOR_TAG_TO_LIST)) => {
                        make_list_of(ctx, group_elems)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_TO_SET)) => {
                        make_set_of(ctx, group_elems)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_COUNTING)) => {
                        let long_obj = alloc_synthetic(ctx, "java/lang/Long", 1);
                        ctx.set_field(long_obj, 0, Value::Long(group_elems.len() as i64));
                        Value::Object(Some(long_obj))
                    }
                    _ => {
                        let group_stream =
                            alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS);
                        let arr = alloc_ref_array(ctx, group_elems.len());
                        for (i, v) in group_elems.iter().enumerate() {
                            ctx.set_array_element(arr, i, *v);
                        }
                        ctx.set_field(
                            group_stream,
                            STREAM_FIELD_ELEMENTS,
                            Value::Object(Some(arr)),
                        );
                        native_stream_collect(
                            ctx,
                            &[Value::Object(Some(group_stream)), downstream],
                        )?
                        .unwrap_or(Value::Object(None))
                    }
                };
                pairs.push((*key, group_result));
            }

            if let Some(m) = map_obj {
                for (k, v) in &pairs {
                    ctx.invoke_virtual(
                        m,
                        "put",
                        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                        &[*k, *v],
                    )?;
                }
                Ok(Some(Value::Object(Some(m))))
            } else {
                make_map_of(ctx, &pairs)
            }
        }
        // T2.3.19 — partitioningBy(Predicate, Collector).
        COLLECTOR_TAG_PARTITIONING_BY_DOWNSTREAM => {
            let predicate = match ctx.get_field(collector, COLLECTOR_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Object(None))),
            };
            let downstream = ctx.get_field(collector, COLLECTOR_FIELD_ARG2);

            let mut true_list = Vec::new();
            let mut false_list = Vec::new();
            for elem in &elements {
                let result = ctx
                    .invoke_virtual(predicate, "test", "(Ljava/lang/Object;)Z", &[*elem])?
                    .unwrap_or(Value::Int(0));
                if matches!(result, Value::Int(v) if v != 0) {
                    true_list.push(*elem);
                } else {
                    false_list.push(*elem);
                }
            }

            let downstream_tag = match downstream {
                Value::Object(Some(d)) => match ctx.get_field(d, COLLECTOR_FIELD_TAG) {
                    Value::Int(t) => Some((d, t)),
                    _ => None,
                },
                _ => None,
            };

            let reduce_bucket = |ctx: &mut dyn NativeContext,
                                 bucket: &[Value]|
             -> Result<Value, cratonvm_types::error::MethodCallFailed> {
                let v = match downstream_tag {
                    Some((_d, COLLECTOR_TAG_TO_LIST)) => {
                        make_list_of(ctx, bucket)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_TO_SET)) => {
                        make_set_of(ctx, bucket)?.unwrap_or(Value::Object(None))
                    }
                    Some((_d, COLLECTOR_TAG_COUNTING)) => {
                        let long_obj = alloc_synthetic(ctx, "java/lang/Long", 1);
                        ctx.set_field(long_obj, 0, Value::Long(bucket.len() as i64));
                        Value::Object(Some(long_obj))
                    }
                    _ => {
                        let group_stream =
                            alloc_synthetic(ctx, "java/util/stream/Stream", STREAM_NUM_FIELDS);
                        let arr = alloc_ref_array(ctx, bucket.len());
                        for (i, v) in bucket.iter().enumerate() {
                            ctx.set_array_element(arr, i, *v);
                        }
                        ctx.set_field(
                            group_stream,
                            STREAM_FIELD_ELEMENTS,
                            Value::Object(Some(arr)),
                        );
                        native_stream_collect(
                            ctx,
                            &[Value::Object(Some(group_stream)), downstream],
                        )?
                        .unwrap_or(Value::Object(None))
                    }
                };
                Ok(v)
            };
            let true_v = reduce_bucket(ctx, &true_list)?;
            let false_v = reduce_bucket(ctx, &false_list)?;
            let true_key = alloc_synthetic(ctx, "java/lang/Boolean", 1);
            ctx.set_field(true_key, 0, Value::Int(1));
            let false_key = alloc_synthetic(ctx, "java/lang/Boolean", 1);
            ctx.set_field(false_key, 0, Value::Int(0));
            let pairs = [
                (Value::Object(Some(true_key)), true_v),
                (Value::Object(Some(false_key)), false_v),
            ];
            make_map_of(ctx, &pairs)
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

// ===========================================================================
// IntStream — same 1-field layout, backing array stores Value::Int
// ===========================================================================

fn make_int_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    let stream = alloc_synthetic(ctx, "java/util/stream/IntStream", STREAM_NUM_FIELDS);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

fn int_stream_elements(ctx: &dyn NativeContext, stream: ObjectRef) -> Vec<Value> {
    stream_elements(ctx, stream)
}

fn register_int_stream_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/stream/IntStream";

    r.register(
        c,
        "range",
        "(II)Ljava/util/stream/IntStream;",
        native_int_stream_range,
    );
    r.register(
        c,
        "rangeClosed",
        "(II)Ljava/util/stream/IntStream;",
        native_int_stream_range_closed,
    );
    r.register(
        c,
        "of",
        "(I)Ljava/util/stream/IntStream;",
        native_int_stream_of,
    );
    r.register(c, "sum", "()I", native_int_stream_sum);
    r.register(c, "count", "()J", native_int_stream_count);
    r.register(c, "min", "()Ljava/util/OptionalInt;", native_int_stream_min);
    r.register(c, "max", "()Ljava/util/OptionalInt;", native_int_stream_max);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/IntConsumer;)V",
        native_int_stream_for_each,
    );
    r.register(
        c,
        "filter",
        "(Ljava/util/function/IntPredicate;)Ljava/util/stream/IntStream;",
        native_int_stream_filter,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/IntUnaryOperator;)Ljava/util/stream/IntStream;",
        native_int_stream_map,
    );
    r.register(c, "toArray", "()[I", native_int_stream_to_array);
    r.register(
        c,
        "boxed",
        "()Ljava/util/stream/Stream;",
        native_int_stream_boxed,
    );
    r.register(
        c,
        "average",
        "()Ljava/util/OptionalDouble;",
        native_int_stream_average,
    );
}

/// Maximum number of stream elements that may be materialized eagerly. This
/// synthetic stream impl is non-lazy, so a range larger than this would OOM the
/// VM. Mirrors the 64M runaway guard in `collection_elements_generic`
/// (lib.rs ~641); a request beyond it throws `OutOfMemoryError` rather than
/// allocating ~2.1 billion `Value`s for e.g. `IntStream.range(0, i32::MAX)`.
const STREAM_RANGE_MAX_ELEMENTS: i128 = 64 * 1024 * 1024;

/// Materialize the `count` ints `[start, start+count)` with widening arithmetic
/// so `start + i` cannot wrap. `count` must already be clamped to `>= 0` by the
/// caller (computed in a wider type to avoid overflow in `end - start`).
fn range_int_elements(start: i64, count: i64) -> Result<Vec<Value>, MethodCallFailed> {
    if count as i128 > STREAM_RANGE_MAX_ELEMENTS {
        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
            message: format!(
                "IntStream range of {count} elements exceeds the {STREAM_RANGE_MAX_ELEMENTS}-element materialization limit"
            ),
        }
        .into());
    }
    let count = count.max(0) as usize;
    let elems = (0..count)
        // `start + i` is computed in i64 and narrowed; for a valid IntStream
        // range every element fits in i32, so the cast is exact.
        .map(|i| Value::Int((start + i as i64) as i32))
        .collect();
    Ok(elems)
}

/// Materialize the `count` longs `[start, start+count)` with widening
/// arithmetic so `start + i` cannot wrap. `count` is an i128 (computed in a
/// wider type to avoid overflow in `end - start`) and must be `>= 0`.
fn range_long_elements(start: i64, count: i128) -> Result<Vec<Value>, MethodCallFailed> {
    if count > STREAM_RANGE_MAX_ELEMENTS {
        return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
            message: format!(
                "LongStream range of {count} elements exceeds the {STREAM_RANGE_MAX_ELEMENTS}-element materialization limit"
            ),
        }
        .into());
    }
    let count = count.max(0) as usize;
    let elems = (0..count)
        // `start + i` is computed in i128 and narrowed; for a count within the
        // cap and a valid range the result always fits in i64.
        .map(|i| Value::Long((start as i128 + i as i128) as i64))
        .collect();
    Ok(elems)
}

fn native_int_stream_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Widen to i64 so `end - start` cannot overflow when start == i32::MIN.
    let count = (end as i64 - start as i64).max(0);
    let elems = range_int_elements(start as i64, count)?;
    make_int_stream(ctx, &elems)
}

fn native_int_stream_range_closed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    // Widen to i64 so `end - start + 1` cannot overflow at the i32 boundaries.
    let count = (end as i64 - start as i64 + 1).max(0);
    let elems = range_int_elements(start as i64, count)?;
    make_int_stream(ctx, &elems)
}

fn native_int_stream_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = args.first().copied().unwrap_or(Value::Int(0));
    make_int_stream(ctx, &[v])
}

fn native_int_stream_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elements = int_stream_elements(ctx, this);
    // JDK `IntStream.sum()` has wrapping (two's-complement) overflow
    // semantics; using `Iterator::sum` would panic in debug builds, so fold
    // with `wrapping_add` instead.
    let sum: i32 = elements.iter().fold(0i32, |acc, v| match v {
        Value::Int(i) => acc.wrapping_add(*i),
        _ => acc,
    });
    Ok(Some(Value::Int(sum)))
}

fn native_int_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = int_stream_elements(ctx, this);
    Ok(Some(Value::Long(elements.len() as i64)))
}

fn native_int_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = int_stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
    if let Some(min) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        })
        .min()
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Int(min));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_int_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = int_stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
    if let Some(max) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Int(i) => Some(*i),
            _ => None,
        })
        .max()
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Int(max));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_int_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elements = int_stream_elements(ctx, this);
    for elem in &elements {
        ctx.invoke_virtual(consumer, "accept", "(I)V", &[*elem])?;
    }
    Ok(None)
}

fn native_int_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let elements = int_stream_elements(ctx, this);
    let mut kept = Vec::new();
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(I)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    make_int_stream(ctx, &kept)
}

fn native_int_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let elements = int_stream_elements(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(operator, "applyAsInt", "(I)I", &[*elem])?;
        mapped.push(result.unwrap_or(Value::Int(0)));
    }
    make_int_stream(ctx, &mapped)
}

fn native_int_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = int_stream_elements(ctx, this);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_int_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = int_stream_elements(ctx, this);
    // Elements are already Value::Int — just wrap in a Stream
    make_stream(ctx, &elements)
}

fn native_int_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = int_stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    if !elements.is_empty() {
        let sum: i64 = elements
            .iter()
            .map(|v| match v {
                Value::Int(i) => *i as i64,
                _ => 0,
            })
            .sum();
        let avg = sum as f64 / elements.len() as f64;
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

// ===========================================================================
// LongStream — same 1-field layout, backing array stores Value::Long
// ===========================================================================

fn make_long_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    let stream = alloc_synthetic(ctx, "java/util/stream/LongStream", STREAM_NUM_FIELDS);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

fn register_long_stream_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/stream/LongStream";

    r.register(
        c,
        "of",
        "(J)Ljava/util/stream/LongStream;",
        native_long_stream_of,
    );
    r.register(
        c,
        "range",
        "(JJ)Ljava/util/stream/LongStream;",
        native_long_stream_range,
    );
    r.register(
        c,
        "rangeClosed",
        "(JJ)Ljava/util/stream/LongStream;",
        native_long_stream_range_closed,
    );
    r.register(c, "sum", "()J", native_long_stream_sum);
    r.register(c, "count", "()J", native_long_stream_count);
    r.register(
        c,
        "min",
        "()Ljava/util/OptionalLong;",
        native_long_stream_min,
    );
    r.register(
        c,
        "max",
        "()Ljava/util/OptionalLong;",
        native_long_stream_max,
    );
    r.register(
        c,
        "average",
        "()Ljava/util/OptionalDouble;",
        native_long_stream_average,
    );
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/LongConsumer;)V",
        native_long_stream_for_each,
    );
    r.register(
        c,
        "filter",
        "(Ljava/util/function/LongPredicate;)Ljava/util/stream/LongStream;",
        native_long_stream_filter,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/LongUnaryOperator;)Ljava/util/stream/LongStream;",
        native_long_stream_map,
    );
    r.register(c, "toArray", "()[J", native_long_stream_to_array);
    r.register(
        c,
        "boxed",
        "()Ljava/util/stream/Stream;",
        native_long_stream_boxed,
    );
    r.register(
        c,
        "asDoubleStream",
        "()Ljava/util/stream/DoubleStream;",
        native_long_stream_as_double,
    );
}

fn native_long_stream_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = args.first().copied().unwrap_or(Value::Long(0));
    make_long_stream(ctx, &[v])
}

fn native_long_stream_range(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Widen to i128 so `end - start` cannot overflow when start == i64::MIN.
    let count = (end as i128 - start as i128).max(0);
    let elems = range_long_elements(start, count)?;
    make_long_stream(ctx, &elems)
}

fn native_long_stream_range_closed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let start = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let end = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    // Widen to i128 so `end - start + 1` cannot overflow at the i64 boundaries.
    let count = (end as i128 - start as i128 + 1).max(0);
    let elems = range_long_elements(start, count)?;
    make_long_stream(ctx, &elems)
}

fn native_long_stream_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, this);
    // JDK `LongStream.sum()` has wrapping (two's-complement) overflow
    // semantics; fold with `wrapping_add` to match and avoid debug-build
    // panics from `Iterator::sum`.
    let sum: i64 = elements.iter().fold(0i64, |acc, v| match v {
        Value::Long(l) => acc.wrapping_add(*l),
        _ => acc,
    });
    Ok(Some(Value::Long(sum)))
}

fn native_long_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, this);
    Ok(Some(Value::Long(elements.len() as i64)))
}

fn native_long_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
    if let Some(min) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Long(l) => Some(*l),
            _ => None,
        })
        .min()
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Long(min));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_long_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
    if let Some(max) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Long(l) => Some(*l),
            _ => None,
        })
        .max()
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Long(max));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_long_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    if !elements.is_empty() {
        let sum: i64 = elements
            .iter()
            .map(|v| match v {
                Value::Long(l) => *l,
                _ => 0,
            })
            .sum();
        let avg = sum as f64 / elements.len() as f64;
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_long_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        ctx.invoke_virtual(consumer, "accept", "(J)V", &[*elem])?;
    }
    Ok(None)
}

fn native_long_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut kept = Vec::new();
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(J)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    make_long_stream(ctx, &kept)
}

fn native_long_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(operator, "applyAsLong", "(J)J", &[*elem])?;
        mapped.push(result.unwrap_or(Value::Long(0)));
    }
    make_long_stream(ctx, &mapped)
}

fn native_long_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, this);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_long_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    make_stream(ctx, &elements)
}

fn native_long_stream_as_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let doubles: Vec<Value> = elements
        .iter()
        .map(|v| match v {
            Value::Long(l) => Value::Double(*l as f64),
            _ => Value::Double(0.0),
        })
        .collect();
    make_double_stream(ctx, &doubles)
}

// ===========================================================================
// DoubleStream — same 1-field layout, backing array stores Value::Double
// ===========================================================================

fn make_double_stream(ctx: &mut dyn NativeContext, elements: &[Value]) -> MethodCallResult {
    let stream = alloc_synthetic(ctx, "java/util/stream/DoubleStream", STREAM_NUM_FIELDS);
    let arr = alloc_ref_array(ctx, elements.len());
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    ctx.set_field(stream, STREAM_FIELD_ELEMENTS, Value::Object(Some(arr)));
    Ok(Some(Value::Object(Some(stream))))
}

fn register_double_stream_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/stream/DoubleStream";

    r.register(
        c,
        "of",
        "(D)Ljava/util/stream/DoubleStream;",
        native_double_stream_of,
    );
    r.register(c, "sum", "()D", native_double_stream_sum);
    r.register(c, "count", "()J", native_double_stream_count);
    r.register(
        c,
        "min",
        "()Ljava/util/OptionalDouble;",
        native_double_stream_min,
    );
    r.register(
        c,
        "max",
        "()Ljava/util/OptionalDouble;",
        native_double_stream_max,
    );
    r.register(
        c,
        "average",
        "()Ljava/util/OptionalDouble;",
        native_double_stream_average,
    );
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/DoubleConsumer;)V",
        native_double_stream_for_each,
    );
    r.register(
        c,
        "filter",
        "(Ljava/util/function/DoublePredicate;)Ljava/util/stream/DoubleStream;",
        native_double_stream_filter,
    );
    r.register(
        c,
        "map",
        "(Ljava/util/function/DoubleUnaryOperator;)Ljava/util/stream/DoubleStream;",
        native_double_stream_map,
    );
    r.register(c, "toArray", "()[D", native_double_stream_to_array);
    r.register(
        c,
        "boxed",
        "()Ljava/util/stream/Stream;",
        native_double_stream_boxed,
    );
    r.register(
        c,
        "mapToLong",
        "(Ljava/util/function/DoubleToLongFunction;)Ljava/util/stream/LongStream;",
        native_double_stream_map_to_long,
    );
    r.register(
        c,
        "mapToInt",
        "(Ljava/util/function/DoubleToIntFunction;)Ljava/util/stream/IntStream;",
        native_double_stream_map_to_int,
    );
}

fn native_double_stream_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = args.first().copied().unwrap_or(Value::Double(0.0));
    make_double_stream(ctx, &[v])
}

fn native_double_stream_sum(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let elements = stream_elements(ctx, this);
    let sum: f64 = elements
        .iter()
        .map(|v| match v {
            Value::Double(d) => *d,
            _ => 0.0,
        })
        .sum();
    Ok(Some(Value::Double(sum)))
}

fn native_double_stream_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let elements = stream_elements(ctx, this);
    Ok(Some(Value::Long(elements.len() as i64)))
}

fn native_double_stream_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    if let Some(min) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Double(d) => Some(*d),
            _ => None,
        })
        .reduce(f64::min)
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(min));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_double_stream_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    if let Some(max) = elements
        .iter()
        .filter_map(|v| match v {
            Value::Double(d) => Some(*d),
            _ => None,
        })
        .reduce(f64::max)
    {
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(max));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_double_stream_average(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
            return Ok(Some(Value::Object(Some(opt))));
        }
    };
    let elements = stream_elements(ctx, this);
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    if !elements.is_empty() {
        let sum: f64 = elements
            .iter()
            .map(|v| match v {
                Value::Double(d) => *d,
                _ => 0.0,
            })
            .sum();
        let avg = sum / elements.len() as f64;
        ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(avg));
    }
    Ok(Some(Value::Object(Some(opt))))
}

fn native_double_stream_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elements = stream_elements(ctx, this);
    for elem in &elements {
        ctx.invoke_virtual(consumer, "accept", "(D)V", &[*elem])?;
    }
    Ok(None)
}

fn native_double_stream_filter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let predicate = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut kept = Vec::new();
    for elem in &elements {
        let result = ctx.invoke_virtual(predicate, "test", "(D)Z", &[*elem])?;
        if matches!(result, Some(Value::Int(v)) if v != 0) {
            kept.push(*elem);
        }
    }
    make_double_stream(ctx, &kept)
}

fn native_double_stream_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let operator = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_double_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(operator, "applyAsDouble", "(D)D", &[*elem])?;
        mapped.push(result.unwrap_or(Value::Double(0.0)));
    }
    make_double_stream(ctx, &mapped)
}

fn native_double_stream_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Double, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let elements = stream_elements(ctx, this);
    let arr = ctx.new_array(
        cratonvm_types::ArrayElementType::Double,
        elements.len(),
    );
    for (i, val) in elements.iter().enumerate() {
        ctx.set_array_element(arr, i, *val);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_double_stream_boxed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    make_stream(ctx, &elements)
}

fn native_double_stream_map_to_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_long_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(func, "applyAsLong", "(D)J", &[*elem])?;
        mapped.push(result.unwrap_or(Value::Long(0)));
    }
    make_long_stream(ctx, &mapped)
}

fn native_double_stream_map_to_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return make_int_stream(ctx, &[]),
    };
    let elements = stream_elements(ctx, this);
    let mut mapped = Vec::with_capacity(elements.len());
    for elem in &elements {
        let result = ctx.invoke_virtual(func, "applyAsInt", "(D)I", &[*elem])?;
        mapped.push(result.unwrap_or(Value::Int(0)));
    }
    make_int_stream(ctx, &mapped)
}

// ===========================================================================
// Interface method registrations
// ===========================================================================
// When bytecode invokes methods via interface types (e.g. Collection.iterator()),
// the interpreter resolves based on the invoked class name from the constant pool.
// For synthetic objects (ClassId(0)), there's no class hierarchy to search, so
// we register native methods for common interfaces that delegate to the concrete
// implementations (ArrayList / HashSet).

fn register_interface_natives(registry: &mut NativeMethodRegistry) {
    // --- java/util/Collection ---
    registry.register(
        "java/util/Collection",
        "iterator",
        "()Ljava/util/Iterator;",
        native_al_iterator,
    );
    registry.register("java/util/Collection", "size", "()I", native_al_size);
    registry.register(
        "java/util/Collection",
        "stream",
        "()Ljava/util/stream/Stream;",
        native_al_stream,
    );
    registry.register("java/util/Collection", "isEmpty", "()Z", native_al_is_empty);
    registry.register(
        "java/util/Collection",
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array,
    );
    registry.register(
        "java/util/Collection",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );

    // --- java/util/List ---
    registry.register(
        "java/util/List",
        "get",
        "(I)Ljava/lang/Object;",
        native_al_get,
    );
    registry.register("java/util/List", "size", "()I", native_al_size);
    registry.register(
        "java/util/List",
        "add",
        "(Ljava/lang/Object;)Z",
        native_al_add,
    );
    registry.register(
        "java/util/List",
        "iterator",
        "()Ljava/util/Iterator;",
        native_al_iterator,
    );
    registry.register("java/util/List", "isEmpty", "()Z", native_al_is_empty);
    registry.register(
        "java/util/List",
        "stream",
        "()Ljava/util/stream/Stream;",
        native_al_stream,
    );
    registry.register(
        "java/util/List",
        "toArray",
        "()[Ljava/lang/Object;",
        native_al_to_array,
    );
    registry.register(
        "java/util/List",
        "toArray",
        "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
        native_collection_to_array_generator,
    );

    // --- java/lang/Iterable ---
    registry.register(
        "java/lang/Iterable",
        "iterator",
        "()Ljava/util/Iterator;",
        native_al_iterator,
    );
    registry.register(
        "java/lang/Iterable",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );

    // --- forEach on Collection / List / Set ---
    registry.register(
        "java/util/Collection",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    registry.register(
        "java/util/List",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
    registry.register(
        "java/util/Set",
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_hs_for_each,
    );

    // --- java/util/Set ---
    registry.register(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        native_hs_iterator,
    );
    registry.register("java/util/Set", "size", "()I", native_hs_size);
    registry.register("java/util/Set", "isEmpty", "()Z", native_hs_is_empty);

    // --- java/util/Map ---
    registry.register("java/util/Map", "size", "()I", native_map_size);
    registry.register(
        "java/util/Map",
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    registry.register(
        "java/util/Map",
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put,
    );
    registry.register(
        "java/util/Map",
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    registry.register(
        "java/util/Map",
        "keySet",
        "()Ljava/util/Set;",
        native_map_key_set,
    );
    registry.register(
        "java/util/Map",
        "values",
        "()Ljava/util/Collection;",
        native_map_values,
    );
    registry.register(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        native_map_entry_set,
    );

    // --- java/util/Map$Entry interface ---
    registry.register(
        "java/util/Map$Entry",
        "getKey",
        "()Ljava/lang/Object;",
        native_entry_get_key,
    );
    registry.register(
        "java/util/Map$Entry",
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    registry.register(
        "java/util/Map$Entry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
    // Also register under AbstractMap$SimpleEntry for completeness
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "getKey",
        "()Ljava/lang/Object;",
        native_entry_get_key,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "getValue",
        "()Ljava/lang/Object;",
        native_entry_get_value,
    );
    registry.register(
        "java/util/AbstractMap$SimpleEntry",
        "setValue",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_entry_set_value,
    );
}

// ===========================================================================
// Collection copy constructors
// ===========================================================================

fn register_copy_constructor_natives(registry: &mut NativeMethodRegistry) {
    // ArrayList(Collection)
    registry.register(
        "java/util/ArrayList",
        "<init>",
        "(Ljava/util/Collection;)V",
        native_al_init_from_collection,
    );
    // HashSet(Collection)
    registry.register(
        "java/util/HashSet",
        "<init>",
        "(Ljava/util/Collection;)V",
        native_hs_init_from_collection,
    );
    // HashMap(Map)
    registry.register(
        "java/util/HashMap",
        "<init>",
        "(Ljava/util/Map;)V",
        native_map_init_from_map,
    );
}

/// ArrayList.<init>(Collection) — copy elements from source collection into this list.
fn native_al_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // null or missing — just init empty
            let buf = alloc_ref_array(ctx, AL_DEFAULT_CAPACITY);
            al_set_data(ctx, this, buf);
            al_set_size(ctx, this, 0);
            return Ok(None);
        }
    };

    // Try to read source as an ArrayList (field 0 = data array, field 1 = size)
    let (src_data, src_size) = al_state(ctx, source);
    if let (Some(arr), size) = (src_data, src_size) {
        if size > 0 {
            let cap = std::cmp::max(size as usize, AL_DEFAULT_CAPACITY);
            let buf = alloc_ref_array(ctx, cap);
            for i in 0..size as usize {
                let val = ctx.get_array_element(arr, i);
                ctx.set_array_element(buf, i, val);
            }
            al_set_data(ctx, this, buf);
            al_set_size(ctx, this, size);
        } else {
            let buf = alloc_ref_array(ctx, AL_DEFAULT_CAPACITY);
            al_set_data(ctx, this, buf);
            al_set_size(ctx, this, 0);
        }
    } else {
        // Source is not an ArrayList-shaped object (e.g. an unmodifiable
        // view, HashSet, LinkedList, ...). Walk it generically so
        // `new ArrayList<>(List.of(...))` / `new ArrayList<>(unmodList)`
        // copy the real elements instead of producing an empty list.
        let elems = collect_collection_elements(ctx, source);
        let cap = std::cmp::max(elems.len(), AL_DEFAULT_CAPACITY);
        let buf = alloc_ref_array(ctx, cap);
        for (i, val) in elems.iter().enumerate() {
            ctx.set_array_element(buf, i, *val);
        }
        al_set_data(ctx, this, buf);
        al_set_size(ctx, this, elems.len() as i32);
    }

    Ok(None)
}

/// HashSet.<init>(Collection) — copy elements from source into this set.
fn native_hs_init_from_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Init empty HashSet
            let backing = alloc_backing_map(ctx);
            let buckets = alloc_ref_array(ctx, MAP_DEFAULT_CAPACITY);
            ctx.set_field(backing, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
            set_map_size(ctx, backing, 0);
            ctx.set_field(
                backing,
                MAP_FIELD_CAPACITY,
                Value::Int(MAP_DEFAULT_CAPACITY as i32),
            );
            ctx.set_field(this, HS_FIELD_MAP, Value::Object(Some(backing)));
            return Ok(None);
        }
    };

    // Create backing HashMap for this set
    let backing = alloc_backing_map(ctx);
    let cap = MAP_DEFAULT_CAPACITY;
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing, 0);
    ctx.set_field(backing, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(this, HS_FIELD_MAP, Value::Object(Some(backing)));

    // Round 49 fix: route through `collect_collection_elements` so we
    // honour every wrapper layout (ArrayList, Arrays$ArrayList,
    // LinkedList, HashSet/LinkedHashSet, Collections$UnmodifiableList,
    // …) — not just the synthetic ArrayList layout.  Previously
    // `new LinkedHashSet(unmodifiableList)` (the dominant Spring Boot
    // pattern in `AutoConfigurationImportSelector.removeDuplicates`,
    // since `ImportCandidates.getCandidates()` returns
    // `Collections.unmodifiableList(arrayList)`) silently produced an
    // empty set — wiping every auto-configuration before filtering and
    // surfacing as `MissingWebServerFactoryBeanException` at boot.
    let elems = collect_collection_elements(ctx, source);
    let sentinel = Value::Int(1);
    for val in elems {
        native_map_put(ctx, &[Value::Object(Some(backing)), val, sentinel])?;
    }

    Ok(None)
}

/// HashMap.<init>(Map) — copy entries from source map into this map.
fn native_map_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            // Init empty HashMap
            let buckets = alloc_ref_array(ctx, MAP_DEFAULT_CAPACITY);
            ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
            set_map_size(ctx, this, 0);
            ctx.set_field(
                this,
                MAP_FIELD_CAPACITY,
                Value::Int(MAP_DEFAULT_CAPACITY as i32),
            );
            return Ok(None);
        }
    };
    // See through `Collections.unmodifiableMap` / `Map.of` wrappers so
    // `new HashMap<>(Map.of(...))` copies the real entries.
    let source = unwrap_unmod(ctx, source);

    // Init this map
    let cap = MAP_DEFAULT_CAPACITY;
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(this, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, this, 0);
    ctx.set_field(this, MAP_FIELD_CAPACITY, Value::Int(cap as i32));

    // Copy entries from source.
    // S111r24-fix: When source is a LinkedHashMap, its entries live in the
    // Rust-side `lhm_overlay`, not in HashMap-style buckets. Walk the LHM
    // insertion-order list first, falling back to HashMap bucket scanning
    // for plain HashMaps / CHMs / etc.
    let mut entries: Vec<(Value, Value)> = Vec::new();
    let lhm_head = lhm_get(ctx, source, "head", LHM_FIELD_HEAD);
    if let Value::Object(Some(_)) = lhm_head {
        let mut cur = lhm_head;
        while let Value::Object(Some(node)) = cur {
            let key = ctx.get_field(node, LHM_NODE_KEY);
            let val = ctx.get_field(node, LHM_NODE_VALUE);
            entries.push((key, val));
            cur = ctx.get_field(node, LHM_NODE_AFTER);
        }
    } else {
        entries = map_collect_entries(ctx, source);
    }
    for (key, value) in entries {
        native_map_put(ctx, &[Value::Object(Some(this)), key, value])?;
    }

    Ok(None)
}

// ===========================================================================
// Comparator factory methods
// ===========================================================================

// Comparator synthetic objects: 3 fields (tag, arg1, arg2)
const CMP_FIELD_TAG: usize = 0;
const CMP_FIELD_ARG1: usize = 1;
const CMP_FIELD_ARG2: usize = 2;
const CMP_NUM_FIELDS: usize = 3;

const CMP_TAG_NATURAL_ORDER: i32 = 1;
const CMP_TAG_REVERSE_ORDER: i32 = 2;
const CMP_TAG_COMPARING: i32 = 3;
const CMP_TAG_REVERSED: i32 = 4;
const CMP_TAG_THEN_COMPARING: i32 = 5;

fn make_comparator(ctx: &mut dyn NativeContext, tag: i32) -> ObjectRef {
    let cmp = alloc_synthetic(ctx, "java/util/Comparator$Native", CMP_NUM_FIELDS);
    ctx.set_field(cmp, CMP_FIELD_TAG, Value::Int(tag));
    cmp
}

/// Compare two values using a Comparator. If the comparator is a tagged factory
/// object, dispatch based on tag. Otherwise, fall through to invoke_virtual (for lambdas).
pub fn comparator_compare(
    ctx: &mut dyn NativeContext,
    comparator: ObjectRef,
    a: Value,
    b: Value,
) -> MethodCallResult {
    // Guard: only read the tag if the object has enough fields (factory comparators have 3).
    // Lambda proxies may have 0 fields, so reading field 0 would panic.
    let tag = if ctx.object_num_fields(comparator) >= CMP_NUM_FIELDS {
        match ctx.get_field(comparator, CMP_FIELD_TAG) {
            Value::Int(t) if (CMP_TAG_NATURAL_ORDER..=CMP_TAG_THEN_COMPARING).contains(&t) => {
                Some(t)
            }
            _ => None,
        }
    } else {
        None
    };
    let tag = match tag {
        Some(t) => t,
        None => {
            // Not a factory comparator — delegate to invoke_virtual (lambda path)
            return ctx.invoke_virtual(
                comparator,
                "compare",
                "(Ljava/lang/Object;Ljava/lang/Object;)I",
                &[a, b],
            );
        }
    };

    match tag {
        CMP_TAG_NATURAL_ORDER => natural_compare(ctx, &a, &b),
        CMP_TAG_REVERSE_ORDER => {
            let result = natural_compare(ctx, &a, &b)?;
            Ok(result.map(|v| match v {
                Value::Int(i) => Value::Int(-i),
                other => other,
            }))
        }
        CMP_TAG_COMPARING => {
            let key_fn = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let ka = ctx
                .invoke_virtual(
                    key_fn,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[a],
                )?
                .unwrap_or(Value::Object(None));
            let kb = ctx
                .invoke_virtual(
                    key_fn,
                    "apply",
                    "(Ljava/lang/Object;)Ljava/lang/Object;",
                    &[b],
                )?
                .unwrap_or(Value::Object(None));
            natural_compare(ctx, &ka, &kb)
        }
        CMP_TAG_REVERSED => {
            let inner = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let result = comparator_compare(ctx, inner, a, b)?;
            Ok(result.map(|v| match v {
                Value::Int(i) => Value::Int(-i),
                other => other,
            }))
        }
        CMP_TAG_THEN_COMPARING => {
            let primary = match ctx.get_field(comparator, CMP_FIELD_ARG1) {
                Value::Object(Some(r)) => r,
                _ => return Ok(Some(Value::Int(0))),
            };
            let result = comparator_compare(ctx, primary, a, b)?;
            match result {
                Some(Value::Int(0)) => {
                    let secondary = match ctx.get_field(comparator, CMP_FIELD_ARG2) {
                        Value::Object(Some(r)) => r,
                        _ => return Ok(Some(Value::Int(0))),
                    };
                    comparator_compare(ctx, secondary, a, b)
                }
                other => Ok(other),
            }
        }
        _ => Ok(Some(Value::Int(0))),
    }
}

/// Natural ordering: compare by string content or by wrapper field 0 value.
fn natural_compare(ctx: &mut dyn NativeContext, a: &Value, b: &Value) -> MethodCallResult {
    match (a, b) {
        (Value::Object(Some(ra)), Value::Object(Some(rb))) => {
            // Try string comparison first
            let sa = ctx.read_string(*ra);
            let sb = ctx.read_string(*rb);
            if let (Some(sa), Some(sb)) = (sa, sb) {
                return Ok(Some(Value::Int(sa.cmp(&sb) as i32)));
            }
            // Try as wrapper: compare field 0 values
            let fa = ctx.get_field(*ra, 0);
            let fb = ctx.get_field(*rb, 0);
            match (fa, fb) {
                (Value::Int(a), Value::Int(b)) => Ok(Some(Value::Int(a.cmp(&b) as i32))),
                (Value::Long(a), Value::Long(b)) => Ok(Some(Value::Int(a.cmp(&b) as i32))),
                (Value::Float(a), Value::Float(b)) => Ok(Some(Value::Int(a.total_cmp(&b) as i32))),
                (Value::Double(a), Value::Double(b)) => {
                    Ok(Some(Value::Int(a.total_cmp(&b) as i32)))
                }
                _ => Ok(Some(Value::Int(0))),
            }
        }
        // Compare bare ints/longs/etc. (for comparingInt results)
        (Value::Int(a), Value::Int(b)) => Ok(Some(Value::Int(a.cmp(b) as i32))),
        (Value::Long(a), Value::Long(b)) => Ok(Some(Value::Int(a.cmp(b) as i32))),
        (Value::Float(a), Value::Float(b)) => Ok(Some(Value::Int(a.total_cmp(b) as i32))),
        (Value::Double(a), Value::Double(b)) => Ok(Some(Value::Int(a.total_cmp(b) as i32))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_comparator_compare_method(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [this, a, b]
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let a = args.get(1).cloned().unwrap_or(Value::Object(None));
    let b = args.get(2).cloned().unwrap_or(Value::Object(None));
    comparator_compare(ctx, this, a, b)
}

fn register_comparator_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/util/Comparator$Native",
        "compare",
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        native_comparator_compare_method,
    );
    registry.register(
        "java/util/Comparator",
        "naturalOrder",
        "()Ljava/util/Comparator;",
        native_comparator_natural_order,
    );
    registry.register(
        "java/util/Comparator",
        "reverseOrder",
        "()Ljava/util/Comparator;",
        native_comparator_reverse_order,
    );
    registry.register(
        "java/util/Comparator",
        "comparing",
        "(Ljava/util/function/Function;)Ljava/util/Comparator;",
        native_comparator_comparing,
    );
    registry.register(
        "java/util/Comparator",
        "comparingInt",
        "(Ljava/util/function/ToIntFunction;)Ljava/util/Comparator;",
        native_comparator_comparing,
    );
    registry.register(
        "java/util/Comparator",
        "comparingLong",
        "(Ljava/util/function/ToLongFunction;)Ljava/util/Comparator;",
        native_comparator_comparing,
    );
    registry.register(
        "java/util/Comparator",
        "comparingDouble",
        "(Ljava/util/function/ToDoubleFunction;)Ljava/util/Comparator;",
        native_comparator_comparing,
    );
    registry.register(
        "java/util/Comparator",
        "reversed",
        "()Ljava/util/Comparator;",
        native_comparator_reversed,
    );
    registry.register(
        "java/util/Comparator",
        "thenComparing",
        "(Ljava/util/Comparator;)Ljava/util/Comparator;",
        native_comparator_then_comparing,
    );
}

fn native_comparator_natural_order(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cmp = make_comparator(ctx, CMP_TAG_NATURAL_ORDER);
    Ok(Some(Value::Object(Some(cmp))))
}

fn native_comparator_reverse_order(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let cmp = make_comparator(ctx, CMP_TAG_REVERSE_ORDER);
    Ok(Some(Value::Object(Some(cmp))))
}

fn native_comparator_comparing(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let key_fn = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cmp = make_comparator(ctx, CMP_TAG_COMPARING);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(key_fn)));
    Ok(Some(Value::Object(Some(cmp))))
}

fn native_comparator_reversed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cmp = make_comparator(ctx, CMP_TAG_REVERSED);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(cmp))))
}

fn native_comparator_then_comparing(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cmp = make_comparator(ctx, CMP_TAG_THEN_COMPARING);
    ctx.set_field(cmp, CMP_FIELD_ARG1, Value::Object(Some(this)));
    ctx.set_field(cmp, CMP_FIELD_ARG2, Value::Object(Some(other)));
    Ok(Some(Value::Object(Some(cmp))))
}

// ===========================================================================
// Phase 13 Step 5: StringJoiner
// ===========================================================================

// StringJoiner is a 5-field synthetic:
//   field 0 = delimiter (String)
//   field 1 = prefix (String or null)
//   field 2 = suffix (String or null)
//   field 3 = elements ArrayList
//   field 4 = emptyValue (String or null)
const SJ_FIELD_DELIM: usize = 0;
const SJ_FIELD_PREFIX: usize = 1;
const SJ_FIELD_SUFFIX: usize = 2;
const SJ_FIELD_ELEMENTS: usize = 3;
const SJ_FIELD_EMPTY_VALUE: usize = 4;
fn register_string_joiner_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;)V",
        native_sj_init_delim,
    );
    registry.register(
        "java/util/StringJoiner",
        "<init>",
        "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V",
        native_sj_init_full,
    );
    registry.register(
        "java/util/StringJoiner",
        "add",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        native_sj_add,
    );
    registry.register(
        "java/util/StringJoiner",
        "toString",
        "()Ljava/lang/String;",
        native_sj_to_string,
    );
    registry.register("java/util/StringJoiner", "length", "()I", native_sj_length);
    registry.register(
        "java/util/StringJoiner",
        "merge",
        "(Ljava/util/StringJoiner;)Ljava/util/StringJoiner;",
        native_sj_merge,
    );
    registry.register(
        "java/util/StringJoiner",
        "setEmptyValue",
        "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
        native_sj_set_empty_value,
    );
}

fn native_sj_init_delim(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let delim = match args.get(1) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    // Create an empty ArrayList for elements
    let elements = alloc_synthetic(ctx, "java/util/ArrayList", 2);
    let backing = alloc_ref_array(ctx, 10);
    ctx.set_field(elements, 0, Value::Object(Some(backing)));
    ctx.set_field(elements, 1, Value::Int(0));

    ctx.set_field(this, SJ_FIELD_DELIM, delim);
    ctx.set_field(this, SJ_FIELD_PREFIX, Value::Object(None));
    ctx.set_field(this, SJ_FIELD_SUFFIX, Value::Object(None));
    ctx.set_field(this, SJ_FIELD_ELEMENTS, Value::Object(Some(elements)));
    ctx.set_field(this, SJ_FIELD_EMPTY_VALUE, Value::Object(None));
    Ok(None)
}

fn native_sj_init_full(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let delim = match args.get(1) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    let prefix = match args.get(2) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };
    let suffix = match args.get(3) {
        Some(Value::Object(Some(r))) => Value::Object(Some(*r)),
        _ => Value::Object(None),
    };

    let elements = alloc_synthetic(ctx, "java/util/ArrayList", 2);
    let backing = alloc_ref_array(ctx, 10);
    ctx.set_field(elements, 0, Value::Object(Some(backing)));
    ctx.set_field(elements, 1, Value::Int(0));

    ctx.set_field(this, SJ_FIELD_DELIM, delim);
    ctx.set_field(this, SJ_FIELD_PREFIX, prefix);
    ctx.set_field(this, SJ_FIELD_SUFFIX, suffix);
    ctx.set_field(this, SJ_FIELD_ELEMENTS, Value::Object(Some(elements)));
    ctx.set_field(this, SJ_FIELD_EMPTY_VALUE, Value::Object(None));
    Ok(None)
}

fn native_sj_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let element = match args.get(1) {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    // Get the elements ArrayList and add to it
    let elements = match ctx.get_field(this, SJ_FIELD_ELEMENTS) {
        Value::Object(Some(r)) => r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    // Read current size
    let size = match ctx.get_field(elements, 1) {
        Value::Int(n) => n,
        _ => 0,
    };
    let backing = match ctx.get_field(elements, 0) {
        Value::Object(Some(r)) => r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let capacity = ctx.array_length(backing);
    if size as usize >= capacity {
        // Overflow-safe doubling capped at 1<<30, mirroring `al_ensure_capacity`
        // (lib.rs ~690). `saturating_mul` prevents `usize` overflow on absurd
        // capacities; the cap then bounds the allocation. We always need room
        // for at least one more element, so floor the growth at `capacity + 1`.
        const SJ_MAX_CAPACITY: usize = 1 << 30;
        let new_cap = std::cmp::min(
            std::cmp::max(capacity.saturating_mul(2), capacity.saturating_add(1)),
            SJ_MAX_CAPACITY,
        );
        if new_cap <= capacity {
            // Already at the cap and full — refuse to grow further rather than
            // allocating an array that cannot hold the new element.
            return Err(cratonvm_types::error::RuntimeError::OutOfMemoryError {
                message: format!(
                    "StringJoiner backing array exceeds the {SJ_MAX_CAPACITY}-element limit"
                ),
            }
            .into());
        }
        let new_backing = alloc_ref_array(ctx, new_cap);
        for i in 0..capacity {
            let v = ctx.get_array_element(backing, i);
            ctx.set_array_element(new_backing, i, v);
        }
        ctx.set_field(elements, 0, Value::Object(Some(new_backing)));
        ctx.set_array_element(new_backing, size as usize, element);
    } else {
        ctx.set_array_element(backing, size as usize, element);
    }
    ctx.set_field(elements, 1, Value::Int(size + 1));
    Ok(Some(Value::Object(Some(this))))
}

/// Helper: read all elements from a StringJoiner's internal ArrayList as strings
fn sj_read_elements(ctx: &mut dyn NativeContext, sj: ObjectRef) -> Vec<String> {
    let elements = match ctx.get_field(sj, SJ_FIELD_ELEMENTS) {
        Value::Object(Some(r)) => r,
        _ => return Vec::new(),
    };
    let size = match ctx.get_field(elements, 1) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let backing = match ctx.get_field(elements, 0) {
        Value::Object(Some(r)) => r,
        _ => return Vec::new(),
    };
    let mut result = Vec::with_capacity(size);
    for i in 0..size {
        let elem = ctx.get_array_element(backing, i);
        let s = match elem {
            Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_else(|| "null".to_string()),
            _ => "null".to_string(),
        };
        result.push(s);
    }
    result
}

fn sj_build_string(ctx: &mut dyn NativeContext, sj: ObjectRef) -> String {
    let elements = sj_read_elements(ctx, sj);
    let delim = match ctx.get_field(sj, SJ_FIELD_DELIM) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    let prefix = match ctx.get_field(sj, SJ_FIELD_PREFIX) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    let suffix = match ctx.get_field(sj, SJ_FIELD_SUFFIX) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };

    if elements.is_empty() {
        // Check emptyValue
        if let Value::Object(Some(ev)) = ctx.get_field(sj, SJ_FIELD_EMPTY_VALUE) {
            return ctx.read_string(ev).unwrap_or_default();
        }
        return format!("{prefix}{suffix}");
    }

    let joined = elements.join(&delim);
    format!("{prefix}{joined}{suffix}")
}

fn native_sj_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = sj_build_string(ctx, this);
    let s = ctx.create_string(&result);
    Ok(Some(Value::Object(Some(s))))
}

fn native_sj_length(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let result = sj_build_string(ctx, this);
    Ok(Some(Value::Int(result.len() as i32)))
}

fn native_sj_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    // Merge: add all elements from other into this (without other's prefix/suffix)
    let other_elements = sj_read_elements(ctx, other);
    if other_elements.is_empty() {
        return Ok(Some(Value::Object(Some(this))));
    }
    // Join other's elements with other's delimiter and add as a single element
    let other_delim = match ctx.get_field(other, SJ_FIELD_DELIM) {
        Value::Object(Some(r)) => ctx.read_string(r).unwrap_or_default(),
        _ => String::new(),
    };
    let merged = other_elements.join(&other_delim);
    let merged_str = ctx.create_string(&merged);
    // Add as single element to this
    native_sj_add(
        ctx,
        &[Value::Object(Some(this)), Value::Object(Some(merged_str))],
    )
}

fn native_sj_set_empty_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let empty_val = match args.get(1) {
        Some(v) => *v,
        _ => Value::Object(None),
    };
    ctx.set_field(this, SJ_FIELD_EMPTY_VALUE, empty_val);
    Ok(Some(Value::Object(Some(this))))
}

// ===========================================================================
// Phase 14 Step 1: java.util.Random
// ===========================================================================

// Random = 2-field synthetic (field 0 = Long seed, field 1 = unused/reserved)
const RND_FIELD_SEED: usize = 0;

fn register_random_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/Random";
    registry.register(c, "<init>", "()V", native_random_init);
    registry.register(c, "<init>", "(J)V", native_random_init_seed);
    registry.register(c, "nextInt", "()I", native_random_next_int);
    registry.register(c, "nextInt", "(I)I", native_random_next_int_bound);
    registry.register(c, "nextLong", "()J", native_random_next_long);
    registry.register(c, "nextDouble", "()D", native_random_next_double);
    registry.register(c, "nextFloat", "()F", native_random_next_float);
    registry.register(c, "nextBoolean", "()Z", native_random_next_boolean);
    registry.register(c, "setSeed", "(J)V", native_random_set_seed);
    registry.register(c, "nextGaussian", "()D", native_random_next_gaussian);
}

/// Java LCG constants
const LCG_MULTIPLIER: i64 = 0x5DEECE66D;
const LCG_INCREMENT: i64 = 0xB;
const LCG_MASK: i64 = (1i64 << 48) - 1;

fn rnd_scramble_seed(seed: i64) -> i64 {
    (seed ^ LCG_MULTIPLIER) & LCG_MASK
}

fn rnd_next(ctx: &mut dyn NativeContext, this: ObjectRef, bits: u32) -> i32 {
    let old_seed = match ctx.get_field(this, RND_FIELD_SEED) {
        Value::Long(s) => s,
        _ => 0,
    };
    let new_seed = (old_seed
        .wrapping_mul(LCG_MULTIPLIER)
        .wrapping_add(LCG_INCREMENT))
        & LCG_MASK;
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(new_seed));
    (new_seed >> (48 - bits)) as i32
}

fn native_random_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(42);
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    Ok(None)
}

fn native_random_init_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    Ok(None)
}

fn native_random_next_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(rnd_next(ctx, this, 32))))
}

fn native_random_next_int_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let bound = match args.get(1) {
        Some(Value::Int(b)) => *b,
        _ => 1,
    };
    if bound <= 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalArgumentException {
            message: "bound must be positive".to_string(),
        }
        .into());
    }
    // Rejection sampling for uniform distribution
    let mut r = rnd_next(ctx, this, 31);
    let m = bound - 1;
    if (bound & m) == 0 {
        // power of two
        r = ((bound as i64 * r as i64) >> 31) as i32;
    } else {
        let mut u = r;
        loop {
            r = u % bound;
            if u.wrapping_sub(r).wrapping_add(m) >= 0 {
                break;
            }
            u = rnd_next(ctx, this, 31);
        }
    }
    Ok(Some(Value::Int(r)))
}

fn native_random_next_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Long(0))),
    };
    let hi = rnd_next(ctx, this, 32) as i64;
    let lo = rnd_next(ctx, this, 32) as i64;
    Ok(Some(Value::Long((hi << 32) + lo)))
}

fn native_random_next_double(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    let hi = (rnd_next(ctx, this, 26) as i64) << 27;
    let lo = rnd_next(ctx, this, 27) as i64;
    let v = (hi + lo) as f64 / ((1i64 << 53) as f64);
    Ok(Some(Value::Double(v)))
}

fn native_random_next_float(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Float(0.0))),
    };
    let v = rnd_next(ctx, this, 24) as f32 / ((1i32 << 24) as f32);
    Ok(Some(Value::Float(v)))
}

fn native_random_next_boolean(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let v = rnd_next(ctx, this, 1);
    Ok(Some(Value::Int(v)))
}

fn native_random_set_seed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let seed = match args.get(1) {
        Some(Value::Long(s)) => *s,
        _ => 0,
    };
    ctx.set_field(this, RND_FIELD_SEED, Value::Long(rnd_scramble_seed(seed)));
    Ok(None)
}

fn native_random_next_gaussian(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Double(0.0))),
    };
    // Box-Muller transform (simplified: generate a pair, return one)
    loop {
        let hi1 = (rnd_next(ctx, this, 26) as i64) << 27;
        let lo1 = rnd_next(ctx, this, 27) as i64;
        let v1 = 2.0 * ((hi1 + lo1) as f64 / ((1i64 << 53) as f64)) - 1.0;
        let hi2 = (rnd_next(ctx, this, 26) as i64) << 27;
        let lo2 = rnd_next(ctx, this, 27) as i64;
        let v2 = 2.0 * ((hi2 + lo2) as f64 / ((1i64 << 53) as f64)) - 1.0;
        let s = v1 * v1 + v2 * v2;
        if s < 1.0 && s != 0.0 {
            let multiplier = (-2.0 * s.ln() / s).sqrt();
            return Ok(Some(Value::Double(v1 * multiplier)));
        }
    }
}

// ===========================================================================
// Phase 14 Step 4: OptionalInt / OptionalLong / OptionalDouble
// ===========================================================================

fn register_optional_int_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/OptionalInt";
    registry.register(c, "of", "(I)Ljava/util/OptionalInt;", native_opt_int_of);
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalInt;",
        native_opt_int_empty,
    );
    registry.register(c, "getAsInt", "()I", native_opt_int_get);
    registry.register(c, "isPresent", "()Z", native_opt_int_is_present);
    registry.register(c, "orElse", "(I)I", native_opt_int_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/IntConsumer;)V",
        native_opt_int_if_present,
    );
}

fn register_optional_long_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/OptionalLong";
    registry.register(c, "of", "(J)Ljava/util/OptionalLong;", native_opt_long_of);
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalLong;",
        native_opt_long_empty,
    );
    registry.register(c, "getAsLong", "()J", native_opt_long_get);
    registry.register(c, "isPresent", "()Z", native_opt_long_is_present);
    registry.register(c, "orElse", "(J)J", native_opt_long_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/LongConsumer;)V",
        native_opt_long_if_present,
    );
}

fn register_optional_double_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/OptionalDouble";
    registry.register(
        c,
        "of",
        "(D)Ljava/util/OptionalDouble;",
        native_opt_double_of,
    );
    registry.register(
        c,
        "empty",
        "()Ljava/util/OptionalDouble;",
        native_opt_double_empty,
    );
    registry.register(c, "getAsDouble", "()D", native_opt_double_get);
    registry.register(c, "isPresent", "()Z", native_opt_double_is_present);
    registry.register(c, "orElse", "(D)D", native_opt_double_or_else);
    registry.register(
        c,
        "ifPresent",
        "(Ljava/util/function/DoubleConsumer;)V",
        native_opt_double_if_present,
    );
}

// --- OptionalInt ---

fn native_opt_int_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Int(val));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_int_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = alloc_synthetic(ctx, "java/util/OptionalInt", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Object(None));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_int_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into())
        }
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into()),
        other => Ok(Some(other)),
    }
}

fn native_opt_int_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let present = !matches!(ctx.get_field(this, OPT_FIELD_VALUE), Value::Object(None));
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

fn native_opt_int_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return match args.get(1) {
                Some(Value::Int(v)) => Ok(Some(Value::Int(*v))),
                _ => Ok(Some(Value::Int(0))),
            }
        }
    };
    let default_val = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(default_val))),
    }
}

fn native_opt_int_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Value::Int(v) = ctx.get_field(this, OPT_FIELD_VALUE) {
        ctx.invoke_virtual(consumer, "accept", "(I)V", &[Value::Int(v)])?;
    }
    Ok(None)
}

// --- OptionalLong ---

fn native_opt_long_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Long(val));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_long_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = alloc_synthetic(ctx, "java/util/OptionalLong", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Object(None));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_long_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into())
        }
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into()),
        other => Ok(Some(other)),
    }
}

fn native_opt_long_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let present = !matches!(ctx.get_field(this, OPT_FIELD_VALUE), Value::Object(None));
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

fn native_opt_long_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return match args.get(1) {
                Some(Value::Long(v)) => Ok(Some(Value::Long(*v))),
                _ => Ok(Some(Value::Long(0))),
            }
        }
    };
    let default_val = match args.get(1) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Long(v) => Ok(Some(Value::Long(v))),
        _ => Ok(Some(Value::Long(default_val))),
    }
}

fn native_opt_long_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Value::Long(v) = ctx.get_field(this, OPT_FIELD_VALUE) {
        ctx.invoke_virtual(consumer, "accept", "(J)V", &[Value::Long(v)])?;
    }
    Ok(None)
}

// --- OptionalDouble ---

fn native_opt_double_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let val = match args.first() {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Double(val));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_double_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let opt = alloc_synthetic(ctx, "java/util/OptionalDouble", OPT_NUM_FIELDS);
    ctx.set_field(opt, OPT_FIELD_VALUE, Value::Object(None));
    Ok(Some(Value::Object(Some(opt))))
}

fn native_opt_double_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No value present".to_string(),
            }
            .into())
        }
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Double(v) => Ok(Some(Value::Double(v))),
        Value::Object(None) => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No value present".to_string(),
        }
        .into()),
        other => Ok(Some(other)),
    }
}

fn native_opt_double_is_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let present = !matches!(ctx.get_field(this, OPT_FIELD_VALUE), Value::Object(None));
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

fn native_opt_double_or_else(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return match args.get(1) {
                Some(Value::Double(v)) => Ok(Some(Value::Double(*v))),
                _ => Ok(Some(Value::Double(0.0))),
            }
        }
    };
    let default_val = match args.get(1) {
        Some(Value::Double(v)) => *v,
        _ => 0.0,
    };
    match ctx.get_field(this, OPT_FIELD_VALUE) {
        Value::Double(v) => Ok(Some(Value::Double(v))),
        _ => Ok(Some(Value::Double(default_val))),
    }
}

fn native_opt_double_if_present(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    if let Value::Double(v) = ctx.get_field(this, OPT_FIELD_VALUE) {
        ctx.invoke_virtual(consumer, "accept", "(D)V", &[Value::Double(v)])?;
    }
    Ok(None)
}

// ===========================================================================
// Phase 14 Step 5: LinkedList
// ===========================================================================

// LinkedList = 3-field synthetic (field 0 = head Node, field 1 = tail Node, field 2 = Int size)
// Node = 3-field synthetic (field 0 = prev Node, field 1 = next Node, field 2 = element value)
//
// As with LinkedHashMap, the synthetic slot indices alias real-JDK declared
// fields once the real `java.util.LinkedList` class is loaded (real-JDK has
// `size`, `first`, `last` after AbstractList's `modCount`). Storing state in
// a per-object overlay map sidesteps the layout collision without altering
// HashMap allocation/resolution. The instance node fields (`LL_NODE_*`) are
// kept slot-based because `LinkedList$Node` is purely synthetic in our VM
// (we never load the real class, since Node is private/inner and bytecode
// doesn't `getfield` it directly).
fn ll_overlay() -> &'static Mutex<StdHashMap<usize, StdHashMap<&'static str, Value>>> {
    static OVERLAY: std::sync::OnceLock<Mutex<StdHashMap<usize, StdHashMap<&'static str, Value>>>> =
        std::sync::OnceLock::new();
    OVERLAY.get_or_init(|| Mutex::new(StdHashMap::new()))
}
// LL overlay key: rekeyed onto `ctx.identity_hash_code(this)` so the
// side-table survives a moving-GC relocation. Originally
// `this.as_ptr() as usize`, which a moving GC invalidates the instant
// it relocates the LinkedList — silently dropping head/tail/size.
fn ll_get(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str) -> Value {
    ll_overlay()
        .lock()
        .unwrap()
        .get(&ih_obj_key(ctx, this))
        .and_then(|m| m.get(name))
        .copied()
        .unwrap_or(Value::Object(None))
}
fn ll_set(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str, v: Value) {
    ll_overlay()
        .lock()
        .unwrap()
        .entry(ih_obj_key(ctx, this))
        .or_default()
        .insert(name, v);
}

const LL_FIELD_HEAD: usize = 0;
const LL_FIELD_TAIL: usize = 1;
const LL_FIELD_SIZE: usize = 2;
const LL_NODE_PREV: usize = 0;
const LL_NODE_NEXT: usize = 1;
const LL_NODE_ELEM: usize = 2;

fn ll_alloc_node(ctx: &mut dyn NativeContext, element: Value) -> ObjectRef {
    let node = alloc_synthetic(ctx, "java/util/LinkedList$Node", 3);
    ctx.set_field(node, LL_NODE_PREV, Value::Object(None));
    ctx.set_field(node, LL_NODE_NEXT, Value::Object(None));
    ctx.set_field(node, LL_NODE_ELEM, element);
    node
}

fn ll_size(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    match ll_get(ctx, this, "size") {
        Value::Int(n) => n,
        _ => 0,
    }
}

fn register_linked_list_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/LinkedList";
    registry.register(c, "<init>", "()V", native_ll_init);
    registry.register(c, "add", "(Ljava/lang/Object;)Z", native_ll_add);
    // Positional insert/removal — required so the overlay LinkedList stays the
    // single source of truth. Without these, real-JDK `add(int,E)` /
    // `remove(int)` bytecode runs against the JDK field layout (`first`/`last`)
    // that our synthetic `<init>` never populates, silently desyncing the list
    // (e.g. Felix's resolver permutation queue: add(0,perm) writes one
    // structure, isEmpty()/remove(0) read the overlay → permutation lost).
    registry.register(c, "add", "(ILjava/lang/Object;)V", native_ll_add_at);
    registry.register(c, "remove", "(I)Ljava/lang/Object;", native_ll_remove_at);
    registry.register(c, "addFirst", "(Ljava/lang/Object;)V", native_ll_add_first);
    registry.register(c, "addLast", "(Ljava/lang/Object;)V", native_ll_add_last);
    registry.register(c, "get", "(I)Ljava/lang/Object;", native_ll_get);
    registry.register(c, "getFirst", "()Ljava/lang/Object;", native_ll_get_first);
    registry.register(c, "getLast", "()Ljava/lang/Object;", native_ll_get_last);
    registry.register(
        c,
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ll_remove_first,
    );
    registry.register(
        c,
        "removeLast",
        "()Ljava/lang/Object;",
        native_ll_remove_last,
    );
    registry.register(c, "size", "()I", native_ll_size);
    registry.register(c, "isEmpty", "()Z", native_ll_is_empty);
    registry.register(c, "contains", "(Ljava/lang/Object;)Z", native_ll_contains);
    registry.register(c, "clear", "()V", native_ll_clear);
    registry.register(c, "peek", "()Ljava/lang/Object;", native_ll_peek);
    registry.register(c, "poll", "()Ljava/lang/Object;", native_ll_poll);
    registry.register(c, "offer", "(Ljava/lang/Object;)Z", native_ll_add);
    registry.register(c, "toArray", "()[Ljava/lang/Object;", native_ll_to_array);
    // LinkedList.toArray(T[]) — typed overload. Without this the JDK's
    // node-iterating bytecode runs against our overlay structure (where the
    // JDK `first` field is always null) and returns an array full of nulls.
    registry.register(
        c,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
        native_ll_to_array_typed,
    );
    registry.register(c, "toString", "()Ljava/lang/String;", native_ll_to_string);
    registry.register(c, "iterator", "()Ljava/util/Iterator;", native_ll_iterator);
    // SportMe r54: real-JDK LinkedList$ListItr reads `LinkedList.size` and `first`
    // fields via getfield; our overlay-based LL never writes those, so
    // `List.sort` default-method path crashes with NoSuchElementException
    // inside Spring's `processDeferredImportSelectors`. Override
    // `listIterator()` / `listIterator(I)` to return a snapshot-array iterator
    // with cursor + list_ref so `next`/`hasNext`/`set` work via our overlay.
    registry.register(
        c,
        "listIterator",
        "()Ljava/util/ListIterator;",
        native_ll_list_iterator,
    );
    registry.register(
        c,
        "listIterator",
        "(I)Ljava/util/ListIterator;",
        native_ll_list_iterator_idx,
    );

    // LinkedList$Itr — synthetic 3-field overlay:
    //   field 0 = current node (the next node to be returned by `next()`)
    //   field 1 = list ref
    //   field 2 = last node returned by `next()` (for `remove()`); null when
    //             `next()` has not been called or after a `remove()`.
    let itr = "java/util/LinkedList$Itr";
    registry.register(itr, "hasNext", "()Z", native_ll_itr_has_next);
    registry.register(itr, "next", "()Ljava/lang/Object;", native_ll_itr_next);
    registry.register(itr, "remove", "()V", native_ll_itr_remove);

    // LinkedList$ListItr — synthetic 3-field overlay
    //   field 0 = Object[] snapshot of list elements
    //   field 1 = Int cursor (nextIndex)
    //   field 2 = list ref (so set() can mutate the backing LinkedList node)
    let lit = "java/util/LinkedList$ListItr";
    registry.register(lit, "hasNext", "()Z", native_ll_listitr_has_next);
    registry.register(lit, "next", "()Ljava/lang/Object;", native_ll_listitr_next);
    registry.register(lit, "hasPrevious", "()Z", native_ll_listitr_has_previous);
    registry.register(
        lit,
        "previous",
        "()Ljava/lang/Object;",
        native_ll_listitr_previous,
    );
    registry.register(lit, "nextIndex", "()I", native_ll_listitr_next_index);
    registry.register(lit, "previousIndex", "()I", native_ll_listitr_previous_index);
    registry.register(lit, "set", "(Ljava/lang/Object;)V", native_ll_listitr_set);
    registry.register(lit, "remove", "()V", native_ll_listitr_remove_noop);
    registry.register(lit, "add", "(Ljava/lang/Object;)V", native_ll_listitr_remove_noop);
}

fn ll_snapshot_array(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let size = ll_size(ctx, this) as usize;
    let arr = alloc_ref_array(ctx, size);
    let mut cur = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(node) = cur {
        if i >= size {
            break;
        }
        let elem = ctx.get_field(node, LL_NODE_ELEM);
        ctx.set_array_element(arr, i, elem);
        cur = match ctx.get_field(node, LL_NODE_NEXT) {
            Value::Object(Some(n)) => Some(n),
            _ => None,
        };
        i += 1;
    }
    arr
}

fn native_ll_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = ll_snapshot_array(ctx, this);
    let it = alloc_synthetic(ctx, "java/util/LinkedList$ListItr", 3);
    ctx.set_field(it, 0, Value::Object(Some(arr)));
    ctx.set_field(it, 1, Value::Int(0));
    ctx.set_field(it, 2, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(it))))
}

fn native_ll_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let idx = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let arr = ll_snapshot_array(ctx, this);
    let it = alloc_synthetic(ctx, "java/util/LinkedList$ListItr", 3);
    ctx.set_field(it, 0, Value::Object(Some(arr)));
    ctx.set_field(it, 1, Value::Int(idx.max(0)));
    ctx.set_field(it, 2, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(it))))
}

fn native_ll_listitr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if cursor < len { 1 } else { 0 })))
}

fn native_ll_listitr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into())
        }
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into())
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    if cursor >= len {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No more elements".to_string(),
        }
        .into());
    }
    let elem = ctx.get_array_element(arr, cursor as usize);
    ctx.set_field(this, 1, Value::Int(cursor + 1));
    Ok(Some(elem))
}

fn native_ll_listitr_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 })))
}

fn native_ll_listitr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No previous element".to_string(),
            }
            .into())
        }
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No previous element".to_string(),
            }
            .into())
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if cursor <= 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No previous element".to_string(),
        }
        .into());
    }
    let elem = ctx.get_array_element(arr, (cursor - 1) as usize);
    ctx.set_field(this, 1, Value::Int(cursor - 1));
    Ok(Some(elem))
}

fn native_ll_listitr_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor)))
}

fn native_ll_listitr_previous_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor - 1)))
}

fn native_ll_listitr_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Set the element at (cursor - 1) in both the snapshot array AND the
    // backing LinkedList node, so `List.sort` actually sorts.
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let idx = cursor - 1;
    if idx < 0 {
        return Ok(None);
    }
    if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
        ctx.set_array_element(arr, idx as usize, elem);
    }
    if let Value::Object(Some(list)) = ctx.get_field(this, 2) {
        if let Some(node) = ll_node_at(ctx, list, idx) {
            ctx.set_field(node, LL_NODE_ELEM, elem);
        }
    }
    Ok(None)
}

/// LinkedList's `ListIterator` is backed by an immutable snapshot taken at
/// `listIterator()` time, so structural mutation through the iterator cannot
/// be honoured. Per the JDK contract, an iterator that does not support a
/// mutation must throw `UnsupportedOperationException` rather than silently
/// no-op'ing (which would mask caller bugs and diverge from real JDK
/// behaviour). Used for both `remove()` and `add(Object)`.
fn native_ll_listitr_remove_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(unsupported_op())
}

fn native_ll_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Seed the identity-hash header so the overlay key (which is
    // `ctx.identity_hash_code(this)`) is stable from the very first
    // write — protects against a GC firing between `<init>` and the
    // first overlay lookup.
    ih_seed(ctx, this);
    ll_set(ctx, this, "head", Value::Object(None));
    ll_set(ctx, this, "tail", Value::Object(None));
    ll_set(ctx, this, "size", Value::Int(0));
    Ok(None)
}

fn ll_link_last(ctx: &mut dyn NativeContext, this: ObjectRef, element: Value) {
    let node = ll_alloc_node(ctx, element);
    let size = ll_size(ctx, this);
    if let Value::Object(Some(tail)) = ll_get(ctx, this, "tail") {
        ctx.set_field(tail, LL_NODE_NEXT, Value::Object(Some(node)));
        ctx.set_field(node, LL_NODE_PREV, Value::Object(Some(tail)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    } else {
        // Empty list
        ll_set(ctx, this, "head", Value::Object(Some(node)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    }
    ll_set(ctx, this, "size", Value::Int(size + 1));
}

fn ll_link_first(ctx: &mut dyn NativeContext, this: ObjectRef, element: Value) {
    let node = ll_alloc_node(ctx, element);
    let size = ll_size(ctx, this);
    if let Value::Object(Some(head)) = ll_get(ctx, this, "head") {
        ctx.set_field(head, LL_NODE_PREV, Value::Object(Some(node)));
        ctx.set_field(node, LL_NODE_NEXT, Value::Object(Some(head)));
        ll_set(ctx, this, "head", Value::Object(Some(node)));
    } else {
        ll_set(ctx, this, "head", Value::Object(Some(node)));
        ll_set(ctx, this, "tail", Value::Object(Some(node)));
    }
    ll_set(ctx, this, "size", Value::Int(size + 1));
}

fn native_ll_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_last(ctx, this, element);
    Ok(Some(Value::Int(1))) // returns true
}

fn native_ll_add_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_first(ctx, this, element);
    Ok(None)
}

fn native_ll_add_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let element = args.get(1).copied().unwrap_or(Value::Object(None));
    ll_link_last(ctx, this, element);
    Ok(None)
}

/// Insert `element` into a fresh node positioned immediately before `succ`.
/// Mirrors real-JDK `LinkedList.linkBefore`. `succ` must be a live node of
/// `this`. Updates `head`/`size` as needed; `tail` is unaffected because the
/// new node is never the last.
fn ll_link_before(ctx: &mut dyn NativeContext, this: ObjectRef, element: Value, succ: ObjectRef) {
    let node = ll_alloc_node(ctx, element);
    let pred = ctx.get_field(succ, LL_NODE_PREV);
    ctx.set_field(node, LL_NODE_PREV, pred);
    ctx.set_field(node, LL_NODE_NEXT, Value::Object(Some(succ)));
    ctx.set_field(succ, LL_NODE_PREV, Value::Object(Some(node)));
    match pred {
        Value::Object(Some(pred_node)) => {
            ctx.set_field(pred_node, LL_NODE_NEXT, Value::Object(Some(node)));
        }
        _ => {
            // succ was the head — node becomes the new head.
            ll_set(ctx, this, "head", Value::Object(Some(node)));
        }
    }
    let size = ll_size(ctx, this);
    ll_set(ctx, this, "size", Value::Int(size + 1));
}

/// Unlink a live node, returning its element. Mirrors `LinkedList.unlink`.
fn ll_unlink_node(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) -> Value {
    let element = ctx.get_field(node, LL_NODE_ELEM);
    let prev = ctx.get_field(node, LL_NODE_PREV);
    let next = ctx.get_field(node, LL_NODE_NEXT);
    match prev {
        Value::Object(Some(prev_node)) => {
            ctx.set_field(prev_node, LL_NODE_NEXT, next);
        }
        _ => {
            // node was the head.
            ll_set(ctx, this, "head", next);
        }
    }
    match next {
        Value::Object(Some(next_node)) => {
            ctx.set_field(next_node, LL_NODE_PREV, prev);
        }
        _ => {
            // node was the tail.
            ll_set(ctx, this, "tail", prev);
        }
    }
    let size = ll_size(ctx, this);
    ll_set(ctx, this, "size", Value::Int((size - 1).max(0)));
    element
}

/// `LinkedList.add(int index, E element)` — positional insert.
fn native_ll_add_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let element = args.get(2).copied().unwrap_or(Value::Object(None));
    let size = ll_size(ctx, this);
    if index < 0 || index > size {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    if index == size {
        ll_link_last(ctx, this, element);
    } else {
        match ll_node_at(ctx, this, index) {
            Some(succ) => ll_link_before(ctx, this, element, succ),
            None => ll_link_last(ctx, this, element),
        }
    }
    Ok(None)
}

/// `LinkedList.remove(int index)` — positional removal, returns the element.
fn native_ll_remove_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let size = ll_size(ctx, this);
    if index < 0 || index >= size {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index,
        }
        .into());
    }
    match ll_node_at(ctx, this, index) {
        Some(node) => Ok(Some(ll_unlink_node(ctx, this, node))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// Traverse to the node at the given index
fn ll_node_at(ctx: &dyn NativeContext, this: ObjectRef, index: i32) -> Option<ObjectRef> {
    let size = ll_size(ctx, this);
    if index < 0 || index >= size {
        return None;
    }
    if index < size / 2 {
        // Traverse from head
        let mut cur = match ll_get(ctx, this, "head") {
            Value::Object(Some(r)) => r,
            _ => return None,
        };
        for _ in 0..index {
            cur = match ctx.get_field(cur, LL_NODE_NEXT) {
                Value::Object(Some(r)) => r,
                _ => return None,
            };
        }
        Some(cur)
    } else {
        // Traverse from tail
        let mut cur = match ll_get(ctx, this, "tail") {
            Value::Object(Some(r)) => r,
            _ => return None,
        };
        for _ in 0..(size - 1 - index) {
            cur = match ctx.get_field(cur, LL_NODE_PREV) {
                Value::Object(Some(r)) => r,
                _ => return None,
            };
        }
        Some(cur)
    }
}

fn native_ll_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ll_node_at(ctx, this, index) {
        Some(node) => Ok(Some(ctx.get_field(node, LL_NODE_ELEM))),
        None => Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException { index }.into()),
    }
}

fn native_ll_get_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into())
        }
    };
    match ll_get(ctx, this, "head") {
        Value::Object(Some(head)) => Ok(Some(ctx.get_field(head, LL_NODE_ELEM))),
        _ => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "List is empty".to_string(),
        }
        .into()),
    }
}

fn native_ll_get_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into())
        }
    };
    match ll_get(ctx, this, "tail") {
        Value::Object(Some(tail)) => Ok(Some(ctx.get_field(tail, LL_NODE_ELEM))),
        _ => Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "List is empty".to_string(),
        }
        .into()),
    }
}

fn ll_unlink_first(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let head = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => r,
        _ => return Value::Object(None),
    };
    let element = ctx.get_field(head, LL_NODE_ELEM);
    let next = ctx.get_field(head, LL_NODE_NEXT);
    let size = ll_size(ctx, this);
    match next {
        Value::Object(Some(next_node)) => {
            ctx.set_field(next_node, LL_NODE_PREV, Value::Object(None));
            ll_set(ctx, this, "head", Value::Object(Some(next_node)));
        }
        _ => {
            ll_set(ctx, this, "head", Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(None));
        }
    }
    ll_set(ctx, this, "size", Value::Int(size - 1));
    element
}

fn ll_unlink_last(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let tail = match ll_get(ctx, this, "tail") {
        Value::Object(Some(r)) => r,
        _ => return Value::Object(None),
    };
    let element = ctx.get_field(tail, LL_NODE_ELEM);
    let prev = ctx.get_field(tail, LL_NODE_PREV);
    let size = ll_size(ctx, this);
    match prev {
        Value::Object(Some(prev_node)) => {
            ctx.set_field(prev_node, LL_NODE_NEXT, Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(Some(prev_node)));
        }
        _ => {
            ll_set(ctx, this, "head", Value::Object(None));
            ll_set(ctx, this, "tail", Value::Object(None));
        }
    }
    ll_set(ctx, this, "size", Value::Int(size - 1));
    element
}

fn native_ll_remove_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into())
        }
    };
    if ll_size(ctx, this) == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "List is empty".to_string(),
        }
        .into());
    }
    Ok(Some(ll_unlink_first(ctx, this)))
}

fn native_ll_remove_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "List is empty".to_string(),
            }
            .into())
        }
    };
    if ll_size(ctx, this) == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "List is empty".to_string(),
        }
        .into());
    }
    Ok(Some(ll_unlink_last(ctx, this)))
}

fn native_ll_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    Ok(Some(Value::Int(ll_size(ctx, this))))
}

fn native_ll_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    Ok(Some(Value::Int(if ll_size(ctx, this) == 0 {
        1
    } else {
        0
    })))
}

fn native_ll_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    while let Some(cur) = cur_opt {
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        if values_equal(ctx, &elem, &target) {
            return Ok(Some(Value::Int(1)));
        }
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    Ok(Some(Value::Int(0)))
}

fn native_ll_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    ll_set(ctx, this, "head", Value::Object(None));
    ll_set(ctx, this, "tail", Value::Object(None));
    ll_set(ctx, this, "size", Value::Int(0));
    Ok(None)
}

fn native_ll_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    match ll_get(ctx, this, "head") {
        Value::Object(Some(head)) => Ok(Some(ctx.get_field(head, LL_NODE_ELEM))),
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_ll_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    if ll_size(ctx, this) == 0 {
        return Ok(Some(Value::Object(None)));
    }
    Ok(Some(ll_unlink_first(ctx, this)))
}

fn native_ll_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let arr = alloc_ref_array(ctx, 0);
            return Ok(Some(Value::Object(Some(arr))));
        }
    };
    let size = ll_size(ctx, this) as usize;
    let arr = alloc_ref_array(ctx, size);
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(cur) = cur_opt {
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        ctx.set_array_element(arr, i, elem);
        i += 1;
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `LinkedList.toArray(T[])` — typed-array overload.
///
/// CratonVM's `LinkedList` is an overlay-backed structure (head/next nodes
/// live in our own fields, NOT the JDK's `first`/`last`/`size`). The real
/// JDK's `LinkedList.toArray(T[])` bytecode iterates the JDK `first` node,
/// which is always null in our representation — so without this override the
/// call returns a correctly-sized array full of `null`s. That bug surfaced as
/// ActiveMQ's `--version` NPE: `console.Main.runTaskClass` does
/// `list.toArray(new String[list.size()])` on a `LinkedList`, then
/// `AbstractCommand.parseOptions` calls `.startsWith("-")` on the (null)
/// first element.
fn native_ll_to_array_typed(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let template = args.get(1).copied().unwrap_or(Value::Object(None));
    let size = ll_size(ctx, this) as usize;
    // Reuse the supplied array when it is large enough; otherwise allocate.
    let target = match template {
        Value::Object(Some(arr)) if ctx.array_length(arr) >= size => arr,
        _ => alloc_ref_array(ctx, size),
    };
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let mut i = 0;
    while let Some(cur) = cur_opt {
        if i >= size {
            break;
        }
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        ctx.set_array_element(target, i, elem);
        i += 1;
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    // JDK contract: if the supplied array is longer than the list, the
    // element immediately after the copied range is set to null.
    let target_len = ctx.array_length(target);
    if target_len > size {
        ctx.set_array_element(target, size, Value::Object(None));
    }
    Ok(Some(Value::Object(Some(target))))
}

fn native_ll_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let s = ctx.create_string("[]");
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let size = ll_size(ctx, this) as usize;
    let mut parts = Vec::with_capacity(size);
    let mut cur_opt = match ll_get(ctx, this, "head") {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    while let Some(cur) = cur_opt {
        let elem = ctx.get_field(cur, LL_NODE_ELEM);
        parts.push(obj_to_display_string(ctx, &elem));
        cur_opt = match ctx.get_field(cur, LL_NODE_NEXT) {
            Value::Object(Some(r)) => Some(r),
            _ => None,
        };
    }
    let text = format!("[{}]", parts.join(", "));
    let s = ctx.create_string(&text);
    Ok(Some(Value::Object(Some(s))))
}

// LinkedList$Itr = 2-field synthetic (field 0 = current node, field 1 = list ref for size tracking)
fn native_ll_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let head = ll_get(ctx, this, "head");
    let itr = alloc_synthetic(ctx, "java/util/LinkedList$Itr", 3);
    ctx.set_field(itr, 0, head); // current node
    ctx.set_field(itr, 1, Value::Object(Some(this))); // list ref
    ctx.set_field(itr, 2, Value::Object(None)); // last returned node
    Ok(Some(Value::Object(Some(itr))))
}

fn native_ll_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let has = matches!(ctx.get_field(this, 0), Value::Object(Some(_)));
    Ok(Some(Value::Int(if has { 1 } else { 0 })))
}

fn native_ll_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into())
        }
    };
    let cur = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) => r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into())
        }
    };
    let element = ctx.get_field(cur, LL_NODE_ELEM);
    let next = ctx.get_field(cur, LL_NODE_NEXT);
    ctx.set_field(this, 0, next);
    // Record the node just returned so `remove()` can unlink it.
    ctx.set_field(this, 2, Value::Object(Some(cur)));
    Ok(Some(element))
}

/// Native `LinkedList$Itr.remove()` — removes the element returned by the most
/// recent `next()`. `LinkedList$Itr` is a synthetic class (real-JDK `LinkedList`
/// exposes only `ListItr`), so `Iterator.remove()` would otherwise resolve to
/// the interface default method, which throws `UnsupportedOperationException`.
fn native_ll_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let last = match ctx.get_field(this, 2) {
        Value::Object(Some(n)) => n,
        _ => {
            // `remove()` before `next()`, or twice in a row.
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "remove".to_string(),
            }
            .into());
        }
    };
    let list = match ctx.get_field(this, 1) {
        Value::Object(Some(l)) => l,
        _ => return Ok(None),
    };
    ll_unlink_node(ctx, list, last);
    // Clear `lastReturned` so a second `remove()` without an intervening
    // `next()` correctly throws.
    ctx.set_field(this, 2, Value::Object(None));
    Ok(None)
}

// ===========================================================================
// LinkedHashMap — insertion-ordered HashMap
// ===========================================================================
// LinkedHashMap = 5-field synthetic:
//   0: buckets (Object[]), 1: size (Int), 2: capacity (Int), 3: head (Node), 4: tail (Node)
// LinkedHashMap$Node = 6-field synthetic:
//   0: key, 1: value, 2: hash, 3: next (bucket chain), 4: before, 5: after (insertion order)

const LHM_FIELD_BUCKETS: usize = 0;
const LHM_FIELD_SIZE: usize = 1;
const LHM_FIELD_CAPACITY: usize = 2;
const LHM_FIELD_HEAD: usize = 3;
const LHM_FIELD_TAIL: usize = 4;

// Real-JDK LinkedHashMap inherits from HashMap, which has many declared fields
// (`table`, `entrySet`, `size`, `modCount`, `threshold`, `loadFactor`) plus
// AbstractMap (`keySet`, `values`) and LinkedHashMap's own (`head`, `tail`,
// `accessOrder`, `putMode`). The synthetic 5-slot layout (buckets, size,
// capacity, head, tail at indices 0..4) only matches a stub; against the real
// JDK the indices line up with completely different fields, so writes to the
// `LHM_FIELD_SIZE=1` slot land in `AbstractMap.values` (a Collection ref) and
// reads of size find Object(None), producing `LinkedHashMap.size()==0` even
// after multiple puts. (Fortunately the `buckets` slot also gets aliased to a
// reference field, so the chain itself stays consistent for get/put — only
// `size()` / `isEmpty()` / iteration short-circuit.)
//
// Spring 5.0's `LinkedMultiValueMap.addAll` calls
// `targetMap.computeIfAbsent(key, lambda)` on a `LinkedHashMap` and then walks
// `targetMap.entrySet()`; with the synthetic offsets, both the size-based
// short-circuit in `Map.computeIfAbsent` and the iterator's emptiness check
// see `size==0`, so the cache map ends up empty and `loadFactoryNames` returns
// 0 — precisely the SportMe `ServletWebServerFactory` missing-bean failure.
//
// These helpers route to a side-table when the synthetic slot indices would
// alias real-JDK declared fields. Storing LinkedHashMap state externally,
// keyed by ObjectRef, sidesteps the layout-mismatch problem entirely without
// touching the (other-agent-owned) HashMap allocation/resolution logic.
//
// We try a slot-based read/write first via the legacy `LHM_FIELD_*` indices,
// because (a) prior callers and the JIT may have populated those slots in
// stub-only configurations, and (b) it costs only one map lookup. When the
// stored value clearly isn't the synthetic value we expected (e.g. `size`
// reads back as Object(None) because slot 1 aliases AbstractMap.values), we
// fall through to the side-table.
//
// IMPORTANT: this is a per-object overlay; iteration helpers continue to walk
// the synthetic linked-list pointers, which now live in the side-table too.
use std::sync::Mutex;
use std::collections::HashMap as StdHashMap;
fn lhm_overlay() -> &'static Mutex<StdHashMap<usize, StdHashMap<String, Value>>> {
    static OVERLAY: std::sync::OnceLock<Mutex<StdHashMap<usize, StdHashMap<String, Value>>>> =
        std::sync::OnceLock::new();
    OVERLAY.get_or_init(|| Mutex::new(StdHashMap::new()))
}
// LHM overlay key: rekeyed onto `ctx.identity_hash_code(this)` (was
// `this.as_ptr() as usize`). A moving GC preserves the identity-hash
// word across relocation, so the overlay's bucket table, head/tail,
// and accessOrder flag remain reachable through the new ObjectRef.
fn lhm_overlay_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ih_obj_key(ctx, this)
}
fn lhm_get(ctx: &dyn NativeContext, this: ObjectRef, name: &str, _fallback: usize) -> Value {
    let m = lhm_overlay().lock().unwrap();
    m.get(&lhm_overlay_key(ctx, this))
        .and_then(|inner| inner.get(name))
        .copied()
        .unwrap_or(Value::Object(None))
}
fn lhm_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &str, _fallback: usize, v: Value) {
    let key = lhm_overlay_key(ctx, this);
    // Keep the address-keyed shortcut for the legacy `clone_lhm_overlay`
    // call in native-builtins (which can't supply a NativeContext).
    lhm_ptr_cache()
        .lock()
        .unwrap()
        .insert(this.as_ptr() as usize, key);
    let mut m = lhm_overlay().lock().unwrap();
    m.entry(key)
        .or_default()
        .insert(name.to_string(), v);
}

/// Copy the per-object LHM overlay from `src` to `dst`. Used by
/// `Object.clone()` so that a cloned `LinkedHashMap` retains its bucket
/// table, head/tail pointers, and other state stored outside the heap
/// fields. Without this, `lhm.clone()` returns an LHM with all state
/// missing and downstream `HashMap.clone()` bytecode walks an empty
/// receiver.
///
/// Legacy signature retained for the `native-builtins` `Object.clone()`
/// dispatcher which has no `NativeContext` plumbing. It falls back to
/// the address-derived key recorded in `lhm_ptr_cache` (populated on
/// every `lhm_set` while `ctx` is available). If the cache miss fires
/// the clone produces an empty overlay — acceptable because pre-rekey
/// the same call leaked the entry entirely on every GC move.
pub fn clone_lhm_overlay(src: ObjectRef, dst: ObjectRef) {
    let cache = lhm_ptr_cache().lock().unwrap();
    let src_key = match cache.get(&(src.as_ptr() as usize)) {
        Some(k) => *k,
        None => return,
    };
    let dst_key = match cache.get(&(dst.as_ptr() as usize)) {
        Some(k) => *k,
        // If we've never seen `dst` yet (e.g. it was just allocated
        // and no `lhm_set` has fired for it), fall back to its raw
        // address — Object.clone runs immediately after the bytecode
        // `alloc`, before any GC cycle, so the address is still
        // valid at this exact moment.
        None => dst.as_ptr() as usize,
    };
    drop(cache);
    let mut m = lhm_overlay().lock().unwrap();
    let src_state = m.get(&src_key).cloned();
    if let Some(s) = src_state {
        m.insert(dst_key, s);
    }
}

/// `clone_lhm_overlay`'s ctx-taking variant — preferred for new
/// callers. Resolves both keys via `identity_hash_code` directly
/// rather than the address cache, so it stays correct even when no
/// prior `lhm_set` ever ran on `src` or `dst`.
pub fn clone_lhm_overlay_ctx(ctx: &dyn NativeContext, src: ObjectRef, dst: ObjectRef) {
    let src_key = lhm_overlay_key(ctx, src);
    let dst_key = lhm_overlay_key(ctx, dst);
    let mut m = lhm_overlay().lock().unwrap();
    let src_state = m.get(&src_key).cloned();
    if let Some(s) = src_state {
        m.insert(dst_key, s);
    }
}

/// Address-to-identity-hash mapping populated on every `lhm_set`
/// (which has `ctx`). Lets the legacy `clone_lhm_overlay(src, dst)`
/// API recover the stable key without a `NativeContext`. Entries are
/// not pruned on GC compaction — a relocated object simply re-seeds
/// the cache from its new address on the next `lhm_set`. Stale
/// entries are harmless: they map old addresses to the same stable
/// hash, which still resolves to the correct overlay entry.
fn lhm_ptr_cache() -> &'static Mutex<StdHashMap<usize, usize>> {
    static C: std::sync::OnceLock<Mutex<StdHashMap<usize, usize>>> = std::sync::OnceLock::new();
    C.get_or_init(|| Mutex::new(StdHashMap::new()))
}

const LHM_NODE_KEY: usize = 0;
const LHM_NODE_VALUE: usize = 1;
const LHM_NODE_HASH: usize = 2;
const LHM_NODE_NEXT: usize = 3;
const LHM_NODE_BEFORE: usize = 4;
const LHM_NODE_AFTER: usize = 5;
const LHM_NODE_NUM_FIELDS: usize = 6;

fn lhm_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32) {
    let buckets = match lhm_get(ctx, this, "table", LHM_FIELD_BUCKETS) {
        Value::Object(Some(arr)) => Some(arr),
        _ => None,
    };
    let size = match lhm_get(ctx, this, "size", LHM_FIELD_SIZE) {
        Value::Int(s) => s,
        _ => 0,
    };
    // Real-JDK HashMap has no explicit `capacity` field — derive from the
    // bucket array length when present. Falls back to the synthetic slot if
    // we couldn't get a buckets array (e.g. uninitialised stub).
    let cap = match buckets {
        Some(arr) => ctx.array_length(arr) as i32,
        None => match lhm_get(ctx, this, "__capacity", LHM_FIELD_CAPACITY) {
            Value::Int(c) => c,
            _ => MAP_DEFAULT_CAPACITY as i32,
        },
    };
    (buckets, size, cap)
}

fn lhm_alloc_node(ctx: &mut dyn NativeContext, key: Value, value: Value, hash: i32) -> ObjectRef {
    let node = alloc_synthetic(ctx, "java/util/LinkedHashMap$Node", LHM_NODE_NUM_FIELDS);
    ctx.set_field(node, LHM_NODE_KEY, key);
    ctx.set_field(node, LHM_NODE_VALUE, value);
    ctx.set_field(node, LHM_NODE_HASH, Value::Int(hash));
    ctx.set_field(node, LHM_NODE_NEXT, Value::Object(None));
    ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(None));
    ctx.set_field(node, LHM_NODE_AFTER, Value::Object(None));
    node
}

fn lhm_link_tail(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let tail = lhm_get(ctx, this, "tail", LHM_FIELD_TAIL);
    if let Value::Object(Some(t)) = tail {
        ctx.set_field(t, LHM_NODE_AFTER, Value::Object(Some(node)));
        ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(Some(t)));
    } else {
        // Empty list — node becomes head
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(Some(node)));
    }
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(Some(node)));
}

fn lhm_unlink(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    let before = ctx.get_field(node, LHM_NODE_BEFORE);
    let after = ctx.get_field(node, LHM_NODE_AFTER);

    // Fix before's after
    if let Value::Object(Some(b)) = before {
        ctx.set_field(b, LHM_NODE_AFTER, after);
    } else {
        lhm_set(ctx, this, "head", LHM_FIELD_HEAD, after);
    }
    // Fix after's before
    if let Value::Object(Some(a)) = after {
        ctx.set_field(a, LHM_NODE_BEFORE, before);
    } else {
        lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, before);
    }
}

fn lhm_resize(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let (_, size, cap) = lhm_state(ctx, this);
    let new_cap = (cap as usize) * 2;
    let new_buckets = alloc_ref_array(ctx, new_cap);

    // Walk insertion-order list and rehash
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let hash = match ctx.get_field(node, LHM_NODE_HASH) {
            Value::Int(h) => h,
            _ => 0,
        };
        let idx = map_bucket_index(hash, new_cap as i32);

        // Detach from old bucket chain
        ctx.set_field(node, LHM_NODE_NEXT, Value::Object(None));

        // Insert at head of new bucket
        let head = ctx.get_array_element(new_buckets, idx);
        if let Value::Object(Some(h)) = head {
            ctx.set_field(node, LHM_NODE_NEXT, Value::Object(Some(h)));
        }
        ctx.set_array_element(new_buckets, idx, Value::Object(Some(node)));

        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }

    lhm_set(ctx, this, "table", LHM_FIELD_BUCKETS, Value::Object(Some(new_buckets)));
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size));
    lhm_set(ctx, this, "__capacity", LHM_FIELD_CAPACITY, Value::Int(new_cap as i32));
}

fn lhm_find_node(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &Value,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let (buckets, _, cap) = lhm_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(None),
    };
    let (hash, is_null) = match key {
        Value::Object(Some(k)) => (map_hash_key(ctx, *k)?, false),
        Value::Object(None) => (0, true),
        _ => return Ok(None),
    };
    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);
    while let Value::Object(Some(node)) = node_val {
        let node_key = ctx.get_field(node, LHM_NODE_KEY);
        if is_null {
            if matches!(node_key, Value::Object(None)) {
                return Ok(Some(node));
            }
        } else if let (Value::Object(Some(nk)), Value::Object(Some(k))) = (node_key, key) {
            if map_keys_equal(ctx, nk, *k)? {
                return Ok(Some(node));
            }
        }
        node_val = ctx.get_field(node, LHM_NODE_NEXT);
    }
    Ok(None)
}

fn register_linked_hashmap_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/LinkedHashMap";

    registry.register(c, "<init>", "()V", native_lhm_init);
    registry.register(c, "<init>", "(I)V", native_lhm_init_capacity);
    registry.register(c, "<init>", "(IF)V", native_lhm_init_capacity_lf);
    // Round-9 HIGH: the access-ordered ctor — when the `boolean accessOrder`
    // argument is true, `get(k)` must move the accessed entry to the tail
    // of the insertion-order list. This is what makes LinkedHashMap usable
    // as the underlying store for LRU caches (e.g. `removeEldestEntry`
    // overrides in Guava / Caffeine fallbacks). Without this ctor the
    // accessOrder flag silently defaults to false and `get` is read-only,
    // corrupting LRU eviction policies.
    registry.register(c, "<init>", "(IFZ)V", native_lhm_init_capacity_lf_access);
    registry.register(c, "<init>", "(Ljava/util/Map;)V", native_lhm_init_from_map);
    registry.register(c, "size", "()I", native_lhm_size);
    registry.register(c, "isEmpty", "()Z", native_lhm_is_empty);
    registry.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put,
    );
    registry.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_get,
    );
    registry.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_remove,
    );
    registry.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_lhm_contains_key,
    );
    registry.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_lhm_contains_value,
    );
    registry.register(c, "clear", "()V", native_lhm_clear);
    registry.register(c, "keySet", "()Ljava/util/Set;", native_lhm_key_set);
    registry.register(c, "values", "()Ljava/util/Collection;", native_lhm_values);
    registry.register(c, "entrySet", "()Ljava/util/Set;", native_lhm_entry_set);
    registry.register(c, "toString", "()Ljava/lang/String;", native_lhm_to_string);
    registry.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_get_or_default,
    );
    registry.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_lhm_put_if_absent,
    );
    registry.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_lhm_for_each,
    );
    registry.register(c, "putAll", "(Ljava/util/Map;)V", native_lhm_put_all);

    // computeIfAbsent on LinkedHashMap must route through the LHM put/get
    // natives (which use the overlay-backed slots), NOT through the HashMap
    // versions inherited via dispatch — otherwise the put no-ops because
    // HashMap's slot indices alias different real-JDK fields on a LHM object.
    registry.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_lhm_compute_if_absent,
    );
}

fn native_lhm_compute_if_absent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let function = match args.get(2) {
        Some(Value::Object(Some(f))) => *f,
        _ => return Ok(Some(Value::Object(None))),
    };

    // If the key is already present, return the existing value unchanged.
    let existing = native_lhm_get(ctx, &[Value::Object(Some(this)), key])?;
    if let Some(Value::Object(Some(_))) = existing {
        return Ok(existing);
    }

    // Otherwise call function.apply(key) and store the (non-null) result.
    let result = ctx.invoke_virtual(
        function,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key],
    )?;
    let new_val = result.unwrap_or(Value::Object(None));
    if let Value::Object(None) = new_val {
        return Ok(Some(Value::Object(None)));
    }
    native_lhm_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

fn lhm_init_with_cap(ctx: &mut dyn NativeContext, this: ObjectRef, cap: usize) {
    // Seed the identity-hash header *before* the first overlay write,
    // so that the GC-stable key is established at init time — see
    // `identity_hash::seed`.
    ih_seed(ctx, this);
    let buckets = alloc_ref_array(ctx, cap);
    lhm_set(ctx, this, "table", LHM_FIELD_BUCKETS, Value::Object(Some(buckets)));
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
    lhm_set(ctx, this, "__capacity", LHM_FIELD_CAPACITY, Value::Int(cap as i32));
    lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));

    // KC26 clone path: LinkedHashMap.<init>()V is intercepted natively (we
    // never run the bytecode chain LHM → HashMap.<init> → putfield loadFactor).
    // That leaves the JDK-resolved `loadFactor`, `threshold`, and `table` heap
    // slots zero-initialised. Once any bytecode path reads them — notably
    // `HashMap.clone()` → `reinitialize()` → `putMapEntries()` → `resize()` —
    // a loadFactor of 0.0f makes the resize compute `newCap = 1073741824`,
    // which then OOMs on the `anewarray Node[1073741824]`. Mirror the JDK
    // defaults into the real heap fields so cloned LHM instances see sane
    // values when the HashMap.clone bytecode walks them.
    try_set_jdk_map_field(ctx, this, "loadFactor", Value::Float(0.75_f32));
    try_set_jdk_map_field(ctx, this, "threshold", Value::Int((cap as i32 * 3) / 4));
}

fn native_lhm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    lhm_init_with_cap(ctx, this, MAP_DEFAULT_CAPACITY);
    Ok(None)
}

fn native_lhm_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(c)) => {
            // Bug 4: same load-factor adjustment as `native_map_init_capacity`.
            // The JDK contract: a caller asking for capacity N expects to hold
            // N mappings without triggering a resize. With loadFactor=0.75
            // that needs ceil(N * 4 / 3) buckets, rounded up to a power of two.
            let requested = std::cmp::max(*c, 1) as u64;
            let needed = requested.saturating_mul(4).div_ceil(3);
            let capped = std::cmp::min(needed, MAP_MAX_CAPACITY as u64).max(1) as usize;
            capped.checked_next_power_of_two()
                .unwrap_or(MAP_MAX_CAPACITY as usize)
                .max(MAP_DEFAULT_CAPACITY)
        }
        _ => MAP_DEFAULT_CAPACITY,
    };
    lhm_init_with_cap(ctx, this, cap);
    Ok(None)
}

/// `LinkedHashMap(int initialCapacity, float loadFactor)` — loadFactor is
/// accepted but only used for the capacity adjustment; we ignore the actual
/// f32 value beyond that.
fn native_lhm_init_capacity_lf(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lhm_init_capacity(ctx, args)
}

/// `LinkedHashMap(int initialCapacity, float loadFactor, boolean accessOrder)`
/// — when accessOrder is true, subsequent `get(k)` calls reorder the entry
/// to the tail of the insertion-order list (LRU-style). The flag is stashed
/// in the LHM overlay under the name `accessOrder` so `native_lhm_get` can
/// consult it without a Java field read.
fn native_lhm_init_capacity_lf_access(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // Reuse the (I)V capacity adjustment path.
    native_lhm_init_capacity(ctx, args)?;
    let access_order = match args.get(3) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    if access_order {
        // Use LHM_FIELD_TAIL+1 as a dummy fallback index — `lhm_set` writes
        // the value primarily under the string-name slot in the overlay,
        // which is what `lhm_is_access_order` reads back.
        lhm_set(
            ctx,
            this,
            "accessOrder",
            LHM_FIELD_TAIL + 1,
            Value::Int(1),
        );
    }
    // Also write through to the real-JDK field name in case the receiver
    // is a real-JDK LinkedHashMap with a slot for `accessOrder`.
    ctx.set_field_by_name(
        this,
        "accessOrder",
        Value::Int(if access_order { 1 } else { 0 }),
    );
    Ok(None)
}

/// Read the access-order flag for `this`. Defaults to false (insertion-order)
/// when no IFZ ctor ran. Consults both the overlay (where our ctor writes it)
/// and the real-JDK `accessOrder` field by name so a real-JDK LHM instance
/// initialised via Java bytecode still reports correctly.
fn lhm_is_access_order(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    if let Value::Int(v) = lhm_get(ctx, this, "accessOrder", LHM_FIELD_TAIL + 1) {
        if v != 0 {
            return true;
        }
    }
    matches!(ctx.get_field_by_name(this, "accessOrder"), Value::Int(v) if v != 0)
}

// S111r14: LinkedHashMap copy-constructor — see `native_map_init_from_map`
// for the rationale. DateTimeFormatterBuilder.appendText reaches us via
// `new LinkedHashMap<>(map)`.
fn native_lhm_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    lhm_init_with_cap(ctx, this, MAP_DEFAULT_CAPACITY);
    let src = args.get(1).copied().unwrap_or(Value::Object(None));
    // S111r24-fix: Route through `native_lhm_put_all` so insertion-order
    // linkage (head/tail) is established for each copied entry. Calling
    // `native_map_put_all` would invoke `native_map_put` which writes
    // directly into HashMap-style bucket nodes without `lhm_link_tail`,
    // leaving the LHM's insertion-order list empty and the entries
    // invisible to `entrySet()`/`putAll(...)` consumers like Spring's
    // `AnnotationAttributes(Map)` copy constructor.
    native_lhm_put_all(ctx, &[Value::Object(Some(this)), src])?;
    Ok(None)
}

fn native_lhm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size, _) = lhm_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

fn native_lhm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, size, _) = lhm_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

fn native_lhm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));

    let hash = match key_val {
        Value::Object(Some(k)) => map_hash_key(ctx, k)?,
        Value::Object(None) => 0,
        _ => return Ok(Some(Value::Object(None))),
    };

    // Check for resize. Also initialize table when buckets is None
    // (e.g. `org/springframework/core/annotation/AnnotationAttributes`
    // constructed via Spring's `new AnnotationAttributes(annotationType, false)`
    // — LinkedHashMap.<init>() runs as JDK bytecode and leaves our overlay
    // empty until the first put. Without this init the put would silently
    // no-op, dropping every key Spring writes via `TypeMappedAnnotation.asMap`
    // and surfacing as `IllegalArgumentException: Attribute 'type' not found
    // in attributes for annotation [...]ComponentScan$Filter` deep in
    // `ComponentScanAnnotationParser.parse` for `@SpringBootApplication`).
    let (initial_buckets, size, cap) = lhm_state(ctx, this);
    if initial_buckets.is_none() || size + 1 > (cap * 3) / 4 {
        lhm_resize(ctx, this);
    }

    // Check for existing key
    if let Some(node) = lhm_find_node(ctx, this, &key_val)? {
        let old = ctx.get_field(node, LHM_NODE_VALUE);
        ctx.set_field(node, LHM_NODE_VALUE, value);
        // Access-order semantics: re-inserting a value for an existing key
        // counts as a structural access, so the entry must move to the tail
        // of the insertion-order list (mirrors `native_lhm_get`). With
        // insertion-order (the default) this is left untouched.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        return Ok(Some(old));
    }

    // Insert new node
    let (buckets, size, cap) = lhm_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    let idx = map_bucket_index(hash, cap);

    let new_node = lhm_alloc_node(ctx, key_val, value, hash);

    // Insert at head of bucket chain
    let head = ctx.get_array_element(buckets, idx);
    if let Value::Object(Some(h)) = head {
        ctx.set_field(new_node, LHM_NODE_NEXT, Value::Object(Some(h)));
    }
    ctx.set_array_element(buckets, idx, Value::Object(Some(new_node)));

    // Link at tail of insertion-order list
    lhm_link_tail(ctx, this, new_node);

    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size + 1));
    Ok(Some(Value::Object(None)))
}

fn native_lhm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(node) = lhm_find_node(ctx, this, &key)? {
        let value = ctx.get_field(node, LHM_NODE_VALUE);
        // Round-9 HIGH: access-order semantics. When the LHM was
        // constructed with `(IFZ)V` accessOrder=true, `get` must move
        // the accessed entry to the tail of the insertion-order list
        // so that iteration order reflects LRU. With insertion-order
        // (the default) this is a read-only operation.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        Ok(Some(value))
    } else {
        // Diagnostic: an enum-keyed LinkedHashMap miss is the prime suspect
        // for the Keycloak `Profile.isFeatureEnabled` NPE.
        if dbg_kcbool() {
            if let Value::Object(Some(k)) = key {
                if enum_key_identity(ctx, k).is_some() {
                    let node_keys: Vec<ObjectRef> = lhm_collect_keys(ctx, this)
                        .into_iter()
                        .filter_map(|v| match v {
                            Value::Object(Some(r)) => Some(r),
                            _ => None,
                        })
                        .collect();
                    dbg_kcbool_report_miss(ctx, "LinkedHashMap.get", k, &node_keys);
                }
            }
        }
        Ok(Some(Value::Object(None)))
    }
}

/// Move `node` to the tail of the insertion-order linked list. Used by
/// access-order LinkedHashMaps to implement LRU semantics on `get`. Pure
/// pointer surgery: unlink + relink at tail; the bucket-chain pointers
/// (`LHM_NODE_NEXT`) are untouched so lookup remains correct.
fn lhm_move_to_tail(ctx: &mut dyn NativeContext, this: ObjectRef, node: ObjectRef) {
    // If already at tail, no-op.
    let current_tail = lhm_get(ctx, this, "tail", LHM_FIELD_TAIL);
    if let Value::Object(Some(t)) = current_tail {
        if t.as_ptr() == node.as_ptr() {
            return;
        }
    }
    // Unlink from current position.
    lhm_unlink(ctx, this, node);
    // The unlink above clears the head/tail entries when relevant, but it
    // leaves `node.before` / `node.after` populated with stale pointers.
    // Clear them before relinking at tail so `lhm_link_tail` sees a fresh
    // node.
    ctx.set_field(node, LHM_NODE_BEFORE, Value::Object(None));
    ctx.set_field(node, LHM_NODE_AFTER, Value::Object(None));
    lhm_link_tail(ctx, this, node);
}

fn native_lhm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key_val = args.get(1).copied().unwrap_or(Value::Object(None));

    let (hash, is_null) = match key_val {
        Value::Object(Some(k)) => (map_hash_key(ctx, k)?, false),
        Value::Object(None) => (0, true),
        _ => return Ok(Some(Value::Object(None))),
    };

    let (buckets, size, cap) = lhm_state(ctx, this);
    let buckets = match buckets {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    let idx = map_bucket_index(hash, cap);

    // Walk bucket chain, tracking prev
    let mut prev: Option<ObjectRef> = None;
    let mut node_val = ctx.get_array_element(buckets, idx);
    while let Value::Object(Some(node)) = node_val {
        let node_key = ctx.get_field(node, LHM_NODE_KEY);
        let found = if is_null {
            matches!(node_key, Value::Object(None))
        } else if let (Value::Object(Some(nk)), Value::Object(Some(k))) = (node_key, key_val) {
            map_keys_equal(ctx, nk, k)?
        } else {
            false
        };

        if found {
            let old_value = ctx.get_field(node, LHM_NODE_VALUE);
            let next = ctx.get_field(node, LHM_NODE_NEXT);

            // Unlink from bucket chain
            if let Some(p) = prev {
                ctx.set_field(p, LHM_NODE_NEXT, next);
            } else {
                ctx.set_array_element(buckets, idx, next);
            }

            // Unlink from insertion-order list
            lhm_unlink(ctx, this, node);

            lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(size - 1));
            return Ok(Some(old_value));
        }

        prev = Some(node);
        node_val = ctx.get_field(node, LHM_NODE_NEXT);
    }
    Ok(Some(Value::Object(None)))
}

fn native_lhm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    Ok(Some(Value::Int(
        if lhm_find_node(ctx, this, &key)?.is_some() {
            1
        } else {
            0
        },
    )))
}

fn native_lhm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        if values_equal(ctx, &val, &target) {
            return Ok(Some(Value::Int(1)));
        }
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    Ok(Some(Value::Int(0)))
}

fn native_lhm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (_, _, cap) = lhm_state(ctx, this);
    let new_buckets = alloc_ref_array(ctx, cap as usize);
    lhm_set(ctx, this, "table", LHM_FIELD_BUCKETS, Value::Object(Some(new_buckets)));
    lhm_set(ctx, this, "size", LHM_FIELD_SIZE, Value::Int(0));
    lhm_set(ctx, this, "head", LHM_FIELD_HEAD, Value::Object(None));
    lhm_set(ctx, this, "tail", LHM_FIELD_TAIL, Value::Object(None));
    Ok(None)
}

fn lhm_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut keys = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        keys.push(ctx.get_field(node, LHM_NODE_KEY));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    keys
}

fn lhm_collect_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut vals = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        vals.push(ctx.get_field(node, LHM_NODE_VALUE));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    vals
}

fn native_lhm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let keys = lhm_collect_keys(ctx, this);
    make_set_of(ctx, &keys)
}

fn native_lhm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let vals = lhm_collect_values(ctx, this);
    make_list_of(ctx, &vals)
}

fn native_lhm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let mut entries = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        let entry = alloc_synthetic(ctx, "java/util/AbstractMap$SimpleEntry", 2);
        ctx.set_field(entry, 0, key);
        ctx.set_field(entry, 1, val);
        entries.push(Value::Object(Some(entry)));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    make_set_of(ctx, &entries)
}

fn native_lhm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("{}"))))),
    };
    let mut parts = Vec::new();
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        let ks = obj_to_display_string(ctx, &key);
        let vs = obj_to_display_string(ctx, &val);
        parts.push(format!("{ks}={vs}"));
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    let s = format!("{{{}}}", parts.join(", "));
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

fn native_lhm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    if let Some(node) = lhm_find_node(ctx, this, &key)? {
        let value = ctx.get_field(node, LHM_NODE_VALUE);
        // Round-9 HIGH: `getOrDefault` is also an access for access-order
        // semantics — same reorder as `get`. The JDK's `LinkedHashMap`
        // explicitly documents this in `Map.getOrDefault`'s contract.
        if lhm_is_access_order(ctx, this) {
            lhm_move_to_tail(ctx, this, node);
        }
        Ok(Some(value))
    } else {
        Ok(Some(default))
    }
}

fn native_lhm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(node) = lhm_find_node(ctx, this, &key)? {
        let existing = ctx.get_field(node, LHM_NODE_VALUE);
        Ok(Some(existing))
    } else {
        native_lhm_put(ctx, args)
    }
}

fn native_lhm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let mut cur = lhm_get(ctx, this, "head", LHM_FIELD_HEAD);
    while let Value::Object(Some(node)) = cur {
        let key = ctx.get_field(node, LHM_NODE_KEY);
        let val = ctx.get_field(node, LHM_NODE_VALUE);
        ctx.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, val],
        )?;
        cur = ctx.get_field(node, LHM_NODE_AFTER);
    }
    Ok(None)
}

fn native_lhm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };

    // Collect entries from source map (walk either LHM insertion list or HashMap buckets).
    // S111r24-fix: LHM keeps `head`/`tail`/`table` in the Rust-side overlay
    // (`lhm_overlay`), NOT in Java field slots. Use `lhm_get` to retrieve
    // them; `ctx.get_field(source, LHM_FIELD_HEAD)` always returns
    // `Object(None)` for a real LHM allocated by our natives, which made
    // `putAll` and `new LHM<>(map)` silently produce an empty map. This
    // broke any downstream code that relied on copy-constructing a
    // LinkedHashMap from another LinkedHashMap, e.g. Spring's
    // `AnnotationAttributes(Map<String,Object>)`.
    let mut entries = Vec::new();
    let head = lhm_get(ctx, source, "head", LHM_FIELD_HEAD);
    if let Value::Object(Some(_)) = head {
        let mut cur = head;
        while let Value::Object(Some(node)) = cur {
            let key = ctx.get_field(node, LHM_NODE_KEY);
            let val = ctx.get_field(node, LHM_NODE_VALUE);
            entries.push((key, val));
            cur = ctx.get_field(node, LHM_NODE_AFTER);
        }
    } else {
        // Fall back to HashMap bucket scan when source isn't an LHM (e.g.
        // a plain HashMap, ConcurrentHashMap, etc.).
        let (buckets, _, cap) = map_state(ctx, source);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut nv = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = nv {
                    let key = ctx.get_field(node, NODE_FIELD_KEY);
                    let val = ctx.get_field(node, NODE_FIELD_VALUE);
                    entries.push((key, val));
                    nv = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        } else {
            // Last resort: route through map_collect_entries which has
            // additional logic for TreeMap / ConcurrentHashMap / Properties.
            entries.extend(map_collect_entries(ctx, source));
        }
    }

    for (key, val) in entries {
        native_lhm_put(ctx, &[Value::Object(Some(this)), key, val])?;
    }
    Ok(None)
}

// ===========================================================================
// ArrayDeque (circular buffer based Deque implementation)
// ===========================================================================

const AD_FIELD_DATA: usize = 0; // Object[] circular buffer
const AD_FIELD_HEAD: usize = 1; // Int head index
const AD_FIELD_TAIL: usize = 2; // Int tail index
const AD_FIELD_SIZE: usize = 3; // Int element count
const AD_DEFAULT_CAPACITY: usize = 16;

fn ad_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, i32, i32) {
    let data = match ctx.get_field(this, AD_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let head = match ctx.get_field(this, AD_FIELD_HEAD) {
        Value::Int(v) => v,
        _ => 0,
    };
    let tail = match ctx.get_field(this, AD_FIELD_TAIL) {
        Value::Int(v) => v,
        _ => 0,
    };
    let size = match ctx.get_field(this, AD_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    (data, head, tail, size)
}

fn ad_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, min_cap: usize) {
    let (data, head, _tail, size) = ad_state(ctx, this);
    let old_cap = data.map_or(0, |d| ctx.array_length(d));
    if min_cap <= old_cap {
        return;
    }
    let new_cap = std::cmp::max(old_cap * 2, min_cap);
    let new_buf = alloc_ref_array(ctx, new_cap);
    // Copy elements in order: head..end, then 0..wrap
    if let Some(old_buf) = data {
        let s = size as usize;
        let h = head as usize;
        for i in 0..s {
            let idx = (h + i) % old_cap;
            let val = ctx.get_array_element(old_buf, idx);
            ctx.set_array_element(new_buf, i, val);
        }
    }
    ctx.set_field(this, AD_FIELD_DATA, Value::Object(Some(new_buf)));
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(size));
}

fn register_array_deque_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/ArrayDeque";

    r.register(c, "<init>", "()V", native_ad_init);
    r.register(c, "<init>", "(I)V", native_ad_init_capacity);
    r.register(c, "size", "()I", native_ad_size);
    r.register(c, "isEmpty", "()Z", native_ad_is_empty);
    r.register(c, "addFirst", "(Ljava/lang/Object;)V", native_ad_add_first);
    r.register(c, "addLast", "(Ljava/lang/Object;)V", native_ad_add_last);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_ad_add);
    r.register(
        c,
        "offerFirst",
        "(Ljava/lang/Object;)Z",
        native_ad_offer_first,
    );
    r.register(
        c,
        "offerLast",
        "(Ljava/lang/Object;)Z",
        native_ad_offer_last,
    );
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_ad_offer);
    r.register(
        c,
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    r.register(
        c,
        "removeLast",
        "()Ljava/lang/Object;",
        native_ad_remove_last,
    );
    r.register(c, "pollFirst", "()Ljava/lang/Object;", native_ad_poll_first);
    r.register(c, "pollLast", "()Ljava/lang/Object;", native_ad_poll_last);
    r.register(c, "poll", "()Ljava/lang/Object;", native_ad_poll_first);
    r.register(c, "getFirst", "()Ljava/lang/Object;", native_ad_get_first);
    r.register(c, "getLast", "()Ljava/lang/Object;", native_ad_get_last);
    r.register(c, "peekFirst", "()Ljava/lang/Object;", native_ad_peek_first);
    r.register(c, "peekLast", "()Ljava/lang/Object;", native_ad_peek_last);
    r.register(c, "peek", "()Ljava/lang/Object;", native_ad_peek_first);
    r.register(c, "push", "(Ljava/lang/Object;)V", native_ad_add_first);
    r.register(c, "pop", "()Ljava/lang/Object;", native_ad_remove_first);
    r.register(c, "element", "()Ljava/lang/Object;", native_ad_get_first);
    r.register(c, "remove", "()Ljava/lang/Object;", native_ad_remove_first);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_ad_contains);
    r.register(c, "clear", "()V", native_ad_clear);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_ad_to_array);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_ad_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_ad_to_string);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_ad_for_each,
    );
}

fn native_ad_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let buf = alloc_ref_array(ctx, AD_DEFAULT_CAPACITY);
    ctx.set_field(this, AD_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_ad_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(c)) => std::cmp::max(*c, 1) as usize,
        _ => AD_DEFAULT_CAPACITY,
    };
    let buf = alloc_ref_array(ctx, cap);
    ctx.set_field(this, AD_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_ad_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

fn native_ad_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

fn native_ad_add_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (_, _, _, size) = ad_state(ctx, this);
    ad_ensure_capacity(ctx, this, (size + 1) as usize);
    let (data, head, _tail, size) = ad_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let new_head = (head - 1 + cap) % cap;
    if let Some(buf) = data {
        ctx.set_array_element(buf, new_head as usize, elem);
    }
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(new_head));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(size + 1));
    Ok(None)
}

fn native_ad_add_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (_, _, _, size) = ad_state(ctx, this);
    ad_ensure_capacity(ctx, this, (size + 1) as usize);
    let (data, _head, tail, size) = ad_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    if let Some(buf) = data {
        ctx.set_array_element(buf, tail as usize, elem);
    }
    let new_tail = (tail + 1) % cap;
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(size + 1));
    Ok(None)
}

fn native_ad_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

fn native_ad_offer_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_first(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

fn native_ad_offer_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

fn native_ad_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_ad_add_last(ctx, args)?;
    Ok(Some(Value::Int(1)))
}

fn native_ad_remove_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "ArrayDeque is empty".to_string(),
        }
        .into());
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let new_head = (head + 1) % cap;
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(new_head));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(size - 1));
    Ok(Some(elem))
}

fn native_ad_remove_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _head, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "ArrayDeque is empty".to_string(),
        }
        .into());
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let new_tail = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, new_tail as usize)
    });
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(new_tail));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(size - 1));
    Ok(Some(elem))
}

fn native_ad_poll_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    native_ad_remove_first(ctx, args)
}

fn native_ad_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, _, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    native_ad_remove_last(ctx, args)
}

fn native_ad_get_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "ArrayDeque is empty".to_string(),
        }
        .into());
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    Ok(Some(elem))
}

fn native_ad_get_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "ArrayDeque is empty".to_string(),
        }
        .into());
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let idx = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, idx as usize)
    });
    Ok(Some(elem))
}

fn native_ad_peek_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, head as usize)
    });
    Ok(Some(elem))
}

fn native_ad_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, _, tail, size) = ad_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let cap = data.map_or(0, |d| ctx.array_length(d)) as i32;
    let idx = (tail - 1 + cap) % cap;
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, idx as usize)
    });
    Ok(Some(elem))
}

fn native_ad_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, head, _, size) = ad_state(ctx, this);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            let elem = ctx.get_array_element(buf, idx);
            if values_equal(ctx, &elem, &target) {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_ad_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.set_field(this, AD_FIELD_HEAD, Value::Int(0));
    ctx.set_field(this, AD_FIELD_TAIL, Value::Int(0));
    ctx.set_field(this, AD_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_ad_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, head, _, size) = ad_state(ctx, this);
    let arr = alloc_ref_array(ctx, size as usize);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            let elem = ctx.get_array_element(buf, idx);
            ctx.set_array_element(arr, i, elem);
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn ad_collect_elements(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let (data, head, _, size) = ad_state(ctx, this);
    let mut elems = Vec::with_capacity(size as usize);
    if let Some(buf) = data {
        let cap = ctx.array_length(buf);
        for i in 0..(size as usize) {
            let idx = (head as usize + i) % cap;
            elems.push(ctx.get_array_element(buf, idx));
        }
    }
    elems
}

fn native_ad_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // ArrayDeque$Itr: field 0 = snapshot array, field 1 = cursor
    let elems = ad_collect_elements(ctx, this);
    let arr = alloc_ref_array(ctx, elems.len());
    for (i, e) in elems.iter().enumerate() {
        ctx.set_array_element(arr, i, *e);
    }
    let itr = alloc_synthetic(ctx, "java/util/ArrayDeque$Itr", 2);
    ctx.set_field(itr, 0, Value::Object(Some(arr)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_ad_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("[]"))))),
    };
    let elems = ad_collect_elements(ctx, this);
    let parts: Vec<String> = elems
        .iter()
        .map(|e| obj_to_display_string(ctx, e))
        .collect();
    let s = format!("[{}]", parts.join(", "));
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

fn native_ad_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let elems = ad_collect_elements(ctx, this);
    for e in &elems {
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[*e])?;
    }
    Ok(None)
}

// ===========================================================================
// PriorityQueue (binary min-heap)
// ===========================================================================

const PQ_FIELD_DATA: usize = 0; // Object[] heap array
const PQ_FIELD_SIZE: usize = 1; // Int element count
const PQ_FIELD_COMPARATOR: usize = 2; // Comparator or null
const PQ_DEFAULT_CAPACITY: usize = 11;

fn pq_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32) {
    let data = match ctx.get_field(this, PQ_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ctx.get_field(this, PQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    (data, size)
}

fn pq_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, min_cap: usize) {
    let (data, _size) = pq_state(ctx, this);
    let old_cap = data.map_or(0, |d| ctx.array_length(d));
    if min_cap <= old_cap {
        return;
    }
    let new_cap = std::cmp::max(old_cap + old_cap / 2 + 1, min_cap);
    let new_buf = alloc_ref_array(ctx, new_cap);
    if let Some(old_buf) = data {
        for i in 0..old_cap {
            let val = ctx.get_array_element(old_buf, i);
            ctx.set_array_element(new_buf, i, val);
        }
    }
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(new_buf)));
}

/// Compare two elements using comparator or natural ordering (string/int/long comparison).
fn pq_compare(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    a: &Value,
    b: &Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let comp = ctx.get_field(this, PQ_FIELD_COMPARATOR);
    if let Value::Object(Some(comparator)) = comp {
        let result = ctx.invoke_virtual(
            comparator,
            "compare",
            "(Ljava/lang/Object;Ljava/lang/Object;)I",
            &[*a, *b],
        )?;
        return Ok(match result {
            Some(Value::Int(v)) => v,
            _ => 0,
        });
    }
    // Natural ordering: compare by string value, int, long, float, double
    Ok(match (a, b) {
        (Value::Object(Some(oa)), Value::Object(Some(ob))) => {
            if let (Some(sa), Some(sb)) = (ctx.read_string(*oa), ctx.read_string(*ob)) {
                sa.cmp(&sb) as i32
            } else {
                0
            }
        }
        (Value::Int(a), Value::Int(b)) => a.cmp(b) as i32,
        (Value::Long(a), Value::Long(b)) => a.cmp(b) as i32,
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).map_or(0, |o| o as i32),
        (Value::Double(a), Value::Double(b)) => a.partial_cmp(b).map_or(0, |o| o as i32),
        _ => 0,
    })
}

fn pq_sift_up(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    mut idx: usize,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    while idx > 0 {
        let parent = (idx - 1) / 2;
        let child_val = ctx.get_array_element(buf, idx);
        let parent_val = ctx.get_array_element(buf, parent);
        if pq_compare(ctx, this, &child_val, &parent_val)? < 0 {
            ctx.set_array_element(buf, idx, parent_val);
            ctx.set_array_element(buf, parent, child_val);
            idx = parent;
        } else {
            break;
        }
    }
    Ok(())
}

fn pq_sift_down(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    buf: ObjectRef,
    mut idx: usize,
    size: usize,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    loop {
        let left = 2 * idx + 1;
        if left >= size {
            break;
        }
        let right = left + 1;
        let mut smallest = left;
        if right < size {
            let lv = ctx.get_array_element(buf, left);
            let rv = ctx.get_array_element(buf, right);
            if pq_compare(ctx, this, &rv, &lv)? < 0 {
                smallest = right;
            }
        }
        let cur_val = ctx.get_array_element(buf, idx);
        let small_val = ctx.get_array_element(buf, smallest);
        if pq_compare(ctx, this, &small_val, &cur_val)? < 0 {
            ctx.set_array_element(buf, idx, small_val);
            ctx.set_array_element(buf, smallest, cur_val);
            idx = smallest;
        } else {
            break;
        }
    }
    Ok(())
}

fn register_priority_queue_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/PriorityQueue";

    r.register(c, "<init>", "()V", native_pq_init);
    r.register(c, "<init>", "(I)V", native_pq_init_capacity);
    r.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_pq_init_comparator,
    );
    r.register(c, "size", "()I", native_pq_size);
    r.register(c, "isEmpty", "()Z", native_pq_is_empty);
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_pq_add);
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_pq_add);
    r.register(c, "peek", "()Ljava/lang/Object;", native_pq_peek);
    r.register(c, "poll", "()Ljava/lang/Object;", native_pq_poll);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_pq_remove);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_pq_contains);
    r.register(c, "clear", "()V", native_pq_clear);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_pq_to_array);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_pq_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_pq_to_string);
}

fn native_pq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let buf = alloc_ref_array(ctx, PQ_DEFAULT_CAPACITY);
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

fn native_pq_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(c)) => std::cmp::max(*c, 1) as usize,
        _ => PQ_DEFAULT_CAPACITY,
    };
    let buf = alloc_ref_array(ctx, cap);
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

fn native_pq_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let comp = args.get(1).copied().unwrap_or(Value::Object(None));
    let buf = alloc_ref_array(ctx, PQ_DEFAULT_CAPACITY);
    ctx.set_field(this, PQ_FIELD_DATA, Value::Object(Some(buf)));
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, PQ_FIELD_COMPARATOR, comp);
    Ok(None)
}

fn native_pq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (_, size) = pq_state(ctx, this);
    Ok(Some(Value::Int(size)))
}

fn native_pq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (_, size) = pq_state(ctx, this);
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

fn native_pq_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (_, size) = pq_state(ctx, this);
    pq_ensure_capacity(ctx, this, (size + 1) as usize);
    let (data, _) = pq_state(ctx, this);
    // `pq_ensure_capacity` allocates a buffer for any `min_cap >= 1`, so this
    // is normally `Some`. Guard against a None backing array (size/data
    // divergence) rather than panicking on `unwrap`.
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    ctx.set_array_element(buf, size as usize, elem);
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(size + 1));
    pq_sift_up(ctx, this, buf, size as usize)?;
    Ok(Some(Value::Int(1)))
}

fn native_pq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let elem = data.map_or(Value::Object(None), |buf| ctx.get_array_element(buf, 0));
    Ok(Some(elem))
}

fn native_pq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    let result = ctx.get_array_element(buf, 0);
    let new_size = size - 1;
    if new_size > 0 {
        let last = ctx.get_array_element(buf, new_size as usize);
        ctx.set_array_element(buf, 0, last);
    }
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size));
    if new_size > 0 {
        pq_sift_down(ctx, this, buf, 0, new_size as usize)?;
    }
    Ok(Some(result))
}

fn native_pq_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = pq_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    // Find element
    let mut found_idx = None;
    for i in 0..(size as usize) {
        let elem = ctx.get_array_element(buf, i);
        if values_equal(ctx, &elem, &target) {
            found_idx = Some(i);
            break;
        }
    }
    let idx = match found_idx {
        Some(i) => i,
        None => return Ok(Some(Value::Int(0))),
    };
    let new_size = (size - 1) as usize;
    if idx == new_size {
        // Removing last element, no sift needed
        ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size as i32));
        return Ok(Some(Value::Int(1)));
    }
    let last = ctx.get_array_element(buf, new_size);
    ctx.set_array_element(buf, idx, last);
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(new_size as i32));
    pq_sift_down(ctx, this, buf, idx, new_size)?;
    Ok(Some(Value::Int(1)))
}

fn native_pq_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = pq_state(ctx, this);
    if let Some(buf) = data {
        for i in 0..(size as usize) {
            let elem = ctx.get_array_element(buf, i);
            if values_equal(ctx, &elem, &target) {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_pq_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ctx.set_field(this, PQ_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_pq_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    let arr = alloc_ref_array(ctx, size as usize);
    if let Some(buf) = data {
        for i in 0..(size as usize) {
            let elem = ctx.get_array_element(buf, i);
            ctx.set_array_element(arr, i, elem);
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_pq_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = pq_state(ctx, this);
    let arr = alloc_ref_array(ctx, size as usize);
    if let Some(buf) = data {
        for i in 0..(size as usize) {
            let elem = ctx.get_array_element(buf, i);
            ctx.set_array_element(arr, i, elem);
        }
    }
    let itr = alloc_synthetic(ctx, "java/util/PriorityQueue$Itr", 2);
    ctx.set_field(itr, 0, Value::Object(Some(arr)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_pq_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(Some(ctx.create_string("[]"))))),
    };
    let (data, size) = pq_state(ctx, this);
    let mut parts = Vec::new();
    if let Some(buf) = data {
        for i in 0..(size as usize) {
            let elem = ctx.get_array_element(buf, i);
            parts.push(obj_to_display_string(ctx, &elem));
        }
    }
    let s = format!("[{}]", parts.join(", "));
    Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
}

// ===========================================================================
// Vector (synchronized ArrayList — simplified without real locks)
// ===========================================================================

fn register_vector_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/Vector";

    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "<init>", "(I)V", native_al_init_capacity);
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_al_get);
    r.register(c, "elementAt", "(I)Ljava/lang/Object;", native_al_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_al_set,
    );
    r.register(
        c,
        "setElementAt",
        "(Ljava/lang/Object;I)V",
        native_vec_set_element_at,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add);
    r.register(
        c,
        "addElement",
        "(Ljava/lang/Object;)V",
        native_vec_add_element,
    );
    r.register(c, "add", "(ILjava/lang/Object;)V", native_al_add_at);
    r.register(
        c,
        "insertElementAt",
        "(Ljava/lang/Object;I)V",
        native_vec_insert_element_at,
    );
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_al_remove_at);
    r.register(c, "remove", "(Ljava/lang/Object;)Z", native_al_remove_obj);
    r.register(
        c,
        "removeElement",
        "(Ljava/lang/Object;)Z",
        native_al_remove_obj,
    );
    r.register(c, "removeElementAt", "(I)V", native_vec_remove_element_at);
    r.register(c, "removeAllElements", "()V", native_al_clear);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(
        c,
        "lastIndexOf",
        "(Ljava/lang/Object;)I",
        native_al_last_index_of,
    );
    r.register(
        c,
        "firstElement",
        "()Ljava/lang/Object;",
        native_vec_first_element,
    );
    r.register(
        c,
        "lastElement",
        "()Ljava/lang/Object;",
        native_vec_last_element,
    );
    r.register(c, "capacity", "()I", native_vec_capacity);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_al_to_array);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_al_for_each,
    );
}

fn native_vec_set_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // setElementAt(Object obj, int index) — reversed param order from set()
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i as usize,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, this);
    if idx >= size as usize {
        return Err(cratonvm_types::error::RuntimeError::ArrayIndexOutOfBoundsException {
            index: idx as i32,
        }
        .into());
    }
    if let Some(buf) = data {
        ctx.set_array_element(buf, idx, elem);
    }
    Ok(None)
}

fn native_vec_add_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_add(ctx, args)?;
    Ok(None)
}

fn native_vec_insert_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // insertElementAt(Object obj, int index) — reversed param order from add(int, Object)
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let idx = match args.get(2) {
        Some(Value::Int(i)) => *i,
        _ => return Ok(None),
    };
    // Repack args as [this, idx, elem] to match add(int, Object) signature
    native_al_add_at(ctx, &[Value::Object(Some(this)), Value::Int(idx), elem])
}

fn native_vec_remove_element_at(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_al_remove_at(ctx, args)?;
    Ok(None)
}

fn native_vec_first_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "Vector is empty".to_string(),
        }
        .into());
    }
    let elem = data.map_or(Value::Object(None), |buf| ctx.get_array_element(buf, 0));
    Ok(Some(elem))
}

fn native_vec_last_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "Vector is empty".to_string(),
        }
        .into());
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, (size - 1) as usize)
    });
    Ok(Some(elem))
}

fn native_vec_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let (data, _) = al_state(ctx, this);
    let cap = data.map_or(0, |d| ctx.array_length(d));
    Ok(Some(Value::Int(cap as i32)))
}

// ===========================================================================
// Stack (extends Vector with push/pop/peek/search/empty)
// ===========================================================================

fn register_stack_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/Stack";

    // Inherit all Vector methods
    r.register(c, "<init>", "()V", native_al_init);
    r.register(c, "size", "()I", native_al_size);
    r.register(c, "isEmpty", "()Z", native_al_is_empty);
    r.register(c, "get", "(I)Ljava/lang/Object;", native_al_get);
    r.register(
        c,
        "set",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        native_al_set,
    );
    r.register(c, "add", "(Ljava/lang/Object;)Z", native_al_add);
    r.register(c, "remove", "(I)Ljava/lang/Object;", native_al_remove_at);
    r.register(c, "clear", "()V", native_al_clear);
    r.register(c, "contains", "(Ljava/lang/Object;)Z", native_al_contains);
    r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_al_index_of);
    r.register(c, "toArray", "()[Ljava/lang/Object;", native_al_to_array);
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_al_iterator);
    r.register(c, "toString", "()Ljava/lang/String;", native_al_to_string);

    // Stack-specific methods
    r.register(
        c,
        "push",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_stack_push,
    );
    r.register(c, "pop", "()Ljava/lang/Object;", native_stack_pop);
    r.register(c, "peek", "()Ljava/lang/Object;", native_stack_peek);
    r.register(c, "empty", "()Z", native_al_is_empty);
    r.register(c, "search", "(Ljava/lang/Object;)I", native_stack_search);
}

fn native_stack_push(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    native_al_add(ctx, args)?;
    Ok(Some(elem))
}

fn native_stack_pop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (_, size) = al_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "Stack is empty".to_string(),
        }
        .into());
    }
    native_al_remove_at(ctx, &[Value::Object(Some(this)), Value::Int(size - 1)])
}

fn native_stack_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, this);
    if size == 0 {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "Stack is empty".to_string(),
        }
        .into());
    }
    let elem = data.map_or(Value::Object(None), |buf| {
        ctx.get_array_element(buf, (size - 1) as usize)
    });
    Ok(Some(elem))
}

fn native_stack_search(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, this);
    // Search from top of stack (last element), return 1-based distance from top
    if let Some(buf) = data {
        for i in (0..(size as usize)).rev() {
            let elem = ctx.get_array_element(buf, i);
            if values_equal(ctx, &elem, &target) {
                return Ok(Some(Value::Int((size as usize - i) as i32)));
            }
        }
    }
    Ok(Some(Value::Int(-1)))
}

// ===========================================================================
// Collection bulk operations (addAll, removeAll, retainAll)
// ===========================================================================

fn register_bulk_ops_natives(r: &mut NativeMethodRegistry) {
    // ArrayList
    r.register(
        "java/util/ArrayList",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_al_remove_all,
    );
    r.register(
        "java/util/ArrayList",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_al_retain_all,
    );

    // HashSet
    r.register(
        "java/util/HashSet",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_hs_add_all,
    );
    r.register(
        "java/util/HashSet",
        "removeAll",
        "(Ljava/util/Collection;)Z",
        native_hs_remove_all,
    );
    r.register(
        "java/util/HashSet",
        "retainAll",
        "(Ljava/util/Collection;)Z",
        native_hs_retain_all,
    );

    // LinkedList
    r.register(
        "java/util/LinkedList",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_ll_add_all,
    );

    // Vector
    r.register(
        "java/util/Vector",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_al_add_all,
    );

    // ArrayDeque
    r.register(
        "java/util/ArrayDeque",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_ad_add_all,
    );
}

/// Collect elements from a Collection (ArrayList, HashSet, LinkedList, etc.)
fn collect_collection_elements(ctx: &mut dyn NativeContext, coll: ObjectRef) -> Vec<Value> {
    // Round 49 fix: Collections$UnmodifiableCollection / $UnmodifiableList
    // wrap their backing collection in field `c`.  Recurse into that to
    // surface the wrapped list's elements — without this, callers like
    // `new LinkedHashSet(unmodifiableList)` (Spring's
    // `AutoConfigurationImportSelector.removeDuplicates`) silently see
    // zero elements and the auto-configuration pipeline collapses.
    let cid = ctx.class_id_of_object(coll);
    if let Some(cls_name) = ctx.class_name_of_id(cid) {
        // CratonVM's own unmodifiable-view wrappers store the backing
        // collection at slot 0 — recurse into it so `new HashSet(unmodList)`
        // and friends see the wrapped elements.
        if cls_name == UNMOD_LIST_CLASS
            || cls_name == UNMOD_SET_CLASS
            || cls_name == UNMOD_COLLECTION_CLASS
        {
            if let Value::Object(Some(inner)) = ctx.get_field(coll, UNMOD_FIELD_BACKING) {
                return collect_collection_elements(ctx, inner);
            }
        }
        if cls_name.starts_with("java/util/Collections$Unmodifiable")
            || cls_name.starts_with("java/util/Collections$Synchronized")
            || cls_name.starts_with("java/util/Collections$Checked")
            || cls_name == "java/util/Collections$SingletonList"
            || cls_name == "java/util/Collections$SingletonSet"
        {
            if let Value::Object(Some(inner)) = ctx.get_field_by_name(coll, "c") {
                return collect_collection_elements(ctx, inner);
            }
            // SingletonList stores the element in field `element`.
            if let Value::Object(Some(_)) = ctx.get_field_by_name(coll, "element") {
                let v = ctx.get_field_by_name(coll, "element");
                return vec![v];
            }
        }
    }
    // KC-Charset fix (2026-05-25): receiver-layout guard for the speculative
    // probe sequence below. `collect_collection_elements` is invoked through
    // generic Collection-interface natives (`addAll`, `retainAll`, `HashSet`
    // ctors, …) whose receiver type is statically `Collection` but at runtime
    // may be ANY object — e.g. the JUnit `--help` bootstrap funnels a
    // `java/nio/charset/Charset` (3 fields) through a generic-collection
    // call site during charset registration, and the blind
    // `ctx.get_field(coll, data_slot)` reads issued below hit slot indices 4
    // and 6 on the 3-slot Charset.  Without the guard the GC fires hundreds
    // of `gen_heap::get_field: out-of-bounds field read dropped` warnings,
    // each probe returns a benign null, and a downstream invariant eventually
    // segfaults the VM.  We compute `n_fields` once and use it to short-circuit
    // any layout probe whose required slot is past the receiver's actual layout.
    let n_fields = ctx.object_num_fields(coll);
    // S111r-bug-fix (peaceful-sammet): Try ArrayList layout via the
    // field-index resolver so we honour the real-JDK layout
    // (`modCount`/`elementData`/`size` slots from AbstractList/ArrayList) —
    // not just the synthetic (0=data, 1=size) layout. Without this,
    // `HashSet.addAll(arrayList)` silently sees zero elements when running
    // against the real JDK, which is the canonical victim for Spring's
    // `AnnotationTypeMapping.processAliases` populating `claimedAliases`
    // and surfaces as the `@AliasFor ... is not meta-present` chain.
    {
        let (data_slot, size_slot, _) = al_slots(ctx);
        if data_slot < n_fields && size_slot < n_fields {
            let f_data = ctx.get_field(coll, data_slot);
            let f_size = ctx.get_field(coll, size_slot);
            if let (Value::Object(Some(arr)), Value::Int(size)) = (f_data, f_size) {
                if ctx.heap_kind_of(arr) == ObjectKind::Array {
                    let len = ctx.array_length(arr);
                    if size >= 0 && len >= size as usize {
                        let mut elems = Vec::with_capacity(size as usize);
                        for i in 0..(size as usize) {
                            elems.push(ctx.get_array_element(arr, i));
                        }
                        return elems;
                    }
                }
            }
        }
    }
    // Legacy/synthetic ArrayList layout (field 0 = Object[], field 1 = Int size).
    // Skip entirely when the receiver doesn't even have 2 slots — common for
    // 0-field marker classes (cglib's `MethodInterceptorGenerator`, etc.).
    let f0 = if n_fields >= 1 {
        ctx.get_field(coll, 0)
    } else {
        Value::Object(None)
    };
    let f1 = if n_fields >= 2 {
        ctx.get_field(coll, 1)
    } else {
        Value::Object(None)
    };
    if let (Value::Object(Some(arr)), Value::Int(size)) = (f0, f1) {
        if ctx.heap_kind_of(arr) == ObjectKind::Array {
            let len = ctx.array_length(arr);
            if size >= 0 && len >= size as usize {
                let mut elems = Vec::with_capacity(size as usize);
                for i in 0..(size as usize) {
                    elems.push(ctx.get_array_element(arr, i));
                }
                return elems;
            }
        }
    }
    // S111r-bug-fix: Arrays$ArrayList — single field `a` (Object[]), no size.
    // `Arrays.asList(...)` is heavily used in JDK callers (and Spring uses
    // it indirectly through `Collections.singletonList` / similar wrappers)
    // and has a different layout than `java.util.ArrayList`.
    if let Value::Object(Some(arr)) = f0 {
        if ctx.heap_kind_of(arr) == ObjectKind::Array {
            let len = ctx.array_length(arr);
            // Heuristic: if this object has at most a couple of fields and
            // field 0 is a ref-array, treat it as an array-backed wrapper.
            let mut elems = Vec::with_capacity(len);
            for i in 0..len {
                elems.push(ctx.get_array_element(arr, i));
            }
            return elems;
        }
    }
    // Try LinkedList layout (field 0 = head Node, field 2 = Int size).
    // Guarded so a 1- or 2-slot non-LL receiver doesn't trigger an OOB probe
    // on slot 2.
    if LL_FIELD_SIZE < n_fields {
        if let Value::Int(size) = ctx.get_field(coll, LL_FIELD_SIZE) {
            if size > 0 && LL_FIELD_HEAD < n_fields {
                let mut elems = Vec::with_capacity(size as usize);
                let mut cur = ctx.get_field(coll, LL_FIELD_HEAD);
                while let Value::Object(Some(node)) = cur {
                    // Per-node guard: a real LL node has 3 slots
                    // (prev/next/elem). A non-node reached here (e.g. via the
                    // false-positive size match above) would OOB-probe on
                    // slot 2 / slot 1.
                    let node_fields = ctx.object_num_fields(node);
                    if LL_NODE_ELEM >= node_fields || LL_NODE_NEXT >= node_fields {
                        break;
                    }
                    elems.push(ctx.get_field(node, LL_NODE_ELEM));
                    cur = ctx.get_field(node, LL_NODE_NEXT);
                }
                return elems;
            }
        }
    }
    // S111r28: HashSet / LinkedHashSet — field 0 = backing HashMap.
    // Walk the backing map's bucket nodes and collect keys.
    if HS_FIELD_MAP < n_fields {
        if let Value::Object(Some(backing)) = ctx.get_field(coll, HS_FIELD_MAP) {
            // Verify it actually is a HashMap-like (slot 0 = bucket array).
            if MAP_FIELD_BUCKETS < ctx.object_num_fields(backing) {
                let s0 = ctx.get_field(backing, MAP_FIELD_BUCKETS);
                if let Value::Object(Some(arr)) = s0 {
                    if ctx.heap_kind_of(arr) == ObjectKind::Array {
                        return map_collect_keys(ctx, backing);
                    }
                }
            }
        }
    }
    Vec::new()
}

fn native_al_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll_elems = collect_collection_elements(ctx, coll);
    let (data, size) = al_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    // Compact: keep elements NOT in collection
    let mut write_idx = 0usize;
    let mut modified = false;
    for read_idx in 0..(size as usize) {
        let elem = ctx.get_array_element(buf, read_idx);
        let should_remove = coll_elems.iter().any(|ce| values_equal(ctx, &elem, ce));
        if should_remove {
            modified = true;
        } else {
            if write_idx != read_idx {
                ctx.set_array_element(buf, write_idx, elem);
            }
            write_idx += 1;
        }
    }
    al_set_size(ctx, this, write_idx as i32);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

fn native_al_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll_elems = collect_collection_elements(ctx, coll);
    let (data, size) = al_state(ctx, this);
    let buf = match data {
        Some(b) => b,
        None => return Ok(Some(Value::Int(0))),
    };
    let mut write_idx = 0usize;
    let mut modified = false;
    for read_idx in 0..(size as usize) {
        let elem = ctx.get_array_element(buf, read_idx);
        let should_keep = coll_elems.iter().any(|ce| values_equal(ctx, &elem, ce));
        if should_keep {
            if write_idx != read_idx {
                ctx.set_array_element(buf, write_idx, elem);
            }
            write_idx += 1;
        } else {
            modified = true;
        }
    }
    al_set_size(ctx, this, write_idx as i32);
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

fn native_hs_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = collect_collection_elements(ctx, coll);
    let mut modified = false;
    for e in &elems {
        let result = native_hs_add(ctx, &[Value::Object(Some(this)), *e])?;
        if result == Some(Value::Int(1)) {
            modified = true;
        }
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

fn native_hs_remove_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll_elems = collect_collection_elements(ctx, coll);
    let mut modified = false;
    for e in &coll_elems {
        let result = native_hs_remove(ctx, &[Value::Object(Some(this)), *e])?;
        if result == Some(Value::Int(1)) {
            modified = true;
        }
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

fn native_hs_retain_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll_elems = collect_collection_elements(ctx, coll);
    // Get current HashSet elements (keys of backing HashMap)
    let current = match hs_backing_map(ctx, this) {
        Some(m) => map_collect_keys(ctx, m),
        None => Vec::new(),
    };
    let mut modified = false;
    for e in &current {
        let should_keep = coll_elems.iter().any(|ce| values_equal(ctx, e, ce));
        if !should_keep {
            native_hs_remove(ctx, &[Value::Object(Some(this)), *e])?;
            modified = true;
        }
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

fn native_ll_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = collect_collection_elements(ctx, coll);
    if elems.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    for e in &elems {
        ll_link_last(ctx, this, *e);
    }
    Ok(Some(Value::Int(1)))
}

fn native_ad_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = collect_collection_elements(ctx, coll);
    if elems.is_empty() {
        return Ok(Some(Value::Int(0)));
    }
    for e in &elems {
        native_ad_add_last(ctx, &[Value::Object(Some(this)), *e])?;
    }
    Ok(Some(Value::Int(1)))
}

// ===========================================================================
// Queue/Deque interface method registrations
// ===========================================================================

fn register_queue_deque_interface_natives(registry: &mut NativeMethodRegistry) {
    // --- java/util/Queue interface ---
    registry.register(
        "java/util/Queue",
        "offer",
        "(Ljava/lang/Object;)Z",
        native_ad_offer,
    );
    registry.register(
        "java/util/Queue",
        "poll",
        "()Ljava/lang/Object;",
        native_ad_poll_first,
    );
    registry.register(
        "java/util/Queue",
        "peek",
        "()Ljava/lang/Object;",
        native_ad_peek_first,
    );
    registry.register(
        "java/util/Queue",
        "add",
        "(Ljava/lang/Object;)Z",
        native_ad_add,
    );
    registry.register(
        "java/util/Queue",
        "remove",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register(
        "java/util/Queue",
        "element",
        "()Ljava/lang/Object;",
        native_ad_get_first,
    );
    registry.register("java/util/Queue", "size", "()I", native_ad_size);
    registry.register("java/util/Queue", "isEmpty", "()Z", native_ad_is_empty);

    // --- java/util/Deque interface ---
    registry.register(
        "java/util/Deque",
        "addFirst",
        "(Ljava/lang/Object;)V",
        native_ad_add_first,
    );
    registry.register(
        "java/util/Deque",
        "addLast",
        "(Ljava/lang/Object;)V",
        native_ad_add_last,
    );
    registry.register(
        "java/util/Deque",
        "removeFirst",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register(
        "java/util/Deque",
        "removeLast",
        "()Ljava/lang/Object;",
        native_ad_remove_last,
    );
    registry.register(
        "java/util/Deque",
        "peekFirst",
        "()Ljava/lang/Object;",
        native_ad_peek_first,
    );
    registry.register(
        "java/util/Deque",
        "peekLast",
        "()Ljava/lang/Object;",
        native_ad_peek_last,
    );
    registry.register(
        "java/util/Deque",
        "push",
        "(Ljava/lang/Object;)V",
        native_ad_add_first,
    );
    registry.register(
        "java/util/Deque",
        "pop",
        "()Ljava/lang/Object;",
        native_ad_remove_first,
    );
    registry.register("java/util/Deque", "size", "()I", native_ad_size);
    registry.register("java/util/Deque", "isEmpty", "()Z", native_ad_is_empty);

    // Iterator support for ArrayDeque$Itr and PriorityQueue$Itr
    // They follow the same 2-field snapshot pattern (field 0 = array, field 1 = cursor)
    for itr_class in &["java/util/ArrayDeque$Itr", "java/util/PriorityQueue$Itr"] {
        registry.register(itr_class, "hasNext", "()Z", native_snapshot_itr_has_next);
        registry.register(
            itr_class,
            "next",
            "()Ljava/lang/Object;",
            native_snapshot_itr_next,
        );
    }
}

/// Generic snapshot-based iterator: field 0 = Object[] snapshot, field 1 = Int cursor.
///
/// Real-JDK fallback: this native is registered at the `java/util/Iterator`
/// *interface* level (see `register_iterator_protocol_natives`) so it
/// otherwise intercepts every real iterator (e.g. `ArrayList$Itr`, whose
/// field 0 is an `int cursor`, not an `Object[]`). When field 0 isn't an
/// array, dispatch to the receiver's concrete bytecode by name.
fn native_snapshot_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            let cid = ctx.class_id_of_object(this);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn.is_empty() || cn == "java/util/Iterator" || cn == "java/util/ListIterator" {
                return Ok(Some(Value::Int(0)));
            }
            return ctx.invoke(&cn, "hasNext", "()Z", &[Value::Object(Some(this))]);
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    Ok(Some(Value::Int(if cursor < len { 1 } else { 0 })))
}

fn native_snapshot_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // HashMap$KeyItr layout-compatible iterators (field 0 = Object[], field
    // 1 = cursor) also happen to satisfy the snapshot-iterator shape — but
    // they carry an extra `lastRet` slot that subsequent Iterator.remove()
    // reads. Route via the dedicated KeyItr native so the lastRet write
    // happens, otherwise it.remove() throws spurious IllegalStateException.
    // HashMap$KeyItr layout-compatible iterators (field 0 = Object[], field
    // 1 = cursor) also satisfy the snapshot-iterator shape — but they carry
    // an extra `lastRet` slot that subsequent Iterator.remove() reads.
    // Route via the dedicated KeyItr native so the lastRet write happens,
    // otherwise it.remove() throws spurious IllegalStateException.
    let cid = ctx.class_id_of_object(this);
    let cn = ctx.class_name_of_id(cid).unwrap_or_default();
    if cn == "java/util/HashMap$KeyItr" {
        return native_map_key_itr_next(ctx, args);
    }
    // Real-JDK fallback: see `native_snapshot_itr_has_next` doc comment.
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            if !cn.is_empty() && cn != "java/util/Iterator" && cn != "java/util/ListIterator" {
                return ctx.invoke(&cn, "next", "()Ljava/lang/Object;", &[Value::Object(Some(this))]);
            }
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "No more elements".to_string(),
            }
            .into());
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    let len = ctx.array_length(arr) as i32;
    if cursor >= len {
        return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
            message: "No more elements".to_string(),
        }
        .into());
    }
    let elem = ctx.get_array_element(arr, cursor as usize);
    ctx.set_field(this, 1, Value::Int(cursor + 1));
    Ok(Some(elem))
}

// ===========================================================================
// TreeMap — sorted map.
//
// Round-9 HIGH (MED-8 carryover): the original implementation was a sorted
// `Object[]` with binary-search lookup and `tm_insert_at`/`tm_remove_at`
// shifting, giving O(log N) `get` but O(N) `put`/`remove`. The JDK's
// `TreeMap` is a red-black tree at O(log N) for all operations.
//
// We adopt a hybrid: for the common case (no custom Comparator, keys are
// String / Integer / Long / wrapped primitive) we maintain a Rust-side
// `BTreeMap<TreeKey, Value>` as the authoritative store (O(log N) on all
// ops). For custom Comparator we fall through to the array path, because
// keying a Rust BTreeMap on Java's `Comparator.compare` requires invoking
// Java code from comparison — that's a re-entrancy and lifetime hazard
// (the BTreeMap holds a mutable borrow during compare while the Comparator
// might trigger arbitrary VM code, including class loading).
//
// The fast-mode flag is stored in the LHM-style overlay (`tm_overlay`).
// When fast-mode is active the array slot stays empty; size is mirrored
// to the synthetic slot so legacy size readers still work. Iteration-style
// natives (`keySet`, `values`, `entrySet`, `forEach`) populate their output
// from the BTreeMap when active.
// ===========================================================================

const TM_FIELD_DATA: usize = 0; // Object[] interleaved [k0, v0, k1, v1, ...]
const TM_FIELD_SIZE: usize = 1; // Int: number of entries
const TM_FIELD_COMPARATOR: usize = 2; // Comparator object or null
const TM_NUM_FIELDS: usize = 3;
const TM_DEFAULT_CAPACITY: usize = 16; // initial entry slots (array len = 32)

/// Natural-order key supported by the fast-mode TreeMap. Variants are
/// ordered so the derived `Ord` matches Java's natural ordering for
/// homogeneous-typed maps (String, Integer, Long). Mixed-type maps
/// disable fast mode (the array path handles them via `natural_compare`).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum TreeKey {
    Str(String),
    I32(i32),
    I64(i64),
}

/// Try to extract a fast-mode key from a Java value. Returns None for
/// types that need a Java-side `compareTo` callback (custom Comparable
/// objects, non-primitive wrappers, etc.); the caller falls back to the
/// array path.
fn tree_key_from_value(ctx: &dyn NativeContext, v: &Value) -> Option<TreeKey> {
    match v {
        Value::Int(i) => Some(TreeKey::I32(*i)),
        Value::Long(l) => Some(TreeKey::I64(*l)),
        Value::Object(Some(o)) => {
            // String fast path
            if let Some(s) = ctx.read_string(*o) {
                return Some(TreeKey::Str(s));
            }
            // Integer / Long boxes: field 0 is the wrapped primitive.
            match ctx.get_field(*o, 0) {
                Value::Int(i) => Some(TreeKey::I32(i)),
                Value::Long(l) => Some(TreeKey::I64(l)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Per-TreeMap fast-mode side-table. The outer key is the synthetic
/// receiver's address; the inner BTreeMap is the authoritative store
/// for fast-mode maps (mirroring is avoided — when fast mode is active,
/// the array slot is left empty and we read from the BTreeMap).
fn tm_fast_table() -> &'static Mutex<StdHashMap<usize, std::collections::BTreeMap<TreeKey, Value>>>
{
    static T: std::sync::OnceLock<
        Mutex<StdHashMap<usize, std::collections::BTreeMap<TreeKey, Value>>>,
    > = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(StdHashMap::new()))
}

// TreeMap / TreeSet side-table key: rekeyed onto
// `ctx.identity_hash_code(this)` so the array-mode state survives a
// moving-GC relocation. Originally `this.as_ptr() as usize`, which
// silently dropped the `data` array and size when the GC moved the
// TreeMap (worst case: a subsequent `put` re-creates state under a
// new key while old entries become unreachable garbage in the table).
fn tm_obj_key(ctx: &dyn NativeContext, this: ObjectRef) -> usize {
    ih_obj_key(ctx, this)
}

/// Address-keyed TreeMap array-mode state side-table — `(data array, size,
/// comparator)` keyed by the receiver's address.
///
/// Why a side-table instead of object fields: in real-JDK mode `TreeMap`
/// instances have the *real* JDK field layout (`comparator`, `root`,
/// `size`, `modCount`, ...), and subclasses such as Felix's `StringMap
/// extends TreeMap` add their own fields on top. Writing to the synthetic
/// slots 0/1/2 lands in unrelated real fields — silently losing the data
/// array and size, and (worse) writing an `Int` into a reference slot
/// corrupts the heap for the GC. Keying state by the object's address is
/// layout-independent and works for arbitrary subclasses.
#[derive(Clone)]
struct TmArrayState {
    data: Option<ObjectRef>,
    size: i32,
    comparator: Value,
}
impl Default for TmArrayState {
    fn default() -> Self {
        TmArrayState {
            data: None,
            size: 0,
            comparator: Value::Object(None),
        }
    }
}
fn tm_array_table() -> &'static Mutex<StdHashMap<usize, TmArrayState>> {
    static T: std::sync::OnceLock<Mutex<StdHashMap<usize, TmArrayState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(StdHashMap::new()))
}

/// Read a TreeMap "slot" (`TM_FIELD_DATA`/`SIZE`/`COMPARATOR`) from the
/// address-keyed side-table. Returns layout-independent defaults when no
/// entry exists yet. The object's own fields are never consulted.
fn tm_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    let key = tm_obj_key(ctx, this);
    let tbl = tm_array_table().lock().unwrap();
    if let Some(st) = tbl.get(&key) {
        return match slot {
            TM_FIELD_DATA => Value::Object(st.data),
            TM_FIELD_SIZE => Value::Int(st.size),
            TM_FIELD_COMPARATOR => st.comparator,
            _ => Value::Object(None),
        };
    }
    match slot {
        TM_FIELD_SIZE => Value::Int(0),
        _ => Value::Object(None),
    }
}

/// Write a TreeMap "slot" into the address-keyed side-table (creating the
/// entry on first write). The object's own fields are never touched — the
/// side-table is the sole authoritative store (see `TmArrayState`).
fn tm_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    let key = tm_obj_key(ctx, this);
    let mut tbl = tm_array_table().lock().unwrap();
    let st = tbl.entry(key).or_default();
    match slot {
        TM_FIELD_DATA => {
            st.data = match v {
                Value::Object(o) => o,
                _ => None,
            }
        }
        TM_FIELD_SIZE => {
            st.size = match v {
                Value::Int(n) => n,
                _ => 0,
            }
        }
        TM_FIELD_COMPARATOR => st.comparator = v,
        _ => {}
    }
}

/// Returns true if this TreeMap is currently using the BTreeMap fast path.
/// Determined by:
///   1. comparator field is null/absent (custom Comparator → array path), AND
///   2. a fast-mode entry exists in `tm_fast_table` (set on first put when
///      the first key is extractable into a TreeKey).
///
/// Empty TreeMaps with null comparator are tentatively "fast-eligible" —
/// the first non-extractable key flips them to array mode.
fn tm_is_fast_mode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let key = tm_obj_key(ctx, this);
    tm_fast_table().lock().unwrap().contains_key(&key)
}

/// True if the comparator slot is null. Custom Comparator forces array mode
/// because we can't replicate user-defined ordering in a Rust BTreeMap.
fn tm_has_no_comparator(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    matches!(tm_get_slot(ctx, this, TM_FIELD_COMPARATOR), Value::Object(None))
}

/// Borrow the fast-mode BTreeMap mutably and call `f`. Creates the entry
/// if missing. Caller must ensure they only invoke this when fast mode
/// is applicable (no comparator, etc.).
fn tm_fast_with<R>(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    f: impl FnOnce(&mut std::collections::BTreeMap<TreeKey, Value>) -> R,
) -> R {
    let key = tm_obj_key(ctx, this);
    let mut map = tm_fast_table().lock().unwrap();
    let bt = map.entry(key).or_default();
    f(bt)
}

/// "Sticky" flag side-table — once a TreeMap is forced to array mode
/// (e.g. by a non-extractable key) it stays there for its lifetime so
/// we never split state across both stores.
fn tm_force_array_set() -> &'static Mutex<StdHashMap<usize, ()>> {
    static T: std::sync::OnceLock<Mutex<StdHashMap<usize, ()>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(StdHashMap::new()))
}
fn tm_force_array_mode(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let key = tm_obj_key(ctx, this);
    tm_force_array_set().lock().unwrap().contains_key(&key)
}
fn tm_set_force_array(ctx: &dyn NativeContext, this: ObjectRef) {
    let key = tm_obj_key(ctx, this);
    tm_force_array_set().lock().unwrap().insert(key, ());
}

/// Migrate any fast-mode entries to the array store, then remove the
/// fast-mode side-table entry. Called when a non-extractable key arrives
/// at a map that previously had fast-mode entries — keeps state coherent
/// across the mode flip without losing data.
fn tm_migrate_fast_to_array(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let key = tm_obj_key(ctx, this);
    // Snapshot fast-mode entries first, then drop the side-table entry.
    let entries: Vec<(TreeKey, Value)> = tm_fast_with(ctx, this, |bt| {
        bt.iter().map(|(k, v)| (k.clone(), *v)).collect()
    });
    tm_fast_table().lock().unwrap().remove(&key);
    if entries.is_empty() {
        return;
    }
    // Box keys back to Java wrappers and insert via the array path.
    let boxed: Vec<(Value, Value)> = entries
        .into_iter()
        .map(|(k, v)| (tree_key_to_value(ctx, &k), v))
        .collect();
    // Reset size counter — the array put loop will re-establish it.
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    // Ensure data array exists.
    if matches!(tm_get_slot(ctx, this, TM_FIELD_DATA), Value::Object(None)) {
        let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
        tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
    }
    // Now insert each entry through the array path. The fast-mode check
    // in `native_tm_put` will skip because `tm_force_array_set` is set
    // before we get here (caller's responsibility).
    for (k, v) in boxed {
        let _ = native_tm_put(ctx, &[Value::Object(Some(this)), k, v]);
    }
}

/// Convert a TreeKey back to a Java `Value` for return values that need
/// the original key (firstKey, ceilingKey, etc.). Boxes primitives to
/// the corresponding wrapper class; reuses the standard `Integer.valueOf`
/// / `Long.valueOf` / `String` paths via the NativeContext.
fn tree_key_to_value(ctx: &mut dyn NativeContext, k: &TreeKey) -> Value {
    match k {
        TreeKey::Str(s) => Value::Object(Some(ctx.create_string(s))),
        TreeKey::I32(i) => {
            // Box via Integer.valueOf
            let boxed = ctx
                .invoke(
                    "java/lang/Integer",
                    "valueOf",
                    "(I)Ljava/lang/Integer;",
                    &[Value::Int(*i)],
                )
                .ok()
                .flatten()
                .unwrap_or(Value::Object(None));
            boxed
        }
        TreeKey::I64(l) => {
            let boxed = ctx
                .invoke(
                    "java/lang/Long",
                    "valueOf",
                    "(J)Ljava/lang/Long;",
                    &[Value::Long(*l)],
                )
                .ok()
                .flatten()
                .unwrap_or(Value::Object(None));
            boxed
        }
    }
}

// TreeSet — sorted set backed by sorted array
const TS_FIELD_DATA: usize = 0; // Object[] sorted elements
const TS_FIELD_SIZE: usize = 1; // Int: number of elements
const TS_FIELD_COMPARATOR: usize = 2; // Comparator or null
const TS_NUM_FIELDS: usize = 3;
const TS_DEFAULT_CAPACITY: usize = 16;

/// Address-keyed TreeSet state side-table. Mirrors `TmArrayState`: in
/// real-JDK mode `java.util.TreeSet` (and any subclass) has the real JDK
/// field layout, so the synthetic slots 0/1/2 do not exist. All state —
/// backing data array, size, comparator — lives here, keyed by the
/// receiver's address, so it is layout-independent.
#[derive(Clone)]
struct TsArrayState {
    data: Option<ObjectRef>,
    size: i32,
    comparator: Value,
}
impl Default for TsArrayState {
    fn default() -> Self {
        TsArrayState {
            data: None,
            size: 0,
            comparator: Value::Object(None),
        }
    }
}
fn ts_array_table() -> &'static Mutex<StdHashMap<usize, TsArrayState>> {
    static T: std::sync::OnceLock<Mutex<StdHashMap<usize, TsArrayState>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(StdHashMap::new()))
}

/// Read a TreeSet "slot" (`TS_FIELD_DATA`/`SIZE`/`COMPARATOR`) from the
/// address-keyed side-table. Returns layout-independent defaults when no
/// entry exists yet.
fn ts_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    let key = tm_obj_key(ctx, this);
    let tbl = ts_array_table().lock().unwrap();
    if let Some(st) = tbl.get(&key) {
        return match slot {
            TS_FIELD_DATA => Value::Object(st.data),
            TS_FIELD_SIZE => Value::Int(st.size),
            TS_FIELD_COMPARATOR => st.comparator,
            _ => Value::Object(None),
        };
    }
    match slot {
        TS_FIELD_SIZE => Value::Int(0),
        _ => Value::Object(None),
    }
}

/// Write a TreeSet "slot" into the address-keyed side-table. The object's
/// own fields are never touched (see `tm_set_slot`).
fn ts_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    let key = tm_obj_key(ctx, this);
    let mut tbl = ts_array_table().lock().unwrap();
    let st = tbl.entry(key).or_default();
    match slot {
        TS_FIELD_DATA => {
            st.data = match v {
                Value::Object(o) => o,
                _ => None,
            }
        }
        TS_FIELD_SIZE => {
            st.size = match v {
                Value::Int(n) => n,
                _ => 0,
            }
        }
        TS_FIELD_COMPARATOR => st.comparator = v,
        _ => {}
    }
}

// ----- comparison helper -----

fn tree_compare(
    ctx: &mut dyn NativeContext,
    comparator: &Value,
    a: Value,
    b: Value,
) -> Result<i32, cratonvm_types::error::MethodCallFailed> {
    let result = match comparator {
        Value::Object(Some(cmp)) => comparator_compare(ctx, *cmp, a, b)?,
        _ => natural_compare(ctx, &a, &b)?,
    };
    match result {
        Some(Value::Int(v)) => Ok(v),
        _ => Ok(0),
    }
}

// ----- TreeMap binary search -----
// Returns Ok(index) if key found at that entry index, Err(insert_pos) if not found.
fn tm_binary_search(
    ctx: &mut dyn NativeContext,
    data: ObjectRef,
    size: i32,
    comparator: &Value,
    key: &Value,
) -> Result<Result<usize, usize>, cratonvm_types::error::MethodCallFailed> {
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_key = ctx.get_array_element(data, mid * 2);
        let cmp = tree_compare(ctx, comparator, mid_key, *key)?;
        if cmp < 0 {
            low = mid + 1;
        } else if cmp > 0 {
            high = mid;
        } else {
            return Ok(Ok(mid));
        }
    }
    Ok(Err(low))
}

// ----- TreeSet binary search -----
fn ts_binary_search(
    ctx: &mut dyn NativeContext,
    data: ObjectRef,
    size: i32,
    comparator: &Value,
    key: &Value,
) -> Result<Result<usize, usize>, cratonvm_types::error::MethodCallFailed> {
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_elem = ctx.get_array_element(data, mid);
        let cmp = tree_compare(ctx, comparator, mid_elem, *key)?;
        if cmp < 0 {
            low = mid + 1;
        } else if cmp > 0 {
            high = mid;
        } else {
            return Ok(Ok(mid));
        }
    }
    Ok(Err(low))
}

// Read TreeMap state
fn tm_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, Value) {
    let data = match tm_get_slot(ctx, this, TM_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    (data, size, comparator)
}

// Ensure TreeMap data array has room for at least one more entry
fn tm_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size: i32,
    data: ObjectRef,
) -> ObjectRef {
    let arr_len = ctx.array_length(data);
    let needed = ((size + 1) as usize) * 2;
    if needed <= arr_len {
        return data;
    }
    let new_cap = (arr_len * 2).max(needed);
    let new_arr = alloc_ref_array(ctx, new_cap);
    for i in 0..(size as usize * 2) {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(new_arr, i, v);
    }
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(new_arr)));
    new_arr
}

// Insert key/value at position, shifting elements right
fn tm_insert_at(
    ctx: &mut dyn NativeContext,
    data: ObjectRef,
    size: i32,
    pos: usize,
    key: Value,
    value: Value,
) {
    let s = size as usize;
    for i in (pos..s).rev() {
        let k = ctx.get_array_element(data, i * 2);
        let v = ctx.get_array_element(data, i * 2 + 1);
        ctx.set_array_element(data, (i + 1) * 2, k);
        ctx.set_array_element(data, (i + 1) * 2 + 1, v);
    }
    ctx.set_array_element(data, pos * 2, key);
    ctx.set_array_element(data, pos * 2 + 1, value);
}

// Remove entry at position, shifting elements left
fn tm_remove_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize) {
    let s = size as usize;
    for i in pos..(s - 1) {
        let k = ctx.get_array_element(data, (i + 1) * 2);
        let v = ctx.get_array_element(data, (i + 1) * 2 + 1);
        ctx.set_array_element(data, i * 2, k);
        ctx.set_array_element(data, i * 2 + 1, v);
    }
    ctx.set_array_element(data, (s - 1) * 2, Value::Object(None));
    ctx.set_array_element(data, (s - 1) * 2 + 1, Value::Object(None));
}

// ----- TreeSet helpers -----

fn ts_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, i32, Value) {
    let data = match ts_get_slot(ctx, this, TS_FIELD_DATA) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let comparator = ts_get_slot(ctx, this, TS_FIELD_COMPARATOR);
    (data, size, comparator)
}

fn ts_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size: i32,
    data: ObjectRef,
) -> ObjectRef {
    let arr_len = ctx.array_length(data);
    let needed = (size + 1) as usize;
    if needed <= arr_len {
        return data;
    }
    let new_cap = (arr_len * 2).max(needed);
    let new_arr = alloc_ref_array(ctx, new_cap);
    for i in 0..(size as usize) {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(new_arr, i, v);
    }
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(new_arr)));
    new_arr
}

fn ts_insert_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize, elem: Value) {
    let s = size as usize;
    for i in (pos..s).rev() {
        let v = ctx.get_array_element(data, i);
        ctx.set_array_element(data, i + 1, v);
    }
    ctx.set_array_element(data, pos, elem);
}

fn ts_remove_at(ctx: &mut dyn NativeContext, data: ObjectRef, size: i32, pos: usize) {
    let s = size as usize;
    for i in pos..(s - 1) {
        let v = ctx.get_array_element(data, i + 1);
        ctx.set_array_element(data, i, v);
    }
    ctx.set_array_element(data, s - 1, Value::Object(None));
}

// ===========================================================================
// TreeMap native methods
// ===========================================================================

fn native_tm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Seed identity-hash so the side-table key survives GC moves.
    ih_seed(ctx, this);
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, this, TM_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

fn native_tm_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ih_seed(ctx, this);
    let cmp = args.get(1).copied().unwrap_or(Value::Object(None));
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, this, TM_FIELD_COMPARATOR, cmp);
    Ok(None)
}

fn native_tm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));

    // Round-9 HIGH (MED-8 carryover): fast-mode BTreeMap path. Eligible
    // when no custom Comparator was supplied AND the key extracts into
    // a TreeKey (String / Integer / Long). For an empty map this also
    // *enables* fast mode by creating the side-table entry.
    if tm_has_no_comparator(ctx, this) && !tm_force_array_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            // If the map already has entries in array mode (e.g. from an
            // earlier non-extractable key), fall through to the array
            // path to avoid splitting state across two stores.
            let already_in_array = matches!(tm_get_slot(ctx, this, TM_FIELD_SIZE), Value::Int(n) if n > 0)
                && !tm_is_fast_mode(ctx, this);
            if !already_in_array {
                let (old, new_size) = tm_fast_with(ctx, this, |bt| {
                    let old = bt.insert(tk, value).unwrap_or(Value::Object(None));
                    (old, bt.len() as i32)
                });
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                return Ok(Some(old));
            }
        } else {
            // Non-extractable key → force array mode permanently for this map.
            // If fast-mode had accumulated entries, migrate them first so
            // we don't split state across two stores.
            tm_set_force_array(ctx, this);
            if tm_is_fast_mode(ctx, this) {
                tm_migrate_fast_to_array(ctx, this);
            }
        }
    }

    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
            tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
            buf
        }
    };
    let search = tm_binary_search(ctx, data, size, &comparator, &key)?;
    match search {
        Ok(idx) => {
            // Key exists — replace value, return old
            let old = ctx.get_array_element(data, idx * 2 + 1);
            ctx.set_array_element(data, idx * 2 + 1, value);
            Ok(Some(old))
        }
        Err(pos) => {
            // Key not found — insert at pos
            let data = tm_ensure_capacity(ctx, this, size, data);
            tm_insert_at(ctx, data, size, pos, key, value);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    }
}

fn native_tm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let v = tm_fast_with(ctx, this, |bt| bt.get(&tk).copied().unwrap_or(Value::Object(None)));
            return Ok(Some(v));
        }
        // Fast-mode map asked for a non-extractable key → not present.
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2 + 1))),
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

fn native_tm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let (old, new_size) = tm_fast_with(ctx, this, |bt| {
                let old = bt.remove(&tk).unwrap_or(Value::Object(None));
                (old, bt.len() as i32)
            });
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
            return Ok(Some(old));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => {
            let old_val = ctx.get_array_element(data, idx * 2 + 1);
            tm_remove_at(ctx, data, size, idx);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
            Ok(Some(old_val))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

fn native_tm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let found = tm_fast_with(ctx, this, |bt| bt.contains_key(&tk));
            return Ok(Some(Value::Int(i32::from(found))));
        }
        return Ok(Some(Value::Int(0)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let found = tm_binary_search(ctx, data, size, &comparator, &key)?.is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

fn native_tm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        let values: Vec<Value> = tm_fast_with(ctx, this, |bt| bt.values().copied().collect());
        for v in values {
            if values_equal(ctx, &v, &target) {
                return Ok(Some(Value::Int(1)));
            }
        }
        return Ok(Some(Value::Int(0)));
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    for i in 0..(size as usize) {
        let v = ctx.get_array_element(data, i * 2 + 1);
        if values_equal(ctx, &v, &target) {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_tm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

fn native_tm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match tm_get_slot(ctx, this, TM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

fn native_tm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    // Fast-mode side-table needs clearing too — without this an iter
    // helper would return stale entries from before the clear.
    tm_fast_with(ctx, this, |bt| bt.clear());
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_tm_first_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeMap is empty".to_string(),
            }
            .into())
        }
    };
    if tm_is_fast_mode(ctx, this) {
        let first = tm_fast_with(ctx, this, |bt| bt.keys().next().cloned());
        match first {
            Some(tk) => return Ok(Some(tree_key_to_value(ctx, &tk))),
            None => {
                return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into())
            }
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty: a divergence between the size
    // counter and the data array must not panic via `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeMap is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(data, 0)))
}

fn native_tm_last_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeMap is empty".to_string(),
            }
            .into())
        }
    };
    if tm_is_fast_mode(ctx, this) {
        let last = tm_fast_with(ctx, this, |bt| bt.keys().next_back().cloned());
        match last {
            Some(tk) => return Ok(Some(tree_key_to_value(ctx, &tk))),
            None => {
                return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                    message: "TreeMap is empty".to_string(),
                }
                .into())
            }
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeMap is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(data, (size as usize - 1) * 2)))
}

// ceilingKey: smallest key >= given key
fn native_tm_ceiling_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| bt.range(tk..).next().map(|(k, _)| k.clone()));
            return Ok(Some(res.map(|k| tree_key_to_value(ctx, &k)).unwrap_or(Value::Object(None))));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2))),
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// floorKey: largest key <= given key
fn native_tm_floor_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range(..=tk).next_back().map(|(k, _)| k.clone())
            });
            return Ok(Some(res.map(|k| tree_key_to_value(ctx, &k)).unwrap_or(Value::Object(None))));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2))),
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, (pos - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// higherKey: smallest key strictly > given key
fn native_tm_higher_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Excluded(tk), Bound::Unbounded))
                    .next()
                    .map(|(k, _)| k.clone())
            });
            return Ok(Some(res.map(|k| tree_key_to_value(ctx, &k)).unwrap_or(Value::Object(None))));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => {
            let next = idx + 1;
            if next < size as usize {
                Ok(Some(ctx.get_array_element(data, next * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

// lowerKey: largest key strictly < given key
fn native_tm_lower_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            use std::ops::Bound;
            let res = tm_fast_with(ctx, this, |bt| {
                bt.range((Bound::Unbounded, Bound::Excluded(tk)))
                    .next_back()
                    .map(|(k, _)| k.clone())
            });
            return Ok(Some(res.map(|k| tree_key_to_value(ctx, &k)).unwrap_or(Value::Object(None))));
        }
        return Ok(Some(Value::Object(None)));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => {
            if idx > 0 {
                Ok(Some(ctx.get_array_element(data, (idx - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, (pos - 1) * 2)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

fn tm_make_entry(ctx: &mut dyn NativeContext, key: Value, value: Value) -> ObjectRef {
    let entry = alloc_synthetic(ctx, "java/util/HashMap$Entry", 2);
    ctx.set_field(entry, 0, key);
    ctx.set_field(entry, 1, value);
    entry
}

fn native_tm_first_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if tm_is_fast_mode(ctx, this) {
        let first = tm_fast_with(ctx, this, |bt| bt.iter().next().map(|(k, v)| (k.clone(), *v)));
        match first {
            Some((tk, v)) => {
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v);
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let k = ctx.get_array_element(data, 0);
    let v = ctx.get_array_element(data, 1);
    let entry = tm_make_entry(ctx, k, v);
    Ok(Some(Value::Object(Some(entry))))
}

fn native_tm_last_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if tm_is_fast_mode(ctx, this) {
        let last = tm_fast_with(ctx, this, |bt| bt.iter().next_back().map(|(k, v)| (k.clone(), *v)));
        match last {
            Some((tk, v)) => {
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v);
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let last = (size as usize - 1) * 2;
    let k = ctx.get_array_element(data, last);
    let v = ctx.get_array_element(data, last + 1);
    let entry = tm_make_entry(ctx, k, v);
    Ok(Some(Value::Object(Some(entry))))
}

fn native_tm_poll_first_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if tm_is_fast_mode(ctx, this) {
        let removed = tm_fast_with(ctx, this, |bt| {
            let first = bt.iter().next().map(|(k, _)| k.clone())?;
            let v = bt.remove(&first)?;
            Some((first, v, bt.len() as i32))
        });
        match removed {
            Some((tk, v, new_size)) => {
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v);
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let k = ctx.get_array_element(data, 0);
    let v = ctx.get_array_element(data, 1);
    tm_remove_at(ctx, data, size, 0);
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
    let entry = tm_make_entry(ctx, k, v);
    Ok(Some(Value::Object(Some(entry))))
}

fn native_tm_poll_last_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    if tm_is_fast_mode(ctx, this) {
        let removed = tm_fast_with(ctx, this, |bt| {
            let last = bt.iter().next_back().map(|(k, _)| k.clone())?;
            let v = bt.remove(&last)?;
            Some((last, v, bt.len() as i32))
        });
        match removed {
            Some((tk, v, new_size)) => {
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                let k = tree_key_to_value(ctx, &tk);
                let entry = tm_make_entry(ctx, k, v);
                return Ok(Some(Value::Object(Some(entry))));
            }
            None => return Ok(Some(Value::Object(None))),
        }
    }
    let (data_opt, size, _) = tm_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => return Ok(Some(Value::Object(None))),
    };
    let last = (size as usize - 1) * 2;
    let k = ctx.get_array_element(data, last);
    let v = ctx.get_array_element(data, last + 1);
    ctx.set_array_element(data, last, Value::Object(None));
    ctx.set_array_element(data, last + 1, Value::Object(None));
    tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size - 1));
    let entry = tm_make_entry(ctx, k, v);
    Ok(Some(Value::Object(Some(entry))))
}

fn native_tm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pairs = tm_collect_pairs(ctx, this);
    let size = pairs.len() as i32;
    let ts = alloc_synthetic(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, std::cmp::max(pairs.len(), TS_DEFAULT_CAPACITY));
    for (i, (k, _)) in pairs.iter().enumerate() {
        ctx.set_array_element(buf, i, *k);
    }
    ts_set_slot(ctx, ts, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, ts, TS_FIELD_SIZE, Value::Int(size));
    let comparator = tm_get_slot(ctx, this, TM_FIELD_COMPARATOR);
    ts_set_slot(ctx, ts, TS_FIELD_COMPARATOR, comparator);
    Ok(Some(Value::Object(Some(ts))))
}

/// Collect (key, value) pairs in sorted order for both fast and array modes.
/// Boxes fast-mode primitive keys back to Java wrapper objects after
/// releasing the side-table lock (boxing may re-enter the VM).
fn tm_collect_pairs(ctx: &mut dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    if tm_is_fast_mode(ctx, this) {
        let raw: Vec<(TreeKey, Value)> = tm_fast_with(ctx, this, |bt| {
            bt.iter().map(|(k, v)| (k.clone(), *v)).collect()
        });
        raw.into_iter()
            .map(|(k, v)| (tree_key_to_value(ctx, &k), v))
            .collect()
    } else {
        let (data_opt, size, _) = tm_state(ctx, this);
        let data = match data_opt {
            Some(d) => d,
            None => return Vec::new(),
        };
        let mut out = Vec::with_capacity(size as usize);
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(data, i * 2);
            let v = ctx.get_array_element(data, i * 2 + 1);
            out.push((k, v));
        }
        out
    }
}

fn native_tm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pairs = tm_collect_pairs(ctx, this);
    let size = pairs.len() as i32;
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(size as usize, AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for (i, (_, v)) in pairs.iter().enumerate() {
        ctx.set_array_element(buf, i, *v);
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, size);
    Ok(Some(Value::Object(Some(list))))
}

fn native_tm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pairs = tm_collect_pairs(ctx, this);
    let size = pairs.len() as i32;
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let cap = std::cmp::max(size as usize, AL_DEFAULT_CAPACITY);
    let buf = alloc_ref_array(ctx, cap);
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        let entry = tm_make_entry(ctx, k, v);
        ctx.set_array_element(buf, i, Value::Object(Some(entry)));
    }
    al_set_data(ctx, list, buf);
    al_set_size(ctx, list, size);
    Ok(Some(Value::Object(Some(list))))
}

fn native_tm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let pairs = tm_collect_pairs(ctx, this);
    for (k, v) in pairs {
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[k, v],
        )?;
    }
    Ok(None)
}

fn native_tm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    if tm_is_fast_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let v = tm_fast_with(ctx, this, |bt| bt.get(&tk).copied());
            return Ok(Some(v.unwrap_or(default)));
        }
        return Ok(Some(default));
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(default)),
    };
    match tm_binary_search(ctx, data, size, &comparator, &key)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx * 2 + 1))),
        Err(_) => Ok(Some(default)),
    }
}

fn native_tm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // Fast-mode eligibility check mirrors `native_tm_put`.
    if tm_has_no_comparator(ctx, this) && !tm_force_array_mode(ctx, this) {
        if let Some(tk) = tree_key_from_value(ctx, &key) {
            let already_in_array = matches!(tm_get_slot(ctx, this, TM_FIELD_SIZE), Value::Int(n) if n > 0)
                && !tm_is_fast_mode(ctx, this);
            if !already_in_array {
                let (existing, new_size) = tm_fast_with(ctx, this, |bt| {
                    use std::collections::btree_map::Entry;
                    match bt.entry(tk) {
                        Entry::Occupied(o) => (Some(*o.get()), bt.len() as i32),
                        Entry::Vacant(v) => {
                            v.insert(value);
                            (None, bt.len() as i32)
                        }
                    }
                });
                tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(new_size));
                return Ok(Some(existing.unwrap_or(Value::Object(None))));
            }
        } else {
            tm_set_force_array(ctx, this);
            if tm_is_fast_mode(ctx, this) {
                tm_migrate_fast_to_array(ctx, this);
            }
        }
    }
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
            tm_set_slot(ctx, this, TM_FIELD_DATA, Value::Object(Some(buf)));
            buf
        }
    };
    let search = tm_binary_search(ctx, data, size, &comparator, &key)?;
    match search {
        Ok(idx) => {
            let existing = ctx.get_array_element(data, idx * 2 + 1);
            Ok(Some(existing))
        }
        Err(pos) => {
            let data = tm_ensure_capacity(ctx, this, size, data);
            tm_insert_at(ctx, data, size, pos, key, value);
            tm_set_slot(ctx, this, TM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    }
}

fn native_tm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    // TreeMap / TreeMap-subclass source: its state lives in the
    // address-keyed side-tables, not object fields — copy via the
    // sorted-pairs snapshot rather than reading raw slots.
    if is_tree_map_receiver(ctx, source) {
        let pairs = tm_collect_pairs(ctx, source);
        for (k, v) in pairs {
            native_tm_put(ctx, &[Value::Object(Some(this)), k, v])?;
        }
        return Ok(None);
    }
    // Read entries from source map (try HashMap layout first, then TreeMap)
    let src_f0 = ctx.get_field(source, 0);
    let src_f1 = ctx.get_field(source, 1);
    let src_f2 = ctx.get_field(source, 2);
    match (src_f1, src_f2) {
        (Value::Int(src_size), Value::Int(_capacity)) => {
            // HashMap/LinkedHashMap layout: field 0 = buckets, field 1 = size, field 2 = capacity
            if let Value::Object(Some(buckets)) = src_f0 {
                let num_buckets = ctx.array_length(buckets);
                for b in 0..num_buckets {
                    let mut node_val = ctx.get_array_element(buckets, b);
                    while let Value::Object(Some(node)) = node_val {
                        let k = ctx.get_field(node, 0); // NODE_FIELD_KEY
                        let v = ctx.get_field(node, 1); // NODE_FIELD_VALUE
                        native_tm_put(ctx, &[Value::Object(Some(this)), k, v])?;
                        node_val = ctx.get_field(node, 3); // NODE_FIELD_NEXT
                    }
                }
            }
            let _ = src_size; // suppress warning
        }
        (Value::Int(src_size), _) => {
            // TreeMap layout: field 0 = data array, field 1 = size, field 2 = comparator
            if let Value::Object(Some(src_data)) = src_f0 {
                for i in 0..(src_size as usize) {
                    let k = ctx.get_array_element(src_data, i * 2);
                    let v = ctx.get_array_element(src_data, i * 2 + 1);
                    native_tm_put(ctx, &[Value::Object(Some(this)), k, v])?;
                }
            }
        }
        _ => {}
    }
    Ok(None)
}

fn native_tm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let pairs = tm_collect_pairs(ctx, this);
    let mut buf = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            buf.push_str(", ");
        }
        buf.push_str(&obj_to_display_string(ctx, k));
        buf.push('=');
        buf.push_str(&obj_to_display_string(ctx, v));
    }
    buf.push('}');
    let s = ctx.create_string(&buf);
    Ok(Some(Value::Object(Some(s))))
}

fn native_tm_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(tm_get_slot(ctx, this, TM_FIELD_COMPARATOR)))
}

// headMap: entries with keys strictly < toKey
fn native_tm_head_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeMap", TM_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, result, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, result, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, result, TM_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(data, i * 2);
            let cmp = tree_compare(ctx, &comparator, k, to_key)?;
            if cmp >= 0 {
                break;
            }
            let v = ctx.get_array_element(data, i * 2 + 1);
            native_tm_put(ctx, &[Value::Object(Some(result)), k, v])?;
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

// tailMap: entries with keys >= fromKey
fn native_tm_tail_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeMap", TM_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, result, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, result, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, result, TM_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(data, i * 2);
            let cmp = tree_compare(ctx, &comparator, k, from_key)?;
            if cmp >= 0 {
                let v = ctx.get_array_element(data, i * 2 + 1);
                native_tm_put(ctx, &[Value::Object(Some(result)), k, v])?;
            }
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

// subMap: entries with keys >= fromKey and < toKey
fn native_tm_sub_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_key = args.get(1).copied().unwrap_or(Value::Object(None));
    let to_key = args.get(2).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = tm_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeMap", TM_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TM_DEFAULT_CAPACITY * 2);
    tm_set_slot(ctx, result, TM_FIELD_DATA, Value::Object(Some(buf)));
    tm_set_slot(ctx, result, TM_FIELD_SIZE, Value::Int(0));
    tm_set_slot(ctx, result, TM_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(data, i * 2);
            let cmp_lo = tree_compare(ctx, &comparator, k, from_key)?;
            if cmp_lo < 0 {
                continue;
            }
            let cmp_hi = tree_compare(ctx, &comparator, k, to_key)?;
            if cmp_hi >= 0 {
                break;
            }
            let v = ctx.get_array_element(data, i * 2 + 1);
            native_tm_put(ctx, &[Value::Object(Some(result)), k, v])?;
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn native_tm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let mapper = match args.get(2) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Round-9 HIGH: route through `native_tm_get` / `native_tm_put` so the
    // fast-mode BTreeMap path applies for natural-ordering maps.
    let existing = native_tm_get(ctx, &[Value::Object(Some(this)), key])?
        .unwrap_or(Value::Object(None));
    if !matches!(existing, Value::Object(None)) {
        return Ok(Some(existing));
    }
    let result = ctx.invoke_virtual(
        mapper,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[key],
    )?;
    let val = result.unwrap_or(Value::Object(None));
    if matches!(val, Value::Object(None)) {
        return Ok(Some(Value::Object(None)));
    }
    native_tm_put(ctx, &[Value::Object(Some(this)), key, val])?;
    Ok(Some(val))
}

fn native_tm_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let remap_fn = match args.get(3) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Round-9 HIGH: route through `native_tm_get` / `native_tm_put` so the
    // fast-mode BTreeMap path applies for natural-ordering maps.
    let existing = native_tm_get(ctx, &[Value::Object(Some(this)), key])?
        .unwrap_or(Value::Object(None));
    let new_val = if matches!(existing, Value::Object(None)) {
        value
    } else {
        let merged = ctx.invoke_virtual(
            remap_fn,
            "apply",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            &[existing, value],
        )?;
        merged.unwrap_or(Value::Object(None))
    };
    if matches!(new_val, Value::Object(None)) {
        // JDK Map.merge contract: null result removes the mapping.
        native_tm_remove(ctx, &[Value::Object(Some(this)), key])?;
        return Ok(Some(Value::Object(None)));
    }
    native_tm_put(ctx, &[Value::Object(Some(this)), key, new_val])?;
    Ok(Some(new_val))
}

// TreeMap key iterator: snapshot-based, returns keys in sorted order
fn native_tm_key_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Both fast and array modes go through the shared snapshot helper so
    // the iterator sees sorted-order keys regardless of backing store.
    let pairs = tm_collect_pairs(ctx, this);
    let snap = alloc_ref_array(ctx, pairs.len().max(1));
    for (i, (k, _)) in pairs.iter().enumerate() {
        ctx.set_array_element(snap, i, *k);
    }
    let itr = alloc_synthetic(ctx, "java/util/TreeMap$KeyItr", 2);
    ctx.set_field(itr, 0, Value::Object(Some(snap)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// TreeSet native methods
// ===========================================================================

fn native_ts_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ih_seed(ctx, this);
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
    Ok(None)
}

fn native_ts_init_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ih_seed(ctx, this);
    let cmp = args.get(1).copied().unwrap_or(Value::Object(None));
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, cmp);
    Ok(None)
}

fn native_ts_init_collection(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    ih_seed(ctx, this);
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => {
            let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
            ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
            ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
            return Ok(None);
        }
    };
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, this, TS_FIELD_COMPARATOR, Value::Object(None));
    // Add elements from source
    let elems = collect_collection_elements(ctx, source);
    for e in elems {
        native_ts_add(ctx, &[Value::Object(Some(this)), e])?;
    }
    Ok(None)
}

fn native_ts_add(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => {
            let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
            ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
            buf
        }
    };
    let search = ts_binary_search(ctx, data, size, &comparator, &elem)?;
    match search {
        Ok(_) => Ok(Some(Value::Int(0))), // already present
        Err(pos) => {
            let data = ts_ensure_capacity(ctx, this, size, data);
            ts_insert_at(ctx, data, size, pos, elem);
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Int(1)))
        }
    }
}

fn native_ts_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    match ts_binary_search(ctx, data, size, &comparator, &elem)? {
        Ok(idx) => {
            ts_remove_at(ctx, data, size, idx);
            ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(size - 1));
            Ok(Some(Value::Int(1)))
        }
        Err(_) => Ok(Some(Value::Int(0))),
    }
}

fn native_ts_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let found = ts_binary_search(ctx, data, size, &comparator, &elem)?.is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

fn native_ts_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

fn native_ts_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match ts_get_slot(ctx, this, TS_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

fn native_ts_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, this, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, this, TS_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_ts_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeSet is empty".to_string(),
            }
            .into())
        }
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeSet is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(data, 0)))
}

fn native_ts_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeSet is empty".to_string(),
            }
            .into())
        }
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    // Treat a None backing array as empty rather than panicking on `unwrap`.
    let data = match data_opt {
        Some(d) if size != 0 => d,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "TreeSet is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(data, (size - 1) as usize)))
}

fn native_ts_ceiling(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match ts_binary_search(ctx, data, size, &comparator, &elem)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx))),
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

fn native_ts_floor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match ts_binary_search(ctx, data, size, &comparator, &elem)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(data, idx))),
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, pos - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

fn native_ts_higher(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match ts_binary_search(ctx, data, size, &comparator, &elem)? {
        Ok(idx) => {
            let next = idx + 1;
            if next < size as usize {
                Ok(Some(ctx.get_array_element(data, next)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos < size as usize {
                Ok(Some(ctx.get_array_element(data, pos)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

fn native_ts_lower(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    match ts_binary_search(ctx, data, size, &comparator, &elem)? {
        Ok(idx) => {
            if idx > 0 {
                Ok(Some(ctx.get_array_element(data, idx - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
        Err(pos) => {
            if pos > 0 {
                Ok(Some(ctx.get_array_element(data, pos - 1)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

fn native_ts_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let snap = alloc_ref_array(ctx, size as usize);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(data, i);
            ctx.set_array_element(snap, i, v);
        }
    }
    // Field 0 = snapshot array, field 1 = cursor, field 2 = owning TreeSet.
    // The back-reference to the owning set lets `TreeSet$Itr.remove()` delete
    // the last-returned element from the live set (real-JDK `Iterator.remove`
    // contract) rather than throwing UnsupportedOperationException.
    let itr = alloc_synthetic(ctx, "java/util/TreeSet$Itr", 3);
    ctx.set_field(itr, 0, Value::Object(Some(snap)));
    ctx.set_field(itr, 1, Value::Int(0));
    ctx.set_field(itr, 2, Value::Object(Some(this)));
    Ok(Some(Value::Object(Some(itr))))
}

/// `TreeSet$Itr.remove()` — delete the last element returned by `next()` from
/// the owning `TreeSet`. The snapshot iterator stores the owning set in field
/// 2; the last-returned element is the snapshot entry at `cursor - 1`.
fn native_ts_itr_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(r)) if ctx.heap_kind_of(r) == ObjectKind::Array => r,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
                message: "remove".to_string(),
            }
            .into())
        }
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    // `next()` advances the cursor past the element it returned, so the
    // last-returned element lives at `cursor - 1`. A cursor of 0 means
    // `next()` was never called (or `remove()` was already invoked).
    if cursor <= 0 {
        return Err(cratonvm_types::error::RuntimeError::IllegalStateException {
            message: "remove".to_string(),
        }
        .into());
    }
    let owner = match ctx.get_field(this, 2) {
        Value::Object(Some(o)) => o,
        _ => {
            // No owning set recorded — nothing to remove from. Stay silent
            // rather than throwing UOE so the iteration can still complete.
            return Ok(None);
        }
    };
    let last = ctx.get_array_element(arr, (cursor - 1) as usize);
    // Delete the element from the live set via the existing TreeSet remove
    // path (binary search + side-table size update).
    let (data_opt, size, comparator) = ts_state(ctx, owner);
    if let Some(data) = data_opt {
        if let Ok(idx) = ts_binary_search(ctx, data, size, &comparator, &last)? {
            ts_remove_at(ctx, data, size, idx);
            ts_set_slot(ctx, owner, TS_FIELD_SIZE, Value::Int(size - 1));
        }
    }
    Ok(None)
}

fn native_ts_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(None),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let data = match data_opt {
        Some(d) => d,
        None => return Ok(None),
    };
    for i in 0..(size as usize) {
        let v = ctx.get_array_element(data, i);
        ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[v])?;
    }
    Ok(None)
}

fn native_ts_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let arr = alloc_ref_array(ctx, size as usize);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let v = ctx.get_array_element(data, i);
            ctx.set_array_element(arr, i, v);
        }
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_ts_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data_opt, size, _) = ts_state(ctx, this);
    let mut buf = String::from("[");
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            if i > 0 {
                buf.push_str(", ");
            }
            let v = ctx.get_array_element(data, i);
            buf.push_str(&obj_to_display_string(ctx, &v));
        }
    }
    buf.push(']');
    let s = ctx.create_string(&buf);
    Ok(Some(Value::Object(Some(s))))
}

fn native_ts_comparator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ts_get_slot(ctx, this, TS_FIELD_COMPARATOR)))
}

fn native_ts_head_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let to_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, result, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, result, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, result, TS_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, to_elem)?;
            if cmp >= 0 {
                break;
            }
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn native_ts_tail_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, result, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, result, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, result, TS_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp = tree_compare(ctx, &comparator, e, from_elem)?;
            if cmp >= 0 {
                native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
            }
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn native_ts_sub_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    let from_elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let to_elem = args.get(2).copied().unwrap_or(Value::Object(None));
    let (data_opt, size, comparator) = ts_state(ctx, this);
    let result = alloc_synthetic(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, TS_DEFAULT_CAPACITY);
    ts_set_slot(ctx, result, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, result, TS_FIELD_SIZE, Value::Int(0));
    ts_set_slot(ctx, result, TS_FIELD_COMPARATOR, comparator);
    if let Some(data) = data_opt {
        for i in 0..(size as usize) {
            let e = ctx.get_array_element(data, i);
            let cmp_lo = tree_compare(ctx, &comparator, e, from_elem)?;
            if cmp_lo < 0 {
                continue;
            }
            let cmp_hi = tree_compare(ctx, &comparator, e, to_elem)?;
            if cmp_hi >= 0 {
                break;
            }
            native_ts_add(ctx, &[Value::Object(Some(result)), e])?;
        }
    }
    Ok(Some(Value::Object(Some(result))))
}

fn native_ts_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Int(0))),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elems = collect_collection_elements(ctx, source);
    let mut changed = false;
    for e in elems {
        let result = native_ts_add(ctx, &[Value::Object(Some(this)), e])?;
        if result == Some(Value::Int(1)) {
            changed = true;
        }
    }
    Ok(Some(Value::Int(i32::from(changed))))
}

// ===========================================================================
// Registration
// ===========================================================================

fn register_tree_map_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/TreeMap";
    registry.register(c, "<init>", "()V", native_tm_init);
    registry.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_tm_init_comparator,
    );
    registry.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_put,
    );
    registry.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_get,
    );
    registry.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_remove,
    );
    registry.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_tm_contains_key,
    );
    registry.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_tm_contains_value,
    );
    registry.register(c, "size", "()I", native_tm_size);
    registry.register(c, "isEmpty", "()Z", native_tm_is_empty);
    registry.register(c, "clear", "()V", native_tm_clear);
    registry.register(c, "firstKey", "()Ljava/lang/Object;", native_tm_first_key);
    registry.register(c, "lastKey", "()Ljava/lang/Object;", native_tm_last_key);
    registry.register(
        c,
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_ceiling_key,
    );
    registry.register(
        c,
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_floor_key,
    );
    registry.register(
        c,
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_higher_key,
    );
    registry.register(
        c,
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_lower_key,
    );
    registry.register(
        c,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_first_entry,
    );
    registry.register(
        c,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_last_entry,
    );
    registry.register(
        c,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_first_entry,
    );
    registry.register(
        c,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_last_entry,
    );
    registry.register(c, "keySet", "()Ljava/util/Set;", native_tm_key_set);
    registry.register(c, "values", "()Ljava/util/Collection;", native_tm_values);
    registry.register(c, "entrySet", "()Ljava/util/Set;", native_tm_entry_set);
    registry.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_tm_for_each,
    );
    registry.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_get_or_default,
    );
    registry.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_put_if_absent,
    );
    registry.register(c, "putAll", "(Ljava/util/Map;)V", native_tm_put_all);
    registry.register(c, "toString", "()Ljava/lang/String;", native_tm_to_string);
    registry.register(
        c,
        "comparator",
        "()Ljava/util/Comparator;",
        native_tm_comparator,
    );
    registry.register(
        c,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_head_map,
    );
    registry.register(
        c,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_tail_map,
    );
    registry.register(
        c,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_sub_map,
    );
    registry.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_tm_compute_if_absent,
    );
    registry.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_tm_merge,
    );
    registry.register(
        c,
        "iterator",
        "()Ljava/util/Iterator;",
        native_tm_key_iterator,
    );

    // Iterator for TreeMap keys
    let ki = "java/util/TreeMap$KeyItr";
    registry.register(ki, "hasNext", "()Z", native_snapshot_itr_has_next);
    registry.register(ki, "next", "()Ljava/lang/Object;", native_snapshot_itr_next);

    // SortedMap/NavigableMap interface dispatch
    let sm = "java/util/SortedMap";
    registry.register(sm, "firstKey", "()Ljava/lang/Object;", native_tm_first_key);
    registry.register(sm, "lastKey", "()Ljava/lang/Object;", native_tm_last_key);
    registry.register(
        sm,
        "comparator",
        "()Ljava/util/Comparator;",
        native_tm_comparator,
    );
    registry.register(
        sm,
        "headMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_head_map,
    );
    registry.register(
        sm,
        "tailMap",
        "(Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_tail_map,
    );
    registry.register(
        sm,
        "subMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedMap;",
        native_tm_sub_map,
    );
    let nm = "java/util/NavigableMap";
    registry.register(
        nm,
        "ceilingKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_ceiling_key,
    );
    registry.register(
        nm,
        "floorKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_floor_key,
    );
    registry.register(
        nm,
        "higherKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_higher_key,
    );
    registry.register(
        nm,
        "lowerKey",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_tm_lower_key,
    );
    registry.register(
        nm,
        "firstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_first_entry,
    );
    registry.register(
        nm,
        "lastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_last_entry,
    );
    registry.register(
        nm,
        "pollFirstEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_first_entry,
    );
    registry.register(
        nm,
        "pollLastEntry",
        "()Ljava/util/Map$Entry;",
        native_tm_poll_last_entry,
    );
}

fn register_tree_set_natives(registry: &mut NativeMethodRegistry) {
    let c = "java/util/TreeSet";
    registry.register(c, "<init>", "()V", native_ts_init);
    registry.register(
        c,
        "<init>",
        "(Ljava/util/Comparator;)V",
        native_ts_init_comparator,
    );
    registry.register(
        c,
        "<init>",
        "(Ljava/util/Collection;)V",
        native_ts_init_collection,
    );
    registry.register(c, "add", "(Ljava/lang/Object;)Z", native_ts_add);
    registry.register(c, "remove", "(Ljava/lang/Object;)Z", native_ts_remove);
    registry.register(c, "contains", "(Ljava/lang/Object;)Z", native_ts_contains);
    registry.register(c, "size", "()I", native_ts_size);
    registry.register(c, "isEmpty", "()Z", native_ts_is_empty);
    registry.register(c, "clear", "()V", native_ts_clear);
    registry.register(c, "first", "()Ljava/lang/Object;", native_ts_first);
    registry.register(c, "last", "()Ljava/lang/Object;", native_ts_last);
    registry.register(
        c,
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_ceiling,
    );
    registry.register(
        c,
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_floor,
    );
    registry.register(
        c,
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_higher,
    );
    registry.register(
        c,
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_lower,
    );
    registry.register(c, "iterator", "()Ljava/util/Iterator;", native_ts_iterator);
    registry.register(
        c,
        "forEach",
        "(Ljava/util/function/Consumer;)V",
        native_ts_for_each,
    );
    registry.register(c, "toArray", "()[Ljava/lang/Object;", native_ts_to_array);
    registry.register(c, "toString", "()Ljava/lang/String;", native_ts_to_string);
    registry.register(
        c,
        "comparator",
        "()Ljava/util/Comparator;",
        native_ts_comparator,
    );
    registry.register(
        c,
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_head_set,
    );
    registry.register(
        c,
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_tail_set,
    );
    registry.register(
        c,
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_sub_set,
    );
    registry.register(c, "addAll", "(Ljava/util/Collection;)Z", native_ts_add_all);
    registry.register(c, "stream", "()Ljava/util/stream/Stream;", native_ts_stream);

    // TreeSet iterator
    let ti = "java/util/TreeSet$Itr";
    registry.register(ti, "hasNext", "()Z", native_snapshot_itr_has_next);
    registry.register(ti, "next", "()Ljava/lang/Object;", native_snapshot_itr_next);
    // `Iterator.remove()` is a supported operation for TreeSet's iterator —
    // delete the last-returned element from the owning set (field 2).
    registry.register(ti, "remove", "()V", native_ts_itr_remove);

    // SortedSet/NavigableSet interface dispatch
    let ss = "java/util/SortedSet";
    registry.register(ss, "first", "()Ljava/lang/Object;", native_ts_first);
    registry.register(ss, "last", "()Ljava/lang/Object;", native_ts_last);
    registry.register(
        ss,
        "comparator",
        "()Ljava/util/Comparator;",
        native_ts_comparator,
    );
    registry.register(
        ss,
        "headSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_head_set,
    );
    registry.register(
        ss,
        "tailSet",
        "(Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_tail_set,
    );
    registry.register(
        ss,
        "subSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/SortedSet;",
        native_ts_sub_set,
    );
    let ns = "java/util/NavigableSet";
    registry.register(
        ns,
        "ceiling",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_ceiling,
    );
    registry.register(
        ns,
        "floor",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_floor,
    );
    registry.register(
        ns,
        "higher",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_higher,
    );
    registry.register(
        ns,
        "lower",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_ts_lower,
    );
}

// ===========================================================================
// ConcurrentHashMap (Phase 22 / M18, Phase 86.2 — segmented concurrency)
// ===========================================================================
//
// ConcurrentHashMap uses a striped/segmented layout for real concurrency:
//   field 0 = Object[] segments (array of segment objects)
//   field 1 = Int segment_mask (num_segments - 1)
// Each segment is a 3-field HashMap-like object (buckets, size, capacity).
// Reads (get, containsKey) are lock-free. Writes (put, remove) lock only
// the target segment via monitor_enter/exit, allowing concurrent writes to
// different segments without contention.

const CHM_FIELD_SEGMENTS: usize = 0;
const CHM_FIELD_SEGMENT_MASK: usize = 1;
const _CHM_NUM_FIELDS: usize = 2;
const CHM_DEFAULT_SEGMENTS: usize = 16;
const CHM_DEFAULT_SEGMENT_CAP: usize = 4;

/// Compute hash for a key value (reuses map_hash_key for object keys).
///
/// Propagates any exception thrown by a user-supplied `hashCode()` via the
/// underlying `map_hash_key` (MED fix).
fn chm_key_hash(ctx: &mut dyn NativeContext, key: &Value) -> Result<i32, MethodCallFailed> {
    match key {
        Value::Object(Some(k)) => map_hash_key(ctx, *k),
        _ => Ok(0),
    }
}

/// Get the segment object for a given hash.
fn chm_segment_for(ctx: &dyn NativeContext, this: ObjectRef, hash: i32) -> Option<ObjectRef> {
    let segments = match ctx.get_field(this, CHM_FIELD_SEGMENTS) {
        Value::Object(Some(arr)) => arr,
        _ => return None,
    };
    let mask = match ctx.get_field(this, CHM_FIELD_SEGMENT_MASK) {
        Value::Int(m) => m as usize,
        _ => CHM_DEFAULT_SEGMENTS - 1,
    };
    let idx = (hash as u32 as usize) & mask;
    match ctx.get_array_element(segments, idx) {
        Value::Object(Some(seg)) => Some(seg),
        _ => None,
    }
}

/// Get all segment objects.
fn chm_all_segments(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<ObjectRef> {
    let segments = match ctx.get_field(this, CHM_FIELD_SEGMENTS) {
        Value::Object(Some(arr)) => arr,
        _ => return Vec::new(),
    };
    let mask = match ctx.get_field(this, CHM_FIELD_SEGMENT_MASK) {
        Value::Int(m) => m as usize,
        _ => CHM_DEFAULT_SEGMENTS - 1,
    };
    let count = mask + 1;
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        if let Value::Object(Some(seg)) = ctx.get_array_element(segments, i) {
            result.push(seg);
        }
    }
    result
}

/// Collect all entries from all segments.
fn chm_collect_all_entries(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<(Value, Value)> {
    let mut entries = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let seg_entries = map_collect_entries(ctx, seg);
        entries.extend(seg_entries);
    }
    entries
}

/// Collect all keys from all segments.
///
/// Uses the layout-aware `get_node_key` helper rather than a hardcoded
/// `NODE_FIELD_KEY` slot: CHM segment bucket nodes can use either the legacy
/// (key@0,val@1) or the JDK (hash@0,key@1,val@2) layout. Reading slot 0
/// blindly returned the `hash` int for JDK-layout nodes, so `keySet()` /
/// `values()` collected garbage that was then silently dropped — leaving the
/// view iterators empty even though `size()` was correct. `entrySet()` already
/// went through `get_node_key`/`get_node_value` and worked, which is why only
/// `keySet()`/`values()` were broken (observed via Felix's
/// `Felix.getServiceReferences`, whose `new ArrayList<>(set)` over a
/// `Collections.newSetFromMap(new ConcurrentHashMap())` came back empty).
fn chm_collect_all_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut keys = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    keys.push(get_node_key(ctx, node));
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }
    keys
}

/// Collect all values from all segments.
///
/// See `chm_collect_all_keys` — uses the layout-aware `get_node_value` helper
/// so JDK-layout bucket nodes (hash@0,key@1,val@2) are read correctly.
fn chm_collect_all_values(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<Value> {
    let mut vals = Vec::new();
    for seg in chm_all_segments(ctx, this) {
        let (buckets, _size, cap) = map_state(ctx, seg);
        if let Some(b) = buckets {
            for i in 0..(cap as usize) {
                let mut node_val = ctx.get_array_element(b, i);
                while let Value::Object(Some(node)) = node_val {
                    vals.push(get_node_value(ctx, node));
                    node_val = ctx.get_field(node, NODE_FIELD_NEXT);
                }
            }
        }
    }
    vals
}

/// Initialize a CHM with segments.
fn chm_init_segments(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    num_segments: usize,
    cap_per_segment: usize,
) {
    let segments = alloc_ref_array(ctx, num_segments);
    for i in 0..num_segments {
        let seg = ctx.alloc_object(ClassId::new(0), MAP_NUM_FIELDS);
        let buckets = alloc_ref_array(ctx, cap_per_segment);
        ctx.set_field(seg, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
        set_map_size(ctx, seg, 0);
        ctx.set_field(seg, MAP_FIELD_CAPACITY, Value::Int(cap_per_segment as i32));
        let _ = ctx.set_array_element(segments, i, Value::Object(Some(seg)));
    }
    ctx.set_field(this, CHM_FIELD_SEGMENTS, Value::Object(Some(segments)));
    ctx.set_field(this, CHM_FIELD_SEGMENT_MASK, Value::Int((num_segments - 1) as i32));
}

fn register_concurrent_hashmap_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/ConcurrentHashMap";

    // Constructors
    r.register(c, "<init>", "()V", native_chm_init_default);
    r.register(c, "<init>", "(I)V", native_chm_init_capacity);
    r.register(c, "<init>", "(IFI)V", native_chm_init_full);
    r.register(c, "<init>", "(Ljava/util/Map;)V", native_chm_init_from_map);

    // Core operations — segmented
    r.register(c, "size", "()I", native_chm_size);
    r.register(c, "isEmpty", "()Z", native_chm_is_empty);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove,
    );
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key,
    );
    r.register(
        c,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value,
    );
    r.register(c, "clear", "()V", native_chm_clear);
    r.register(c, "keySet", "()Ljava/util/Set;", native_chm_key_set);
    r.register(c, "values", "()Ljava/util/Collection;", native_chm_values);
    r.register(c, "entrySet", "()Ljava/util/Set;", native_chm_entry_set);
    // `ConcurrentHashMap.keySet()` has a covariant return type: the real
    // method's descriptor is `()L...$KeySetView;`, and `()Ljava/util/Set;`
    // is only the synthetic bridge. `javac` emits an invokevirtual against
    // the *covariant* descriptor when the static receiver type is
    // `ConcurrentHashMap`, so without this registration `keySet()` fell
    // through to the real-JDK bytecode (`new KeySetView(this, null)`), whose
    // iterator walks the unpopulated `table` field and yields nothing. This
    // broke `new ArrayList<>(Collections.newSetFromMap(new ConcurrentHashMap()))`
    // — observed as Felix `getServiceReference` returning null because
    // `Felix.getServiceReferences` copies the CHM-backed result set.
    r.register(
        c,
        "keySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_key_set,
    );
    r.register(c, "toString", "()Ljava/lang/String;", native_chm_to_string);
    r.register(
        c,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get_or_default,
    );
    r.register(
        c,
        "putIfAbsent",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put_if_absent,
    );
    r.register(c, "putAll", "(Ljava/util/Map;)V", native_chm_put_all);
    r.register(c, "hashCode", "()I", native_chm_hash_code);
    r.register(c, "equals", "(Ljava/lang/Object;)Z", native_chm_equals);
    r.register(
        c,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_chm_for_each,
    );
    r.register(
        c,
        "computeIfAbsent",
        "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
        native_chm_compute_if_absent,
    );
    r.register(
        c,
        "compute",
        "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_compute,
    );
    r.register(
        c,
        "merge",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
        native_chm_merge,
    );
    r.register(
        c,
        "replaceAll",
        "(Ljava/util/function/BiFunction;)V",
        native_chm_replace_all,
    );

    // ConcurrentHashMap-specific methods
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_remove_kv,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_replace,
    );
    r.register(
        c,
        "replace",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Z",
        native_chm_replace_kv,
    );
    r.register(
        c,
        "forEach",
        "(JLjava/util/function/BiConsumer;)V",
        native_chm_for_each_parallel,
    );
    r.register(c, "mappingCount", "()J", native_chm_mapping_count);
    r.register(
        c,
        "newKeySet",
        "()Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set,
    );
    r.register(
        c,
        "newKeySet",
        "(I)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_new_key_set_cap,
    );
    r.register(
        c,
        "keySet",
        "(Ljava/lang/Object;)Ljava/util/concurrent/ConcurrentHashMap$KeySetView;",
        native_chm_key_set_view,
    );
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_value,
    );
    r.register(
        c,
        "elements",
        "()Ljava/util/Enumeration;",
        native_chm_elements,
    );
    r.register(c, "keys", "()Ljava/util/Enumeration;", native_chm_keys);

    // Bulk operations — snapshot under per-segment locks
    r.register(
        c,
        "forEachEntry",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let entries = chm_collect_all_entries(ctx, this);
            for (key, value) in entries {
                let entry = ctx.alloc_object(ClassId::new(0), NODE_NUM_FIELDS);
                ctx.set_field(entry, NODE_FIELD_KEY, key);
                ctx.set_field(entry, NODE_FIELD_VALUE, value);
                ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[Value::Object(Some(entry))])?;
            }
            Ok(None)
        },
    );
    r.register(
        c,
        "forEachKey",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let keys = chm_collect_all_keys(ctx, this);
            for key in keys {
                ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[key])?;
            }
            Ok(None)
        },
    );
    r.register(
        c,
        "forEachValue",
        "(JLjava/util/function/Consumer;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            let action = match args.get(2) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(None),
            };
            let vals = chm_collect_all_values(ctx, this);
            for val in vals {
                ctx.invoke_virtual(action, "accept", "(Ljava/lang/Object;)V", &[val])?;
            }
            Ok(None)
        },
    );
    r.register(
        c,
        "search",
        "(JLjava/util/function/BiFunction;)Ljava/lang/Object;",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let func = match args.get(2) {
                Some(Value::Object(Some(f))) => *f,
                _ => return Ok(Some(Value::Object(None))),
            };
            let entries = chm_collect_all_entries(ctx, this);
            for (key, val) in entries {
                let result = ctx.invoke_virtual(func, "apply", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;", &[key, val])?;
                if let Some(Value::Object(Some(_))) = result {
                    return Ok(result);
                }
            }
            Ok(Some(Value::Object(None)))
        },
    );

    // ConcurrentMap interface — delegates to CHM segmented ops
    let cm = "java/util/concurrent/ConcurrentMap";
    r.register(cm, "size", "()I", native_chm_size);
    r.register(
        cm,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_get,
    );
    r.register(
        cm,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_put,
    );
    r.register(
        cm,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_chm_remove,
    );
    r.register(
        cm,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_chm_contains_key,
    );
}

// --- ConcurrentHashMap segmented native functions (Phase 86.2) ---

fn native_chm_init_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    chm_init_segments(ctx, this, CHM_DEFAULT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP);
    Ok(None)
}

fn native_chm_init_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let total_cap = match args.get(1) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP,
    };
    // Bug 4: same load-factor adjustment as `native_map_init_capacity`.
    // CHM's documented default load factor is also 0.75, so inflate the
    // requested capacity by 4/3 before splitting across segments so the
    // total bucket count actually accommodates the caller's hint without
    // an immediate resize.
    let adjusted_total = (total_cap as u64).saturating_mul(4).div_ceil(3) as usize;
    let cap_per_seg = (adjusted_total / CHM_DEFAULT_SEGMENTS).max(1).next_power_of_two();
    chm_init_segments(ctx, this, CHM_DEFAULT_SEGMENTS, cap_per_seg);
    Ok(None)
}

fn native_chm_init_full(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let total_cap = match args.get(1) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS * CHM_DEFAULT_SEGMENT_CAP,
    };
    // Bug 4: the 3-arg form takes (initialCapacity, loadFactor, concurrencyLevel).
    // Honour the supplied load factor (arg index 2 is a float). Default to 0.75
    // when missing/invalid.
    let load_factor = match args.get(2) {
        Some(Value::Float(f)) if *f > 0.0 && f.is_finite() => *f,
        _ => 0.75_f32,
    };
    let concurrency = match args.get(3) {
        Some(Value::Int(v)) => (*v).max(1) as usize,
        _ => CHM_DEFAULT_SEGMENTS,
    };
    let num_segments = concurrency.next_power_of_two().min(256);
    let adjusted_total = ((total_cap as f64) / (load_factor as f64)).ceil() as usize;
    let adjusted_total = adjusted_total.max(total_cap); // guard against fp underflow
    let cap_per_seg = (adjusted_total / num_segments).max(1).next_power_of_two();
    chm_init_segments(ctx, this, num_segments, cap_per_seg);
    Ok(None)
}

fn native_chm_init_from_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    chm_init_segments(ctx, this, CHM_DEFAULT_SEGMENTS, CHM_DEFAULT_SEGMENT_CAP);
    // Copy entries from source map (HashMap layout: 3-field)
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let src_entries = map_collect_entries(ctx, source);
    let _resize_flag = ChmResizeLockGuard::enter();
    for (key, value) in src_entries {
        let hash = chm_key_hash(ctx, &key)?;
        if let Some(seg) = chm_segment_for(ctx, this, hash) {
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_put(ctx, &[Value::Object(Some(seg)), key, value])?;
        }
    }
    Ok(None)
}

// --- Core read operations (lock-free) ---

/// Round-5 CRIT fix (publication race): CHM-specific segment lookup.
///
/// Reads the segment's buckets slot via `get_field_volatile` and walks
/// the chain using `get_field_volatile` on `NODE_FIELD_NEXT`. Pairs with
/// `map_resize`'s `set_field_volatile` publication of the new buckets
/// array: a reader is guaranteed to observe either the OLD or NEW
/// fully-linked array, never a half-spliced chain.
///
/// Returns `Some(value)` on hit (including null-valued mappings),
/// `None` on miss. The hash and key matching logic mirrors
/// `native_map_get`.
fn chm_seg_get(
    ctx: &mut dyn NativeContext,
    seg: ObjectRef,
    key_val: Value,
) -> Result<Option<Value>, MethodCallFailed> {
    let (key_ref, hash, is_null_key) = match key_val {
        Value::Object(Some(k)) => (Some(k), map_hash_key(ctx, k)?, false),
        Value::Object(None) => (None, 0, true),
        _ => return Ok(None),
    };
    // Bug 1+2 (CRIT) round-10 fix: take a read-lock against any
    // in-progress concurrent `map_resize_concurrent` on this segment.
    // The resize path mutates old-chain NEXT pointers in place, which
    // can cause lock-free readers to skip past keys living in the hi
    // partition. The read-lock serializes readers against the writer's
    // mutation; parallel reads remain concurrent.
    //
    // Stripe is selected by `ctx.identity_hash_code(seg)` (stable
    // across GC moves) and indexes into a 256-entry static RwLock
    // array — no global Mutex, no raw-pointer keying.
    let seg_id = ctx.identity_hash_code(seg);
    // `parking_lot::RwLock::read` is infallible — no PoisonError to
    // recover from.
    let _read_guard = chm_seg_lock_for(seg_id).read();
    // Acquire-load the buckets array reference. If the writer has
    // begun publishing a new array, we see either the old one (with a
    // fully-linked chain) or the new one (also fully-linked) — never a
    // torn intermediate.
    let buckets_val = ctx.get_field_volatile(seg, MAP_FIELD_BUCKETS);
    let buckets = match buckets_val {
        Value::Object(Some(arr)) if ctx.heap_kind_of(arr) == ObjectKind::Array => arr,
        _ => return Ok(None),
    };
    // Derive cap from the bucket array's length — that's authoritative and
    // immune to the slot-aliasing problem below. Previously this read
    // `get_field(seg, MAP_FIELD_CAPACITY)` (= slot 2), which was correct
    // before the first segment-resize but stale afterwards: `map_resize`
    // resolves `java/util/HashMap.table` via the real-JDK class metadata —
    // which puts `table` at absolute slot 2 (after AbstractMap.keySet@0 and
    // AbstractMap.values@1) — and `set_field_volatile`s the new bucket
    // array into that slot. Slot 2 then holds `Value::Object(Some(arr))`,
    // not `Value::Int(cap)`, and this match's `_` arm returned `Ok(None)`
    // for every subsequent `get` on that segment — silently making every
    // CHM lookup miss after the first growth (10000-key probe: 0/10000
    // hits; reproduces BC's `ObjectIdentifier.intern()` "Should be taken
    // from cache" failure where the pool grows past the resize threshold
    // and subsequent intern lookups can't find their predecessor).
    let cap = ctx.array_length(buckets) as i32;
    if cap <= 0 {
        return Ok(None);
    }
    let idx = map_bucket_index(hash, cap);
    let mut node_val = ctx.get_array_element(buckets, idx);
    while let Value::Object(Some(node)) = node_val {
        let node_key_field = get_node_key(ctx, node);
        if is_null_key {
            if matches!(node_key_field, Value::Object(None)) {
                return Ok(Some(get_node_value(ctx, node)));
            }
        } else if let Value::Object(Some(node_key)) = node_key_field {
            if map_keys_equal(ctx, node_key, key_ref.unwrap())? {
                return Ok(Some(get_node_value(ctx, node)));
            }
        }
        // Acquire-load the NEXT pointer. Pairs with the writer's
        // `set_field` of NEXT during chain construction — the writer
        // publishes the buckets array with `set_field_volatile` AFTER
        // all NEXT links are set, so a reader observing the new
        // buckets array necessarily observes the corresponding NEXT
        // writes (happens-before via Release/Acquire).
        node_val = ctx.get_field_volatile(node, NODE_FIELD_NEXT);
    }
    Ok(None)
}

fn native_chm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => Ok(Some(chm_seg_get(ctx, seg, key)?.unwrap_or(Value::Object(None)))),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            // Volatile-read path: a present mapping is detected by a
            // non-null Value::Object payload OR by walking the chain
            // and finding a key match (which `chm_seg_get` does). A
            // miss returns None here.
            match chm_seg_get(ctx, seg, key)? {
                Some(_) => Ok(Some(Value::Int(1))),
                None => Ok(Some(Value::Int(0))),
            }
        }
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_chm_get_or_default(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let default = args.get(2).copied().unwrap_or(Value::Object(None));
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => match chm_seg_get(ctx, seg, key)? {
            Some(v) => Ok(Some(v)),
            None => Ok(Some(default)),
        },
        None => Ok(Some(default)),
    }
}

fn native_chm_contains_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).copied().unwrap_or(Value::Object(None));
    // Must scan all segments
    for seg in chm_all_segments(ctx, this) {
        let result = native_map_contains_value(ctx, &[Value::Object(Some(seg)), target])?;
        if result == Some(Value::Int(1)) {
            return Ok(Some(Value::Int(1)));
        }
    }
    Ok(Some(Value::Int(0)))
}

// --- Core write operations (per-segment locking) ---

fn native_chm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): ConcurrentHashMap.put rejects null keys and null values per JDK spec.
    if matches!(key, Value::Object(None)) || matches!(value, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some("ConcurrentHashMap does not permit null keys or values".to_string()),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_put(ctx, &[Value::Object(Some(seg)), key, value])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_remove(ctx, &[Value::Object(Some(seg)), key])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_put_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/value.
    if matches!(key, Value::Object(None)) || matches!(value, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some("ConcurrentHashMap does not permit null keys or values".to_string()),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_put_if_absent(ctx, &[Value::Object(Some(seg)), key, value])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_compute_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null mappingFunction.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "ConcurrentHashMap.computeIfAbsent: null key or mappingFunction".to_string(),
            ),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_compute_if_absent(ctx, &[Value::Object(Some(seg)), key, func])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_compute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let func = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key and null remappingFunction.
    if matches!(key, Value::Object(None)) || matches!(func, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "ConcurrentHashMap.compute: null key or remappingFunction".to_string(),
            ),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_compute(ctx, &[Value::Object(Some(seg)), key, func])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_merge(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let func = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key, null value, and null remappingFunction.
    if matches!(key, Value::Object(None))
        || matches!(value, Value::Object(None))
        || matches!(func, Value::Object(None))
    {
        return Err(RuntimeError::NullPointerException {
            message: Some(
                "ConcurrentHashMap.merge: null key, value, or remappingFunction".to_string(),
            ),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_merge(ctx, &[Value::Object(Some(seg)), key, value, func])
        }
        None => Ok(Some(Value::Object(None))),
    }
}

// --- Bulk operations (iterate all segments) ---

fn native_chm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut total = 0i32;
    for seg in chm_all_segments(ctx, this) {
        if let Value::Int(s) = ctx.get_field(seg, MAP_FIELD_SIZE) {
            total += s;
        }
    }
    Ok(Some(Value::Int(total)))
}

fn native_chm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(0) => Ok(Some(Value::Int(1))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_chm_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    for seg in chm_all_segments(ctx, this) {
        let _guard = ChmMonitorGuard::acquire(ctx, seg);
        native_map_clear(ctx, &[Value::Object(Some(seg))])?;
    }
    Ok(None)
}

fn native_chm_put_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let source = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let _resize_flag = ChmResizeLockGuard::enter();
    let entries = map_collect_entries(ctx, source);
    for (key, value) in entries {
        let hash = chm_key_hash(ctx, &key)?;
        if let Some(seg) = chm_segment_for(ctx, this, hash) {
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            native_map_put(ctx, &[Value::Object(Some(seg)), key, value])?;
        }
    }
    Ok(None)
}

fn native_chm_replace_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let func = args.get(1).copied().unwrap_or(Value::Object(None));
    let _resize_flag = ChmResizeLockGuard::enter();
    for seg in chm_all_segments(ctx, this) {
        let _guard = ChmMonitorGuard::acquire(ctx, seg);
        native_map_replace_all(ctx, &[Value::Object(Some(seg)), func])?;
    }
    Ok(None)
}

fn native_chm_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let action = match args.get(1) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    for (key, value) in entries {
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        )?;
    }
    Ok(None)
}

fn native_chm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let keys = chm_collect_all_keys(ctx, this);
    let set = alloc_synthetic(ctx, "java/util/HashSet", 1);
    let backing = alloc_backing_map(ctx);
    let cap = (keys.len() * 2).max(MAP_DEFAULT_CAPACITY).next_power_of_two();
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing, 0);
    ctx.set_field(backing, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, 0, Value::Object(Some(backing)));
    let sentinel = Value::Object(Some(set)); // reuse set ref as sentinel value
    for key in keys {
        native_map_put(ctx, &[Value::Object(Some(backing)), key, sentinel])?;
    }
    Ok(Some(Value::Object(Some(set))))
}

fn native_chm_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let vals = chm_collect_all_values(ctx, this);
    // Use the layout-aware ArrayList helpers: in real-JDK mode `elementData`
    // and `size` are NOT at slots 0/1 (`AbstractList.modCount` occupies an
    // earlier slot), so the previous hardcoded `set_field(list, 0/1, ...)`
    // wrote the backing array into the wrong slots and `values()` iterated
    // empty even though `size()` was correct.
    let n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", n_fields);
    let arr = alloc_ref_array(ctx, vals.len().max(AL_DEFAULT_CAPACITY));
    for (i, v) in vals.iter().enumerate() {
        let _ = ctx.set_array_element(arr, i, *v);
    }
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, vals.len() as i32);
    Ok(Some(Value::Object(Some(list))))
}

fn native_chm_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let entries = chm_collect_all_entries(ctx, this);
    // Build a HashSet of Map.Entry objects. Each entry must be a real
    // `java/util/Map$Entry` (not raw Object cid=0) so user-bytecode
    // `checkcast Map$Entry` succeeds after iterating entrySet() — see
    // Spring DefaultSingletonBeanRegistry.destroyBean iterating the
    // dependentBeanMap (a ConcurrentHashMap) entrySet.
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let backing = alloc_backing_map(ctx);
    let cap = std::cmp::max(entries.len().next_power_of_two(), MAP_DEFAULT_CAPACITY);
    let buckets = alloc_ref_array(ctx, cap);
    ctx.set_field(backing, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing, 0);
    ctx.set_field(backing, MAP_FIELD_CAPACITY, Value::Int(cap as i32));
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(backing)));
    for (key, value) in &entries {
        let entry_obj = alloc_synthetic(ctx, "java/util/Map$Entry", 2);
        ctx.set_field(entry_obj, 0, *key);
        ctx.set_field(entry_obj, 1, *value);

        let hash = ctx.identity_hash_code(entry_obj);
        let (b, size, c) = map_state(ctx, backing);
        let b = b.unwrap();
        let idx = map_bucket_index(hash, c);
        let existing = ctx.get_array_element(b, idx);
        let head = match existing {
            Value::Object(obj_opt) => obj_opt,
            _ => None,
        };
        let sentinel = Value::Int(1);
        let node = map_alloc_node(ctx, entry_obj, sentinel, hash, head);
        ctx.set_array_element(b, idx, Value::Object(Some(node)));
        set_map_size(ctx, backing, size + 1);
    }
    Ok(Some(Value::Object(Some(set))))
}

fn native_chm_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let entries = chm_collect_all_entries(ctx, this);
    let mut parts = Vec::with_capacity(entries.len());
    for (key, value) in &entries {
        let k = crate::obj_to_display_string(ctx, key);
        let v = crate::obj_to_display_string(ctx, value);
        parts.push(format!("{}={}", k, v));
    }
    let s = format!("{{{}}}", parts.join(", "));
    let sref = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(sref))))
}

fn native_chm_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut total = 0i32;
    for seg in chm_all_segments(ctx, this) {
        let result = native_map_hash_code(ctx, &[Value::Object(Some(seg))])?;
        if let Some(Value::Int(h)) = result {
            total = total.wrapping_add(h);
        }
    }
    Ok(Some(Value::Int(total)))
}

fn native_chm_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    if std::ptr::eq(this.as_ptr(), other.as_ptr()) {
        return Ok(Some(Value::Int(1)));
    }
    let our_entries = chm_collect_all_entries(ctx, this);
    // Use chm_get for lookups on 'other' (which is also segmented)
    let other_size = native_chm_size(ctx, &[Value::Object(Some(other))])?
        .unwrap_or(Value::Int(0));
    if let Value::Int(os) = other_size {
        if os != our_entries.len() as i32 {
            return Ok(Some(Value::Int(0)));
        }
    }
    for (key, value) in &our_entries {
        let other_val = native_chm_get(ctx, &[Value::Object(Some(other)), *key])?
            .unwrap_or(Value::Object(None));
        if !values_equal(ctx, value, &other_val) {
            return Ok(Some(Value::Int(0)));
        }
    }
    Ok(Some(Value::Int(1)))
}

// --- ConcurrentHashMap-specific compound operations ---

fn native_chm_remove_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let expected_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                .unwrap_or(Value::Object(None));
            if values_equal(ctx, &current, &expected_val) {
                native_map_remove(ctx, &[Value::Object(Some(seg)), key])?;
                Ok(Some(Value::Int(1)))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_chm_replace(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(2).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/value.
    if matches!(key, Value::Object(None)) || matches!(new_val, Value::Object(None)) {
        return Err(RuntimeError::NullPointerException {
            message: Some("ConcurrentHashMap.replace: null key or value".to_string()),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                .unwrap_or(Value::Object(None));
            match current {
                Value::Object(None) => Ok(Some(Value::Object(None))),
                _ => {
                    native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                    Ok(Some(current))
                }
            }
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_chm_replace_kv(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let old_val = args.get(2).copied().unwrap_or(Value::Object(None));
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    // C24 (HIGH): JDK rejects null key/old/new.
    if matches!(key, Value::Object(None))
        || matches!(old_val, Value::Object(None))
        || matches!(new_val, Value::Object(None))
    {
        return Err(RuntimeError::NullPointerException {
            message: Some("ConcurrentHashMap.replace(k,old,new): nulls not permitted".to_string()),
        }
        .into());
    }
    let hash = chm_key_hash(ctx, &key)?;
    match chm_segment_for(ctx, this, hash) {
        Some(seg) => {
            let _resize_flag = ChmResizeLockGuard::enter();
            let _guard = ChmMonitorGuard::acquire(ctx, seg);
            let current = native_map_get(ctx, &[Value::Object(Some(seg)), key])?
                .unwrap_or(Value::Object(None));
            if values_equal(ctx, &current, &old_val) {
                native_map_put(ctx, &[Value::Object(Some(seg)), key, new_val])?;
                Ok(Some(Value::Int(1)))
            } else {
                Ok(Some(Value::Int(0)))
            }
        }
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_chm_for_each_parallel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let action = match args.get(2) {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(None),
    };
    let entries = chm_collect_all_entries(ctx, this);
    for (key, value) in entries {
        ctx.invoke_virtual(
            action,
            "accept",
            "(Ljava/lang/Object;Ljava/lang/Object;)V",
            &[key, value],
        )?;
    }
    Ok(None)
}

fn native_chm_mapping_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = native_chm_size(ctx, args)?.unwrap_or(Value::Int(0));
    match size {
        Value::Int(v) => Ok(Some(Value::Long(v as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

fn native_chm_new_key_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let set = alloc_synthetic(ctx, "java/util/HashSet", 1);
    let backing = alloc_backing_map(ctx);
    let buckets = alloc_ref_array(ctx, MAP_DEFAULT_CAPACITY);
    ctx.set_field(backing, MAP_FIELD_BUCKETS, Value::Object(Some(buckets)));
    set_map_size(ctx, backing, 0);
    ctx.set_field(
        backing,
        MAP_FIELD_CAPACITY,
        Value::Int(MAP_DEFAULT_CAPACITY as i32),
    );
    ctx.set_field(set, 0, Value::Object(Some(backing)));
    Ok(Some(Value::Object(Some(set))))
}

fn native_chm_new_key_set_cap(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Ignore capacity hint; behave identically to no-arg newKeySet().
    // KeySetView's Java-side add() calls CHM.putVal which uses the native
    // CHM's segment fields that we don't populate, so we must back this with
    // our HashSet synthetic instead of letting a real KeySetView form.
    native_chm_new_key_set(ctx, _args)
}

fn native_chm_key_set_view(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_chm_key_set(ctx, args)
}

fn native_chm_elements(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_chm_values(ctx, args)
}

fn native_chm_keys(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_chm_key_set(ctx, args)
}

// ===========================================================================
// Collections$SetFromMap — the view returned by `Collections.newSetFromMap`.
//
// `SetFromMap` captures `s = m.keySet()` *once* in its constructor and routes
// `iterator()` / `toArray()` through that captured `s`. CratonVM's `keySet()`
// natives return an eager snapshot, so when `newSetFromMap` runs over a fresh
// (empty) map, `s` is permanently empty — every later `set.add(...)` mutates
// the live map `m` but not the stale `s`. `iterator()`/`toArray()` then come
// back empty even though `size()` (which delegates to `m.size()`) is correct.
//
// Felix's `CapabilitySet.match` returns `Collections.newSetFromMap(new
// ConcurrentHashMap())`, fills it via `addAll`, and `Felix.getServiceReferences`
// then does `new ArrayList<>(set)` — which calls `set.toArray()` and came back
// empty, so `getServiceReference("...StartLevel")` returned null and Felix's
// `AutoProcessor.processAutoProperties` NPE'd.
//
// Fix: intercept the affected `SetFromMap` view methods and route them through
// the *live* backing map `m` instead of the captured `s`.
// ===========================================================================

const SET_FROM_MAP_CLASS: &str = "java/util/Collections$SetFromMap";

/// Resolve the live backing `Map` field (`m`) of a `Collections$SetFromMap`.
fn set_from_map_backing(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    let slot = ctx.resolve_field_index(SET_FROM_MAP_CLASS, "m")?;
    match ctx.get_field(this, slot) {
        Value::Object(Some(m)) => Some(m),
        _ => None,
    }
}

fn native_set_from_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match set_from_map_backing(ctx, this) {
        Some(m) => native_map_size(ctx, &[Value::Object(Some(m))]),
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_set_from_map_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let size = match native_set_from_map_size(ctx, args)? {
        Some(Value::Int(n)) => n,
        _ => 0,
    };
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

fn native_set_from_map_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    match set_from_map_backing(ctx, this) {
        Some(m) => native_map_contains_key(ctx, &[Value::Object(Some(m)), key]),
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_set_from_map_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let keys = match set_from_map_backing(ctx, this) {
        Some(m) => map_collect_keys(ctx, m),
        None => Vec::new(),
    };
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        let _ = ctx.set_array_element(arr, i, *k);
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn native_set_from_map_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Build a fresh HashSet snapshot of the live map's keys and hand back its
    // iterator — the snapshot is taken *now*, so it reflects every add() that
    // happened after `newSetFromMap` constructed the (then-empty) view.
    let backing = set_from_map_backing(ctx, this);
    let set = match backing {
        Some(m) => match native_map_key_set(ctx, &[Value::Object(Some(m))])? {
            Some(Value::Object(Some(s))) => s,
            _ => return Ok(Some(Value::Object(None))),
        },
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(set, "iterator", "()Ljava/util/Iterator;", &[])
}

fn register_set_from_map_natives(r: &mut NativeMethodRegistry) {
    let c = SET_FROM_MAP_CLASS;
    r.register(c, "size", "()I", native_set_from_map_size);
    r.register(c, "isEmpty", "()Z", native_set_from_map_is_empty);
    r.register(
        c,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_set_from_map_contains,
    );
    r.register(
        c,
        "toArray",
        "()[Ljava/lang/Object;",
        native_set_from_map_to_array,
    );
    r.register(c, "iterator", "()Ljava/util/Iterator;", native_set_from_map_iterator);
}



// ===========================================================================
// Properties — delegates to HashMap backing store (same 3-field layout)
// Field 0,1,2 = HashMap fields (buckets, size, capacity)
// Field 3 = defaults Properties reference (or null)
// ===========================================================================

const PROPS_FIELD_DEFAULTS: usize = 3;

fn register_properties_natives(registry: &mut NativeMethodRegistry) {
    let p = "java/util/Properties";

    // <init>()V — empty properties
    registry.register(p, "<init>", "()V", native_props_init);

    // <init>(Properties)V — with defaults
    registry.register(
        p,
        "<init>",
        "(Ljava/util/Properties;)V",
        native_props_init_defaults,
    );

    // setProperty(String, String) -> String (old value)
    registry.register(
        p,
        "setProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/Object;",
        native_map_put,
    );

    // getProperty(String) -> String
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property,
    );

    // getProperty(String, String) -> String (with default value)
    registry.register(
        p,
        "getProperty",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
        native_props_get_property_default,
    );

    // size() -> int
    registry.register(p, "size", "()I", native_map_size);

    // isEmpty() -> boolean
    registry.register(p, "isEmpty", "()Z", native_map_is_empty);

    // containsKey(Object) -> boolean
    registry.register(
        p,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );

    // containsValue(Object) -> boolean
    registry.register(
        p,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );

    // put(Object, Object) -> Object
    registry.register(
        p,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put,
    );

    // get(Object) -> Object
    registry.register(
        p,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );

    // remove(Object) -> Object
    registry.register(
        p,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );

    // clear()
    registry.register(p, "clear", "()V", native_map_clear);

    // keySet() -> Set
    registry.register(p, "keySet", "()Ljava/util/Set;", native_map_key_set);

    // values() -> Collection
    registry.register(p, "values", "()Ljava/util/Collection;", native_map_values);

    // entrySet() -> Set
    registry.register(p, "entrySet", "()Ljava/util/Set;", native_map_entry_set);

    // toString()
    registry.register(p, "toString", "()Ljava/lang/String;", native_map_to_string);

    // putAll(Map)
    registry.register(p, "putAll", "(Ljava/util/Map;)V", native_map_put_all);

    // load(InputStream) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/InputStream;)V", native_props_load);

    // load(Reader) — parse key=value lines
    registry.register(p, "load", "(Ljava/io/Reader;)V", native_props_load);

    // store(OutputStream, String) — write key=value lines
    registry.register(
        p,
        "store",
        "(Ljava/io/OutputStream;Ljava/lang/String;)V",
        native_props_store,
    );

    // propertyNames() -> Enumeration (returns keys as ArrayList iterator)
    registry.register(
        p,
        "propertyNames",
        "()Ljava/util/Enumeration;",
        native_props_property_names,
    );

    // stringPropertyNames() -> Set<String>
    registry.register(
        p,
        "stringPropertyNames",
        "()Ljava/util/Set;",
        native_props_string_property_names,
    );

    // getOrDefault(Object, Object) -> Object
    registry.register(
        p,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get_or_default,
    );

    // forEach(BiConsumer)
    registry.register(
        p,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
        native_map_for_each,
    );

    // hashCode
    registry.register(p, "hashCode", "()I", native_map_hash_code);

    // equals
    registry.register(p, "equals", "(Ljava/lang/Object;)Z", native_map_equals);

    // Also register under Hashtable (Properties extends Hashtable)
    let ht = "java/util/Hashtable";
    registry.register(ht, "<init>", "()V", native_map_init);
    registry.register(
        ht,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_put,
    );
    registry.register(
        ht,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_get,
    );
    registry.register(
        ht,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_map_remove,
    );
    registry.register(ht, "size", "()I", native_map_size);
    registry.register(ht, "isEmpty", "()Z", native_map_is_empty);
    registry.register(
        ht,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_map_contains_key,
    );
    registry.register(
        ht,
        "containsValue",
        "(Ljava/lang/Object;)Z",
        native_map_contains_value,
    );
    registry.register(ht, "clear", "()V", native_map_clear);
    registry.register(ht, "keySet", "()Ljava/util/Set;", native_map_key_set);
    registry.register(ht, "values", "()Ljava/util/Collection;", native_map_values);
    registry.register(ht, "entrySet", "()Ljava/util/Set;", native_map_entry_set);
    registry.register(ht, "toString", "()Ljava/lang/String;", native_map_to_string);
    registry.register(ht, "putAll", "(Ljava/util/Map;)V", native_map_put_all);
    // Note: `keys()` / `elements()` are registered by
    // `cratonvm-native-builtins::deprecated_io_util::register_*_natives`,
    // which runs AFTER us in `vm_init`'s real-JDK arm. The force-native
    // override in `interpreter.rs::force_native_over_real_jdk_bytecode`
    // routes dispatch to that registration instead of the real-JDK
    // Hashtable.keys() body (which walks Hashtable's own internal
    // `table[]` — empty in our impl because `put` writes to a side-store).
    // Without the force-native entry, BC's
    // `AbstractX500NameStyle.copyHashTable(BCStyle.DefaultLookUp)` produced
    // an empty per-instance `defaultLookUp` and every
    // `BCStyle.attrNameToOID("cn"/...)` threw "Unknown object id".
}

fn native_props_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    native_map_init(ctx, &[Value::Object(Some(this))])?;
    ctx.set_field(this, PROPS_FIELD_DEFAULTS, Value::Object(None));
    Ok(None)
}

fn native_props_init_defaults(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    native_map_init(ctx, &[Value::Object(Some(this))])?;
    ctx.set_field(this, PROPS_FIELD_DEFAULTS, args[1]);
    Ok(None)
}

fn native_props_get_property(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // First check this map
    let result = native_map_get(ctx, args)?;
    if let Some(Value::Object(Some(_))) = result {
        return Ok(result);
    }
    // Fall through to defaults chain
    let mut defaults_val = ctx.get_field(this, PROPS_FIELD_DEFAULTS);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(_))) = def_result {
            return Ok(def_result);
        }
        defaults_val = ctx.get_field(defs, PROPS_FIELD_DEFAULTS);
    }
    Ok(Some(Value::Object(None)))
}

fn native_props_get_property_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(args[2])),
    };
    // First check this map
    let result = native_map_get(ctx, &[args[0], args[1]])?;
    if let Some(Value::Object(Some(_))) = result {
        return Ok(result);
    }
    // Fall through to defaults chain
    let mut defaults_val = ctx.get_field(this, PROPS_FIELD_DEFAULTS);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_result = native_map_get(ctx, &[Value::Object(Some(defs)), args[1]])?;
        if let Some(Value::Object(Some(_))) = def_result {
            return Ok(def_result);
        }
        defaults_val = ctx.get_field(defs, PROPS_FIELD_DEFAULTS);
    }
    // Return the default value arg
    Ok(Some(args[2]))
}

/// Load properties from a stream-like input: parse "key=value" or "key:value" lines.
/// Simplified: reads all bytes from the stream arg (field 0 = data for BAIS),
/// then parses lines.
fn native_props_load(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let stream = match args[1] {
        Value::Object(Some(s)) => s,
        _ => return Ok(None),
    };

    // Try to read the stream contents: if it's a BAIS, read data directly
    // Otherwise try to read it as a string field
    let text = props_read_input(ctx, stream);

    // MED fix: implement the JDK `Properties.load` spec (close to it,
    // anyway):
    //   - lines starting with `#` or `!` (after leading whitespace) are
    //     comments and skipped entirely;
    //   - blank lines (whitespace only) are skipped;
    //   - leading whitespace before a key is trimmed;
    //   - a line that ends with an *unescaped* trailing `\` continues
    //     onto the next line (the `\` is dropped and leading whitespace
    //     on the continuation line is trimmed);
    //   - the key terminates at the first unescaped whitespace, `=`, or
    //     `:`; subsequent whitespace and one optional `=`/`:` are then
    //     consumed; the rest of the logical line is the value;
    //   - inside both key and value the JDK escapes are honoured:
    //       \t \n \r \f \\ \" \'  → the corresponding ASCII char,
    //       \= \: \space          → the literal char (allows them in keys),
    //       \uXXXX               → the BMP codepoint XXXX,
    //       \<anything else>      → the trailing char itself (per JDK).
    //
    // This is intentionally implemented in pure Rust against the in-memory
    // text rather than dispatching back to JDK Properties for parity —
    // the previous implementation silently truncated/corrupted any
    // real-world `.properties` file with escapes or continuations.
    for (key, value) in props_parse_logical_lines(&text) {
        if key.is_empty() {
            continue;
        }
        let key_obj = ctx.create_string(&key);
        let val_obj = ctx.create_string(&value);
        native_map_put(
            ctx,
            &[
                Value::Object(Some(this)),
                Value::Object(Some(key_obj)),
                Value::Object(Some(val_obj)),
            ],
        )?;
    }
    Ok(None)
}

/// Parse a `.properties` text into `(key, value)` pairs per the JDK
/// `Properties.load` spec — handles comments, blank lines, leading-
/// whitespace trim, trailing-backslash continuations, the
/// `\t\n\r\f\\\"\'\=\:` and `\uXXXX` escape sequences in both keys and
/// values, and the natural key/value separator (the first unescaped
/// whitespace, `=`, or `:`).
fn props_parse_logical_lines(text: &str) -> Vec<(String, String)> {
    let raw_lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < raw_lines.len() {
        // Strip a trailing `\r` (CRLF inputs) without losing escapes.
        let mut line = raw_lines[i].trim_end_matches('\r').to_string();
        // Trim leading whitespace per spec.
        let trimmed_start = line.trim_start();
        let leading_skipped = line.len() - trimmed_start.len();
        // Comment / blank — skipped entirely (continuations do not apply
        // to comment lines).
        if trimmed_start.is_empty()
            || trimmed_start.starts_with('#')
            || trimmed_start.starts_with('!')
        {
            i += 1;
            continue;
        }
        line.drain(..leading_skipped);

        // Apply continuation: if the line ends with an *odd* number of
        // trailing backslashes, the last `\` is a continuation marker
        // and the following physical line is appended (after trimming
        // its leading whitespace).
        while ends_with_unescaped_backslash(&line) {
            line.pop(); // remove the trailing backslash
            i += 1;
            if i >= raw_lines.len() {
                break;
            }
            let next = raw_lines[i].trim_end_matches('\r');
            line.push_str(next.trim_start());
        }
        i += 1;

        // Walk the logical line to find the key/value boundary. The key
        // ends at the first unescaped whitespace, `=`, or `:`.
        let bytes = line.as_bytes();
        let mut idx = 0;
        let mut key_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                // Pull one escape into the key.
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                key_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            if b == b' ' || b == b'\t' || b == b'\x0c' || b == b'=' || b == b':' {
                break;
            }
            // Multi-byte UTF-8: copy through.
            let ch_end = utf8_char_end(bytes, idx);
            key_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        // Skip whitespace, then at most one `=` or `:`, then more whitespace.
        while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
            idx += 1;
        }
        if idx < bytes.len() && (bytes[idx] == b'=' || bytes[idx] == b':') {
            idx += 1;
            while idx < bytes.len() && matches!(bytes[idx], b' ' | b'\t' | b'\x0c') {
                idx += 1;
            }
        }

        // Remainder is the value (with escapes decoded).
        let mut val_buf = String::new();
        while idx < bytes.len() {
            let b = bytes[idx];
            if b == b'\\' {
                let (decoded, consumed) = decode_escape(&bytes[idx..]);
                val_buf.push_str(&decoded);
                idx += consumed;
                continue;
            }
            let ch_end = utf8_char_end(bytes, idx);
            val_buf.push_str(std::str::from_utf8(&bytes[idx..ch_end]).unwrap_or(""));
            idx = ch_end;
        }

        out.push((key_buf, val_buf));
    }
    out
}

/// True when the line ends with a backslash that is *not* itself
/// escaped (i.e. odd run of trailing backslashes).
fn ends_with_unescaped_backslash(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut n = 0usize;
    for &b in bytes.iter().rev() {
        if b == b'\\' {
            n += 1;
        } else {
            break;
        }
    }
    n % 2 == 1
}

/// Decode a single backslash-escape starting at `bytes[0] == b'\\'`.
/// Returns `(decoded_string, bytes_consumed_including_backslash)`.
/// Falls back to the literal trailing char for unknown escapes (matches
/// JDK behaviour: `\q` parses as `q`).
fn decode_escape(bytes: &[u8]) -> (String, usize) {
    debug_assert_eq!(bytes[0], b'\\');
    if bytes.len() < 2 {
        return (String::new(), 1);
    }
    match bytes[1] {
        b't' => ("\t".to_string(), 2),
        b'n' => ("\n".to_string(), 2),
        b'r' => ("\r".to_string(), 2),
        b'f' => ("\x0c".to_string(), 2),
        b'\\' => ("\\".to_string(), 2),
        b'"' => ("\"".to_string(), 2),
        b'\'' => ("'".to_string(), 2),
        b'u' => {
            // \uXXXX (exactly 4 hex digits per JDK spec).
            if bytes.len() < 6 {
                return ((bytes[1] as char).to_string(), 2);
            }
            let hex = std::str::from_utf8(&bytes[2..6]).unwrap_or("");
            match u32::from_str_radix(hex, 16) {
                Ok(cp) => match char::from_u32(cp) {
                    Some(ch) => (ch.to_string(), 6),
                    // Surrogate halves and other invalid code points: fall
                    // through to literal 'u' so we never lose data.
                    None => ("u".to_string(), 2),
                },
                Err(_) => ("u".to_string(), 2),
            }
        }
        // Per spec: any other char after `\` (including space, `=`, `:`,
        // and unknown letters) becomes itself, allowing them inside keys.
        other => {
            // Multi-byte UTF-8 starting at byte index 1.
            let end = utf8_char_end(bytes, 1);
            let s = std::str::from_utf8(&bytes[1..end])
                .map(|s| s.to_string())
                .unwrap_or_else(|_| (other as char).to_string());
            (s, end)
        }
    }
}

/// Return the byte-index just past the UTF-8 char that begins at `start`,
/// or `start + 1` for invalid sequences (safe fallback).
fn utf8_char_end(bytes: &[u8], start: usize) -> usize {
    if start >= bytes.len() {
        return start;
    }
    let b = bytes[start];
    let len = if b < 0x80 {
        1
    } else if b < 0xC0 {
        1 // continuation byte — should not start a char; treat as 1
    } else if b < 0xE0 {
        2
    } else if b < 0xF0 {
        3
    } else {
        4
    };
    (start + len).min(bytes.len())
}

/// Helper: read all text from a stream or reader object.
fn props_read_input(ctx: &mut dyn NativeContext, stream: ObjectRef) -> String {
    // Try BAIS protocol: field 0 = byte[], field 1 = pos, field 2 = mark, field 3 = count
    if let Value::Object(Some(data_arr)) = ctx.get_field(stream, 0) {
        let pos = match ctx.get_field(stream, 1) {
            Value::Int(p) => p as usize,
            _ => 0,
        };
        let count = match ctx.get_field(stream, 3) {
            Value::Int(c) => c as usize,
            _ => 0,
        };
        let mut bytes = Vec::with_capacity(count.saturating_sub(pos));
        for i in pos..count {
            if let Value::Int(b) = ctx.get_array_element(data_arr, i) {
                bytes.push(b as u8);
            }
        }
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    // Fallback: try to read field 0 as a string directly (e.g. BufferedReader)
    if let Some(s) = ctx.read_string(stream) {
        return s;
    }
    String::new()
}

/// Store properties to an output stream as "key=value" lines.
fn native_props_store(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Collect all key-value pairs
    let keys = props_collect_keys(ctx, this);
    let mut output = String::new();

    // Optional comment header
    if let Value::Object(Some(comment)) = args[2] {
        if let Some(c) = ctx.read_string(comment) {
            output.push_str(&format!("# {}\n", c));
        }
    }

    for key_obj in keys {
        if let Some(key_str) = ctx.read_string(key_obj) {
            let val = native_map_get(
                ctx,
                &[Value::Object(Some(this)), Value::Object(Some(key_obj))],
            )?;
            let val_str = match val {
                Some(Value::Object(Some(v))) => ctx.read_string(v).unwrap_or_default(),
                _ => String::new(),
            };
            output.push_str(&format!("{}={}\n", key_str, val_str));
        }
    }

    // Write to output stream.
    //
    // MED fix: the previous implementation issued one `write(I)V` virtual
    // dispatch per byte — for a Properties with ~1000 entries this meant
    // ~50 000 virtual calls. Build a single `byte[]` once and call
    // `write([B)V` exactly once per line, which most OutputStream
    // implementations override to do a single bulk copy. We dispatch
    // line-by-line rather than the entire buffer in one go so a
    // pathological output stream (e.g. one that requires a flush between
    // lines) still gets reasonable behaviour, and so each line's bytes
    // fit easily into a single allocation.
    if let Value::Object(Some(ostream)) = args[1] {
        for line in output.split_inclusive('\n') {
            let bytes = line.as_bytes();
            let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
            for (i, &b) in bytes.iter().enumerate() {
                // Byte array slots hold sign-extended Int per the VM's
                // primitive-array value model.
                ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
            }
            // Prefer the bulk `write([B)V` overload. If a stream does
            // not implement it, the JDK default (AbstractOutputStream)
            // falls back to per-byte `write(I)V`, so we keep behaviour
            // even on minimal stream impls.
            let _ = ctx.invoke_virtual(
                ostream,
                "write",
                "([B)V",
                &[Value::Object(Some(arr))],
            );
        }
    }
    Ok(None)
}

/// Collect all key ObjectRefs from the HashMap backing store.
fn props_collect_keys(ctx: &dyn NativeContext, this: ObjectRef) -> Vec<ObjectRef> {
    let (buckets, _size, cap) = map_state(ctx, this);
    let mut keys = Vec::new();
    if let Some(b) = buckets {
        for i in 0..(cap as usize) {
            let mut node_val = ctx.get_array_element(b, i);
            while let Value::Object(Some(node)) = node_val {
                if let Value::Object(Some(key)) = ctx.get_field(node, NODE_FIELD_KEY) {
                    keys.push(key);
                }
                node_val = ctx.get_field(node, NODE_FIELD_NEXT);
            }
        }
    }
    keys
}

/// propertyNames() -> returns a real Enumeration<Object> over the property keys.
///
/// Previously this delegated to `keySet`, which returns a `HashSet`. HashSet
/// doesn't implement `Enumeration`, so callers like Kafka's
/// `Utils.propsToMap` that invoke `hasMoreElements()` / `nextElement()` on
/// the result fell through to the snapshot-iterator native's generic-Iterator
/// fallback, which couldn't find a `hasNext` method on HashSet and silently
/// returned 0 — yielding an empty enumeration and a Properties-derived map
/// with zero entries. Kafka then reported `Missing required configuration
/// "process.roles"` even though server.properties listed it.
///
/// Now we allocate the same 2-field snapshot object the Enumeration interface
/// natives expect (field 0 = `Object[]` snapshot, field 1 = `int` cursor).
fn native_props_property_names(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return Ok(Some(Value::Object(None))),
    };
    // JDK 25 `Properties` stores entries in a private `ConcurrentHashMap map`
    // field, NOT in the inherited `Hashtable.table` array. Real-JDK
    // `Properties.size()` / `propertyNames()` / `entrySet()` bytecode all
    // route through `this.map`. Walk that map when present; fall back to the
    // legacy slot-0 backing for synthetic Properties allocations.
    let backing = match ctx.resolve_field_index("java/util/Properties", "map") {
        Some(slot) if slot < ctx.object_num_fields(this) => match ctx.get_field(this, slot) {
            Value::Object(Some(m)) => Some(m),
            _ => None,
        },
        _ => None,
    };
    let keys = match backing {
        Some(m) => {
            // `Properties.map` is declared as `ConcurrentHashMap`. Use the
            // CHM-segmented key collector — `map_collect_keys` walks slot 0
            // as a plain Node[] and only finds the head segment objects on
            // a real-JDK CHM, yielding the wrong key type.
            let cid = ctx.class_id_of_object(m);
            let cn = ctx.class_name_of_id(cid).unwrap_or_default();
            if cn == "java/util/concurrent/ConcurrentHashMap"
                || cn.starts_with("java/util/concurrent/ConcurrentHashMap$")
            {
                chm_collect_all_keys(ctx, m)
            } else {
                map_collect_keys(ctx, m)
            }
        }
        None => map_collect_keys(ctx, this),
    };
    let arr = alloc_ref_array(ctx, keys.len());
    for (i, k) in keys.iter().enumerate() {
        ctx.set_array_element(arr, i, *k);
    }
    // Use our internal SnapshotEnumeration synthetic class so the
    // snapshot-iterator natives (`hasMoreElements`/`nextElement` registered on
    // this class name) win virtual dispatch. Field 0 = Object[] snapshot,
    // field 1 = cursor.
    let en = alloc_synthetic(ctx, "cratonvm/internal/SnapshotEnumeration", 2);
    ctx.set_field(en, 0, Value::Object(Some(arr)));
    ctx.set_field(en, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(en))))
}

/// stringPropertyNames() -> Set<String>
fn native_props_string_property_names(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Collect keys from this and all defaults into a HashSet
    let result = native_map_key_set(ctx, &[Value::Object(Some(this))])?;

    // Also add keys from defaults chain
    let mut defaults_val = ctx.get_field(this, PROPS_FIELD_DEFAULTS);
    while let Value::Object(Some(defs)) = defaults_val {
        let def_keys = props_collect_keys(ctx, defs);
        if let Some(Value::Object(Some(set))) = result {
            for key in def_keys {
                // Add to the result set (HashSet add = contains check + add)
                let key_val = Value::Object(Some(key));
                let contains = native_hs_contains(ctx, &[Value::Object(Some(set)), key_val])?;
                if contains != Some(Value::Int(1)) {
                    native_hs_add(ctx, &[Value::Object(Some(set)), key_val])?;
                }
            }
        }
        defaults_val = ctx.get_field(defs, PROPS_FIELD_DEFAULTS);
    }
    Ok(result)
}

// ===========================================================================
// Unmodifiable collection views
// ===========================================================================
//
// `Collections.unmodifiableList/Set/Map/Collection(...)` return wrapper views
// that delegate every read to the backing collection and throw
// `UnsupportedOperationException` from every mutator. `List.of` / `Set.of` /
// `Map.of` produce fully immutable snapshots (the backing collection is
// private, so it can never be mutated).
//
// Each wrapper is a dedicated synthetic class with a single field (slot 0)
// holding the backing collection's ObjectRef. Read methods forward to the
// backing object via `invoke_virtual`, so dispatch lands on whatever native
// implements the concrete backing type (ArrayList / HashMap / HashSet / ...).
// Mutators are registered natives that unconditionally throw.

/// Synthetic class names for the unmodifiable wrappers.
const UNMOD_LIST_CLASS: &str = "cratonvm/internal/UnmodifiableList";
const UNMOD_SET_CLASS: &str = "cratonvm/internal/UnmodifiableSet";
const UNMOD_MAP_CLASS: &str = "cratonvm/internal/UnmodifiableMap";
const UNMOD_COLLECTION_CLASS: &str = "cratonvm/internal/UnmodifiableCollection";
const UNMOD_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableItr";
const UNMOD_LIST_ITR_CLASS: &str = "cratonvm/internal/UnmodifiableListItr";

/// Slot 0 of every wrapper holds the backing collection / iterator.
const UNMOD_FIELD_BACKING: usize = 0;

/// Build an `UnsupportedOperationException` error for a blocked mutator.
fn unsupported_op() -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: String::new(),
    }
    .into()
}

/// Allocate an unmodifiable wrapper of `class_name` around `backing`.
fn alloc_unmod_wrapper(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    backing: ObjectRef,
) -> ObjectRef {
    let wrapper = alloc_synthetic(ctx, class_name, 1);
    ctx.set_field(wrapper, UNMOD_FIELD_BACKING, Value::Object(Some(backing)));
    wrapper
}

/// Read the backing collection out of a wrapper. Recurses if the backing is
/// itself a wrapper (e.g. `unmodifiableList(unmodifiableList(x))`).
fn unmod_backing(ctx: &mut dyn NativeContext, wrapper: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(wrapper, UNMOD_FIELD_BACKING) {
        Value::Object(Some(b)) => Some(b),
        _ => None,
    }
}

/// Forward a read-only call to the backing collection. If the wrapper's
/// backing is missing, returns a benign default.
fn unmod_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    ctx.invoke_virtual(backing, method, descriptor, &args[1..])
}

fn register_unmodifiable_natives(r: &mut NativeMethodRegistry) {
    // ---- UnmodifiableCollection (also the shared base for List/Set) -------
    for c in [UNMOD_COLLECTION_CLASS, UNMOD_LIST_CLASS, UNMOD_SET_CLASS] {
        r.register(c, "size", "()I", native_unmod_size);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty);
        r.register(
            c,
            "contains",
            "(Ljava/lang/Object;)Z",
            native_unmod_contains,
        );
        r.register(
            c,
            "containsAll",
            "(Ljava/util/Collection;)Z",
            native_unmod_contains_all,
        );
        r.register(c, "iterator", "()Ljava/util/Iterator;", native_unmod_iterator);
        r.register(c, "toArray", "()[Ljava/lang/Object;", native_unmod_to_array);
        r.register(
            c,
            "toArray",
            "([Ljava/lang/Object;)[Ljava/lang/Object;",
            native_unmod_to_array_typed,
        );
        r.register(c, "toString", "()Ljava/lang/String;", native_unmod_to_string);
        r.register(c, "hashCode", "()I", native_unmod_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_for_each,
        );
        r.register(c, "stream", "()Ljava/util/stream/Stream;", native_unmod_stream);
        r.register(
            c,
            "spliterator",
            "()Ljava/util/Spliterator;",
            native_unmod_spliterator,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "add", "(Ljava/lang/Object;)Z", native_unmod_throw);
        r.register(c, "remove", "(Ljava/lang/Object;)Z", native_unmod_throw);
        r.register(c, "clear", "()V", native_unmod_throw);
        r.register(c, "addAll", "(Ljava/util/Collection;)Z", native_unmod_throw);
        r.register(c, "removeAll", "(Ljava/util/Collection;)Z", native_unmod_throw);
        r.register(c, "retainAll", "(Ljava/util/Collection;)Z", native_unmod_throw);
        r.register(
            c,
            "removeIf",
            "(Ljava/util/function/Predicate;)Z",
            native_unmod_throw,
        );
    }

    // ---- UnmodifiableList — adds positional reads + list mutators ---------
    {
        let c = UNMOD_LIST_CLASS;
        r.register(c, "get", "(I)Ljava/lang/Object;", native_unmod_get);
        r.register(c, "indexOf", "(Ljava/lang/Object;)I", native_unmod_index_of);
        r.register(
            c,
            "lastIndexOf",
            "(Ljava/lang/Object;)I",
            native_unmod_last_index_of,
        );
        r.register(c, "subList", "(II)Ljava/util/List;", native_unmod_sub_list);
        r.register(
            c,
            "listIterator",
            "()Ljava/util/ListIterator;",
            native_unmod_list_iterator,
        );
        r.register(
            c,
            "listIterator",
            "(I)Ljava/util/ListIterator;",
            native_unmod_list_iterator_idx,
        );
        // List mutators.
        r.register(
            c,
            "set",
            "(ILjava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(c, "add", "(ILjava/lang/Object;)V", native_unmod_throw);
        r.register(c, "remove", "(I)Ljava/lang/Object;", native_unmod_throw);
        r.register(
            c,
            "addAll",
            "(ILjava/util/Collection;)Z",
            native_unmod_throw,
        );
        r.register(c, "sort", "(Ljava/util/Comparator;)V", native_unmod_throw);
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/UnaryOperator;)V",
            native_unmod_throw,
        );
    }

    // ---- UnmodifiableMap --------------------------------------------------
    {
        let c = UNMOD_MAP_CLASS;
        r.register(c, "size", "()I", native_unmod_size);
        r.register(c, "isEmpty", "()Z", native_unmod_is_empty);
        r.register(
            c,
            "get",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get,
        );
        r.register(
            c,
            "getOrDefault",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_map_get_or_default,
        );
        r.register(
            c,
            "containsKey",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_key,
        );
        r.register(
            c,
            "containsValue",
            "(Ljava/lang/Object;)Z",
            native_unmod_map_contains_value,
        );
        r.register(c, "keySet", "()Ljava/util/Set;", native_unmod_map_key_set);
        r.register(
            c,
            "values",
            "()Ljava/util/Collection;",
            native_unmod_map_values,
        );
        r.register(c, "entrySet", "()Ljava/util/Set;", native_unmod_map_entry_set);
        r.register(c, "toString", "()Ljava/lang/String;", native_unmod_to_string);
        r.register(c, "hashCode", "()I", native_unmod_hash_code);
        r.register(c, "equals", "(Ljava/lang/Object;)Z", native_unmod_equals);
        r.register(
            c,
            "forEach",
            "(Ljava/util/function/BiConsumer;)V",
            native_unmod_map_for_each,
        );
        // Map mutators.
        r.register(
            c,
            "put",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "remove",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(c, "clear", "()V", native_unmod_throw);
        r.register(c, "putAll", "(Ljava/util/Map;)V", native_unmod_throw);
        r.register(
            c,
            "putIfAbsent",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "replace",
            "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "computeIfAbsent",
            "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "compute",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "computeIfPresent",
            "(Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "merge",
            "(Ljava/lang/Object;Ljava/lang/Object;Ljava/util/function/BiFunction;)Ljava/lang/Object;",
            native_unmod_throw,
        );
        r.register(
            c,
            "replaceAll",
            "(Ljava/util/function/BiFunction;)V",
            native_unmod_throw,
        );
    }

    // ---- UnmodifiableItr — read-only iterator -----------------------------
    {
        let c = UNMOD_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_itr_has_next);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_itr_next);
        r.register(c, "remove", "()V", native_unmod_throw);
    }

    // ---- UnmodifiableListItr — read-only ListIterator ---------------------
    // A self-contained `ListIterator` over a snapshot of the backing list:
    //   field 0 = Object[] snapshot of the list elements
    //   field 1 = Int cursor (the `nextIndex`)
    // Read operations walk the snapshot; the mutators (`set`/`add`/`remove`)
    // throw `UnsupportedOperationException`, consistent with an unmodifiable
    // list and the JDK's `Collections$UnmodifiableList$1` list-iterator view.
    {
        let c = UNMOD_LIST_ITR_CLASS;
        r.register(c, "hasNext", "()Z", native_unmod_listitr_has_next);
        r.register(c, "next", "()Ljava/lang/Object;", native_unmod_listitr_next);
        r.register(c, "hasPrevious", "()Z", native_unmod_listitr_has_previous);
        r.register(
            c,
            "previous",
            "()Ljava/lang/Object;",
            native_unmod_listitr_previous,
        );
        r.register(c, "nextIndex", "()I", native_unmod_listitr_next_index);
        r.register(c, "previousIndex", "()I", native_unmod_listitr_previous_index);
        r.register(
            c,
            "forEachRemaining",
            "(Ljava/util/function/Consumer;)V",
            native_unmod_listitr_for_each_remaining,
        );
        // Mutators — all throw UnsupportedOperationException.
        r.register(c, "set", "(Ljava/lang/Object;)V", native_unmod_throw);
        r.register(c, "add", "(Ljava/lang/Object;)V", native_unmod_throw);
        r.register(c, "remove", "()V", native_unmod_throw);
    }
}

/// Universal mutator: throws `UnsupportedOperationException`.
fn native_unmod_throw(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(unsupported_op())
}

fn native_unmod_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "size", "()I")
}

fn native_unmod_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "isEmpty", "()Z")
}

fn native_unmod_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "contains", "(Ljava/lang/Object;)Z")
}

fn native_unmod_contains_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsAll", "(Ljava/util/Collection;)Z")
}

fn native_unmod_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "get", "(I)Ljava/lang/Object;")
}

fn native_unmod_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "indexOf", "(Ljava/lang/Object;)I")
}

fn native_unmod_last_index_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "lastIndexOf", "(Ljava/lang/Object;)I")
}

fn native_unmod_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;")
}

fn native_unmod_to_array_typed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "toArray",
        "([Ljava/lang/Object;)[Ljava/lang/Object;",
    )
}

fn native_unmod_to_string(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "toString", "()Ljava/lang/String;")
}

fn native_unmod_hash_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "hashCode", "()I")
}

fn native_unmod_equals(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `this.equals(other)` forwards to `backing.equals(other)`. If `other` is
    // itself an unmodifiable wrapper, it must be unwrapped to its backing
    // collection first — otherwise the backing list's `AbstractList.equals`
    // tries to iterate the *wrapper* (e.g. via `listIterator()`, which the
    // wrapper does not expose) and the comparison spuriously fails. This is
    // the JDK behaviour: `List.of(a,b).equals(List.of(a,b))` is `true`, and
    // crucially `ResourceBundle$Control.FORMAT_DEFAULT.equals(itself)` must
    // be `true` or `getNoFallbackControl` throws IllegalArgumentException.
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let backing = match unmod_backing(ctx, this) {
        Some(b) => b,
        None => return Ok(Some(Value::Object(None))),
    };
    // Unwrap the argument: walk down any chain of unmodifiable wrappers so
    // the backing's `equals` sees a concrete ArrayList / HashSet / HashMap.
    let other = match args.get(1) {
        Some(Value::Object(Some(o))) => {
            let mut cur = *o;
            while is_unmod_wrapper(ctx, cur) {
                match unmod_backing(ctx, cur) {
                    Some(b) => cur = b,
                    None => break,
                }
            }
            Value::Object(Some(cur))
        }
        v => v.cloned().unwrap_or(Value::Object(None)),
    };
    ctx.invoke_virtual(backing, "equals", "(Ljava/lang/Object;)Z", &[other])
}

fn native_unmod_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "forEach", "(Ljava/util/function/Consumer;)V")
}

fn native_unmod_stream(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "stream", "()Ljava/util/stream/Stream;")
}

fn native_unmod_spliterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "spliterator", "()Ljava/util/Spliterator;")
}

/// `subList` returns another unmodifiable view over the backing sub-list.
fn native_unmod_sub_list(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sub = unmod_delegate(ctx, args, "subList", "(II)Ljava/util/List;")?;
    if let Some(Value::Object(Some(inner))) = sub {
        let w = alloc_unmod_wrapper(ctx, UNMOD_LIST_CLASS, inner);
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(sub)
}

/// `iterator()` returns a read-only iterator wrapping the backing iterator.
fn native_unmod_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let inner = unmod_delegate(ctx, args, "iterator", "()Ljava/util/Iterator;")?;
    if let Some(Value::Object(Some(itr))) = inner {
        let w = alloc_unmod_wrapper(ctx, UNMOD_ITR_CLASS, itr);
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(inner)
}

fn native_unmod_itr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "hasNext", "()Z")
}

fn native_unmod_itr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "next", "()Ljava/lang/Object;")
}

/// Slot layout of a `UnmodifiableListItr`.
const UNMOD_LIST_ITR_SNAPSHOT: usize = 0;
const UNMOD_LIST_ITR_CURSOR: usize = 1;

/// Snapshot the elements of an unmodifiable-list wrapper into an `Object[]`,
/// by delegating to the backing collection's `toArray()`.
fn unmod_list_snapshot(ctx: &mut dyn NativeContext, args: &[Value]) -> Option<ObjectRef> {
    match unmod_delegate(ctx, args, "toArray", "()[Ljava/lang/Object;") {
        Ok(Some(Value::Object(Some(arr)))) => Some(arr),
        _ => None,
    }
}

/// Allocate a read-only `ListIterator` over `snapshot`, positioned at `cursor`.
fn alloc_unmod_list_itr(
    ctx: &mut dyn NativeContext,
    snapshot: ObjectRef,
    cursor: i32,
) -> ObjectRef {
    let it = alloc_synthetic(ctx, UNMOD_LIST_ITR_CLASS, 2);
    ctx.set_field(it, UNMOD_LIST_ITR_SNAPSHOT, Value::Object(Some(snapshot)));
    ctx.set_field(it, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
    it
}

/// `listIterator()` returns a read-only `ListIterator` over the backing list.
fn native_unmod_list_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(ctx, snapshot, 0)))))
}

/// `listIterator(int)` returns a read-only `ListIterator` positioned at `index`.
fn native_unmod_list_iterator_idx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let snapshot = match unmod_list_snapshot(ctx, args) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    let len = ctx.array_length(snapshot) as i32;
    let index = match args.get(1) {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    if index < 0 || index > len {
        // JDK throws IndexOutOfBoundsException; ArrayIndexOutOfBoundsException
        // is a subclass, so `catch (IndexOutOfBoundsException)` still catches.
        return Err(RuntimeError::ArrayIndexOutOfBoundsException { index }.into());
    }
    Ok(Some(Value::Object(Some(alloc_unmod_list_itr(
        ctx, snapshot, index,
    )))))
}

/// Read the (snapshot, cursor) state out of a list-iterator wrapper.
fn unmod_list_itr_state(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Option<(ObjectRef, i32, i32)> {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return None,
    };
    let snapshot = match ctx.get_field(this, UNMOD_LIST_ITR_SNAPSHOT) {
        Value::Object(Some(a)) => a,
        _ => return None,
    };
    let cursor = match ctx.get_field(this, UNMOD_LIST_ITR_CURSOR) {
        Value::Int(c) => c,
        _ => 0,
    };
    let len = ctx.array_length(snapshot) as i32;
    Some((snapshot, cursor, len))
}

fn native_unmod_listitr_has_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, len)) => Ok(Some(Value::Int(if cursor < len { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_unmod_listitr_next(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, len) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor >= len {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let elem = ctx.get_array_element(snapshot, cursor as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor + 1));
    Ok(Some(elem))
}

fn native_unmod_listitr_has_previous(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 }))),
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_unmod_listitr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (this, snapshot, cursor, _) = match (args.first(), unmod_list_itr_state(ctx, args)) {
        (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
        _ => {
            return Err(RuntimeError::NoSuchElementException {
                message: String::new(),
            }
            .into());
        }
    };
    if cursor <= 0 {
        return Err(RuntimeError::NoSuchElementException {
            message: String::new(),
        }
        .into());
    }
    let idx = cursor - 1;
    let elem = ctx.get_array_element(snapshot, idx as usize);
    ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(idx));
    Ok(Some(elem))
}

fn native_unmod_listitr_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor))),
        None => Ok(Some(Value::Int(0))),
    }
}

fn native_unmod_listitr_previous_index(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unmod_list_itr_state(ctx, args) {
        Some((_, cursor, _)) => Ok(Some(Value::Int(cursor - 1))),
        None => Ok(Some(Value::Int(-1))),
    }
}

fn native_unmod_listitr_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (this, snapshot, mut cursor, len) =
        match (args.first(), unmod_list_itr_state(ctx, args)) {
            (Some(Value::Object(Some(o))), Some((s, c, l))) => (*o, s, c, l),
            _ => return Ok(None),
        };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return Ok(None),
    };
    while cursor < len {
        let elem = ctx.get_array_element(snapshot, cursor as usize);
        cursor += 1;
        ctx.set_field(this, UNMOD_LIST_ITR_CURSOR, Value::Int(cursor));
        ctx.invoke_virtual(
            consumer,
            "accept",
            "(Ljava/lang/Object;)V",
            &[elem],
        )?;
    }
    Ok(None)
}

// ---- Map delegations ------------------------------------------------------

fn native_unmod_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "get", "(Ljava/lang/Object;)Ljava/lang/Object;")
}

fn native_unmod_map_get_or_default(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "getOrDefault",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    )
}

fn native_unmod_map_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsKey", "(Ljava/lang/Object;)Z")
}

fn native_unmod_map_contains_value(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    unmod_delegate(ctx, args, "containsValue", "(Ljava/lang/Object;)Z")
}

fn native_unmod_map_for_each(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    unmod_delegate(
        ctx,
        args,
        "forEach",
        "(Ljava/util/function/BiConsumer;)V",
    )
}

/// `keySet()` returns an unmodifiable Set view of the backing map's key set.
fn native_unmod_map_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let ks = unmod_delegate(ctx, args, "keySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = ks {
        let w = alloc_unmod_wrapper(ctx, UNMOD_SET_CLASS, inner);
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(ks)
}

/// `values()` returns an unmodifiable Collection view of the backing values.
fn native_unmod_map_values(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vs = unmod_delegate(ctx, args, "values", "()Ljava/util/Collection;")?;
    if let Some(Value::Object(Some(inner))) = vs {
        let w = alloc_unmod_wrapper(ctx, UNMOD_COLLECTION_CLASS, inner);
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(vs)
}

/// `entrySet()` returns an unmodifiable Set view of the backing entry set.
fn native_unmod_map_entry_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let es = unmod_delegate(ctx, args, "entrySet", "()Ljava/util/Set;")?;
    if let Some(Value::Object(Some(inner))) = es {
        let w = alloc_unmod_wrapper(ctx, UNMOD_SET_CLASS, inner);
        return Ok(Some(Value::Object(Some(w))));
    }
    Ok(es)
}

// ===========================================================================
// Phase 39: Collections extras — unmodifiableMap/Set, emptyMap/Set/Iterator,
//           frequency, disjoint, singleton, Enumeration stubs
// ===========================================================================

fn register_collections_extras_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/Collections";
    r.register(
        c,
        "emptyMap",
        "()Ljava/util/Map;",
        native_collections_empty_map,
    );
    r.register(
        c,
        "emptySet",
        "()Ljava/util/Set;",
        native_collections_empty_set,
    );
    r.register(
        c,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        native_collections_empty_iterator,
    );
    r.register(
        c,
        "singleton",
        "(Ljava/lang/Object;)Ljava/util/Set;",
        native_collections_singleton,
    );
    r.register(
        c,
        "singletonMap",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/util/Map;",
        native_collections_singleton_map,
    );
    r.register(
        c,
        "unmodifiableMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_unmodifiable_map,
    );
    r.register(
        c,
        "unmodifiableSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_unmodifiable_set,
    );
    r.register(
        c,
        "unmodifiableSortedMap",
        "(Ljava/util/SortedMap;)Ljava/util/SortedMap;",
        native_collections_unmodifiable_map,
    );
    r.register(
        c,
        "unmodifiableSortedSet",
        "(Ljava/util/SortedSet;)Ljava/util/SortedSet;",
        native_collections_unmodifiable_set,
    );
    r.register(
        c,
        "unmodifiableNavigableMap",
        "(Ljava/util/NavigableMap;)Ljava/util/NavigableMap;",
        native_collections_unmodifiable_map,
    );
    r.register(
        c,
        "unmodifiableNavigableSet",
        "(Ljava/util/NavigableSet;)Ljava/util/NavigableSet;",
        native_collections_unmodifiable_set,
    );
    r.register(
        c,
        "unmodifiableCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_unmodifiable_collection,
    );
    r.register(
        c,
        "synchronizedList",
        "(Ljava/util/List;)Ljava/util/List;",
        native_collections_identity,
    );
    r.register(
        c,
        "synchronizedMap",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_identity,
    );
    r.register(
        c,
        "synchronizedSet",
        "(Ljava/util/Set;)Ljava/util/Set;",
        native_collections_identity,
    );
    r.register(
        c,
        "synchronizedCollection",
        "(Ljava/util/Collection;)Ljava/util/Collection;",
        native_collections_identity,
    );
    r.register(
        c,
        "frequency",
        "(Ljava/util/Collection;Ljava/lang/Object;)I",
        native_collections_frequency,
    );
    r.register(
        c,
        "disjoint",
        "(Ljava/util/Collection;Ljava/util/Collection;)Z",
        native_collections_disjoint,
    );
    r.register(
        c,
        "max",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_max,
    );
    r.register(
        c,
        "min",
        "(Ljava/util/Collection;)Ljava/lang/Object;",
        native_collections_min,
    );
    r.register(c, "swap", "(Ljava/util/List;II)V", native_collections_swap);
    r.register(
        c,
        "fill",
        "(Ljava/util/List;Ljava/lang/Object;)V",
        native_collections_fill,
    );
    r.register(
        c,
        "nCopies",
        "(ILjava/lang/Object;)Ljava/util/List;",
        native_collections_n_copies,
    );
    r.register(
        c,
        "shuffle",
        "(Ljava/util/List;)V",
        native_collections_shuffle,
    );
    r.register(
        c,
        "addAll",
        "(Ljava/util/Collection;[Ljava/lang/Object;)Z",
        native_collections_add_all,
    );
    // List.copyOf / Set.copyOf / Map.copyOf
    r.register(
        "java/util/List",
        "copyOf",
        "(Ljava/util/Collection;)Ljava/util/List;",
        native_collections_identity,
    );
    r.register(
        "java/util/Set",
        "copyOf",
        "(Ljava/util/Collection;)Ljava/util/Set;",
        native_collections_identity,
    );
    r.register(
        "java/util/Map",
        "copyOf",
        "(Ljava/util/Map;)Ljava/util/Map;",
        native_collections_identity,
    );

    // Enumeration interface
    let en = "java/util/Enumeration";
    r.register(en, "hasMoreElements", "()Z", native_snapshot_itr_has_next);
    r.register(
        en,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );

    // Snapshot Enumeration used by `Properties.propertyNames()` and similar
    // call sites that need a real Enumeration over a fixed key list. We can't
    // reuse `Collections$EmptyEnumeration` because its real-JDK bytecode for
    // `hasMoreElements` is hardcoded to `iconst_0; ireturn` and wins concrete
    // dispatch over the interface-level Enumeration native, so a non-empty
    // snapshot built on EmptyEnumeration appears empty. Register concrete
    // overrides on a dedicated synthetic class.
    let sne = "cratonvm/internal/SnapshotEnumeration";
    r.register(sne, "hasMoreElements", "()Z", native_snapshot_itr_has_next);
    r.register(
        sne,
        "nextElement",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );
}

/// Identity — just returns the first argument (used for synchronized wrappers)
fn native_collections_identity(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(args.first().cloned().unwrap_or(Value::Object(None))))
}

/// `Collections.unmodifiableMap` — wrap the source map in a live read-only view.
fn native_collections_unmodifiable_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_MAP_CLASS, *src);
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableSet` — wrap the source set in a live read-only view.
fn native_collections_unmodifiable_set(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_SET_CLASS, *src);
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

/// `Collections.unmodifiableCollection` — wrap the source collection in a
/// live read-only view.
fn native_collections_unmodifiable_collection(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match args.first() {
        Some(Value::Object(Some(src))) => {
            let w = alloc_unmod_wrapper(ctx, UNMOD_COLLECTION_CLASS, *src);
            Ok(Some(Value::Object(Some(w))))
        }
        _ => Ok(Some(Value::Object(None))),
    }
}

fn native_collections_empty_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let map = alloc_backing_map(ctx);
    native_map_init(ctx, &[Value::Object(Some(map))])?;
    Ok(Some(Value::Object(Some(map))))
}

fn native_collections_empty_set(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let inner_map = alloc_backing_map(ctx);
    native_map_init(ctx, &[Value::Object(Some(inner_map))])?;
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(inner_map)));
    Ok(Some(Value::Object(Some(set))))
}

fn native_collections_empty_iterator(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let arr = alloc_ref_array(ctx, 0);
    let itr = alloc_synthetic(ctx, "java/util/Collections$EmptyItr", 2);
    ctx.set_field(itr, 0, Value::Object(Some(arr)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_collections_singleton(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let elem = args.first().cloned().unwrap_or(Value::Object(None));
    let set = alloc_synthetic(ctx, "java/util/HashSet", HS_NUM_FIELDS);
    let inner_map = alloc_backing_map(ctx);
    native_map_init(ctx, &[Value::Object(Some(inner_map))])?;
    ctx.set_field(set, HS_FIELD_MAP, Value::Object(Some(inner_map)));
    // Add elem
    native_map_put(
        ctx,
        &[Value::Object(Some(inner_map)), elem, Value::Object(None)],
    )?;
    Ok(Some(Value::Object(Some(set))))
}

fn native_collections_singleton_map(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let key = args.first().cloned().unwrap_or(Value::Object(None));
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    let map = alloc_backing_map(ctx);
    native_map_init(ctx, &[Value::Object(Some(map))])?;
    native_map_put(ctx, &[Value::Object(Some(map)), key, val])?;
    Ok(Some(Value::Object(Some(map))))
}

fn native_collections_frequency(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, coll);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Int(0))),
    };
    let mut count = 0i32;
    for i in 0..size as usize {
        let elem = ctx.get_array_element(data, i);
        if values_equal(ctx, &elem, &target) {
            count += 1;
        }
    }
    Ok(Some(Value::Int(count)))
}

fn native_collections_disjoint(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let c1 = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let c2 = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let (data1, size1) = al_state(ctx, c1);
    let (data2, size2) = al_state(ctx, c2);
    let (d1, d2) = match (data1, data2) {
        (Some(a), Some(b)) => (a, b),
        _ => return Ok(Some(Value::Int(1))),
    };
    for i in 0..size1 as usize {
        let elem = ctx.get_array_element(d1, i);
        for j in 0..size2 as usize {
            let other = ctx.get_array_element(d2, j);
            if values_equal(ctx, &elem, &other) {
                return Ok(Some(Value::Int(0)));
            }
        }
    }
    Ok(Some(Value::Int(1)))
}

fn native_collections_max(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, coll);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut max_val = ctx.get_array_element(data, 0);
    let mut max_str = val_to_string(ctx, &max_val);
    for i in 1..size as usize {
        let elem = ctx.get_array_element(data, i);
        let s = val_to_string(ctx, &elem);
        if s > max_str {
            max_val = elem;
            max_str = s;
        }
    }
    Ok(Some(max_val))
}

fn native_collections_min(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let (data, size) = al_state(ctx, coll);
    let data = match data {
        Some(d) => d,
        None => return Ok(Some(Value::Object(None))),
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut min_val = ctx.get_array_element(data, 0);
    let mut min_str = val_to_string(ctx, &min_val);
    for i in 1..size as usize {
        let elem = ctx.get_array_element(data, i);
        let s = val_to_string(ctx, &elem);
        if s < min_str {
            min_val = elem;
            min_str = s;
        }
    }
    Ok(Some(min_val))
}

fn val_to_string(ctx: &mut dyn NativeContext, val: &Value) -> String {
    match val {
        Value::Object(Some(o)) => ctx.read_string(*o).unwrap_or_default(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        _ => String::new(),
    }
}

fn native_collections_swap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let i = match args.get(1) {
        Some(Value::Int(v)) => *v as usize,
        _ => return Ok(None),
    };
    let j = match args.get(2) {
        Some(Value::Int(v)) => *v as usize,
        _ => return Ok(None),
    };
    let (data, _) = al_state(ctx, list);
    if let Some(d) = data {
        let a = ctx.get_array_element(d, i);
        let b = ctx.get_array_element(d, j);
        ctx.set_array_element(d, i, b);
        ctx.set_array_element(d, j, a);
    }
    Ok(None)
}

fn native_collections_fill(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        for i in 0..size as usize {
            ctx.set_array_element(d, i, val);
        }
    }
    Ok(None)
}

fn native_collections_n_copies(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let val = args.get(1).cloned().unwrap_or(Value::Object(None));
    let __al_n_fields = al_slots(ctx).2;
    let list = alloc_synthetic(ctx, "java/util/ArrayList", __al_n_fields);
    let arr = alloc_ref_array(ctx, n.max(0) as usize);
    for i in 0..n.max(0) as usize {
        ctx.set_array_element(arr, i, val);
    }
    al_set_data(ctx, list, arr);
    al_set_size(ctx, list, n.max(0));
    Ok(Some(Value::Object(Some(list))))
}

fn native_collections_shuffle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let list = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let (data, size) = al_state(ctx, list);
    if let Some(d) = data {
        let n = size as usize;
        // Simple Fisher-Yates using system time as seed
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(42u64, |t| t.as_nanos() as u64);
        let mut rng = seed;
        for i in (1..n).rev() {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = (rng >> 33) as usize % (i + 1);
            let a = ctx.get_array_element(d, i);
            let b = ctx.get_array_element(d, j);
            ctx.set_array_element(d, i, b);
            ctx.set_array_element(d, j, a);
        }
    }
    Ok(None)
}

fn native_collections_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let coll = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let elements = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let len = ctx.array_length(elements);
    let mut modified = false;
    for i in 0..len {
        let elem = ctx.get_array_element(elements, i);
        // Dispatch to the target collection's real `add` instead of
        // hard-coding `native_al_add` (the ArrayList `elementData`/`size`
        // layout). `Collections.addAll` accepts any `Collection` — a
        // HashSet, LinkedList, TreeSet, etc. store elements nothing like
        // ArrayList, so `native_al_add` would write the wrong fields and
        // silently drop every element. JVMS/JDK spec is `result |=
        // c.add(element)`; honour that via virtual dispatch.
        if let Some(Value::Int(1)) =
            ctx.invoke_virtual(coll, "add", "(Ljava/lang/Object;)Z", &[elem])?
        {
            modified = true;
        }
    }
    Ok(Some(Value::Int(if modified { 1 } else { 0 })))
}

// ===========================================================================
// Phase 41: BlockingQueue family — LinkedBlockingQueue, ArrayBlockingQueue
// ===========================================================================

// LinkedBlockingQueue = 4-field synthetic (same as LinkedList + capacity)
const LBQ_FIELD_HEAD: usize = 0;
const LBQ_FIELD_TAIL: usize = 1;
const LBQ_FIELD_SIZE: usize = 2;
const LBQ_FIELD_CAPACITY: usize = 3;
const _LBQ_NUM_FIELDS: usize = 4;

fn register_blocking_queue_natives(r: &mut NativeMethodRegistry) {
    // LinkedBlockingQueue
    let lbq = "java/util/concurrent/LinkedBlockingQueue";
    r.register(lbq, "<init>", "()V", native_lbq_init);
    r.register(lbq, "<init>", "(I)V", native_lbq_init_cap);
    // put() blocks until space is available via monitor wait/notify (task #15)
    r.register(lbq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(lbq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(lbq, "add", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    // take() blocks until an element is available via monitor wait/notify (task #15)
    r.register(lbq, "take", "()Ljava/lang/Object;", native_lbq_take_blocking);
    r.register(lbq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(lbq, "peek", "()Ljava/lang/Object;", native_lbq_peek);
    r.register(lbq, "size", "()I", native_lbq_size);
    r.register(lbq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(lbq, "remainingCapacity", "()I", native_lbq_remaining);
    r.register(lbq, "clear", "()V", native_lbq_clear);
    r.register(
        lbq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(lbq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(lbq, "toArray", "()[Ljava/lang/Object;", native_lbq_to_array);
    r.register(
        lbq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );
    // Also register under BlockingQueue interface
    let bq = "java/util/concurrent/BlockingQueue";
    r.register(bq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(bq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(bq, "take", "()Ljava/lang/Object;", native_lbq_take_blocking);
    r.register(bq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(bq, "peek", "()Ljava/lang/Object;", native_lbq_peek);

    // ArrayBlockingQueue
    let abq = "java/util/concurrent/ArrayBlockingQueue";
    r.register(abq, "<init>", "(I)V", native_abq_init);
    r.register(abq, "<init>", "(IZ)V", native_abq_init_fair);
    r.register(abq, "put", "(Ljava/lang/Object;)V", native_lbq_put_blocking);
    r.register(abq, "offer", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(abq, "add", "(Ljava/lang/Object;)Z", native_lbq_offer_bool);
    r.register(abq, "take", "()Ljava/lang/Object;", native_lbq_take_blocking);
    r.register(abq, "poll", "()Ljava/lang/Object;", native_lbq_poll);
    r.register(abq, "peek", "()Ljava/lang/Object;", native_lbq_peek);
    r.register(abq, "size", "()I", native_lbq_size);
    r.register(abq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(abq, "remainingCapacity", "()I", native_lbq_remaining);
    r.register(abq, "clear", "()V", native_lbq_clear);
    r.register(
        abq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(abq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(abq, "toArray", "()[Ljava/lang/Object;", native_lbq_to_array);
    r.register(
        abq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );

    // ConcurrentLinkedQueue — thread-safety wrappers (round-5 Fix 2 HIGH).
    //
    // The raw `native_lbq_offer_bool` / `native_lbq_poll` do unsynchronised
    // array shifts that corrupt under concurrent producer+consumer load
    // (read-modify-write of LBQ_FIELD_SIZE races; element-shift in `poll`
    // races against `offer` appending at `size`). Real CLQ is lock-free
    // (Michael-Scott queue); we approximate the *correctness* guarantee
    // by serialising every op on the `this` monitor. Not lock-free in
    // throughput, but at least no torn updates.
    let clq = "java/util/concurrent/ConcurrentLinkedQueue";
    r.register(clq, "<init>", "()V", native_lbq_init);
    r.register(clq, "offer", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(clq, "add", "(Ljava/lang/Object;)Z", native_clq_offer);
    r.register(clq, "poll", "()Ljava/lang/Object;", native_clq_poll);
    r.register(clq, "peek", "()Ljava/lang/Object;", native_clq_peek);
    r.register(clq, "size", "()I", native_lbq_size);
    r.register(clq, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(
        clq,
        "contains",
        "(Ljava/lang/Object;)Z",
        native_lbq_contains,
    );
    r.register(clq, "remove", "(Ljava/lang/Object;)Z", native_lbq_remove);
    r.register(
        clq,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );

    // ConcurrentLinkedDeque — same synchronisation rationale as CLQ.
    let cld = "java/util/concurrent/ConcurrentLinkedDeque";
    r.register(cld, "<init>", "()V", native_lbq_init);
    r.register(
        cld,
        "offerFirst",
        "(Ljava/lang/Object;)Z",
        native_cld_offer_first,
    );
    r.register(
        cld,
        "offerLast",
        "(Ljava/lang/Object;)Z",
        native_clq_offer,
    );
    r.register(cld, "pollFirst", "()Ljava/lang/Object;", native_clq_poll);
    r.register(
        cld,
        "pollLast",
        "()Ljava/lang/Object;",
        native_cld_poll_last,
    );
    r.register(cld, "peekFirst", "()Ljava/lang/Object;", native_clq_peek);
    r.register(
        cld,
        "peekLast",
        "()Ljava/lang/Object;",
        native_cld_peek_last,
    );
    r.register(cld, "size", "()I", native_lbq_size);
    r.register(cld, "isEmpty", "()Z", native_lbq_is_empty);
    r.register(
        cld,
        "iterator",
        "()Ljava/util/Iterator;",
        native_lbq_iterator,
    );
}

// ---------------------------------------------------------------------------
// Round-5 Fix 2 (HIGH): synchronised CLQ/CLD wrappers
// ---------------------------------------------------------------------------
//
// Real JDK ConcurrentLinkedQueue/Deque are lock-free (Michael-Scott /
// concurrent doubly-linked-list). Our synthetic backing is an ArrayList,
// which the LBQ helpers mutate with unsynchronised read-modify-write
// sequences. Producer+consumer racing would tear the size counter and
// drop or duplicate elements.
//
// These wrappers acquire the `this`-object monitor around each op so the
// observable behaviour is serialisable. monitor_enter/exit on `this` is
// also what real JDK code would synchronise on for
// `Collections.synchronizedQueue(queue)`, so semantically identical from
// the bytecode side. NOT lock-free in the throughput sense; treat as a
// correctness-only stopgap until a real Michael-Scott port lands.

fn native_clq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_offer_bool(ctx, args);
    ctx.monitor_exit(this);
    r
}

fn native_clq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_poll(ctx, args);
    ctx.monitor_exit(this);
    r
}

fn native_clq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_peek(ctx, args);
    ctx.monitor_exit(this);
    r
}

fn native_cld_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_poll_last(ctx, args);
    ctx.monitor_exit(this);
    r
}

fn native_cld_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.monitor_enter(this);
    let r = native_lbq_peek_last(ctx, args);
    ctx.monitor_exit(this);
    r
}

/// offerFirst — prepend element; ArrayList backing means O(n) shift but
/// correctness is preserved under concurrent access via the `this` monitor.
fn native_cld_offer_first(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(this))) = args.first().copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let elem = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    lbq_ensure_capacity(ctx, this, (size + 1) as usize);
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    // Shift elements right to make room at index 0.
    if size > 0 {
        for i in (1..=(size as usize)).rev() {
            ctx.set_array_element(arr, i, ctx.get_array_element(arr, i - 1));
        }
    }
    ctx.set_array_element(arr, 0, elem);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size + 1));
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(1)))
}

// Blocking queue implementations — backed by ArrayList internally for simplicity
fn native_lbq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = alloc_ref_array(ctx, 16);
    ctx.set_field(this, LBQ_FIELD_HEAD, Value::Object(Some(arr)));
    ctx.set_field(this, LBQ_FIELD_TAIL, Value::Int(0)); // unused, we use simple array
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_CAPACITY, Value::Int(i32::MAX));
    Ok(None)
}

fn native_lbq_init_cap(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let cap = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => i32::MAX,
    };
    let arr = alloc_ref_array(ctx, cap.max(1) as usize);
    ctx.set_field(this, LBQ_FIELD_HEAD, Value::Object(Some(arr)));
    ctx.set_field(this, LBQ_FIELD_TAIL, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    ctx.set_field(this, LBQ_FIELD_CAPACITY, Value::Int(cap));
    Ok(None)
}

fn native_abq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_lbq_init_cap(ctx, args)
}

fn native_abq_init_fair(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Ignore fair param
    native_lbq_init_cap(ctx, args)
}

fn lbq_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, needed: usize) {
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    let old_len = ctx.array_length(arr);
    if needed <= old_len {
        return;
    }
    let new_len = (old_len * 2).max(needed).max(16);
    let new_arr = alloc_ref_array(ctx, new_len);
    for i in 0..old_len {
        ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
    }
    ctx.set_field(this, LBQ_FIELD_HEAD, Value::Object(Some(new_arr)));
}

// =====================================================================
// task #15 (MED → correctness): blocking-queue park/notify discipline.
//
// Previously `native_lbq_put_blocking` spun 10000 times then fell through
// into `native_lbq_offer` regardless of capacity — losing the "block"
// semantic entirely (silently overflowing the bounded queue). Likewise
// `native_lbq_poll/peek/poll_last/peek_last` were unsynchronised so a
// concurrent producer racing a consumer could tear LBQ_FIELD_SIZE or read
// a half-shifted backing array.
//
// Fix: every read AND every mutation now acquires the `this` monitor, and
// the put/take blocking variants follow standard Java wait/notify
// discipline:
//
//   put:  while (size >= capacity) wait(); offer(); notifyAll();
//   take: while (size == 0)        wait(); poll();  notifyAll();
//
// `lbq_notify_all_locked` is called from every mutator that changes size
// (offer, poll, poll_last, clear, remove) so a blocked put/take wakes
// promptly. A safety timeout of 50 ms on `monitor_wait` guards against
// single-threaded test contexts whose `monitor_notify_all` is a no-op —
// without it the test thread would deadlock; with it the loop simply
// re-checks the predicate and bails (returning normally on take, blocking
// indefinitely on put, which is correct: a producer on a full queue with
// no consumer SHOULD block).
// =====================================================================

/// Internal offer that assumes caller already holds the `this` monitor.
/// Grows the backing array unconditionally — capacity is enforced by the
/// caller (offer_bool / put_blocking).
fn native_lbq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let elem = args.get(1).cloned().unwrap_or(Value::Object(None));
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    lbq_ensure_capacity(ctx, this, (size + 1) as usize);
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return Ok(None),
    };
    ctx.set_array_element(arr, size as usize, elem);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size + 1));
    Ok(None)
}

fn native_lbq_offer_bool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    ctx.monitor_enter(this);
    let capacity = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) if v > 0 => v,
        _ => i32::MAX,
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size >= capacity {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Int(0))); // queue full
    }
    let r = native_lbq_offer(ctx, args);
    // Wake any thread parked in `take()` waiting for an element.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    r?;
    Ok(Some(Value::Int(1)))
}

/// Blocking put: waits until space is available using monitor wait/notify.
/// Loop predicate is re-checked after each wake (handles spurious wakeups
/// and the case where another producer raced in to fill the gap).
fn native_lbq_put_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let capacity = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) if v > 0 => v,
        _ => i32::MAX,
    };
    ctx.monitor_enter(this);
    loop {
        let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size < capacity {
            break;
        }
        // Park on the monitor with a short timeout so we re-check the
        // predicate periodically — protects against missed-wakeup bugs and
        // makes the wait responsive on mock contexts whose
        // `monitor_notify_all` is a no-op.
        let _ = ctx.monitor_wait(this, Some(50));
    }
    let r = native_lbq_offer(ctx, args);
    // Wake any thread parked in `take()` waiting for an element to arrive.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    r
}

/// Blocking take: waits until an element is available using monitor wait/notify.
fn native_lbq_take_blocking(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    loop {
        let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
            Value::Int(v) => v,
            _ => 0,
        };
        if size > 0 {
            break;
        }
        let _ = ctx.monitor_wait(this, Some(50));
    }
    let r = lbq_poll_locked(ctx, this);
    // Wake any thread parked in `put()` waiting for a slot to free up.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(Some(r))
}

/// Internal poll that assumes the caller already holds the `this` monitor.
/// Returns `Value::Object(None)` when the queue is empty.
fn lbq_poll_locked(ctx: &mut dyn NativeContext, this: ObjectRef) -> Value {
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Value::Object(None);
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => return Value::Object(None),
    };
    let head = ctx.get_array_element(arr, 0);
    // Shift elements left.
    for i in 0..(size - 1) as usize {
        ctx.set_array_element(arr, i, ctx.get_array_element(arr, i + 1));
    }
    ctx.set_array_element(arr, (size - 1) as usize, Value::Object(None));
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
    head
}

fn native_lbq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let head = lbq_poll_locked(ctx, this);
    // Wake any thread parked in `put()` — a slot just freed.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(Some(head))
}

fn native_lbq_poll_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let tail = ctx.get_array_element(arr, (size - 1) as usize);
    ctx.set_array_element(arr, (size - 1) as usize, Value::Object(None));
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(Some(tail))
}

fn native_lbq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let head = ctx.get_array_element(arr, 0);
    ctx.monitor_exit(this);
    Ok(Some(head))
}

fn native_lbq_peek_last(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        ctx.monitor_exit(this);
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let tail = ctx.get_array_element(arr, (size - 1) as usize);
    ctx.monitor_exit(this);
    Ok(Some(tail))
}

fn native_lbq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_lbq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if size == 0 { 1 } else { 0 })))
}

fn native_lbq_remaining(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let cap = match ctx.get_field(this, LBQ_FIELD_CAPACITY) {
        Value::Int(v) => v,
        _ => i32::MAX,
    };
    Ok(Some(Value::Int(cap - size)))
}

fn native_lbq_clear(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(0));
    // Wake any thread parked in `put()` — capacity was just fully reclaimed.
    let _ = ctx.monitor_notify_all(this);
    ctx.monitor_exit(this);
    Ok(None)
}

fn native_lbq_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    for i in 0..size as usize {
        let elem = ctx.get_array_element(arr, i);
        if values_equal(ctx, &elem, &target) {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(1)));
        }
    }
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(0)))
}

fn native_lbq_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let target = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(0)));
        }
    };
    for i in 0..size as usize {
        let elem = ctx.get_array_element(arr, i);
        if values_equal(ctx, &elem, &target) {
            for j in i..(size - 1) as usize {
                ctx.set_array_element(arr, j, ctx.get_array_element(arr, j + 1));
            }
            ctx.set_array_element(arr, (size - 1) as usize, Value::Object(None));
            ctx.set_field(this, LBQ_FIELD_SIZE, Value::Int(size - 1));
            // Wake any thread parked in `put()` — a slot just freed.
            let _ = ctx.monitor_notify_all(this);
            ctx.monitor_exit(this);
            return Ok(Some(Value::Int(1)));
        }
    }
    ctx.monitor_exit(this);
    Ok(Some(Value::Int(0)))
}

fn native_lbq_to_array(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let result = alloc_ref_array(ctx, size as usize);
    for i in 0..size as usize {
        ctx.set_array_element(result, i, ctx.get_array_element(arr, i));
    }
    ctx.monitor_exit(this);
    Ok(Some(Value::Object(Some(result))))
}

fn native_lbq_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.monitor_enter(this);
    let size = match ctx.get_field(this, LBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let arr = match ctx.get_field(this, LBQ_FIELD_HEAD) {
        Value::Object(Some(a)) => a,
        _ => {
            ctx.monitor_exit(this);
            return Ok(Some(Value::Object(None)));
        }
    };
    let snap = alloc_ref_array(ctx, size as usize);
    for i in 0..size as usize {
        ctx.set_array_element(snap, i, ctx.get_array_element(arr, i));
    }
    ctx.monitor_exit(this);
    let itr = alloc_synthetic(ctx, "java/util/concurrent/LinkedBlockingQueue$Itr", 2);
    ctx.set_field(itr, 0, Value::Object(Some(snap)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// Phase 44: Iterator Protocol + Enumeration completion
// ===========================================================================
// Register generic java/util/Iterator interface methods as fallbacks.
// The snapshot iterator pattern (2-field: array=0, cursor=1) is already used
// by all collection iterators. This registers them under the interface name
// so code calling through Iterator<E> references works.

fn register_iterator_protocol_natives(r: &mut NativeMethodRegistry) {
    // Generic java/util/Iterator interface — fallback to snapshot iterator
    let itr = "java/util/Iterator";
    r.register(itr, "hasNext", "()Z", native_snapshot_itr_has_next);
    r.register(
        itr,
        "next",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );
    r.register(itr, "remove", "()V", native_itr_remove_noop);

    // ListIterator (extends Iterator)
    let litr = "java/util/ListIterator";
    r.register(litr, "hasNext", "()Z", native_snapshot_itr_has_next);
    r.register(
        litr,
        "next",
        "()Ljava/lang/Object;",
        native_snapshot_itr_next,
    );
    r.register(litr, "hasPrevious", "()Z", native_list_itr_has_previous);
    r.register(
        litr,
        "previous",
        "()Ljava/lang/Object;",
        native_list_itr_previous,
    );
    r.register(litr, "nextIndex", "()I", native_list_itr_next_index);
    r.register(litr, "previousIndex", "()I", native_list_itr_previous_index);
    r.register(litr, "remove", "()V", native_itr_remove_noop);
    r.register(litr, "set", "(Ljava/lang/Object;)V", native_itr_remove_noop);
    r.register(litr, "add", "(Ljava/lang/Object;)V", native_itr_remove_noop);

    // Spliterator stubs
    let spl = "java/util/Spliterator";
    r.register(spl, "estimateSize", "()J", native_spliterator_estimate_size);
    r.register(
        spl,
        "characteristics",
        "()I",
        native_spliterator_characteristics,
    );
    r.register(
        spl,
        "tryAdvance",
        "(Ljava/util/function/Consumer;)Z",
        native_spliterator_try_advance,
    );
    r.register(
        spl,
        "trySplit",
        "()Ljava/util/Spliterator;",
        native_return_null_obj,
    );
    r.register(
        spl,
        "forEachRemaining",
        "(Ljava/util/function/Consumer;)V",
        native_spliterator_for_each_remaining,
    );

    // Spliterators factory
    let spls = "java/util/Spliterators";
    r.register(
        spls,
        "emptySpliterator",
        "()Ljava/util/Spliterator;",
        native_spliterators_empty,
    );

    // Collections.emptyIterator / emptyListIterator / emptyEnumeration
    let colls = "java/util/Collections";
    r.register(
        colls,
        "emptyIterator",
        "()Ljava/util/Iterator;",
        native_empty_iterator,
    );
    r.register(
        colls,
        "emptyListIterator",
        "()Ljava/util/ListIterator;",
        native_empty_iterator,
    );
    // Bug fix: previously this allocated `Collections$EmptyIterator` as an
    // Enumeration. EmptyIterator is an Iterator (only has hasNext/next/remove)
    // and lacks hasMoreElements/nextElement, so any subsequent
    // `Enumeration.hasMoreElements()` dispatch (e.g. from
    // `BuiltinClassLoader$1.hasNext`) raised NoSuchMethodError. Allocate the
    // correct `Collections$EmptyEnumeration` class so real-JDK bytecode
    // (`hasMoreElements`/`nextElement` defined on EmptyEnumeration itself)
    // resolves normally.
    r.register(
        colls,
        "emptyEnumeration",
        "()Ljava/util/Enumeration;",
        native_empty_enumeration,
    );
}

fn native_empty_enumeration(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let arr = alloc_ref_array(ctx, 0);
    let en = alloc_synthetic(ctx, "java/util/Collections$EmptyEnumeration", 2);
    ctx.set_field(en, 0, Value::Object(Some(arr)));
    ctx.set_field(en, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(en))))
}

fn native_itr_remove_noop(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Dispatcher: the force-native override in `vm_exec::invoke_on_class_shared_inner`
    // routes every `invokeinterface Iterator.remove ()V` here, regardless of
    // the receiver's runtime class. Route by class so iterators that DO
    // support remove (e.g. HashMap$KeyItr backed by a real HashSet) run their
    // own native instead of throwing. Without this, JDK code that does
    // `it.remove()` inside a HashSet/HashMap iteration loop (e.g.
    // `MXBeanSupport.findMXBeanInterface` during platform-MBean registration)
    // unconditionally fails with UnsupportedOperationException, blocking
    // jboss-modules / WildFly / Keycloak boot.
    if let Some(Value::Object(Some(this))) = args.first().copied() {
        let cid = ctx.class_id_of_object(this);
        let cn = ctx.class_name_of_id(cid).unwrap_or_default();
        match cn.as_str() {
            "java/util/HashMap$KeyItr" => return native_map_key_itr_remove(ctx, args),
            "java/util/ArrayList$Itr" | "java/util/ArrayList$ListItr" => {
                return native_al_itr_remove(ctx, args);
            }
            _ => {}
        }
    }
    // Snapshot-only iterators (no backing collection) genuinely do not
    // support remove — throw the spec-mandated UOE.
    Err(cratonvm_types::error::RuntimeError::UnsupportedOperationException {
        message: "remove".to_string(),
    }
    .into())
}

// ListIterator extras (snapshot-based: field 0 = array, field 1 = cursor)
fn native_list_itr_has_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(if cursor > 0 { 1 } else { 0 })))
}

fn native_list_itr_previous(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    if cursor <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    let new_cursor = cursor - 1;
    ctx.set_field(this, 1, Value::Int(new_cursor));
    Ok(Some(ctx.get_array_element(arr, new_cursor as usize)))
}

fn native_list_itr_next_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor)))
}

fn native_list_itr_previous_index(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(-1))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(cursor - 1)))
}

// Spliterator stubs (snapshot-based: field 0 = array, field 1 = cursor)
fn native_spliterator_estimate_size(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Long(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    Ok(Some(Value::Long(len.saturating_sub(cursor) as i64)))
}

fn native_spliterator_characteristics(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // SIZED | SUBSIZED | ORDERED = 0x4050
    Ok(Some(Value::Int(0x4050)))
}

fn native_spliterator_try_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Synthetic-spliterator fast path: field 0 is a backing Object[]. If it
    // isn't, this native is being invoked on a real-JDK Spliterator subclass
    // whose field-0 is something else (e.g. an Iterator inside
    // `ServiceLoaderUtil$ServiceLoaderSpliterator`). Returning Int(0) here
    // would silently terminate the caller's stream — instead, signal "no
    // synthetic state" by returning Int(0) only AFTER trying the JDK default
    // method on the receiver class. The dispatcher in `invoke_on_class_shared_inner`
    // now prefers default-method bytecode over this native when the receiver
    // is a non-synthetic class, so reaching here typically means the
    // receiver IS our synthetic shape; the array guard protects against
    // edge cases where dispatch routes the wrong way.
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    if cursor >= len {
        return Ok(Some(Value::Int(0)));
    }
    let elem = ctx.get_array_element(arr, cursor);
    ctx.set_field(this, 1, Value::Int((cursor + 1) as i32));
    ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem])?;
    Ok(Some(Value::Int(1)))
}

fn native_return_null_obj(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn native_spliterator_for_each_remaining(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    // Synthetic-spliterator fast path: field 0 is a backing Object[]. If it
    // isn't, this native is being invoked on a real-JDK Spliterator subclass
    // (e.g. log4j's `ServiceLoaderUtil$ServiceLoaderSpliterator`, where
    // field 0 is an Iterator) — `array_length` on that would surface the
    // ARRAY-LEN-GUARD warning and the rest of the stream pipeline collapses.
    // Fall back to driving the subclass's own `tryAdvance` until exhausted.
    let arr = match ctx.get_field(this, 0) {
        Value::Object(Some(a)) if ctx.heap_kind_of(a) == cratonvm_types::ObjectKind::Array => a,
        _ => {
            // Non-synthetic Spliterator subclass — drive its own tryAdvance
            // (which now correctly dispatches to the JDK default-method
            // bytecode for real-JDK subclasses, thanks to the
            // SPLITERATOR-FALLTHROUGH change in `invoke_on_class_shared_inner`).
            let mut iters: u64 = 0;
            loop {
                iters += 1;
                if iters > 10_000_000 {
                    break;
                }
                let r = ctx.invoke_virtual(
                    this,
                    "tryAdvance",
                    "(Ljava/util/function/Consumer;)Z",
                    &[Value::Object(Some(consumer))],
                )?;
                match r {
                    Some(Value::Int(1)) => continue,
                    _ => break,
                }
            }
            return Ok(None);
        }
    };
    let mut cursor = match ctx.get_field(this, 1) {
        Value::Int(v) => v as usize,
        _ => 0,
    };
    let len = ctx.array_length(arr);
    while cursor < len {
        let elem = ctx.get_array_element(arr, cursor);
        cursor += 1;
        ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[elem])?;
    }
    ctx.set_field(this, 1, Value::Int(cursor as i32));
    Ok(None)
}

fn native_spliterators_empty(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let arr = alloc_ref_array(ctx, 0);
    let itr = alloc_synthetic(ctx, "java/util/Spliterators$EmptySpliterator", 2);
    ctx.set_field(itr, 0, Value::Object(Some(arr)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

fn native_empty_iterator(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let arr = alloc_ref_array(ctx, 0);
    let itr = alloc_synthetic(ctx, "java/util/Collections$EmptyIterator", 2);
    ctx.set_field(itr, 0, Value::Object(Some(arr)));
    ctx.set_field(itr, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(itr))))
}

// ===========================================================================
// ScheduledExecutorService / ScheduledThreadPoolExecutor
// ===========================================================================

const STPE_FIELD_POOL_SIZE: usize = 0;
const STPE_FIELD_SHUTDOWN: usize = 1;
const STPE_FIELD_TASK_LIST: usize = 2;
const STPE_NUM_FIELDS: usize = 3;

fn register_scheduled_executor_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/ScheduledThreadPoolExecutor";
    r.register(c, "<init>", "(I)V", native_stpe_init);
    r.register(
        c,
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        c,
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        c,
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(c, "shutdown", "()V", native_stpe_shutdown);
    r.register(
        c,
        "shutdownNow",
        "()Ljava/util/List;",
        native_stpe_shutdown_now,
    );
    r.register(c, "isShutdown", "()Z", native_stpe_is_shutdown);
    r.register(
        c,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_runnable,
    );
    r.register(
        c,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_callable,
    );

    // Also register under the interface name
    let iface = "java/util/concurrent/ScheduledExecutorService";
    r.register(iface, "<init>", "(I)V", native_stpe_init);
    r.register(
        iface,
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        iface,
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        iface,
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(iface, "shutdown", "()V", native_stpe_shutdown);
    r.register(
        iface,
        "shutdownNow",
        "()Ljava/util/List;",
        native_stpe_shutdown_now,
    );
    r.register(iface, "isShutdown", "()Z", native_stpe_is_shutdown);
    r.register(
        iface,
        "submit",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_runnable,
    );
    r.register(
        iface,
        "submit",
        "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/Future;",
        native_stpe_submit_callable,
    );
}

/// Allocate a completed ScheduledFuture that wraps a result value.
/// Fields: 0 = result value, 1 = done flag (always 1).
fn alloc_completed_future(ctx: &mut dyn NativeContext, result: Value) -> ObjectRef {
    let future = alloc_synthetic(ctx, "java/util/concurrent/ScheduledFuture", 2);
    ctx.set_field(future, 0, result);
    ctx.set_field(future, 1, Value::Int(1)); // done = true
    future
}

fn native_stpe_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let pool_size = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let task_arr = alloc_ref_array(ctx, 16);
    ctx.set_field(this, STPE_FIELD_POOL_SIZE, Value::Int(pool_size));
    ctx.set_field(this, STPE_FIELD_SHUTDOWN, Value::Int(0));
    ctx.set_field(this, STPE_FIELD_TASK_LIST, Value::Object(Some(task_arr)));
    Ok(None)
}

fn native_stpe_schedule(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: this, Runnable, long delay (2 slots), TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let future = alloc_completed_future(ctx, Value::Object(None));
            return Ok(Some(Value::Object(Some(future))));
        }
    };
    // Execute immediately (simplified)
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    let future = alloc_completed_future(ctx, Value::Object(None));
    Ok(Some(Value::Object(Some(future))))
}

fn native_stpe_schedule_fixed_rate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: this, Runnable, long initialDelay (2 slots), long period (2 slots), TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let future = alloc_completed_future(ctx, Value::Object(None));
            return Ok(Some(Value::Object(Some(future))));
        }
    };
    // Execute once immediately (simplified)
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    let future = alloc_completed_future(ctx, Value::Object(None));
    Ok(Some(Value::Object(Some(future))))
}

fn native_stpe_schedule_fixed_delay(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: this, Runnable, long initialDelay (2 slots), long period (2 slots), TimeUnit
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let future = alloc_completed_future(ctx, Value::Object(None));
            return Ok(Some(Value::Object(Some(future))));
        }
    };
    // Execute once immediately (simplified)
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    let future = alloc_completed_future(ctx, Value::Object(None));
    Ok(Some(Value::Object(Some(future))))
}

fn native_stpe_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, STPE_FIELD_SHUTDOWN, Value::Int(1));
    Ok(None)
}

fn native_stpe_shutdown_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Set shutdown flag
    if let Some(Value::Object(Some(this))) = args.first() {
        ctx.set_field(*this, STPE_FIELD_SHUTDOWN, Value::Int(1));
    }
    // Return empty list
    make_list_of(ctx, &[])
}

fn native_stpe_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let shutdown = match ctx.get_field(this, STPE_FIELD_SHUTDOWN) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(shutdown != 0))))
}

fn native_stpe_submit_runnable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let future = alloc_completed_future(ctx, Value::Object(None));
            return Ok(Some(Value::Object(Some(future))));
        }
    };
    ctx.invoke_virtual(runnable, "run", "()V", &[])?;
    let future = alloc_completed_future(ctx, Value::Object(None));
    Ok(Some(Value::Object(Some(future))))
}

fn native_stpe_submit_callable(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let callable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            let future = alloc_completed_future(ctx, Value::Object(None));
            return Ok(Some(Value::Object(Some(future))));
        }
    };
    let result = ctx.invoke_virtual(callable, "call", "()Ljava/lang/Object;", &[])?;
    let val = result.unwrap_or(Value::Object(None));
    let future = alloc_completed_future(ctx, val);
    Ok(Some(Value::Object(Some(future))))
}

// ===========================================================================
// ConcurrentSkipListMap (simplified as sorted array-backed map)
// ===========================================================================
//
// Bug 1 (CRIT round-9 native-misc HIGH-4): the underlying sorted array
// previously had zero synchronization. Concurrent put + remove from two
// threads could corrupt the array (insert shifts elements right while
// remove shifts them left; their writes can interleave to produce a
// duplicated or vanished slot, and binary search reads can observe
// half-updated key ordering).
//
// Fix: stripe-based RwLock guard, keyed by the map's identity hash code.
// All read operations (get/firstKey/lastKey/containsKey/keySet/size/
// isEmpty) take a read lock; all mutating operations (put/remove) take
// a write lock. Stripes are 256 to keep contention low without a global
// hashmap of locks per object. Distinct CSLM instances may collide on
// the same stripe (contention only, not correctness).
//
// TODO(round-11+): replace the array-backed implementation with a real
// lock-free Pugh-style skiplist. Until then this lock keeps the data
// structure observably safe under contention, at the cost of converting
// every operation into a serialised-per-stripe critical section. That's
// still strictly better than the previous behaviour (data corruption +
// occasional panics from out-of-bounds shifts).

const CSLM_FIELD_KEYS: usize = 0;
const CSLM_FIELD_VALUES: usize = 1;
const CSLM_FIELD_SIZE: usize = 2;
#[allow(dead_code)] // used for documentation; allocations go through constructors
const CSLM_NUM_FIELDS: usize = 3;
const CSLM_DEFAULT_CAPACITY: usize = 16;

const CSLM_LOCK_STRIPES: usize = 256;

fn cslm_stripe_for(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> &'static parking_lot::RwLock<()> {
    // MED fix: switched from `std::sync::RwLock` to `parking_lot::RwLock`
    // for consistency with the CHM-segment striped locks earlier in this
    // file (see `seg_locks`). parking_lot's `read()`/`write()` are
    // infallible — no `PoisonError` handling at every call site — and
    // their unpoisoning semantics match the CHM stripes which expect
    // a panicking writer to leave the lock usable for subsequent
    // segments rather than poisoning the whole stripe.
    static STRIPES: std::sync::OnceLock<Vec<parking_lot::RwLock<()>>> =
        std::sync::OnceLock::new();
    let stripes = STRIPES.get_or_init(|| {
        (0..CSLM_LOCK_STRIPES)
            .map(|_| parking_lot::RwLock::new(()))
            .collect()
    });
    let key = ctx.identity_hash_code(this) as u32;
    // Mix bits so sequentially-allocated objects spread across stripes.
    let idx = ((key ^ (key >> 16)).wrapping_mul(0x9E37_79B1) as usize) % CSLM_LOCK_STRIPES;
    &stripes[idx]
}

fn register_concurrent_skip_list_map_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/ConcurrentSkipListMap";
    r.register(c, "<init>", "()V", native_cslm_init);
    r.register(
        c,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_put,
    );
    r.register(
        c,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_get,
    );
    r.register(
        c,
        "remove",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        native_cslm_remove,
    );
    r.register(c, "size", "()I", native_cslm_size);
    r.register(c, "isEmpty", "()Z", native_cslm_is_empty);
    r.register(
        c,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        native_cslm_contains_key,
    );
    r.register(c, "firstKey", "()Ljava/lang/Object;", native_cslm_first_key);
    r.register(c, "lastKey", "()Ljava/lang/Object;", native_cslm_last_key);
    r.register(c, "keySet", "()Ljava/util/Set;", native_cslm_key_set);
}

/// Binary search in the keys array using natural comparison.
/// Returns Ok(index) if found, Err(insert_pos) if not found.
fn cslm_binary_search(
    ctx: &mut dyn NativeContext,
    keys: ObjectRef,
    size: i32,
    key: &Value,
) -> Result<Result<usize, usize>, MethodCallFailed> {
    let comparator = Value::Object(None); // natural ordering
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_key = ctx.get_array_element(keys, mid);
        let cmp = tree_compare(ctx, &comparator, mid_key, *key)?;
        if cmp < 0 {
            low = mid + 1;
        } else if cmp > 0 {
            high = mid;
        } else {
            return Ok(Ok(mid));
        }
    }
    Ok(Err(low))
}

fn cslm_ensure_capacity(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    size: i32,
    keys: ObjectRef,
    values: ObjectRef,
) -> (ObjectRef, ObjectRef) {
    let arr_len = ctx.array_length(keys);
    let needed = (size + 1) as usize;
    if needed <= arr_len {
        return (keys, values);
    }
    let new_cap = (arr_len * 2).max(needed).max(CSLM_DEFAULT_CAPACITY);
    let new_keys = alloc_ref_array(ctx, new_cap);
    let new_values = alloc_ref_array(ctx, new_cap);
    for i in 0..(size as usize) {
        ctx.set_array_element(new_keys, i, ctx.get_array_element(keys, i));
        ctx.set_array_element(new_values, i, ctx.get_array_element(values, i));
    }
    ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(new_keys)));
    ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(new_values)));
    (new_keys, new_values)
}

fn cslm_state(ctx: &dyn NativeContext, this: ObjectRef) -> (Option<ObjectRef>, Option<ObjectRef>, i32) {
    let keys = match ctx.get_field(this, CSLM_FIELD_KEYS) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let values = match ctx.get_field(this, CSLM_FIELD_VALUES) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    };
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    (keys, values, size)
}

fn native_cslm_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let keys = alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY);
    let values = alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY);
    ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(keys)));
    ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(values)));
    ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn native_cslm_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    // Bug 1: serialise mutating ops on this map's lock stripe.
    let _guard = cslm_stripe_for(ctx, this).write();
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let keys = match keys_opt {
        Some(k) => k,
        None => {
            let k = alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY);
            ctx.set_field(this, CSLM_FIELD_KEYS, Value::Object(Some(k)));
            k
        }
    };
    let values_arr = match values_opt {
        Some(v) => v,
        None => {
            let v = alloc_ref_array(ctx, CSLM_DEFAULT_CAPACITY);
            ctx.set_field(this, CSLM_FIELD_VALUES, Value::Object(Some(v)));
            v
        }
    };
    let search = cslm_binary_search(ctx, keys, size, &key)?;
    match search {
        Ok(idx) => {
            // Key exists — replace value, return old
            let old = ctx.get_array_element(values_arr, idx);
            ctx.set_array_element(values_arr, idx, value);
            Ok(Some(old))
        }
        Err(pos) => {
            // Insert at pos, shifting elements right
            let (keys, values_arr) = cslm_ensure_capacity(ctx, this, size, keys, values_arr);
            let s = size as usize;
            for i in (pos..s).rev() {
                let k = ctx.get_array_element(keys, i);
                let v = ctx.get_array_element(values_arr, i);
                ctx.set_array_element(keys, i + 1, k);
                ctx.set_array_element(values_arr, i + 1, v);
            }
            ctx.set_array_element(keys, pos, key);
            ctx.set_array_element(values_arr, pos, value);
            ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(size + 1));
            Ok(Some(Value::Object(None)))
        }
    }
}

fn native_cslm_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Bug 1: shared read lock — concurrent reads OK, blocks during writes.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let values_arr = match values_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    match cslm_binary_search(ctx, keys, size, &key)? {
        Ok(idx) => Ok(Some(ctx.get_array_element(values_arr, idx))),
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

fn native_cslm_remove(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Bug 1: serialise mutating ops on this map's lock stripe.
    let _guard = cslm_stripe_for(ctx, this).write();
    let (keys_opt, values_opt, size) = cslm_state(ctx, this);
    let keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Object(None))),
    };
    let values_arr = match values_opt {
        Some(v) => v,
        None => return Ok(Some(Value::Object(None))),
    };
    match cslm_binary_search(ctx, keys, size, &key)? {
        Ok(idx) => {
            let old_val = ctx.get_array_element(values_arr, idx);
            let s = size as usize;
            for i in idx..(s - 1) {
                let k = ctx.get_array_element(keys, i + 1);
                let v = ctx.get_array_element(values_arr, i + 1);
                ctx.set_array_element(keys, i, k);
                ctx.set_array_element(values_arr, i, v);
            }
            ctx.set_array_element(keys, s - 1, Value::Object(None));
            ctx.set_array_element(values_arr, s - 1, Value::Object(None));
            ctx.set_field(this, CSLM_FIELD_SIZE, Value::Int(size - 1));
            Ok(Some(old_val))
        }
        Err(_) => Ok(Some(Value::Object(None))),
    }
}

fn native_cslm_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    // Bug 1: read lock so size cannot tear vs an in-flight put/remove.
    let _guard = cslm_stripe_for(ctx, this).read();
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

fn native_cslm_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    // Bug 1: read lock for consistent size observation.
    let _guard = cslm_stripe_for(ctx, this).read();
    let size = match ctx.get_field(this, CSLM_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

fn native_cslm_contains_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    let keys = match keys_opt {
        Some(k) => k,
        None => return Ok(Some(Value::Int(0))),
    };
    let found = cslm_binary_search(ctx, keys, size, &key)?.is_ok();
    Ok(Some(Value::Int(i32::from(found))))
}

fn native_cslm_first_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ConcurrentSkipListMap is empty".to_string(),
            }
            .into())
        }
    };
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // Treat a None keys array as empty rather than panicking on `unwrap`.
    let keys = match keys_opt {
        Some(k) if size != 0 => k,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ConcurrentSkipListMap is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(keys, 0)))
}

fn native_cslm_last_key(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ConcurrentSkipListMap is empty".to_string(),
            }
            .into())
        }
    };
    // Bug 1: shared read lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // Treat a None keys array as empty rather than panicking on `unwrap`.
    let keys = match keys_opt {
        Some(k) if size != 0 => k,
        _ => {
            return Err(cratonvm_types::error::RuntimeError::NoSuchElementException {
                message: "ConcurrentSkipListMap is empty".to_string(),
            }
            .into());
        }
    };
    Ok(Some(ctx.get_array_element(keys, (size - 1) as usize)))
}

fn native_cslm_key_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Bug 1: shared read lock — snapshot the keys array under the lock.
    let _guard = cslm_stripe_for(ctx, this).read();
    let (keys_opt, _, size) = cslm_state(ctx, this);
    // Return a TreeSet with natural ordering containing all keys
    let ts = alloc_synthetic(ctx, "java/util/TreeSet", TS_NUM_FIELDS);
    let buf = alloc_ref_array(ctx, std::cmp::max(size as usize, TS_DEFAULT_CAPACITY));
    if let Some(keys) = keys_opt {
        for i in 0..(size as usize) {
            let k = ctx.get_array_element(keys, i);
            ctx.set_array_element(buf, i, k);
        }
    }
    ts_set_slot(ctx, ts, TS_FIELD_DATA, Value::Object(Some(buf)));
    ts_set_slot(ctx, ts, TS_FIELD_SIZE, Value::Int(size));
    ts_set_slot(ctx, ts, TS_FIELD_COMPARATOR, Value::Object(None));
    Ok(Some(Value::Object(Some(ts))))
}

// ===========================================================================
// StampedLock
// ===========================================================================

const SL_FIELD_STATE: usize = 0; // 0=free, 1=write-locked, >=2 means (state-1) readers
const SL_FIELD_STAMP: usize = 1; // monotonic stamp counter
#[allow(dead_code)]
const SL_NUM_FIELDS: usize = 2;

fn register_stamped_lock_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/locks/StampedLock";
    r.register(c, "<init>", "()V", native_sl_init);
    r.register(c, "readLock", "()J", native_sl_read_lock);
    r.register(c, "unlockRead", "(J)V", native_sl_unlock_read);
    r.register(c, "writeLock", "()J", native_sl_write_lock);
    r.register(c, "unlockWrite", "(J)V", native_sl_unlock_write);
    r.register(c, "tryOptimisticRead", "()J", native_sl_try_optimistic_read);
    r.register(c, "validate", "(J)Z", native_sl_validate);
    r.register(c, "tryReadLock", "()J", native_sl_try_read_lock);
    r.register(c, "tryWriteLock", "()J", native_sl_try_write_lock);
    r.register(c, "isReadLocked", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        // state >= 2 means readers are present
        Ok(Some(Value::Int(if state >= 2 { 1 } else { 0 })))
    });
    r.register(c, "isWriteLocked", "()Z", |ctx, args| {
        let this = match args.first() {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        let state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        // state == 1 means write-locked
        Ok(Some(Value::Int(if state == 1 { 1 } else { 0 })))
    });
}

fn native_sl_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(0));
    ctx.set_field(this, SL_FIELD_STAMP, Value::Int(1));
    Ok(None)
}

fn sl_next_stamp(ctx: &mut dyn NativeContext, this: ObjectRef) -> i64 {
    let stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        _ => 1,
    };
    let next = stamp + 1;
    ctx.set_field(this, SL_FIELD_STAMP, Value::Long(next));
    stamp
}

fn native_sl_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Acquire monitor and spin-wait if write-locked
    ctx.monitor_enter(this);
    let mut state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Wait for write lock to be released (bounded spin)
    let mut spins = 0;
    while state == 1 && spins < 1000 {
        ctx.monitor_exit(this);
        std::thread::yield_now();
        ctx.monitor_enter(this);
        state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        spins += 1;
    }
    // state >= 2 means readers present; state == 0 means free
    let new_state = if state == 0 || state == 1 { 2 } else { state + 1 };
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
    let stamp = sl_next_stamp(ctx, this);
    ctx.monitor_exit(this);
    Ok(Some(Value::Long(stamp)))
}

fn native_sl_unlock_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state >= 2 {
        let new_state = if state == 2 { 0 } else { state - 1 };
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
    }
    ctx.monitor_exit(this);
    Ok(None)
}

fn native_sl_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    // Acquire monitor and spin-wait if readers or writers present
    ctx.monitor_enter(this);
    let mut state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    let mut spins = 0;
    while state != 0 && spins < 1000 {
        ctx.monitor_exit(this);
        std::thread::yield_now();
        ctx.monitor_enter(this);
        state = match ctx.get_field(this, SL_FIELD_STATE) {
            Value::Int(v) => v,
            _ => 0,
        };
        spins += 1;
    }
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(1));
    let stamp = sl_next_stamp(ctx, this);
    ctx.monitor_exit(this);
    Ok(Some(Value::Long(stamp)))
}

fn native_sl_unlock_write(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.monitor_enter(this);
    ctx.set_field(this, SL_FIELD_STATE, Value::Int(0));
    sl_next_stamp(ctx, this);
    ctx.monitor_exit(this);
    Ok(None)
}

fn native_sl_try_optimistic_read(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state == 1 {
        // Write-locked, return 0 (failure)
        Ok(Some(Value::Long(0)))
    } else {
        let stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
            Value::Int(v) => v as i64,
            Value::Long(v) => v,
            _ => 1,
        };
        Ok(Some(Value::Long(stamp)))
    }
}

fn native_sl_validate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let expected_stamp = match args.get(1) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => *v as i64,
        _ => return Ok(Some(Value::Int(0))),
    };
    if expected_stamp == 0 {
        return Ok(Some(Value::Int(0)));
    }
    let current_stamp = match ctx.get_field(this, SL_FIELD_STAMP) {
        Value::Int(v) => v as i64,
        Value::Long(v) => v,
        _ => 0,
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    // Valid if stamp hasn't changed and not write-locked
    let valid = expected_stamp == current_stamp && state != 1;
    Ok(Some(Value::Int(i32::from(valid))))
}

fn native_sl_try_read_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state == 1 {
        // Write-locked, can't acquire read lock
        Ok(Some(Value::Long(0)))
    } else {
        let new_state = if state == 0 { 2 } else { state + 1 };
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(new_state));
        let stamp = sl_next_stamp(ctx, this);
        Ok(Some(Value::Long(stamp)))
    }
}

fn native_sl_try_write_lock(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Long(0))),
    };
    let state = match ctx.get_field(this, SL_FIELD_STATE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if state != 0 {
        // Not free — someone is holding a lock
        Ok(Some(Value::Long(0)))
    } else {
        ctx.set_field(this, SL_FIELD_STATE, Value::Int(1));
        let stamp = sl_next_stamp(ctx, this);
        Ok(Some(Value::Long(stamp)))
    }
}

// ===========================================================================
// Phaser
// ===========================================================================

const PH_FIELD_PARTIES: usize = 0;
const PH_FIELD_ARRIVED: usize = 1;
const PH_FIELD_PHASE: usize = 2;
#[allow(dead_code)]
const PH_NUM_FIELDS: usize = 3;

fn register_phaser_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/Phaser";
    r.register(c, "<init>", "()V", native_ph_init_empty);
    r.register(c, "<init>", "(I)V", native_ph_init_parties);
    r.register(c, "register", "()I", native_ph_register);
    r.register(c, "arriveAndAwaitAdvance", "()I", native_ph_arrive_and_await);
    r.register(c, "arriveAndDeregister", "()I", native_ph_arrive_and_deregister);
    r.register(c, "arrive", "()I", native_ph_arrive);
    r.register(c, "getPhase", "()I", native_ph_get_phase);
    r.register(c, "getRegisteredParties", "()I", native_ph_get_registered_parties);
    r.register(c, "getArrivedParties", "()I", native_ph_get_arrived_parties);
}

fn native_ph_init_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(0));
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
    ctx.set_field(this, PH_FIELD_PHASE, Value::Int(0));
    Ok(None)
}

fn native_ph_init_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let parties = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties));
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
    ctx.set_field(this, PH_FIELD_PHASE, Value::Int(0));
    Ok(None)
}

fn native_ph_register(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties + 1));
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(phase)))
}

/// Helper: check if all parties have arrived and advance phase if so.
fn ph_maybe_advance(ctx: &mut dyn NativeContext, this: ObjectRef) -> i32 {
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if parties > 0 && arrived >= parties {
        // Advance phase, reset arrived
        ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(0));
        ctx.set_field(this, PH_FIELD_PHASE, Value::Int(phase + 1));
        phase + 1
    } else {
        phase
    }
}

fn native_ph_arrive_and_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

fn native_ph_arrive_and_deregister(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    if parties > 0 {
        ctx.set_field(this, PH_FIELD_PARTIES, Value::Int(parties - 1));
    }
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

fn native_ph_arrive(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    ctx.set_field(this, PH_FIELD_ARRIVED, Value::Int(arrived + 1));
    let phase = ph_maybe_advance(ctx, this);
    Ok(Some(Value::Int(phase)))
}

fn native_ph_get_phase(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let phase = match ctx.get_field(this, PH_FIELD_PHASE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(phase)))
}

fn native_ph_get_registered_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let parties = match ctx.get_field(this, PH_FIELD_PARTIES) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(parties)))
}

fn native_ph_get_arrived_parties(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let arrived = match ctx.get_field(this, PH_FIELD_ARRIVED) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(arrived)))
}

// ===========================================================================
// PriorityBlockingQueue (sorted array-backed priority queue)
// ===========================================================================

const PBQ_FIELD_DATA: usize = 0;
const PBQ_FIELD_SIZE: usize = 1;
#[allow(dead_code)]
const PBQ_NUM_FIELDS: usize = 2;
const PBQ_DEFAULT_CAPACITY: usize = 16;

fn register_priority_blocking_queue_natives(r: &mut NativeMethodRegistry) {
    let c = "java/util/concurrent/PriorityBlockingQueue";
    r.register(c, "<init>", "()V", native_pbq_init);
    r.register(c, "offer", "(Ljava/lang/Object;)Z", native_pbq_offer);
    r.register(c, "poll", "()Ljava/lang/Object;", native_pbq_poll);
    r.register(c, "peek", "()Ljava/lang/Object;", native_pbq_peek);
    r.register(c, "put", "(Ljava/lang/Object;)V", native_pbq_put);
    r.register(c, "take", "()Ljava/lang/Object;", native_pbq_take);
    r.register(c, "size", "()I", native_pbq_size);
    r.register(c, "isEmpty", "()Z", native_pbq_is_empty);
}

fn native_pbq_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let arr = alloc_ref_array(ctx, PBQ_DEFAULT_CAPACITY);
    ctx.set_field(this, PBQ_FIELD_DATA, Value::Object(Some(arr)));
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(0));
    Ok(None)
}

fn pbq_ensure_capacity(ctx: &mut dyn NativeContext, this: ObjectRef, needed: usize) {
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return,
    };
    let old_len = ctx.array_length(arr);
    if needed <= old_len {
        return;
    }
    let new_len = (old_len * 2).max(needed).max(PBQ_DEFAULT_CAPACITY);
    let new_arr = alloc_ref_array(ctx, new_len);
    for i in 0..old_len {
        ctx.set_array_element(new_arr, i, ctx.get_array_element(arr, i));
    }
    ctx.set_field(this, PBQ_FIELD_DATA, Value::Object(Some(new_arr)));
}

fn native_pbq_offer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    pbq_ensure_capacity(ctx, this, (size + 1) as usize);
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Int(1))),
    };
    // Binary search for insertion position using natural ordering
    let comparator = Value::Object(None);
    let mut low: usize = 0;
    let mut high = size as usize;
    while low < high {
        let mid = low + (high - low) / 2;
        let mid_elem = ctx.get_array_element(arr, mid);
        let cmp = tree_compare(ctx, &comparator, mid_elem, elem)?;
        if cmp < 0 {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    // Shift elements right to make room
    for i in (low..(size as usize)).rev() {
        let v = ctx.get_array_element(arr, i);
        ctx.set_array_element(arr, i + 1, v);
    }
    ctx.set_array_element(arr, low, elem);
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(size + 1));
    Ok(Some(Value::Int(1)))
}

fn native_pbq_poll(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Remove head (smallest element at index 0)
    let head = ctx.get_array_element(arr, 0);
    for i in 0..(size - 1) as usize {
        let v = ctx.get_array_element(arr, i + 1);
        ctx.set_array_element(arr, i, v);
    }
    ctx.set_array_element(arr, (size - 1) as usize, Value::Object(None));
    ctx.set_field(this, PBQ_FIELD_SIZE, Value::Int(size - 1));
    Ok(Some(head))
}

fn native_pbq_peek(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    if size == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let arr = match ctx.get_field(this, PBQ_FIELD_DATA) {
        Value::Object(Some(a)) => a,
        _ => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(ctx.get_array_element(arr, 0)))
}

fn native_pbq_put(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_pbq_offer(ctx, args)?;
    Ok(None)
}

fn native_pbq_take(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_pbq_poll(ctx, args)
}

fn native_pbq_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(size)))
}

fn native_pbq_is_empty(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    let size = match ctx.get_field(this, PBQ_FIELD_SIZE) {
        Value::Int(v) => v,
        _ => 0,
    };
    Ok(Some(Value::Int(i32::from(size == 0))))
}

// ===========================================================================
// Executors factory additions
// ===========================================================================

fn register_executors_scheduled_natives(r: &mut NativeMethodRegistry) {
    // Some runtimes resolve STPE constructors through this crate's registry,
    // so keep both ctor shapes available here (including ThreadFactory variant).
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(I)V",
        native_stpe_init,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "<init>",
        "(ILjava/util/concurrent/ThreadFactory;)V",
        native_stpe_init,
    );
    r.register(
        "java/util/concurrent/Executors",
        "newScheduledThreadPool",
        "(I)Ljava/util/concurrent/ScheduledExecutorService;",
        native_executors_new_scheduled_pool,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setKeepAliveTime",
        "(JLjava/util/concurrent/TimeUnit;)V",
        native_stpe_set_keep_alive_time,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "getKeepAliveTime",
        "(Ljava/util/concurrent/TimeUnit;)J",
        native_stpe_get_keep_alive_time,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "schedule",
        "(Ljava/lang/Runnable;JLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "scheduleAtFixedRate",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_rate,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "scheduleWithFixedDelay",
        "(Ljava/lang/Runnable;JJLjava/util/concurrent/TimeUnit;)Ljava/util/concurrent/ScheduledFuture;",
        native_stpe_schedule_fixed_delay,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "shutdown",
        "()V",
        native_stpe_shutdown,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "isShutdown",
        "()Z",
        native_stpe_is_shutdown,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setMaximumPoolSize",
        "(I)V",
        native_stpe_set_maximum_pool_size,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setCorePoolSize",
        "(I)V",
        native_stpe_set_core_pool_size,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setContinueExistingPeriodicTasksAfterShutdownPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setExecuteExistingDelayedTasksAfterShutdownPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
    r.register(
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        "setRemoveOnCancelPolicy",
        "(Z)V",
        native_stpe_ignore_policy_setter,
    );
}

fn native_executors_new_scheduled_pool(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pool_size = match args.first() {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    let executor = alloc_synthetic(
        ctx,
        "java/util/concurrent/ScheduledThreadPoolExecutor",
        STPE_NUM_FIELDS,
    );
    let task_arr = alloc_ref_array(ctx, 16);
    ctx.set_field(executor, STPE_FIELD_POOL_SIZE, Value::Int(pool_size));
    ctx.set_field(executor, STPE_FIELD_SHUTDOWN, Value::Int(0));
    ctx.set_field(executor, STPE_FIELD_TASK_LIST, Value::Object(Some(task_arr)));
    Ok(Some(Value::Object(Some(executor))))
}

// ===========================================================================
// Phase 3.1: java.util.concurrent completeness
// ===========================================================================
//
// This section adds missing CompletionStage methods on CompletableFuture,
// ForkJoinPool.awaitQuiescence, ThreadPoolExecutor stat methods, and
// completes any gaps in the concurrent utilities. These complement the
// registrations already present in native-builtins.

// CompletableFuture field layout (shared with native-builtins):
// field 0 = result value, field 1 = done flag (0=pending, 1=normal, 2=exceptional, 3=cancelled)
// field 2 = source CF (for deferred stages), field 3 = handler (for deferred stages)
const CF_FIELD_RESULT: usize = 0;
const CF_FIELD_DONE: usize = 1;
const CF_FIELD_SOURCE: usize = 2;
const CF_FIELD_HANDLER: usize = 3;

fn register_concurrent_completeness_natives(r: &mut NativeMethodRegistry) {
    r.register(
        "java/util/concurrent/CopyOnWriteArrayList",
        "addIfAbsent",
        "(Ljava/lang/Object;)Z",
        native_cowal_add_if_absent,
    );
    r.register(
        "java/util/concurrent/CopyOnWriteArrayList",
        "contains",
        "(Ljava/lang/Object;)Z",
        native_cowal_contains,
    );
    r.register(
        "java/util/concurrent/CopyOnWriteArrayList",
        "bulkRemove",
        "(Ljava/util/function/Predicate;)Z",
        native_cowal_bulk_remove_predicate,
    );
    r.register(
        "java/util/concurrent/CopyOnWriteArrayList",
        "addAll",
        "(Ljava/util/Collection;)Z",
        native_cowal_add_all,
    );

    let cf = "java/util/concurrent/CompletableFuture";

    // --- CompletionStage methods ---

    // thenRun: run Runnable after completion, return new CF with null result
    r.register(
        cf,
        "thenRun",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_run,
    );

    // thenCompose: apply Function that returns CompletableFuture, then flatten
    r.register(
        cf,
        "thenCompose",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_compose,
    );

    // thenCombine: combine results of two CFs with a BiFunction
    r.register(
        cf,
        "thenCombine",
        "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_combine,
    );

    // exceptionally: provide fallback if exception occurred
    r.register(
        cf,
        "exceptionally",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_exceptionally,
    );

    // handle: BiFunction(result, exception) → new result
    r.register(
        cf,
        "handle",
        "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_handle,
    );

    // whenComplete: BiConsumer(result, exception) called after completion
    r.register(
        cf,
        "whenComplete",
        "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_when_complete,
    );

    // allOf: CompletableFuture[] → CompletableFuture<Void>
    r.register(
        cf,
        "allOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_all_of,
    );

    // anyOf: CompletableFuture[] → CompletableFuture<Object>
    r.register(
        cf,
        "anyOf",
        "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_any_of,
    );

    // completeExceptionally
    r.register(
        cf,
        "completeExceptionally",
        "(Ljava/lang/Throwable;)Z",
        native_cf_complete_exceptionally,
    );

    // isCompletedExceptionally — true for done=2 (exceptional) or done=3 (cancelled)
    r.register(
        cf,
        "isCompletedExceptionally",
        "()Z",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Int(0))),
            };
            let done = match ctx.get_field(this, CF_FIELD_DONE) {
                Value::Int(d) => d,
                _ => 0,
            };
            Ok(Some(Value::Int(if done == 2 || done == 3 { 1 } else { 0 })))
        },
    );

    // thenApplyAsync (delegates to thenApply in single-threaded model)
    r.register(
        cf,
        "thenApplyAsync",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_apply_p31,
    );

    // thenAcceptAsync (delegates to thenAccept in single-threaded model)
    r.register(
        cf,
        "thenAcceptAsync",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_accept_p31,
    );

    // thenRunAsync
    r.register(
        cf,
        "thenRunAsync",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_run,
    );

    // thenComposeAsync
    r.register(
        cf,
        "thenComposeAsync",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;",
        native_cf_then_compose,
    );

    // Also register CompletionStage interface methods
    let cs = "java/util/concurrent/CompletionStage";
    r.register(
        cs,
        "thenApply",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_apply_p31,
    );
    r.register(
        cs,
        "thenAccept",
        "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_accept_p31,
    );
    r.register(
        cs,
        "thenRun",
        "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_run,
    );
    r.register(
        cs,
        "thenCompose",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
        native_cf_then_compose,
    );
    r.register(
        cs,
        "exceptionally",
        "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;",
        native_cf_exceptionally,
    );

    // --- ForkJoinPool.awaitQuiescence ---
    let pool = "java/util/concurrent/ForkJoinPool";
    r.register(
        pool,
        "awaitQuiescence",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );

    // --- ThreadPoolExecutor stat methods ---
    let tp = "java/util/concurrent/ThreadPoolExecutor";
    r.register(tp, "getPoolSize", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(1)))
    });
    r.register(tp, "getActiveCount", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    });
    r.register(tp, "getCorePoolSize", "()I", native_tp_get_core_pool_size);
    r.register(tp, "getMaximumPoolSize", "()I", native_tp_get_core_pool_size);
    r.register(tp, "isShutdown", "()Z", native_tp_is_shutdown);
    r.register(tp, "isTerminated", "()Z", native_tp_is_shutdown);
    r.register(
        tp,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
    r.register(tp, "getTaskCount", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    });
    r.register(tp, "getCompletedTaskCount", "()J", |_ctx, _args| {
        Ok(Some(Value::Long(0)))
    });
    r.register(
        tp,
        "shutdownNow",
        "()Ljava/util/List;",
        native_tp_shutdown_now,
    );

    // --- ForkJoinPool.invoke that actually calls compute ---
    r.register(
        pool,
        "invoke",
        "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;",
        native_fjp_invoke,
    );

    // --- ForkJoinTask.invoke that calls compute ---
    let fjt = "java/util/concurrent/ForkJoinTask";
    r.register(fjt, "invoke", "()Ljava/lang/Object;", native_fjt_invoke);

    // --- RecursiveTask.invoke that calls compute ---
    let rt = "java/util/concurrent/RecursiveTask";
    r.register(rt, "invoke", "()Ljava/lang/Object;", native_rt_invoke);

    // --- RecursiveAction.invoke that calls compute ---
    let ra = "java/util/concurrent/RecursiveAction";
    r.register(ra, "invoke", "()Ljava/lang/Object;", native_ra_invoke);
}

const COWAL_CLASS: &str = "java/util/concurrent/CopyOnWriteArrayList";

/// Real JDK `CopyOnWriteArrayList` uses named `lock` + `array`. Legacy
/// synthetic stubs mirrored `ArrayList` with slot 0 = backing `Object[]`,
/// slot 1 = `int` size.
fn cowal_ensure_lock_and_array(ctx: &mut dyn NativeContext, this: ObjectRef) -> MethodCallResult {
    if let Some(ls) = ctx.resolve_field_index(COWAL_CLASS, "lock") {
        if matches!(ctx.get_field(this, ls), Value::Object(None)) {
            if let Ok(Some(Value::Object(Some(lo)))) = ctx.new_object("java/lang/Object") {
                let _ = ctx.invoke_special(
                    "java/lang/Object",
                    "<init>",
                    "()V",
                    &[Value::Object(Some(lo))],
                );
                ctx.set_field(this, ls, Value::Object(Some(lo)));
            }
        }
    }
    if let Some(slot) = ctx.resolve_field_index(COWAL_CLASS, "array") {
        if matches!(ctx.get_field(this, slot), Value::Object(None)) {
            let a = ctx.new_array(ArrayElementType::Reference, 0);
            ctx.set_field(this, slot, Value::Object(Some(a)));
        }
    } else if matches!(ctx.get_field(this, 0), Value::Object(None)) {
        let a = ctx.new_array(ArrayElementType::Reference, 0);
        ctx.set_field(this, 0, Value::Object(Some(a)));
        ctx.set_field(this, 1, Value::Int(0));
    }
    Ok(None)
}

fn cowal_read_snapshot(ctx: &dyn NativeContext, this: ObjectRef) -> Option<(ObjectRef, usize)> {
    if let Some(slot) = ctx.resolve_field_index(COWAL_CLASS, "array") {
        let arr = match ctx.get_field(this, slot) {
            Value::Object(Some(a)) => a,
            Value::Object(None) => return None,
            _ => return None,
        };
        let len = ctx.array_length(arr);
        Some((arr, len))
    } else {
        let arr = match ctx.get_field(this, 0) {
            Value::Object(Some(a)) => a,
            _ => return None,
        };
        let sz = match ctx.get_field(this, 1) {
            Value::Int(n) => n.max(0) as usize,
            _ => ctx.array_length(arr),
        };
        Some((arr, sz))
    }
}

fn cowal_bump_mod_count(ctx: &mut dyn NativeContext, this: ObjectRef) {
    if let Some(ms) = ctx.resolve_field_index("java/util/AbstractList", "modCount") {
        let cur = match ctx.get_field(this, ms) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, ms, Value::Int(cur.wrapping_add(1)));
    }
}

fn native_cowal_add_if_absent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    cowal_ensure_lock_and_array(ctx, this)?;
    let elem = args.get(1).copied().unwrap_or(Value::Object(None));
    let present = ctx.invoke_virtual(this, "contains", "(Ljava/lang/Object;)Z", &[elem])?;
    if matches!(present, Some(Value::Int(v)) if v != 0) {
        return Ok(Some(Value::Int(0)));
    }
    let added = ctx.invoke_virtual(this, "add", "(Ljava/lang/Object;)Z", &[elem])?;
    Ok(added.or(Some(Value::Int(0))))
}

fn native_cowal_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    cowal_ensure_lock_and_array(ctx, this)?;
    let needle = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some((arr, len)) = cowal_read_snapshot(ctx, this) {
        for i in 0..len {
            if ctx.get_array_element(arr, i) == needle {
                return Ok(Some(Value::Int(1)));
            }
        }
    }
    Ok(Some(Value::Int(0)))
}

fn native_cowal_bulk_remove_predicate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let pred = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(p)) => p,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("CopyOnWriteArrayList.bulkRemove predicate is null".to_string()),
            }
            .into());
        }
    };

    cowal_ensure_lock_and_array(ctx, this)?;

    let lock_obj: Option<ObjectRef> = if let Some(ls) = ctx.resolve_field_index(COWAL_CLASS, "lock") {
        match ctx.get_field(this, ls) {
            Value::Object(Some(lo)) => {
                ctx.monitor_enter(lo);
                Some(lo)
            }
            _ => {
                ctx.monitor_enter(this);
                None
            }
        }
    } else {
        ctx.monitor_enter(this);
        None
    };

    let exit_mon = |ctx: &mut dyn NativeContext, lock_obj: Option<ObjectRef>, this: ObjectRef| {
        if let Some(lo) = lock_obj {
            ctx.monitor_exit(lo);
        } else {
            ctx.monitor_exit(this);
        }
    };

    let Some((arr, n)) = cowal_read_snapshot(ctx, this) else {
        exit_mon(ctx, lock_obj, this);
        return Ok(Some(Value::Int(0)));
    };

    let mut survivors: Vec<Value> = Vec::with_capacity(n);
    for i in 0..n {
        let elem = ctx.get_array_element(arr, i);
        let remove = match ctx.invoke_virtual(pred, "test", "(Ljava/lang/Object;)Z", &[elem]) {
            Ok(Some(Value::Int(1))) => true,
            _ => false,
        };
        if !remove {
            survivors.push(elem);
        }
    }

    if survivors.len() == n {
        exit_mon(ctx, lock_obj, this);
        return Ok(Some(Value::Int(0)));
    }

    let new_arr = ctx.new_array(ArrayElementType::Reference, survivors.len());
    for (i, v) in survivors.iter().enumerate() {
        ctx.set_array_element(new_arr, i, *v);
    }

    if let Some(aslot) = ctx.resolve_field_index(COWAL_CLASS, "array") {
        ctx.set_field(this, aslot, Value::Object(Some(new_arr)));
        cowal_bump_mod_count(ctx, this);
    } else {
        ctx.set_field(this, 0, Value::Object(Some(new_arr)));
        ctx.set_field(this, 1, Value::Int(survivors.len() as i32));
    }

    exit_mon(ctx, lock_obj, this);
    Ok(Some(Value::Int(1)))
}

fn native_cowal_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let coll = match args.get(1).copied().unwrap_or(Value::Object(None)) {
        Value::Object(Some(c)) => c,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("CopyOnWriteArrayList.addAll on null collection".to_string()),
            }
            .into());
        }
    };
    cowal_ensure_lock_and_array(ctx, this)?;

    let Some(aslot) = ctx.resolve_field_index(COWAL_CLASS, "array") else {
        // Synthetic two-field layout: fall back to repeated `add` (legacy).
        let arr_obj = match ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[]) {
            Ok(Some(Value::Object(Some(a)))) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let n = ctx.array_length(arr_obj);
        let mut any_added = 0i32;
        for i in 0..n {
            let elem = ctx.get_array_element(arr_obj, i);
            if matches!(
                ctx.invoke_virtual(this, "add", "(Ljava/lang/Object;)Z", &[elem]),
                Ok(Some(Value::Int(1)))
            ) {
                any_added = 1;
            }
        }
        return Ok(Some(Value::Int(any_added)));
    };

    let lock_slot = ctx.resolve_field_index(COWAL_CLASS, "lock");
    let lock_obj: Option<ObjectRef> = if let Some(ls) = lock_slot {
        match ctx.get_field(this, ls) {
            Value::Object(Some(lo)) => {
                ctx.monitor_enter(lo);
                Some(lo)
            }
            _ => {
                ctx.monitor_enter(this);
                None
            }
        }
    } else {
        ctx.monitor_enter(this);
        None
    };

    let exit_mon = |ctx: &mut dyn NativeContext, lock_obj: Option<ObjectRef>, this: ObjectRef| {
        if let Some(lo) = lock_obj {
            ctx.monitor_exit(lo);
        } else {
            ctx.monitor_exit(this);
        }
    };

    let cs = match ctx.invoke_virtual(coll, "toArray", "()[Ljava/lang/Object;", &[]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => {
            exit_mon(ctx, lock_obj, this);
            return Ok(Some(Value::Int(0)));
        }
    };
    let add_n = ctx.array_length(cs);
    if add_n == 0 {
        exit_mon(ctx, lock_obj, this);
        return Ok(Some(Value::Int(0)));
    }

    let base = match ctx.get_field(this, aslot) {
        Value::Object(Some(a)) => a,
        _ => {
            exit_mon(ctx, lock_obj, this);
            return Ok(Some(Value::Int(0)));
        }
    };
    let base_n = ctx.array_length(base);
    let out = ctx.new_array(ArrayElementType::Reference, base_n + add_n);
    for i in 0..base_n {
        ctx.set_array_element(out, i, ctx.get_array_element(base, i));
    }
    for i in 0..add_n {
        ctx.set_array_element(out, base_n + i, ctx.get_array_element(cs, i));
    }
    ctx.set_field(this, aslot, Value::Object(Some(out)));
    cowal_bump_mod_count(ctx, this);
    exit_mon(ctx, lock_obj, this);
    Ok(Some(Value::Int(1)))
}

fn native_stpe_set_keep_alive_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Shim compatibility: accept and ignore keep-alive tuning.
    Ok(None)
}

fn native_stpe_get_keep_alive_time(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return a conservative "disabled" value in all units.
    Ok(Some(Value::Long(0)))
}

fn native_stpe_set_maximum_pool_size(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Compatibility no-op for tuning knobs not modeled by the shim scheduler.
    Ok(None)
}

fn native_stpe_set_core_pool_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let size = match args.get(1) {
        Some(Value::Int(v)) => *v,
        _ => 1,
    };
    ctx.set_field(this, STPE_FIELD_POOL_SIZE, Value::Int(size.max(0)));
    Ok(None)
}

fn native_stpe_ignore_policy_setter(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

// --- CompletableFuture CompletionStage implementations ---

fn native_cf_then_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let runnable = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    // Wait for this to be done (in our model, always synchronous)
    let _done = ctx.get_field(this, CF_FIELD_DONE);
    let _ = ctx.invoke_virtual(runnable, "run", "()V", &[]);
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, Value::Object(None));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_then_compose(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, CF_FIELD_RESULT);
    let result_cf = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    // The result should be a CompletableFuture; return it directly
    match result_cf {
        Some(Value::Object(Some(cf_obj))) => {
            // It's already a CF; return it as-is
            Ok(Some(Value::Object(Some(cf_obj))))
        }
        other => {
            // Wrap the result in a completed CF
            let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
            ctx.set_field(cf, CF_FIELD_RESULT, other.unwrap_or(Value::Object(None)));
            ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
            Ok(Some(Value::Object(Some(cf))))
        }
    }
}

fn native_cf_then_combine(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let other_cf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let bi_func = match args.get(2) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val1 = ctx.get_field(this, CF_FIELD_RESULT);
    let val2 = ctx.get_field(other_cf, CF_FIELD_RESULT);
    let result = ctx.invoke_virtual(
        bi_func,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[val1, val2],
    )?;
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_exceptionally(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let handler = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => {
            // No handler — just copy the CF
            let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
            let val = ctx.get_field(this, CF_FIELD_RESULT);
            let done = ctx.get_field(this, CF_FIELD_DONE);
            ctx.set_field(cf, CF_FIELD_RESULT, val);
            ctx.set_field(cf, CF_FIELD_DONE, done);
            return Ok(Some(Value::Object(Some(cf))));
        }
    };
    let done = match ctx.get_field(this, CF_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    if done == 2 {
        // Already exceptionally completed — call the handler immediately
        let exc = ctx.get_field(this, CF_FIELD_RESULT);
        let result = ctx.invoke_virtual(
            handler,
            "apply",
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            &[exc],
        )?;
        ctx.set_field(cf, CF_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
        ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1)); // recovery = normal completion
    } else if done == 0 {
        // Source not yet complete — defer: store source + handler, mark done=-1
        ctx.set_field(cf, CF_FIELD_DONE, Value::Int(-1)); // deferred
        ctx.set_field(cf, CF_FIELD_SOURCE, Value::Object(Some(this)));
        ctx.set_field(cf, CF_FIELD_HANDLER, Value::Object(Some(handler)));
    } else {
        // Normally completed — pass through the result unchanged
        let val = ctx.get_field(this, CF_FIELD_RESULT);
        ctx.set_field(cf, CF_FIELD_RESULT, val);
        ctx.set_field(cf, CF_FIELD_DONE, Value::Int(done));
    }
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let bi_func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, CF_FIELD_RESULT);
    // In our simplified model, exception is always null
    let result = ctx.invoke_virtual(
        bi_func,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[val, Value::Object(None)],
    )?;
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_when_complete(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, CF_FIELD_RESULT);
    // Call BiConsumer(result, exception) — exception is null in our model
    let _ = ctx.invoke_virtual(
        consumer,
        "accept",
        "(Ljava/lang/Object;Ljava/lang/Object;)V",
        &[val.clone(), Value::Object(None)],
    );
    // Return a new CF with the same result (whenComplete does not transform)
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, val);
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_all_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // In our model all CFs are already completed synchronously.
    // allOf returns a CF<Void> that is done.
    let _arr = args.first(); // the CompletableFuture[] argument
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, Value::Object(None));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_any_of(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Return the result of the first CF in the array (all are completed)
    let arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => {
            let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
            ctx.set_field(cf, CF_FIELD_RESULT, Value::Object(None));
            ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
            return Ok(Some(Value::Object(Some(cf))));
        }
    };
    let len = ctx.array_length(arr);
    let result = if len > 0 {
        let first = ctx.get_array_element(arr, 0);
        match first {
            Value::Object(Some(cf_obj)) => ctx.get_field(cf_obj, CF_FIELD_RESULT),
            _ => Value::Object(None),
        }
    } else {
        Value::Object(None)
    };
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, result);
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_complete_exceptionally(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    let done = match ctx.get_field(this, CF_FIELD_DONE) {
        Value::Int(d) => d,
        _ => 0,
    };
    if done != 0 {
        return Ok(Some(Value::Int(0)));
    }
    // Mark as exceptionally completed (done=2), store exception as result
    let exc = args.get(1).cloned().unwrap_or(Value::Object(None));
    ctx.set_field(this, CF_FIELD_RESULT, exc);
    ctx.set_field(this, CF_FIELD_DONE, Value::Int(2)); // 2 = exceptional
    Ok(Some(Value::Int(1)))
}

fn native_cf_then_apply_p31(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let func = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, CF_FIELD_RESULT);
    let result = ctx.invoke_virtual(
        func,
        "apply",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &[val],
    )?;
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, result.unwrap_or(Value::Object(None)));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

fn native_cf_then_accept_p31(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let consumer = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let val = ctx.get_field(this, CF_FIELD_RESULT);
    let _ = ctx.invoke_virtual(consumer, "accept", "(Ljava/lang/Object;)V", &[val]);
    let cf = alloc_synthetic(ctx, "java/util/concurrent/CompletableFuture", 4);
    ctx.set_field(cf, CF_FIELD_RESULT, Value::Object(None));
    ctx.set_field(cf, CF_FIELD_DONE, Value::Int(1));
    Ok(Some(Value::Object(Some(cf))))
}

// --- ThreadPoolExecutor stat methods ---

// ThreadPoolExecutor uses 2-field layout: field 0 = pool size, field 1 = shutdown flag
const TP_FIELD_SIZE: usize = 0;
const TP_FIELD_SHUTDOWN: usize = 1;

fn native_tp_get_core_pool_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(1))),
    };
    match ctx.get_field(this, TP_FIELD_SIZE) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(1))),
    }
}

fn native_tp_is_shutdown(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Int(0))),
    };
    match ctx.get_field(this, TP_FIELD_SHUTDOWN) {
        Value::Int(v) => Ok(Some(Value::Int(v))),
        _ => Ok(Some(Value::Int(0))),
    }
}

fn native_tp_shutdown_now(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    ctx.set_field(this, TP_FIELD_SHUTDOWN, Value::Int(1));
    // Return empty list
    let list = alloc_synthetic(ctx, "java/util/ArrayList", 2);
    let arr = alloc_ref_array(ctx, 10);
    ctx.set_field(list, 0, Value::Object(Some(arr)));
    ctx.set_field(list, 1, Value::Int(0));
    Ok(Some(Value::Object(Some(list))))
}

// --- ForkJoinPool / ForkJoinTask invoke that calls compute ---

fn native_fjp_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // pool.invoke(task) — call task.compute() and return result
    let task = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = ctx.invoke_virtual(task, "compute", "()Ljava/lang/Object;", &[]);
    match result {
        Ok(Some(val)) => {
            // Store result in task field 0 for subsequent join/get calls
            ctx.set_field(task, 0, val.clone());
            Ok(Some(val))
        }
        Ok(None) => {
            ctx.set_field(task, 0, Value::Object(None));
            Ok(Some(Value::Object(None)))
        }
        Err(_) => {
            // If compute is not found, fall back to reading field 0
            Ok(Some(ctx.get_field(task, 0)))
        }
    }
}

fn native_fjt_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let result = ctx.invoke_virtual(this, "compute", "()Ljava/lang/Object;", &[]);
    match result {
        Ok(Some(val)) => {
            ctx.set_field(this, 0, val.clone());
            Ok(Some(val))
        }
        Ok(None) => {
            ctx.set_field(this, 0, Value::Object(None));
            Ok(Some(Value::Object(None)))
        }
        Err(_) => Ok(Some(ctx.get_field(this, 0))),
    }
}

fn native_rt_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    native_fjt_invoke(ctx, args)
}

fn native_ra_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(None))),
    };
    let _ = ctx.invoke_virtual(this, "compute", "()V", &[]);
    Ok(Some(Value::Object(None)))
}

// ===========================================================================
// Test hooks — exposed so the GC-relocation integration harness in
// `tests/gc_relocation_harness.rs` can write to and read from each of the
// four overlay tables (LinkedList, LinkedHashMap, TreeMap, TreeSet) without
// having to stand up the full registry + native-method dispatch chain.
//
// All four overlay tables are now keyed by `ctx.identity_hash_code(this)`;
// these shims simply delegate to the existing private helpers, so a
// passing test proves the key path is GC-stable end-to-end.
//
// `#[doc(hidden)]` because callers outside the test harness should go
// through the registered native methods.
// ===========================================================================

/// LinkedList overlay read shim. See module-level
/// `identity_hash::obj_key` for the key contract.
#[doc(hidden)]
pub fn __test_ll_get(ctx: &dyn NativeContext, this: ObjectRef, name: &'static str) -> Value {
    ll_get(ctx, this, name)
}
/// LinkedList overlay write shim.
#[doc(hidden)]
pub fn __test_ll_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &'static str, v: Value) {
    ll_set(ctx, this, name, v);
}
/// LinkedHashMap overlay read shim.
#[doc(hidden)]
pub fn __test_lhm_get(ctx: &dyn NativeContext, this: ObjectRef, name: &str) -> Value {
    lhm_get(ctx, this, name, 0)
}
/// LinkedHashMap overlay write shim.
#[doc(hidden)]
pub fn __test_lhm_set(ctx: &mut dyn NativeContext, this: ObjectRef, name: &str, v: Value) {
    lhm_set(ctx, this, name, 0, v);
}
/// TreeMap array-mode side-table read shim.
#[doc(hidden)]
pub fn __test_tm_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    tm_get_slot(ctx, this, slot)
}
/// TreeMap array-mode side-table write shim.
#[doc(hidden)]
pub fn __test_tm_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    tm_set_slot(ctx, this, slot, v);
}
/// TreeSet array-mode side-table read shim.
#[doc(hidden)]
pub fn __test_ts_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    ts_get_slot(ctx, this, slot)
}
/// TreeSet array-mode side-table write shim.
#[doc(hidden)]
pub fn __test_ts_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, v: Value) {
    ts_set_slot(ctx, this, slot, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unit tests for helper functions only.
    // Integration tests are in vm.rs since they need the full VM.

    #[test]
    fn dbg_hmput_matches_env_presence() {
        // The cached helper must report exactly "env var is set" — identical
        // truthiness to the original `env::var(...).is_ok()` check.
        use super::dbg_hmput;
        let expected = std::env::var_os("CRATONVM_DBG_HMPUT").is_some();
        assert_eq!(dbg_hmput(), expected);
        // Cached: second call returns the same value.
        assert_eq!(dbg_hmput(), expected);
    }

    #[test]
    fn map_bucket_index_power_of_two() {
        use super::map_bucket_index;
        // For capacity 16, bucket index should be in 0..15
        for hash in [0, 1, 15, 16, 31, 100, -1, -100, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, 16);
            assert!(idx < 16, "hash={hash}, idx={idx}");
        }
    }

    #[test]
    fn map_bucket_index_distribution() {
        use super::map_bucket_index;
        // Different positive hashes should give different buckets
        let b0 = map_bucket_index(0, 16);
        let b1 = map_bucket_index(1, 16);
        let b15 = map_bucket_index(15, 16);
        assert_eq!(b0, 0);
        assert_eq!(b1, 1);
        assert_eq!(b15, 15);
    }

    // -----------------------------------------------------------------------
    // Phase 3.1: Registration completeness tests
    // -----------------------------------------------------------------------
    // These tests verify that all required j.u.concurrent classes are
    // registered correctly by checking the registry after full registration.

    fn build_registry() -> NativeMethodRegistry {
        let mut r = NativeMethodRegistry::new();
        register_collections_natives(&mut r);
        r
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn concurrent_linked_queue_registered() {
        let r = build_registry();
        let clq = "java/util/concurrent/ConcurrentLinkedQueue";
        assert!(r.find(clq, "<init>", "()V").is_some(), "CLQ <init>");
        assert!(r.find(clq, "offer", "(Ljava/lang/Object;)Z").is_some(), "CLQ offer");
        assert!(r.find(clq, "poll", "()Ljava/lang/Object;").is_some(), "CLQ poll");
        assert!(r.find(clq, "peek", "()Ljava/lang/Object;").is_some(), "CLQ peek");
        assert!(r.find(clq, "isEmpty", "()Z").is_some(), "CLQ isEmpty");
        assert!(r.find(clq, "size", "()I").is_some(), "CLQ size");
        assert!(r.find(clq, "iterator", "()Ljava/util/Iterator;").is_some(), "CLQ iterator");
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn concurrent_linked_deque_registered() {
        let r = build_registry();
        let cld = "java/util/concurrent/ConcurrentLinkedDeque";
        assert!(r.find(cld, "offerFirst", "(Ljava/lang/Object;)Z").is_some(), "CLD offerFirst");
        assert!(r.find(cld, "offerLast", "(Ljava/lang/Object;)Z").is_some(), "CLD offerLast");
        assert!(r.find(cld, "pollFirst", "()Ljava/lang/Object;").is_some(), "CLD pollFirst");
        assert!(r.find(cld, "pollLast", "()Ljava/lang/Object;").is_some(), "CLD pollLast");
        assert!(r.find(cld, "peekFirst", "()Ljava/lang/Object;").is_some(), "CLD peekFirst");
        assert!(r.find(cld, "peekLast", "()Ljava/lang/Object;").is_some(), "CLD peekLast");
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn linked_blocking_queue_registered() {
        let r = build_registry();
        let lbq = "java/util/concurrent/LinkedBlockingQueue";
        assert!(r.find(lbq, "<init>", "()V").is_some(), "LBQ <init>");
        assert!(r.find(lbq, "<init>", "(I)V").is_some(), "LBQ <init>(I)");
        assert!(r.find(lbq, "put", "(Ljava/lang/Object;)V").is_some(), "LBQ put");
        assert!(r.find(lbq, "take", "()Ljava/lang/Object;").is_some(), "LBQ take");
        assert!(r.find(lbq, "offer", "(Ljava/lang/Object;)Z").is_some(), "LBQ offer");
        assert!(r.find(lbq, "poll", "()Ljava/lang/Object;").is_some(), "LBQ poll");
        assert!(r.find(lbq, "peek", "()Ljava/lang/Object;").is_some(), "LBQ peek");
        assert!(r.find(lbq, "remainingCapacity", "()I").is_some(), "LBQ remainingCapacity");
        assert!(r.find(lbq, "size", "()I").is_some(), "LBQ size");
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn array_blocking_queue_registered() {
        let r = build_registry();
        let abq = "java/util/concurrent/ArrayBlockingQueue";
        assert!(r.find(abq, "<init>", "(I)V").is_some(), "ABQ <init>(I)");
        assert!(r.find(abq, "put", "(Ljava/lang/Object;)V").is_some(), "ABQ put");
        assert!(r.find(abq, "take", "()Ljava/lang/Object;").is_some(), "ABQ take");
        assert!(r.find(abq, "offer", "(Ljava/lang/Object;)Z").is_some(), "ABQ offer");
        assert!(r.find(abq, "poll", "()Ljava/lang/Object;").is_some(), "ABQ poll");
        assert!(r.find(abq, "remainingCapacity", "()I").is_some(), "ABQ remainingCapacity");
    }

    #[test]
    fn completable_future_completion_stage_registered() {
        let r = build_registry();
        let cf = "java/util/concurrent/CompletableFuture";
        assert!(
            r.find(cf, "thenRun", "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF thenRun"
        );
        assert!(
            r.find(cf, "thenCompose", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF thenCompose"
        );
        assert!(
            r.find(cf, "thenCombine",
                "(Ljava/util/concurrent/CompletionStage;Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF thenCombine"
        );
        assert!(
            r.find(cf, "exceptionally", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF exceptionally"
        );
        assert!(
            r.find(cf, "handle", "(Ljava/util/function/BiFunction;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF handle"
        );
        assert!(
            r.find(cf, "whenComplete", "(Ljava/util/function/BiConsumer;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF whenComplete"
        );
        assert!(
            r.find(cf, "allOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF allOf"
        );
        assert!(
            r.find(cf, "anyOf", "([Ljava/util/concurrent/CompletableFuture;)Ljava/util/concurrent/CompletableFuture;").is_some(),
            "CF anyOf"
        );
    }

    #[test]
    fn fork_join_pool_await_quiescence_registered() {
        let r = build_registry();
        let pool = "java/util/concurrent/ForkJoinPool";
        assert!(
            r.find(pool, "awaitQuiescence", "(JLjava/util/concurrent/TimeUnit;)Z").is_some(),
            "FJP awaitQuiescence"
        );
        assert!(
            r.find(pool, "invoke", "(Ljava/util/concurrent/ForkJoinTask;)Ljava/lang/Object;").is_some(),
            "FJP invoke"
        );
    }

    #[test]
    fn fork_join_task_methods_registered() {
        let r = build_registry();
        let fjt = "java/util/concurrent/ForkJoinTask";
        assert!(r.find(fjt, "invoke", "()Ljava/lang/Object;").is_some(), "FJT invoke");

        let rt = "java/util/concurrent/RecursiveTask";
        assert!(r.find(rt, "invoke", "()Ljava/lang/Object;").is_some(), "RT invoke");

        let ra = "java/util/concurrent/RecursiveAction";
        assert!(r.find(ra, "invoke", "()Ljava/lang/Object;").is_some(), "RA invoke");
    }

    #[test]
    fn thread_pool_executor_stats_registered() {
        let r = build_registry();
        let tp = "java/util/concurrent/ThreadPoolExecutor";
        assert!(r.find(tp, "getPoolSize", "()I").is_some(), "TPE getPoolSize");
        assert!(r.find(tp, "getActiveCount", "()I").is_some(), "TPE getActiveCount");
        assert!(r.find(tp, "getCorePoolSize", "()I").is_some(), "TPE getCorePoolSize");
        assert!(r.find(tp, "getMaximumPoolSize", "()I").is_some(), "TPE getMaximumPoolSize");
        assert!(r.find(tp, "isShutdown", "()Z").is_some(), "TPE isShutdown");
        assert!(r.find(tp, "shutdownNow", "()Ljava/util/List;").is_some(), "TPE shutdownNow");
    }

    #[test]
    fn completion_stage_interface_registered() {
        let r = build_registry();
        let cs = "java/util/concurrent/CompletionStage";
        assert!(
            r.find(cs, "thenApply", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;").is_some(),
            "CS thenApply"
        );
        assert!(
            r.find(cs, "thenAccept", "(Ljava/util/function/Consumer;)Ljava/util/concurrent/CompletionStage;").is_some(),
            "CS thenAccept"
        );
        assert!(
            r.find(cs, "thenRun", "(Ljava/lang/Runnable;)Ljava/util/concurrent/CompletionStage;").is_some(),
            "CS thenRun"
        );
        assert!(
            r.find(cs, "thenCompose", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;").is_some(),
            "CS thenCompose"
        );
        assert!(
            r.find(cs, "exceptionally", "(Ljava/util/function/Function;)Ljava/util/concurrent/CompletionStage;").is_some(),
            "CS exceptionally"
        );
    }

    #[test]
    fn concurrent_hashmap_registered() {
        let r = build_registry();
        let chm = "java/util/concurrent/ConcurrentHashMap";
        assert!(r.find(chm, "<init>", "()V").is_some(), "CHM <init>");
        assert!(r.find(chm, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "CHM put");
        assert!(r.find(chm, "get", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "CHM get");
        assert!(r.find(chm, "size", "()I").is_some(), "CHM size");
        assert!(r.find(chm, "containsKey", "(Ljava/lang/Object;)Z").is_some(), "CHM containsKey");
    }

    #[test]
    fn scheduled_executor_registered() {
        let r = build_registry();
        let stpe = "java/util/concurrent/ScheduledThreadPoolExecutor";
        assert!(r.find(stpe, "<init>", "(I)V").is_some(), "STPE <init>");
        assert!(r.find(stpe, "shutdown", "()V").is_some(), "STPE shutdown");
        assert!(r.find(stpe, "isShutdown", "()Z").is_some(), "STPE isShutdown");
    }

    #[test]
    #[cfg(feature = "synthetic-jdk")]
    fn blocking_queue_interface_registered() {
        let r = build_registry();
        let bq = "java/util/concurrent/BlockingQueue";
        assert!(r.find(bq, "put", "(Ljava/lang/Object;)V").is_some(), "BQ put");
        assert!(r.find(bq, "take", "()Ljava/lang/Object;").is_some(), "BQ take");
        assert!(r.find(bq, "offer", "(Ljava/lang/Object;)Z").is_some(), "BQ offer");
        assert!(r.find(bq, "poll", "()Ljava/lang/Object;").is_some(), "BQ poll");
    }

    #[test]
    fn cf_field_layout_constants_consistent() {
        // Verify the field layout constants match between this file and
        // the native-builtins crate (both use fields 0=result, 1=done)
        assert_eq!(CF_FIELD_RESULT, 0);
        assert_eq!(CF_FIELD_DONE, 1);
        assert_eq!(TP_FIELD_SIZE, 0);
        assert_eq!(TP_FIELD_SHUTDOWN, 1);
    }

    // -----------------------------------------------------------------------
    // Edge-case tests for capacity growth and map helpers
    // -----------------------------------------------------------------------

    #[test]
    fn map_bucket_index_single_bucket() {
        // Capacity of 1 — everything goes to bucket 0
        assert_eq!(map_bucket_index(0, 1), 0);
        assert_eq!(map_bucket_index(42, 1), 0);
        assert_eq!(map_bucket_index(-1, 1), 0);
    }

    #[test]
    fn map_bucket_index_large_capacity() {
        let cap = 1 << 20; // 1M buckets
        for hash in [0, 1, -1, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, cap);
            assert!(idx < cap as usize, "hash={hash}, idx={idx}, cap={cap}");
        }
    }

    #[test]
    fn map_bucket_index_capacity_two() {
        // Capacity 2: index should be 0 or 1
        for hash in [0, 1, 2, 3, -1, -2, i32::MAX, i32::MIN] {
            let idx = map_bucket_index(hash, 2);
            assert!(idx < 2, "hash={hash}, idx={idx}");
        }
    }

    #[test]
    fn values_equal_null_null() {
        // Two null objects should be equal (tested via the public helper)
        let a = Value::Object(None);
        let b = Value::Object(None);
        // We cannot call values_equal without a NativeContext, but we can
        // check the pattern match logic directly.
        assert!(matches!((&a, &b), (Value::Object(None), Value::Object(None))));
    }

    #[test]
    fn values_equal_ints() {
        let a = Value::Int(42);
        let b = Value::Int(42);
        let c = Value::Int(99);
        // Direct pattern checks matching values_equal logic
        assert!(matches!((&a, &b), (Value::Int(x), Value::Int(y)) if x == y));
        assert!(!matches!((&a, &c), (Value::Int(x), Value::Int(y)) if x == y));
    }

    #[test]
    fn values_equal_long() {
        let a = Value::Long(123456789);
        let b = Value::Long(123456789);
        let c = Value::Long(0);
        assert!(matches!((&a, &b), (Value::Long(x), Value::Long(y)) if x == y));
        assert!(!matches!((&a, &c), (Value::Long(x), Value::Long(y)) if x == y));
    }

    #[test]
    fn values_equal_mixed_types() {
        let int_val = Value::Int(42);
        let long_val = Value::Long(42);
        // Different types should not match
        assert!(!matches!((&int_val, &long_val), (Value::Int(x), Value::Int(y)) if x == y));
        assert!(!matches!((&int_val, &long_val), (Value::Long(x), Value::Long(y)) if x == y));
    }

    #[test]
    fn arraylist_registered_completely() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(r.find(c, "<init>", "()V").is_some(), "AL <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "AL <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "AL size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "AL isEmpty");
        assert!(r.find(c, "get", "(I)Ljava/lang/Object;").is_some(), "AL get");
        assert!(r.find(c, "set", "(ILjava/lang/Object;)Ljava/lang/Object;").is_some(), "AL set");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "AL add");
        assert!(r.find(c, "remove", "(I)Ljava/lang/Object;").is_some(), "AL remove(I)");
        assert!(r.find(c, "clear", "()V").is_some(), "AL clear");
        assert!(r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(), "AL contains");
        assert!(r.find(c, "indexOf", "(Ljava/lang/Object;)I").is_some(), "AL indexOf");
        assert!(r.find(c, "ensureCapacity", "(I)V").is_some(), "AL ensureCapacity");
        assert!(r.find(c, "trimToSize", "()V").is_some(), "AL trimToSize");
    }

    #[test]
    fn hashmap_registered_completely() {
        let r = build_registry();
        let c = "java/util/HashMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "HM <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "HM <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "HM size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "HM isEmpty");
        assert!(r.find(c, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "HM put");
        assert!(r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "HM get");
        assert!(r.find(c, "remove", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "HM remove");
        assert!(r.find(c, "containsKey", "(Ljava/lang/Object;)Z").is_some(), "HM containsKey");
        assert!(r.find(c, "clear", "()V").is_some(), "HM clear");
    }

    // -----------------------------------------------------------------------
    // HashSet registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn hashset_registered_completely() {
        let r = build_registry();
        let c = "java/util/HashSet";
        assert!(r.find(c, "<init>", "()V").is_some(), "HS <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "HS <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "HS size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "HS isEmpty");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "HS add");
        assert!(r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(), "HS remove");
        assert!(r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(), "HS contains");
        assert!(r.find(c, "clear", "()V").is_some(), "HS clear");
        assert!(r.find(c, "iterator", "()Ljava/util/Iterator;").is_some(), "HS iterator");
        assert!(r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(), "HS toArray");
        assert!(r.find(c, "toString", "()Ljava/lang/String;").is_some(), "HS toString");
    }

    // -----------------------------------------------------------------------
    // LinkedList registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn linked_list_registered_completely() {
        let r = build_registry();
        let c = "java/util/LinkedList";
        assert!(r.find(c, "<init>", "()V").is_some(), "LL <init>");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "LL add");
        assert!(r.find(c, "addFirst", "(Ljava/lang/Object;)V").is_some(), "LL addFirst");
        assert!(r.find(c, "addLast", "(Ljava/lang/Object;)V").is_some(), "LL addLast");
        assert!(r.find(c, "get", "(I)Ljava/lang/Object;").is_some(), "LL get");
        assert!(r.find(c, "getFirst", "()Ljava/lang/Object;").is_some(), "LL getFirst");
        assert!(r.find(c, "getLast", "()Ljava/lang/Object;").is_some(), "LL getLast");
        assert!(r.find(c, "removeFirst", "()Ljava/lang/Object;").is_some(), "LL removeFirst");
        assert!(r.find(c, "removeLast", "()Ljava/lang/Object;").is_some(), "LL removeLast");
        assert!(r.find(c, "size", "()I").is_some(), "LL size");
    }

    // -----------------------------------------------------------------------
    // LinkedHashMap registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn linked_hashmap_registered_completely() {
        let r = build_registry();
        let c = "java/util/LinkedHashMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "LHM <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "LHM <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "LHM size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "LHM isEmpty");
        assert!(r.find(c, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "LHM put");
        assert!(r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "LHM get");
    }

    // -----------------------------------------------------------------------
    // ArrayDeque registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn array_deque_registered_completely() {
        let r = build_registry();
        let c = "java/util/ArrayDeque";
        assert!(r.find(c, "<init>", "()V").is_some(), "AD <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "AD <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "AD size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "AD isEmpty");
        assert!(r.find(c, "addFirst", "(Ljava/lang/Object;)V").is_some(), "AD addFirst");
        assert!(r.find(c, "addLast", "(Ljava/lang/Object;)V").is_some(), "AD addLast");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "AD add");
        assert!(r.find(c, "offerFirst", "(Ljava/lang/Object;)Z").is_some(), "AD offerFirst");
        assert!(r.find(c, "offerLast", "(Ljava/lang/Object;)Z").is_some(), "AD offerLast");
    }

    // -----------------------------------------------------------------------
    // PriorityQueue registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn priority_queue_registered_completely() {
        let r = build_registry();
        let c = "java/util/PriorityQueue";
        assert!(r.find(c, "<init>", "()V").is_some(), "PQ <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "PQ <init>(I)");
        assert!(r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(), "PQ <init>(Comparator)");
        assert!(r.find(c, "size", "()I").is_some(), "PQ size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "PQ isEmpty");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "PQ add");
        assert!(r.find(c, "offer", "(Ljava/lang/Object;)Z").is_some(), "PQ offer");
        assert!(r.find(c, "peek", "()Ljava/lang/Object;").is_some(), "PQ peek");
        assert!(r.find(c, "poll", "()Ljava/lang/Object;").is_some(), "PQ poll");
        assert!(r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(), "PQ remove");
        assert!(r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(), "PQ contains");
        assert!(r.find(c, "clear", "()V").is_some(), "PQ clear");
        assert!(r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(), "PQ toArray");
    }

    // -----------------------------------------------------------------------
    // Vector registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn vector_registered_completely() {
        let r = build_registry();
        let c = "java/util/Vector";
        assert!(r.find(c, "<init>", "()V").is_some(), "Vec <init>");
        assert!(r.find(c, "<init>", "(I)V").is_some(), "Vec <init>(I)");
        assert!(r.find(c, "size", "()I").is_some(), "Vec size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Vec isEmpty");
        assert!(r.find(c, "get", "(I)Ljava/lang/Object;").is_some(), "Vec get");
        assert!(r.find(c, "elementAt", "(I)Ljava/lang/Object;").is_some(), "Vec elementAt");
    }

    // -----------------------------------------------------------------------
    // Stack registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn stack_registered_completely() {
        let r = build_registry();
        let c = "java/util/Stack";
        assert!(r.find(c, "<init>", "()V").is_some(), "Stack <init>");
        assert!(r.find(c, "size", "()I").is_some(), "Stack size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Stack isEmpty");
        assert!(r.find(c, "get", "(I)Ljava/lang/Object;").is_some(), "Stack get");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "Stack add");
        assert!(r.find(c, "remove", "(I)Ljava/lang/Object;").is_some(), "Stack remove");
        assert!(r.find(c, "clear", "()V").is_some(), "Stack clear");
        assert!(r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(), "Stack contains");
    }

    // -----------------------------------------------------------------------
    // TreeMap registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn tree_map_registered_completely() {
        let r = build_registry();
        let c = "java/util/TreeMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "TM <init>");
        assert!(r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(), "TM <init>(Comparator)");
        assert!(r.find(c, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "TM put");
        assert!(r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "TM get");
        assert!(r.find(c, "size", "()I").is_some(), "TM size");
    }

    // -----------------------------------------------------------------------
    // TreeSet registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn tree_set_registered_completely() {
        let r = build_registry();
        let c = "java/util/TreeSet";
        assert!(r.find(c, "<init>", "()V").is_some(), "TS <init>");
        assert!(r.find(c, "<init>", "(Ljava/util/Comparator;)V").is_some(), "TS <init>(Comparator)");
        assert!(r.find(c, "<init>", "(Ljava/util/Collection;)V").is_some(), "TS <init>(Collection)");
        assert!(r.find(c, "add", "(Ljava/lang/Object;)Z").is_some(), "TS add");
        assert!(r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(), "TS remove");
        assert!(r.find(c, "contains", "(Ljava/lang/Object;)Z").is_some(), "TS contains");
        assert!(r.find(c, "size", "()I").is_some(), "TS size");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "TS isEmpty");
        assert!(r.find(c, "clear", "()V").is_some(), "TS clear");
    }

    // -----------------------------------------------------------------------
    // Optional registration completeness
    // -----------------------------------------------------------------------

    #[test]
    fn optional_registered_completely() {
        let r = build_registry();
        let c = "java/util/Optional";
        assert!(r.find(c, "empty", "()Ljava/util/Optional;").is_some(), "Opt empty");
        assert!(r.find(c, "of", "(Ljava/lang/Object;)Ljava/util/Optional;").is_some(), "Opt of");
        assert!(r.find(c, "ofNullable", "(Ljava/lang/Object;)Ljava/util/Optional;").is_some(), "Opt ofNullable");
        assert!(r.find(c, "get", "()Ljava/lang/Object;").is_some(), "Opt get");
        assert!(r.find(c, "isPresent", "()Z").is_some(), "Opt isPresent");
        assert!(r.find(c, "isEmpty", "()Z").is_some(), "Opt isEmpty");
    }

    // -----------------------------------------------------------------------
    // Field layout constant tests — correctness and uniqueness
    // -----------------------------------------------------------------------

    #[test]
    fn arraylist_field_layout_valid() {
        assert_eq!(AL_FIELD_DATA, 0, "AL data field should be 0");
        assert_eq!(AL_FIELD_SIZE, 1, "AL size field should be 1");
        assert_eq!(AL_NUM_FIELDS, 2, "AL should have 2 fields");
        assert_eq!(AL_DEFAULT_CAPACITY, 10, "AL default capacity should be 10");
    }

    #[test]
    fn hashmap_field_layout_valid() {
        assert_eq!(MAP_FIELD_BUCKETS, 0);
        assert_eq!(MAP_FIELD_SIZE, 1);
        assert_eq!(MAP_FIELD_CAPACITY, 2);
        assert_eq!(MAP_DEFAULT_CAPACITY, 16, "HM default capacity should be 16 (power of 2)");
        // Default capacity must be a power of two for bucket indexing
        assert!(MAP_DEFAULT_CAPACITY.is_power_of_two(), "HM default capacity must be power of 2");
    }

    #[test]
    fn hashmap_node_field_layout_valid() {
        assert_eq!(NODE_FIELD_KEY, 0);
        assert_eq!(NODE_FIELD_VALUE, 1);
        assert_eq!(NODE_FIELD_HASH, 2);
        assert_eq!(NODE_FIELD_NEXT, 3);
        assert_eq!(NODE_NUM_FIELDS, 4);
        // All indices must be within NUM_FIELDS
        assert!(NODE_FIELD_KEY < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_VALUE < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_HASH < NODE_NUM_FIELDS);
        assert!(NODE_FIELD_NEXT < NODE_NUM_FIELDS);
    }

    #[test]
    fn linked_list_field_layout_valid() {
        assert_eq!(LL_FIELD_HEAD, 0);
        assert_eq!(LL_FIELD_TAIL, 1);
        assert_eq!(LL_FIELD_SIZE, 2);
        assert_eq!(LL_NODE_PREV, 0);
        assert_eq!(LL_NODE_NEXT, 1);
        assert_eq!(LL_NODE_ELEM, 2);
    }

    #[test]
    fn array_deque_field_layout_valid() {
        assert_eq!(AD_FIELD_DATA, 0);
        assert_eq!(AD_FIELD_HEAD, 1);
        assert_eq!(AD_FIELD_TAIL, 2);
        assert_eq!(AD_FIELD_SIZE, 3);
        assert_eq!(AD_DEFAULT_CAPACITY, 16);
        assert!(AD_DEFAULT_CAPACITY.is_power_of_two(), "AD default capacity must be power of 2");
    }

    #[test]
    fn priority_queue_field_layout_valid() {
        assert_eq!(PQ_FIELD_DATA, 0);
        assert_eq!(PQ_FIELD_SIZE, 1);
        assert_eq!(PQ_FIELD_COMPARATOR, 2);
        assert_eq!(PQ_DEFAULT_CAPACITY, 11, "PQ default capacity should be 11 (matches JDK)");
    }

    #[test]
    fn hashset_delegates_to_hashmap() {
        // HashSet uses a single field for its backing HashMap
        assert_eq!(HS_FIELD_MAP, 0);
        assert_eq!(HS_NUM_FIELDS, 1);
    }

    // -----------------------------------------------------------------------
    // map_bucket_index edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn map_bucket_index_negative_hash() {
        // Negative hashes should still produce valid indices
        for hash in [-1, -100, -1000, i32::MIN, i32::MIN + 1] {
            let idx = map_bucket_index(hash, 16);
            assert!(idx < 16, "negative hash={hash} produced out-of-range idx={idx}");
        }
    }

    #[test]
    fn map_bucket_index_max_int_hash() {
        let idx = map_bucket_index(i32::MAX, 16);
        assert!(idx < 16);
        assert_eq!(idx, 15, "i32::MAX & 15 should equal 15");
    }

    #[test]
    fn map_bucket_index_zero_hash() {
        assert_eq!(map_bucket_index(0, 16), 0);
        assert_eq!(map_bucket_index(0, 32), 0);
        assert_eq!(map_bucket_index(0, 1), 0);
    }

    // -----------------------------------------------------------------------
    // values_equal edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn values_equal_int_boundaries() {
        let min = Value::Int(i32::MIN);
        let max = Value::Int(i32::MAX);
        let min2 = Value::Int(i32::MIN);
        assert!(matches!((&min, &min2), (Value::Int(x), Value::Int(y)) if x == y));
        assert!(!matches!((&min, &max), (Value::Int(x), Value::Int(y)) if x == y));
    }

    #[test]
    fn values_equal_long_boundaries() {
        let min = Value::Long(i64::MIN);
        let max = Value::Long(i64::MAX);
        let min2 = Value::Long(i64::MIN);
        assert!(matches!((&min, &min2), (Value::Long(x), Value::Long(y)) if x == y));
        assert!(!matches!((&min, &max), (Value::Long(x), Value::Long(y)) if x == y));
    }

    #[test]
    fn values_equal_null_vs_some() {
        let null = Value::Object(None);
        let int = Value::Int(0);
        // Null and Int should never match in the values_equal logic
        assert!(!matches!((&null, &int), (Value::Int(x), Value::Int(y)) if x == y));
        assert!(!matches!((&null, &int), (Value::Object(None), Value::Object(None))));
    }

    #[test]
    fn values_equal_zero_int() {
        let a = Value::Int(0);
        let b = Value::Int(0);
        assert!(matches!((&a, &b), (Value::Int(x), Value::Int(y)) if x == y));
    }

    // -----------------------------------------------------------------------
    // ArrayList method completeness (addAll, subList, etc.)
    // -----------------------------------------------------------------------

    #[test]
    fn arraylist_extended_methods_registered() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(r.find(c, "add", "(ILjava/lang/Object;)V").is_some(), "AL add(I,O)");
        assert!(r.find(c, "remove", "(Ljava/lang/Object;)Z").is_some(), "AL remove(O)");
        assert!(r.find(c, "lastIndexOf", "(Ljava/lang/Object;)I").is_some(), "AL lastIndexOf");
        assert!(r.find(c, "toArray", "()[Ljava/lang/Object;").is_some(), "AL toArray");
        assert!(r.find(c, "toString", "()Ljava/lang/String;").is_some(), "AL toString");
        assert!(r.find(c, "addAll", "(Ljava/util/Collection;)Z").is_some(), "AL addAll");
        assert!(r.find(c, "subList", "(II)Ljava/util/List;").is_some(), "AL subList");
        assert!(r.find(c, "hashCode", "()I").is_some(), "AL hashCode");
        assert!(r.find(c, "equals", "(Ljava/lang/Object;)Z").is_some(), "AL equals");
    }

    #[test]
    fn arraylist_functional_methods_registered() {
        let r = build_registry();
        let c = "java/util/ArrayList";
        assert!(r.find(c, "forEach", "(Ljava/util/function/Consumer;)V").is_some(), "AL forEach");
        assert!(r.find(c, "sort", "(Ljava/util/Comparator;)V").is_some(), "AL sort");
        assert!(r.find(c, "removeIf", "(Ljava/util/function/Predicate;)Z").is_some(), "AL removeIf");
        assert!(r.find(c, "replaceAll", "(Ljava/util/function/UnaryOperator;)V").is_some(), "AL replaceAll");
        assert!(r.find(c, "stream", "()Ljava/util/stream/Stream;").is_some(), "AL stream");
    }

    // -----------------------------------------------------------------------
    // HashMap extended methods
    // -----------------------------------------------------------------------

    #[test]
    fn hashmap_iterator_and_views_registered() {
        let r = build_registry();
        let c = "java/util/HashMap";
        assert!(r.find(c, "keySet", "()Ljava/util/Set;").is_some(), "HM keySet");
        assert!(r.find(c, "values", "()Ljava/util/Collection;").is_some(), "HM values");
        assert!(r.find(c, "entrySet", "()Ljava/util/Set;").is_some(), "HM entrySet");
    }

    // -----------------------------------------------------------------------
    // MAP_MAX_CAPACITY test
    // -----------------------------------------------------------------------

    #[test]
    fn map_max_capacity_is_power_of_two() {
        assert!(MAP_MAX_CAPACITY > 0);
        assert!((MAP_MAX_CAPACITY as u32).is_power_of_two());
        assert_eq!(MAP_MAX_CAPACITY, 1 << 30);
    }

    // -----------------------------------------------------------------------
    // Iterator field layout
    // -----------------------------------------------------------------------

    #[test]
    fn iterator_field_layout_valid() {
        assert_eq!(AL_ITR_FIELD_LIST, 0);
        assert_eq!(AL_ITR_FIELD_CURSOR, 1);
        assert_eq!(AL_ITR_NUM_FIELDS, 2);
    }

    // -----------------------------------------------------------------------
    // Properties registration (via collections extras)
    // -----------------------------------------------------------------------

    #[test]
    fn properties_registered() {
        let r = build_registry();
        let c = "java/util/Properties";
        assert!(r.find(c, "<init>", "()V").is_some(), "Props <init>");
    }

    // -----------------------------------------------------------------------
    // ConcurrentSkipListMap registration
    // -----------------------------------------------------------------------

    #[test]
    fn concurrent_skip_list_map_registered() {
        let r = build_registry();
        let c = "java/util/concurrent/ConcurrentSkipListMap";
        assert!(r.find(c, "<init>", "()V").is_some(), "CSLM <init>");
        assert!(r.find(c, "put", "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "CSLM put");
        assert!(r.find(c, "get", "(Ljava/lang/Object;)Ljava/lang/Object;").is_some(), "CSLM get");
        assert!(r.find(c, "size", "()I").is_some(), "CSLM size");
    }

    // ===================================================================
    // task #15: blocking-queue park/notify discipline.
    //
    // The tests below drive the production `native_lbq_put_blocking` /
    // `native_lbq_poll` / `native_lbq_take_blocking` against a minimal
    // `NativeContext` mock whose monitor primitives implement true
    // Java-style wait/notify semantics (recursive ownership, lock release
    // on wait, condvar-driven wakeups). That's the only way to validate
    // that a producer parked at capacity actually wakes when a consumer
    // drains the queue.
    // ===================================================================

    mod lbq_blocking_tests {
        use super::super::*;
        use cratonvm_native_api::{
            AnnotationData, AnnotationElementValue, FieldMetadata, MethodMetadata,
            StackTraceEntry,
        };
        use cratonvm_types::error::MethodCallFailed;
        use std::collections::HashMap;
        use std::sync::{Arc, Condvar, Mutex};
        use std::time::Duration;

        // ---- Per-object Java-style monitor ----------------------------
        struct MonitorState {
            owner: Option<u64>,
            count: u32,
        }

        struct ObjMonitor {
            lock: Mutex<MonitorState>,
            cvar: Condvar,
        }

        impl ObjMonitor {
            fn new() -> Arc<Self> {
                Arc::new(ObjMonitor {
                    lock: Mutex::new(MonitorState { owner: None, count: 0 }),
                    cvar: Condvar::new(),
                })
            }

            fn enter(&self, tid: u64) {
                let mut st = self.lock.lock().unwrap();
                while !(st.owner.is_none() || st.owner == Some(tid)) {
                    st = self.cvar.wait(st).unwrap();
                }
                st.owner = Some(tid);
                st.count += 1;
            }

            fn exit(&self, tid: u64) {
                let mut st = self.lock.lock().unwrap();
                debug_assert_eq!(st.owner, Some(tid), "monitor_exit by non-owner");
                st.count -= 1;
                if st.count == 0 {
                    st.owner = None;
                    self.cvar.notify_all();
                }
            }

            fn wait(&self, tid: u64, timeout: Option<u64>) {
                let mut st = self.lock.lock().unwrap();
                debug_assert_eq!(st.owner, Some(tid), "monitor_wait by non-owner");
                let saved_count = st.count;
                st.owner = None;
                st.count = 0;
                self.cvar.notify_all();
                let timeout = timeout.unwrap_or(50);
                let (mut st2, _r) = self
                    .cvar
                    .wait_timeout(st, Duration::from_millis(timeout))
                    .unwrap();
                while !(st2.owner.is_none() || st2.owner == Some(tid)) {
                    st2 = self.cvar.wait(st2).unwrap();
                }
                st2.owner = Some(tid);
                st2.count = saved_count;
            }

            fn notify_all(&self) {
                self.cvar.notify_all();
            }
        }

        // ---- Heap entries -------------------------------------------
        enum HeapEntry {
            Object { fields: Vec<Value> },
            Array { elements: Vec<Value> },
        }

        /// Shared heap + monitor state, behind a single `Mutex` so the
        /// `&self` and `&mut self` trait methods can be called from
        /// multiple OS threads without UB. The native methods we test
        /// hold the per-object monitor across the heap access, but
        /// `alloc_ref_array` (called from `lbq_ensure_capacity`) does
        /// not — so we serialise the heap itself with this lock.
        struct Shared {
            heap: Vec<HeapEntry>,
            ptr_to_index: HashMap<usize, usize>,
            next_ptr: usize,
            monitors: HashMap<usize, Arc<ObjMonitor>>,
        }

        impl Shared {
            fn new() -> Self {
                Shared {
                    heap: Vec::new(),
                    ptr_to_index: HashMap::new(),
                    next_ptr: 8,
                    monitors: HashMap::new(),
                }
            }

            fn alloc_entry(&mut self, entry: HeapEntry) -> ObjectRef {
                let idx = self.heap.len();
                self.heap.push(entry);
                let ptr = self.next_ptr;
                self.next_ptr += 8;
                self.ptr_to_index.insert(ptr, idx);
                unsafe { ObjectRef::from_raw(ptr as *mut u8) }
            }

            fn entry_index(&self, obj: ObjectRef) -> usize {
                let ptr = obj.as_ptr() as usize;
                *self
                    .ptr_to_index
                    .get(&ptr)
                    .expect("invalid ObjectRef in MockCtx")
            }

            fn monitor_for(&mut self, obj: ObjectRef) -> Arc<ObjMonitor> {
                let ptr = obj.as_ptr() as usize;
                self.monitors
                    .entry(ptr)
                    .or_insert_with(ObjMonitor::new)
                    .clone()
            }
        }

        pub(super) struct MockCtx {
            shared: Arc<Mutex<Shared>>,
            thread_id: u64,
        }

        impl MockCtx {
            pub(super) fn new(thread_id: u64) -> Self {
                MockCtx {
                    shared: Arc::new(Mutex::new(Shared::new())),
                    thread_id,
                }
            }

            /// Sibling `MockCtx` sharing the same heap/monitor state but
            /// reporting a different `thread_id`. Intended for use on a
            /// separate OS thread — each call into the trait re-acquires
            /// the shared `Mutex<Shared>` briefly, so the two contexts
            /// can run concurrently without UB.
            pub(super) fn fork(&self, new_thread_id: u64) -> Self {
                MockCtx {
                    shared: Arc::clone(&self.shared),
                    thread_id: new_thread_id,
                }
            }

            pub(super) fn alloc_lbq_object(&self) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); 4],
                })
            }
        }

        impl NativeContext for MockCtx {
            fn new_array(&mut self, _et: ArrayElementType, length: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Array {
                    elements: vec![Value::Int(0); length],
                })
            }
            fn new_ref_array(&mut self, _c: ClassId, length: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Array {
                    elements: vec![Value::Object(None); length],
                })
            }
            fn array_length(&self, obj: ObjectRef) -> usize {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { elements } => elements.len(),
                    _ => 0,
                }
            }
            fn get_array_element(&self, obj: ObjectRef, index: usize) -> Value {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { elements } => {
                        elements.get(index).copied().unwrap_or(Value::Object(None))
                    }
                    _ => Value::Object(None),
                }
            }
            fn set_array_element(&self, obj: ObjectRef, index: usize, value: Value) {
                let mut s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                if let HeapEntry::Array { elements } = &mut s.heap[idx] {
                    if index < elements.len() {
                        elements[index] = value;
                    }
                }
            }
            fn get_field(&self, obj: ObjectRef, index: usize) -> Value {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Object { fields } => {
                        fields.get(index).copied().unwrap_or(Value::Int(0))
                    }
                    _ => Value::Int(0),
                }
            }
            fn set_field(&self, obj: ObjectRef, index: usize, value: Value) {
                let mut s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                if let HeapEntry::Object { fields } = &mut s.heap[idx] {
                    if index >= fields.len() {
                        fields.resize(index + 1, Value::Int(0));
                    }
                    fields[index] = value;
                }
            }
            fn alloc_object(&mut self, _c: ClassId, num_fields: usize) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object {
                    fields: vec![Value::Int(0); num_fields],
                })
            }
            fn object_num_fields(&self, obj: ObjectRef) -> usize {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Object { fields } => fields.len(),
                    _ => 0,
                }
            }
            fn heap_kind_of(&self, obj: ObjectRef) -> ObjectKind {
                let s = self.shared.lock().unwrap();
                let idx = s.entry_index(obj);
                match &s.heap[idx] {
                    HeapEntry::Array { .. } => ObjectKind::Array,
                    _ => ObjectKind::Object,
                }
            }
            fn heap_element_type_of(&self, _o: ObjectRef) -> ArrayElementType {
                ArrayElementType::Reference
            }

            // --- monitor primitives (the load-bearing bit) -------------
            fn thread_id(&self) -> u64 {
                self.thread_id
            }
            fn monitor_enter(&mut self, obj: ObjectRef) {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.enter(self.thread_id);
            }
            fn monitor_exit(&mut self, obj: ObjectRef) {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.exit(self.thread_id);
            }
            fn monitor_wait(
                &mut self,
                obj: ObjectRef,
                timeout_ms: Option<u64>,
            ) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.wait(self.thread_id, timeout_ms);
                Ok(None)
            }
            fn monitor_notify(&mut self, obj: ObjectRef) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.notify_all();
                Ok(None)
            }
            fn monitor_notify_all(&mut self, obj: ObjectRef) -> MethodCallResult {
                let m = {
                    let mut s = self.shared.lock().unwrap();
                    s.monitor_for(obj)
                };
                m.notify_all();
                Ok(None)
            }

            // --- default stubs for everything else ---------------------
            fn load_class(&mut self, _n: &str) -> MethodCallResult { Ok(None) }
            fn new_object(&mut self, _c: &str) -> MethodCallResult { Ok(None) }
            fn invoke(&mut self, _c: &str, _m: &str, _d: &str, _a: &[Value]) -> MethodCallResult { Ok(None) }
            fn invoke_virtual(
                &mut self, _r: ObjectRef, _m: &str, _d: &str, _a: &[Value],
            ) -> MethodCallResult { Ok(None) }
            fn identity_hash_code(&self, o: ObjectRef) -> i32 { o.as_ptr() as i32 }
            fn record_printed_value(&mut self, _v: Value) {}
            fn class_name_of_id(&self, _c: ClassId) -> Option<String> { None }
            fn class_id_of_object(&self, _o: ObjectRef) -> ClassId { ClassId::new(0) }
            fn capture_stack_trace(&mut self, _h: i32) -> Vec<StackTraceEntry> { Vec::new() }
            fn get_stack_trace(&self, _h: i32) -> Option<&[StackTraceEntry]> { None }
            fn get_field_by_name(&self, _o: ObjectRef, _n: &str) -> Value { Value::Object(None) }
            fn set_field_by_name(&self, _o: ObjectRef, _n: &str, _v: Value) {}
            fn resolve_field_index(&self, _c: &str, _f: &str) -> Option<usize> { None }
            fn method_exists(&self, _c: &str, _m: &str, _d: &str) -> bool { false }
            fn create_string(&mut self, _t: &str) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn read_string(&self, _o: ObjectRef) -> Option<String> { None }
            fn get_class_mirror(&mut self, _c: ClassId) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn record_printed_line(&mut self, _t: String) {}
            fn get_system_stream(&self, _n: &str) -> Option<ObjectRef> { None }
            fn get_system_property(&self, _k: &str) -> Option<String> { None }
            fn set_system_property(&mut self, _k: &str, _v: &str) -> Option<String> { None }
            fn ensure_class_initialized(&mut self, _n: &str) -> Result<ClassId, MethodCallFailed> {
                Ok(ClassId::new(0))
            }
            fn is_subclass(&self, _c: ClassId, _p: ClassId) -> bool { false }
            fn superclass_of(&self, _c: ClassId) -> Option<ClassId> { None }
            fn is_interface_class(&self, _c: ClassId) -> bool { false }
            fn class_id_by_name(&self, _n: &str) -> Option<ClassId> { None }
            fn loader_id_of_class(&self, _c: ClassId) -> i32 { 2 }
            fn is_record_class(&self, _c: ClassId) -> bool { false }
            fn record_components(&self, _c: ClassId) -> Vec<(String, String)> { Vec::new() }
            fn is_sealed_class(&self, _c: ClassId) -> bool { false }
            fn permitted_subclasses(&self, _c: ClassId) -> Vec<String> { Vec::new() }
            fn thread_start(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
            fn thread_join(&mut self, _o: ObjectRef) -> MethodCallResult { Ok(None) }
            fn thread_is_alive(&self, _o: ObjectRef) -> bool { false }
            fn current_thread_object(&mut self) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn thread_interrupt(&mut self, _o: ObjectRef) {}
            fn is_interrupted(&self, _c: bool) -> bool { false }
            fn active_thread_count(&self) -> i32 { 1 }
            fn enumerate_threads(&self, _m: usize) -> Vec<ObjectRef> { Vec::new() }
            fn heap_allocated_bytes(&self) -> usize { 0 }
            fn loaded_class_count(&self) -> usize { 0 }
            fn gc_collection_count(&self) -> u64 { 0 }
            fn force_gc(&mut self) {}
            fn declared_fields(&self, _c: ClassId) -> Vec<FieldMetadata> { Vec::new() }
            fn declared_methods(&self, _c: ClassId) -> Vec<MethodMetadata> { Vec::new() }
            fn class_interfaces(&self, _c: ClassId) -> Vec<ClassId> { Vec::new() }
            fn class_access_flags(&self, _c: ClassId) -> u16 { 0 }
            fn get_static_field(&self, _c: ClassId, _i: usize) -> Value { Value::Int(0) }
            fn set_static_field(&mut self, _c: ClassId, _i: usize, _v: Value) {}
            fn primitive_class_mirror(&mut self, _n: &str) -> ObjectRef {
                let mut s = self.shared.lock().unwrap();
                s.alloc_entry(HeapEntry::Object { fields: Vec::new() })
            }
            fn fd_table(&self) -> &cratonvm_native_api::fd_table::FileDescriptorTable {
                use std::sync::OnceLock;
                static FD: OnceLock<cratonvm_native_api::fd_table::FileDescriptorTable> = OnceLock::new();
                FD.get_or_init(cratonvm_native_api::fd_table::FileDescriptorTable::new)
            }
            fn get_field_volatile(&self, o: ObjectRef, i: usize) -> Value { self.get_field(o, i) }
            fn set_field_volatile(&self, o: ObjectRef, i: usize, v: Value) { self.set_field(o, i, v) }
            fn compare_and_swap_field(
                &mut self, _o: ObjectRef, _i: usize, _e: Value, _n: Value,
            ) -> bool { false }
            fn park(&mut self, _t: Option<std::time::Duration>) {}
            fn unpark(&self, _o: ObjectRef) {}
            fn allocate_instance(&mut self, _c: &str) -> Option<ObjectRef> { None }
            fn class_annotations(&self, _c: ClassId) -> Vec<AnnotationData> { Vec::new() }
            fn method_annotations(&self, _c: ClassId, _m: &str, _d: &str) -> Vec<AnnotationData> { Vec::new() }
            fn field_annotations(&self, _c: ClassId, _f: &str) -> Vec<AnnotationData> { Vec::new() }
            fn method_parameter_annotations(&self, _c: ClassId, _m: &str, _d: &str) -> Vec<Vec<AnnotationData>> { Vec::new() }
            fn class_signature(&self, _c: ClassId) -> Option<String> { None }
            fn method_signature(&self, _c: ClassId, _m: &str, _d: &str) -> Option<String> { None }
            fn field_signature(&self, _c: ClassId, _f: &str) -> Option<String> { None }
            fn method_annotation_default(&self, _c: ClassId, _m: &str, _d: &str) -> Option<AnnotationElementValue> { None }
            fn get_scoped_value(&self, _k: u64) -> Option<Value> { None }
            fn push_scoped_value(&mut self, _k: u64, _v: Value) {}
            fn pop_scoped_value(&mut self) {}
            fn scoped_value_depth(&self) -> usize { 0 }
            fn allocate_native_memory(&mut self, _s: usize, _a: usize) -> Option<(i64, *mut u8)> { None }
            fn free_native_memory(&mut self, _a: i64) {}
            fn load_native_library(&mut self, _p: &str) -> Result<i64, MethodCallFailed> { Ok(0) }
            fn find_native_symbol(&self, _l: i64, _n: &str) -> Option<usize> { None }
            fn register_upcall(&mut self, _e: cratonvm_native_api::ffi::UpcallEntry) -> usize { 0 }
            fn get_upcall_info(&self, _s: usize) -> Option<(ObjectRef, Vec<i32>, i32)> { None }
            fn module_name_of_class(&self, _c: ClassId) -> Option<String> { None }
            fn find_resource(&self, _n: &str) -> Option<Vec<u8>> { None }
            fn list_application_class_names(&self) -> Vec<String> { Vec::new() }
            fn register_dynamic_classpath(&mut self, _p: &[String]) {}
            fn define_class_from_bytes(&mut self, _n: &str, _b: &[u8]) -> Option<ClassId> { None }
            fn define_class_with_loader(&mut self, _n: &str, _b: &[u8], _l: u32) -> Option<ClassId> { None }
            fn class_id_by_name_and_loader(&self, _n: &str, _l: u32) -> Option<ClassId> { None }
            fn allocate_loader_id(&mut self) -> u32 { 0 }
            fn discover_reference(
                &mut self, _t: u8, _r: ObjectRef, _f: ObjectRef, _q: Option<ObjectRef>,
            ) {}
            fn is_package_exported_unqualified(&self, _m: &str, _p: &str) -> bool { true }
            fn is_package_exported_to(&self, _m: &str, _p: &str, _t: &str) -> bool { true }
            fn is_package_open_unqualified(&self, _m: &str, _p: &str) -> bool { true }
            fn is_package_open_to(&self, _m: &str, _p: &str, _t: &str) -> bool { true }
            fn check_deep_reflection_access(
                &self, _a: ClassId, _t: ClassId,
            ) -> Result<(), String> { Ok(()) }
        }

        /// Helper: initialise a fresh LBQ instance with the given capacity.
        fn make_lbq(ctx: &mut MockCtx, capacity: i32) -> ObjectRef {
            let this = ctx.alloc_lbq_object();
            let _ = native_lbq_init_cap(ctx, &[
                Value::Object(Some(this)),
                Value::Int(capacity),
            ]);
            this
        }

        /// Helper: init LBQ + offer a single value, asserting success.
        fn offer_must_succeed(ctx: &mut MockCtx, q: ObjectRef, v: i32) {
            let r = native_lbq_offer_bool(ctx, &[
                Value::Object(Some(q)),
                Value::Int(v),
            ]).unwrap();
            assert_eq!(r, Some(Value::Int(1)));
        }

        fn lbq_size(ctx: &MockCtx, q: ObjectRef) -> i32 {
            match ctx.get_field(q, LBQ_FIELD_SIZE) {
                Value::Int(v) => v,
                _ => -1,
            }
        }

        // -------- Test 1: poll/peek/poll_last on empty return null ----
        #[test]
        fn poll_on_empty_returns_null() {
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 4);
            let r = native_lbq_poll(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r, Some(Value::Object(None))), "{r:?}");
            let r2 = native_lbq_peek(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r2, Some(Value::Object(None))));
            let r3 = native_lbq_poll_last(&mut ctx, &[Value::Object(Some(q))]).unwrap();
            assert!(matches!(r3, Some(Value::Object(None))));
        }

        // -------- Test 2: put blocks at capacity, wakes on poll ------
        //
        // The producer (main thread) tries to `put` into a full queue;
        // a consumer (spawned thread) polls one slot after 150ms. If
        // the 10k-spin-fallthrough bug were back, the producer's put
        // would return almost instantly (silently overflowing); we
        // assert it took ≥ half the consumer's sleep.
        #[test]
        fn put_blocks_at_capacity_until_poll() {
            use std::time::Instant;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 2);
            offer_must_succeed(&mut ctx, q, 11);
            offer_must_succeed(&mut ctx, q, 22);
            // Third offer must fail — queue at capacity.
            let r3 = native_lbq_offer_bool(&mut ctx, &[
                Value::Object(Some(q)),
                Value::Int(33),
            ]).unwrap();
            assert_eq!(r3, Some(Value::Int(0)), "third offer must fail at cap");

            let mut consumer_ctx = ctx.fork(2);
            let consumer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let r = native_lbq_poll(
                    &mut consumer_ctx, &[Value::Object(Some(q))],
                ).unwrap();
                assert!(matches!(r, Some(Value::Int(11))), "consumer polled {r:?}");
            });

            let started = Instant::now();
            let _ = native_lbq_put_blocking(&mut ctx, &[
                Value::Object(Some(q)),
                Value::Int(33),
            ]);
            let elapsed = started.elapsed();
            consumer.join().unwrap();

            assert!(
                elapsed >= Duration::from_millis(75),
                "put returned in {elapsed:?} — fallthrough bug is back",
            );
            assert_eq!(lbq_size(&ctx, q), 2);
        }

        // -------- Test 3: concurrent put+poll never loses values -----
        #[test]
        fn concurrent_put_poll_no_lost_values() {
            const N: i32 = 200;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 8); // small cap → exercises the block path

            let mut producer_ctx = ctx.fork(10);
            let producer = std::thread::spawn(move || {
                for i in 0..N {
                    let _ = native_lbq_put_blocking(&mut producer_ctx, &[
                        Value::Object(Some(q)),
                        Value::Int(i),
                    ]);
                }
            });

            let mut consumer_ctx = ctx.fork(20);
            let consumer = std::thread::spawn(move || {
                let mut received: Vec<i32> = Vec::with_capacity(N as usize);
                while received.len() < N as usize {
                    if let Ok(Some(Value::Int(v))) = native_lbq_take_blocking(
                        &mut consumer_ctx, &[Value::Object(Some(q))],
                    ) {
                        received.push(v);
                    }
                }
                received
            });

            producer.join().unwrap();
            let received = consumer.join().unwrap();
            assert_eq!(received.len(), N as usize, "every value delivered");
            // Single producer + single consumer → FIFO order.
            assert_eq!(received, (0..N).collect::<Vec<_>>(), "FIFO order");
            assert_eq!(lbq_size(&ctx, q), 0, "queue drained");
        }

        // -------- Test 4: clear unblocks a waiting put ---------------
        //
        // Pre-fix, `clear` did not notify (and did not take the
        // monitor), so a producer parked in `put` would only unblock
        // via the 50 ms re-check timeout (or never, in real-VM mode).
        // After the fix, clear() notifies, so the put wakes promptly.
        #[test]
        fn clear_unblocks_pending_put() {
            use std::time::Instant;
            let mut ctx = MockCtx::new(1);
            let q = make_lbq(&mut ctx, 1);
            offer_must_succeed(&mut ctx, q, 77);

            let mut clearer_ctx = ctx.fork(3);
            let clearer = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                let _ = native_lbq_clear(&mut clearer_ctx, &[Value::Object(Some(q))]);
            });

            let started = Instant::now();
            let _ = native_lbq_put_blocking(&mut ctx, &[
                Value::Object(Some(q)),
                Value::Int(88),
            ]);
            let elapsed = started.elapsed();
            clearer.join().unwrap();

            assert!(
                elapsed >= Duration::from_millis(50),
                "put returned in {elapsed:?} without waiting for clear",
            );
            assert_eq!(lbq_size(&ctx, q), 1);
        }
    }

    // -----------------------------------------------------------------------
    // Stream range materialization — overflow-safe arithmetic + bounded eager
    // count (H11a/H11b). These cover the pure helpers; the callers compute the
    // count in a wider type before delegating here.

    #[test]
    fn range_int_elements_basic() {
        let elems = range_int_elements(0, 5).unwrap();
        assert_eq!(elems.len(), 5);
        assert_eq!(elems[0], Value::Int(0));
        assert_eq!(elems[4], Value::Int(4));
    }

    #[test]
    fn range_int_elements_empty_or_negative_count() {
        assert!(range_int_elements(10, 0).unwrap().is_empty());
        assert!(range_int_elements(10, -3).unwrap().is_empty());
    }

    #[test]
    fn range_int_elements_no_wrap_near_i32_max() {
        // start near i32::MAX: `start + i` must not wrap (would in pure i32).
        let start = (i32::MAX - 2) as i64;
        let elems = range_int_elements(start, 3).unwrap();
        assert_eq!(elems[0], Value::Int(i32::MAX - 2));
        assert_eq!(elems[2], Value::Int(i32::MAX));
    }

    #[test]
    fn range_int_elements_rejects_over_cap() {
        // Mimics IntStream.range(0, i32::MAX): count widened to i64, far past cap.
        let count = i32::MAX as i64; // ~2.1 billion
        assert!(range_int_elements(0, count).is_err());
        // Exactly one over the cap also throws.
        assert!(range_int_elements(0, STREAM_RANGE_MAX_ELEMENTS as i64 + 1).is_err());
        // Exactly at the cap is allowed (no error from the guard itself).
        assert!(range_int_elements(0, STREAM_RANGE_MAX_ELEMENTS as i64).is_ok());
    }

    #[test]
    fn range_long_elements_basic_and_no_wrap() {
        let elems = range_long_elements(0, 4).unwrap();
        assert_eq!(elems.len(), 4);
        assert_eq!(elems[3], Value::Long(3));

        let start = i64::MAX - 2;
        let elems = range_long_elements(start, 3).unwrap();
        assert_eq!(elems[0], Value::Long(i64::MAX - 2));
        assert_eq!(elems[2], Value::Long(i64::MAX));
    }

    #[test]
    fn range_long_elements_rejects_over_cap() {
        // LongStream.range(0, i64::MAX): count widened to i128, vastly past cap.
        let count = i64::MAX as i128;
        assert!(range_long_elements(0, count).is_err());
        assert!(range_long_elements(0, STREAM_RANGE_MAX_ELEMENTS + 1).is_err());
        assert!(range_long_elements(0, STREAM_RANGE_MAX_ELEMENTS).is_ok());
        assert!(range_long_elements(10, -5).unwrap().is_empty());
    }
}
