// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Java Object Serialization native method implementations.
//!
//! Covers ObjectOutputStream, ObjectInputStream, ObjectStreamClass,
//! ObjectStreamField, serialization filters (JEP 290), and related
//! exception types.
//!
//! SYNTHETIC-LIBRARY ONLY. Every registration here is reached solely from
//! `lib.rs` `register_synthetic_overrides` (`#[cfg(feature = "synthetic-jdk")]`);
//! the shipping builds (default features, `--real-jdk` and `--compatible`) run
//! the JDK's own `java.io` serialization bytecode. The
//! `experimental-serialization` feature only compiles this module (for its unit
//! tests) and registers nothing. The streams this stand-in writes are private
//! to CratonVM synthetic builds and are NOT the JDK's grammar -- a classified
//! known limitation of that non-shipping mode, listed in
//! `docs/synthetic_methods.md` (Tier 3, "Known limitation: serialization
//! streams") and
//! `i86-L2-synthetic-serialization-stand-in-is-not-hotspots`. It goes away with
//! the synthetic-library mode; do not repair it toward the JDK's wire format.

use crate::lang_class::ClassTextName;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::lang_class::{
    create_constructor_object, create_method_object, read_constructor_descriptor,
};
use crate::lang_invoke::alloc_method_handle;
use crate::{native_noop, obj_arg, try_alloc_concurrent_synthetic};
use cratonvm_native_api::{MethodMetadata, NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectKind, ObjectRef, Value};

fn serialization_not_supported(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Err(RuntimeError::UnsupportedOperationException {
        message: "Java object serialization is not yet supported".into(),
    }
    .into())
}

/// A real `java.io.NotSerializableException` for `class_name`, thrown as an
/// object rather than as an `IOException` that names the class in its text.
///
/// HotSpot's `ObjectOutputStream.writeObject0` ends with
/// `throw new NotSerializableException(cl.getName())`, and the message is the
/// class name alone. Code catching it does so BY CLASS
/// (`catch (NotSerializableException e)`), which no `IOException` carrying the
/// name in a string can satisfy.
///
/// Falls back to the old `IOException` shape if the exception class cannot be
/// constructed: the write must fail either way, and a wrong-classed failure is
/// strictly better than a silent success.
fn not_serializable_exception(ctx: &mut dyn NativeContext, class_name: &str) -> MethodCallFailed {
    let dotted = class_name.class_text(&*ctx);
    let msg = ctx.create_string(&dotted);
    cratonvm_native_api::throwable_builder::throw_new(
        ctx,
        "java/io/NotSerializableException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
        || {
            RuntimeError::IOException {
                message: format!("java.io.NotSerializableException: {dotted}"),
            }
            .into()
        },
    )
}

// ---------------------------------------------------------------------------
// Serialization byte-buffer registry (M24)
//
// Each ObjectOutputStream / ObjectInputStream instance gets a Rust-side
// Vec<u8> keyed by the stream's STREAM KEY ([`stream_key`]).  Writers append
// protocol-compliant bytes; readers consume them.
//
// gc-common w21-c (`common-w20c-synthetic-serialization-tables-keyed-by-raw-address`):
// every per-stream table in this module used to be keyed by the stream's raw
// heap address, and the writer's back-reference table by each written
// object's raw address. A moving collection in the middle of a write sent the
// rest of the stream to a new, empty buffer (a truncated stream), left the
// writer's table naming vacated addresses (a later object allocated there was
// emitted as a `TC_REFERENCE` to the first), and the reader's handle table
// and the per-stream filter object were raw `ObjectRef`s nothing kept alive
// or current; no row went away with its stream.
//
// Now:
// * a stream's rows are filed under its WEAK LOCK KEY
//   (`crate::gc_stable_weak_lock_key`): minted per VM, unique across VMs,
//   stable across moves, and freed when the stream dies or its VM goes --
//   the registry then hands it to [`forget_serialization_keys`];
// * the writer's back-reference table keys each written object the same way
//   (a dead object's key is never minted again, so a new object at its old
//   address cannot inherit its handle);
// * the reader's materialised objects and a stream's `ObjectInputFilter` are
//   GLOBAL ROOTS (the JDK's handle table and `serialFilter` field hold them
//   strongly), released by `close()`, or -- for a stream that dies unclosed
//   -- moved to the owning VM's retired list and released by that VM's next
//   `ObjectInputStream.<init>` / `close()` ([`serial_drain_retired_roots`]);
// * the stream "this" the `writeExternal` / `readExternal` / `readObject`
//   dispatch needs, and the curObj frames, are per-thread call-scoped state
//   pinned by the entry native ([`enter_stream`], [`cur_push`]), not rows.
// ---------------------------------------------------------------------------

/// The key every per-stream table files `stream`'s rows under (and the
/// writer's back-reference table files each written object under): its weak
/// lock key, minted on first use. Stable across moves; freed with the object.
fn stream_key(ctx: &dyn NativeContext, stream: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, stream)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// One object a stream holds strongly (a reader's materialised object, a
/// stream's `ObjectInputFilter`): a global root of `vm`, or -- in a context
/// without global roots (`add_global_root` answered 0) -- the raw reference.
#[derive(Clone, Copy)]
struct SerialRoot {
    root: usize,
    fallback: ObjectRef,
}

impl SerialRoot {
    fn hold(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Self {
        SerialRoot {
            root: ctx.add_global_root(obj),
            fallback: obj,
        }
    }

    fn current(&self, ctx: &dyn NativeContext) -> Option<ObjectRef> {
        if self.root != 0 {
            ctx.resolve_global_root(self.root)
        } else {
            Some(self.fallback)
        }
    }
}

/// Global roots whose stream died unclosed (or whose VM's rows were dropped
/// with their keys), as `(vm, root)`. Released by that VM's next
/// [`serial_drain_retired_roots`], dropped unreleased at its teardown
/// ([`forget_vm_serialization_state`]).
fn serial_retired_roots() -> &'static Mutex<Vec<(usize, usize)>> {
    static INSTANCE: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    &INSTANCE
}

fn retire_serial_roots(vm: usize, roots: impl IntoIterator<Item = usize>) {
    let mut roots = roots.into_iter().filter(|r| *r != 0).peekable();
    if roots.peek().is_none() {
        return;
    }
    serial_retired_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(roots.map(|r| (vm, r)));
}

/// Release the calling VM's retired roots (see [`serial_retired_roots`]).
fn serial_drain_retired_roots(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    let mine: Vec<usize> = {
        let mut retired = serial_retired_roots()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if retired.is_empty() {
            return;
        }
        let mut mine = Vec::new();
        retired.retain(|&(owner, root)| {
            if owner == vm {
                mine.push(root);
                false
            } else {
                true
            }
        });
        mine
    };
    for root in mine {
        ctx.remove_global_root(root);
    }
}

/// Release `roots` of `vm` now when the caller is that VM, else retire them.
fn release_serial_roots(ctx: &mut dyn NativeContext, vm: usize, roots: Vec<usize>) {
    if vm == ctx.vm_identity() {
        for root in roots.into_iter().filter(|r| *r != 0) {
            ctx.remove_global_root(root);
        }
    } else {
        retire_serial_roots(vm, roots);
    }
}

/// Drop every row filed under `keys`, which the lock-key registry has just
/// freed (the stream or written object died, or its VM is being torn down)
/// and will never mint again. Called from `lib.rs::sweep_lock_keys` and
/// `forget_vm_lock_keys` with the registry lock released, in a collection
/// epilogue: no context, so the global roots a dead reader held move to its
/// VM's retired list. Each table lock is a leaf, taken alone, poison-tolerant.
pub fn forget_serialization_keys(keys: &[usize]) {
    if keys.is_empty() {
        return;
    }
    fn drop_rows<V>(table: &Mutex<HashMap<usize, V>>, keys: &[usize]) {
        let mut map = table.lock().unwrap_or_else(|e| e.into_inner());
        if !map.is_empty() {
            for k in keys {
                map.remove(k);
            }
        }
    }
    drop_rows(oos_buffers(), keys);
    drop_rows(ois_buffers(), keys);
    drop_rows(ois_stream_filters(), keys);
    // A freed key is either a dead stream's (its whole writer state goes) or
    // a dead written object's (its back-reference row goes from every
    // writer; nothing can write that object again).
    {
        let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
        if !registry.is_empty() {
            for k in keys {
                registry.remove(k);
            }
            if registry.values().any(|s| !s.key_to_handle.is_empty()) {
                let freed: std::collections::HashSet<usize> = keys.iter().copied().collect();
                for state in registry.values_mut() {
                    state.forget_keys(&freed);
                }
            }
        }
    }
    if ois_filter_state_count().load(Ordering::Relaxed) != 0 {
        for k in keys {
            filter_state_remove(*k);
        }
    }
    let dead_readers: Vec<OisHandleRow> = {
        let mut map = ois_handles().lock().unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            Vec::new()
        } else {
            keys.iter().filter_map(|k| map.remove(k)).collect()
        }
    };
    for row in dead_readers {
        retire_serial_roots(row.vm, row.slots.iter().map(|s| s.root));
    }
    let dead_filters: Vec<(usize, SerialRoot)> = {
        let mut map = ois_stream_filter_objs()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if map.is_empty() {
            Vec::new()
        } else {
            keys.iter().filter_map(|k| map.remove(k)).collect()
        }
    };
    for (vm, held) in dead_filters {
        retire_serial_roots(vm, [held.root]);
    }
}

/// VM teardown (from `lib.rs::forget_vm_native_root_stores`, after
/// `forget_vm_lock_keys`): drop `vm`'s reader rows, filter rows and retired
/// roots without releasing them -- the VM's global-root table goes with it.
/// The other tables carry no VM column; their rows went with `vm`'s keys.
/// Idempotent.
pub fn forget_vm_serialization_state(vm: usize) {
    ois_handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, row| row.vm != vm);
    ois_stream_filter_objs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, (owner, _)| *owner != vm);
    serial_retired_roots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(owner, _)| *owner != vm);
}

fn oos_buffers() -> &'static Mutex<HashMap<usize, Vec<u8>>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, Vec<u8>>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ois_buffers() -> &'static Mutex<HashMap<usize, (Vec<u8>, usize)>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, (Vec<u8>, usize)>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Wire handle registry for object reference tracking during serialization.
/// Maps a writer's stream key -> its [`HandleState`] (next handle, and the
/// written objects' keys -> handles).
fn handle_registry() -> &'static Mutex<HashMap<usize, HandleState>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, HandleState>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One reader's wire-handle table: the objects materialised from the stream
/// so far, in handle order, each held as a global root of `vm`
/// ([`HandleSlot::root`]). `fallback == None` is a null slot (`TC_CLASS`, a
/// lambda whose proxy could not be registered).
struct OisHandleRow {
    vm: usize,
    slots: Vec<HandleSlot>,
}

#[derive(Clone, Copy)]
struct HandleSlot {
    root: usize,
    fallback: Option<ObjectRef>,
}

/// Reader-side wire handle table. Maps an ObjectInputStream's stream key to
/// the ordered list of objects that have been materialized from the stream
/// so far. The index into the vec plus `BASE_WIRE_HANDLE` gives the wire
/// handle that a future `TC_REFERENCE` can use to refer back to the same
/// object instance — that's how the JDK round-trips cyclic graphs.
///
/// We keep it separate from the writer-side `handle_registry` because the
/// writer keys the sender's objects while the reader holds the freshly
/// allocated objects that live on the *reader's* heap -- strongly, as the
/// JDK's handle table does (gc-common w21-c: they were raw `ObjectRef`s, so a
/// `TC_REFERENCE` to an object the caller had already dropped, or that a
/// collection had moved, resolved to a dead or vacated address).
fn ois_handles() -> &'static Mutex<HashMap<usize, OisHandleRow>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, OisHandleRow>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Assign the next wire handle (post-`BASE_WIRE_HANDLE`) to `obj` on the
/// reader side. The handle must be assigned *before* the object's field
/// contents are read so that cyclic graphs (a field that refers back to its
/// enclosing object) resolve via `TC_REFERENCE` to the same instance.
///
/// Takes the root before the table lock (`add_global_root` takes the VM's
/// root-table lock; this table's lock is a leaf).
fn ois_push_handle(ctx: &mut dyn NativeContext, ois_key: usize, obj: Option<ObjectRef>) -> u32 {
    let slot = match obj {
        Some(o) => HandleSlot {
            root: ctx.add_global_root(o),
            fallback: Some(o),
        },
        None => HandleSlot {
            root: 0,
            fallback: None,
        },
    };
    let vm = ctx.vm_identity();
    let mut map = ois_handles().lock().unwrap_or_else(|e| e.into_inner());
    let row = map.entry(ois_key).or_insert_with(|| OisHandleRow {
        vm,
        slots: Vec::new(),
    });
    let idx = row.slots.len() as u32;
    row.slots.push(slot);
    BASE_WIRE_HANDLE + idx
}

/// Look up a previously-deserialized object by wire handle, at its CURRENT
/// address (the root is resolved outside the table lock).
fn ois_lookup_handle(ctx: &dyn NativeContext, ois_key: usize, handle: u32) -> Option<ObjectRef> {
    let slot = {
        let map = ois_handles().lock().unwrap_or_else(|e| e.into_inner());
        let row = map.get(&ois_key)?;
        let idx = handle.checked_sub(BASE_WIRE_HANDLE)? as usize;
        row.slots.get(idx).copied()?
    };
    let fallback = slot.fallback?;
    if slot.root != 0 {
        ctx.resolve_global_root(slot.root)
    } else {
        Some(fallback)
    }
}

/// Reset the reader-side handle table for a given stream, releasing the
/// roots it held.
fn ois_clear_handles(ctx: &mut dyn NativeContext, ois_key: usize) {
    let row = ois_handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&ois_key);
    if let Some(row) = row {
        let roots: Vec<usize> = row.slots.iter().map(|s| s.root).collect();
        release_serial_roots(ctx, row.vm, roots);
    }
}

// ---------------------------------------------------------------------------
// JEP-290 ObjectInputFilter resource-limit state
//
// One `ObjectInputFilterState` lives per ObjectInputStream (keyed by the
// stream's raw address). It tracks the per-stream running counters
// (depth/refs/bytes) and the configured caps (max_depth, max_refs,
// max_bytes, max_array). A cap of `0` means "unbounded" (no limit
// clause appeared in the filter string).
//
// Enforcement is split between three call sites in the deserialization
// path:
//   * `ois_buf_read` increments `bytes` on every read and trips
//     `rejected` if `max_bytes > 0 && bytes > max_bytes`.
//   * `ois_read_value` calls `filter_check_depth` on entry (and
//     decrements on exit) so the deepest nesting recorded equals the
//     real graph depth.
//   * The `TC_REFERENCE` arm in `ois_read_value` bumps `refs` and
//     consults `max_refs`.
//   * `ois_read_array` consults `max_array` against the on-wire length
//     before allocating the backing array.
//
// Once `rejected` is set the `readObject` native turns it into an
// `IOException("filter status: REJECTED: ...")` — that's the
// `InvalidClassException` flow per JEP-290 §`ObjectInputFilter.Status`.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct ObjectInputFilterState {
    /// Current recursion depth.
    pub depth: u32,
    /// Number of back-references resolved so far.
    pub refs: u32,
    /// Total bytes consumed from the stream.
    pub bytes: u64,
    /// `0` = unbounded.
    pub max_depth: u32,
    /// `0` = unbounded.
    pub max_refs: u32,
    /// `0` = unbounded.
    pub max_bytes: u64,
    /// `0` = unbounded.
    pub max_array: u32,
    /// Sticky reject flag — once set, every subsequent operation is a
    /// short-circuit. Cleared only when the state is dropped at stream
    /// close.
    pub rejected: bool,
    /// Reason string used to build the `IOException` message.
    pub reason: String,
    /// Compiled class-name pattern clauses (FQCN / glob / `!`-reject) from
    /// the same filter string the numeric limits came from. `None` means the
    /// spec carried no pattern clauses (limits-only). The synthetic read path
    /// consults this via `evaluate_serial_filters` so per-stream pattern
    /// filters are honored, not just the four resource limits.
    pub patterns: Option<SerialFilter>,
}

impl ObjectInputFilterState {
    /// Permissive constructor: every dimension unbounded.
    pub fn unbounded() -> Self {
        Self::default()
    }
}

fn ois_filter_state() -> &'static Mutex<HashMap<usize, ObjectInputFilterState>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, ObjectInputFilterState>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// PERF: count of currently-installed per-stream filter states.
///
/// The hot deserialization path (`ois_buf_read` → `filter_account_bytes`,
/// plus `filter_enter_depth` / `filter_exit_depth` / `filter_account_ref`
/// in `ois_read_value`) consults `ois_filter_state` on EVERY byte/ref/depth
/// op. Acquiring the process-wide `ois_filter_state` mutex and hashing the
/// stream address per op is pure overhead for the overwhelmingly common case
/// where the stream installed no JEP-290 filter at all (no `setObjectInput-
/// Filter`, no `parse_serial_filter`, and no class ever rejected). This
/// atomic mirrors `ois_filter_state().len()` exactly so those accessors can
/// short-circuit to their "unbounded / proceed" answer with a single relaxed
/// load — no lock, no hash — when the map is empty.
///
/// CORRECTNESS: the counter must move in lock-step with map membership. We
/// keep it consistent by funnelling every insert through `filter_state_set`
/// and every removal through `filter_state_remove`, which updates the count
/// while holding the map lock so
/// the count is never observed to under-report a live entry. A short-circuit
/// only fires when the count reads zero, which (because increments happen
/// before/with the insert under the lock) can only mean no entry exists for
/// ANY address — strictly the same outcome the locked lookup would have
/// produced (`None` → proceed). When the count is non-zero we always take the
/// original locked path, so behaviour for streams that DO have a filter is
/// byte-for-byte unchanged.
fn ois_filter_state_count() -> &'static AtomicUsize {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    &COUNT
}

/// Insert/replace a filter-state entry, keeping `ois_filter_state_count` in
/// sync with map membership (increment only when a brand-new key is added).
fn filter_state_set(addr: usize, state: ObjectInputFilterState) {
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    if map.insert(addr, state).is_none() {
        ois_filter_state_count().fetch_add(1, Ordering::Relaxed);
    }
}

/// Remove a filter-state entry, keeping the count in sync (decrement only
/// when an entry was actually present).
fn filter_state_remove(addr: usize) {
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    if map.remove(&addr).is_some() {
        ois_filter_state_count().fetch_sub(1, Ordering::Relaxed);
    }
}

/// Install (or replace) the filter state for a stream. Returning a
/// fresh `unbounded()` is also the natural lazy-init in `ois_buf_read`
/// when no explicit state was set yet — that path keeps existing
/// callers that don't care about JEP-290 limits working unchanged.
pub fn ois_set_filter_state(addr: usize, state: ObjectInputFilterState) {
    // PERF: funnel through `filter_state_set` so `ois_filter_state_count`
    // stays in lock-step with map membership (enables the no-lock fast path
    // in the per-op `filter_*` accessors).
    filter_state_set(addr, state);
}

/// Drop the per-stream filter state — called from `ObjectInputStream.close()`.
fn ois_clear_filter_state(addr: usize) {
    // PERF: funnel through `filter_state_remove` to keep the count in sync.
    filter_state_remove(addr);
}

/// Read-only snapshot of the filter state for tests / introspection.
#[cfg(test)]
fn ois_get_filter_state(addr: usize) -> Option<ObjectInputFilterState> {
    let map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    map.get(&addr).cloned()
}

/// Increment the byte counter and trip `rejected` if `max_bytes` is
/// exceeded. Returns `true` if the read should proceed, `false` if the
/// filter has already rejected the stream (in which case the caller
/// should not consume additional bytes — `ois_buf_read` itself returns
/// zero-padding so we don't need a hard failure path here, just the
/// sticky flag for `readObject` to surface as `IOException`).
fn filter_account_bytes(addr: usize, n: usize) -> bool {
    // PERF fast path: no filter installed for ANY stream → no byte cap to
    // enforce. Skip the process-wide lock + per-call hash entirely. Identical
    // result to the locked `get_mut` returning `None` (→ `true`, "unbounded").
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return true;
    }
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    let st = match map.get_mut(&addr) {
        Some(s) => s,
        None => return true, // no filter installed -> unbounded
    };
    if st.rejected {
        return false;
    }
    st.bytes = st.bytes.saturating_add(n as u64);
    if st.max_bytes > 0 && st.bytes > st.max_bytes {
        st.rejected = true;
        st.reason = format!(
            "stream bytes {} exceeds maxbytes={}",
            st.bytes, st.max_bytes
        );
        return false;
    }
    true
}

/// Increment recursion depth. Returns `false` if `max_depth` is now
/// exceeded; the caller should stop recursing and surface
/// `IOException` once control returns to `readObject`.
fn filter_enter_depth(addr: usize) -> bool {
    // PERF fast path: no filter installed → no depth cap. Same result as the
    // locked `get_mut` returning `None` (→ `true`, "unbounded"). Because no
    // state exists, there is nothing to increment; the matching
    // `filter_exit_depth` is likewise a no-op for this stream.
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return true;
    }
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    let st = match map.get_mut(&addr) {
        Some(s) => s,
        None => return true,
    };
    if st.rejected {
        return false;
    }
    st.depth = st.depth.saturating_add(1);
    if st.max_depth > 0 && st.depth > st.max_depth {
        st.rejected = true;
        st.reason = format!("graph depth {} exceeds maxdepth={}", st.depth, st.max_depth);
        return false;
    }
    true
}

/// Pop recursion depth on the way out of `ois_read_value`.
fn filter_exit_depth(addr: usize) {
    // PERF fast path: no filter installed → nothing to decrement. Same result
    // as the locked `get_mut` returning `None` (no-op). `saturating_sub`
    // already floors `depth` at 0, so even if a filter is installed mid-graph
    // (between an enter and its exit) this can never underflow.
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return;
    }
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(st) = map.get_mut(&addr) {
        st.depth = st.depth.saturating_sub(1);
    }
}

/// Account one back-reference and check `max_refs`.
fn filter_account_ref(addr: usize) -> bool {
    // PERF fast path: no filter installed → no ref cap. Same result as the
    // locked `get_mut` returning `None` (→ `true`, "unbounded").
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return true;
    }
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    let st = match map.get_mut(&addr) {
        Some(s) => s,
        None => return true,
    };
    if st.rejected {
        return false;
    }
    st.refs = st.refs.saturating_add(1);
    if st.max_refs > 0 && st.refs > st.max_refs {
        st.rejected = true;
        st.reason = format!(
            "back-references {} exceeds maxrefs={}",
            st.refs, st.max_refs
        );
        return false;
    }
    true
}

/// Check an array allocation against `max_array`. Returns `false` if
/// `length > max_array` (trips the reject flag too).
fn filter_check_array(addr: usize, length: usize) -> bool {
    // PERF fast path: no filter installed → no array-length cap. Same result
    // as the locked `get_mut` returning `None` (→ `true`, "unbounded").
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return true;
    }
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    let st = match map.get_mut(&addr) {
        Some(s) => s,
        None => return true,
    };
    if st.rejected {
        return false;
    }
    if st.max_array > 0 && length as u64 > st.max_array as u64 {
        st.rejected = true;
        st.reason = format!("array length {} exceeds maxarray={}", length, st.max_array);
        return false;
    }
    true
}

/// Poll the sticky reject flag.
fn filter_is_rejected(addr: usize) -> Option<String> {
    // PERF fast path: no filter installed for any stream → nothing could have
    // been rejected. Same result as the locked `get` returning `None`.
    if ois_filter_state_count().load(Ordering::Relaxed) == 0 {
        return None;
    }
    let map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    map.get(&addr)
        .filter(|s| s.rejected)
        .map(|s| s.reason.clone())
}

/// SECURITY (JEP-290 deserialization filter bypass, CRITICAL): mark the
/// per-stream filter state sticky-rejected because a class was rejected by
/// the serial filter (a per-stream `setObjectInputFilter` rule or the
/// process-wide `jdk.serialFilter`). The synthetic read-path
/// (`ois_read_object` / `ois_read_array` / `TC_ENUM`) instantiates classes
/// directly without ever passing through the JDK `resolveClass` natives, so
/// before this fix the filter was simply never consulted and gadget classes
/// were never blocked. Callers invoke this *before* `ensure_class_initialized`
/// and then abort the read; the top-level `readObject` / `readUnshared`
/// natives poll `filter_is_rejected` and turn the sticky flag into the
/// `IOException("filter status: REJECTED: ...")` flow (`InvalidClassException`
/// extends `IOException`). Lazily creates an `unbounded()` state entry if the
/// stream had no resource-limit filter installed, so the reject is recorded
/// even when only a pattern-based filter (no `=N` clauses) is in force.
fn filter_reject_class(addr: usize, class_name: &str) {
    let mut map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
    // PERF/CORRECTNESS: `or_insert_with` can create a brand-new entry here
    // (lazy `unbounded()` when only a pattern filter, or none, was installed).
    // Whenever it does, bump `ois_filter_state_count` so the no-lock fast path
    // in the per-op accessors never under-reports this now-live entry.
    let existed = map.contains_key(&addr);
    let st = map
        .entry(addr)
        .or_insert_with(ObjectInputFilterState::unbounded);
    if !existed {
        ois_filter_state_count().fetch_add(1, Ordering::Relaxed);
    }
    if !st.rejected {
        st.rejected = true;
        st.reason = format!(
            "class \"{}\" rejected by ObjectInputFilter (InvalidClassException)",
            class_name
        );
    }
}

/// SECURITY (JEP-290 deserialization filter bypass, CRITICAL): run a class
/// name through the serial filter from inside the synthetic read-path. If
/// the merged decision is `Rejected`, trip the sticky reject flag (via
/// `filter_reject_class`) and return `true` so the caller aborts the read
/// *before* initializing or instantiating the class. `Allowed` and
/// `Undecided` both return `false` (proceed) — matching the JDK, where an
/// undecided filter falls through to normal resolution.
fn synthetic_read_class_rejected(vm: usize, addr: usize, class_name: &str) -> bool {
    if evaluate_serial_filters(vm, addr, class_name) == FilterStatus::Rejected {
        filter_reject_class(addr, class_name);
        true
    } else {
        false
    }
}

/// Parse a JEP-290 serial-filter string into an `ObjectInputFilterState`.
///
/// Recognises the four resource-limit clauses (`maxdepth=N`, `maxrefs=N`,
/// `maxbytes=N`, `maxarray=N`) AND the class-name pattern clauses (FQCN,
/// glob, `!`-prefixed reject patterns). The numeric limits drive the
/// running per-stream counters; the pattern clauses are compiled into a
/// `SerialFilter` (the same FQCN/glob/`!`-reject matcher used for the
/// process-wide `jdk.serialFilter`) and stored in `patterns` so the
/// synthetic read path can honor per-stream class-name filters — not just
/// the resource limits — via `evaluate_serial_filters`.
///
/// Multiple clauses are separated by `;`. Whitespace is trimmed.
/// Unknown clauses are silently ignored so the parser is forward-
/// compatible with future JDK additions.
///
/// Caps are stored as `u32` / `u64`; a parse failure on the `=N` value
/// (negative, non-numeric, overflow) leaves the dimension unbounded.
pub fn parse_serial_filter(spec: &str) -> ObjectInputFilterState {
    let mut state = ObjectInputFilterState::unbounded();
    for raw in spec.split(';') {
        let clause = raw.trim();
        if clause.is_empty() {
            continue;
        }
        // `key=N` clauses are the only ones we enforce here as limits; every
        // other (non-`=`) clause is a class-name pattern handled below.
        let (key, value) = match clause.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue, // pattern clause — compiled below via SerialFilter::parse
        };
        match key {
            "maxdepth" => {
                if let Ok(n) = value.parse::<u32>() {
                    state.max_depth = n;
                }
            }
            "maxrefs" => {
                if let Ok(n) = value.parse::<u32>() {
                    state.max_refs = n;
                }
            }
            "maxbytes" => {
                if let Ok(n) = value.parse::<u64>() {
                    state.max_bytes = n;
                }
            }
            "maxarray" => {
                if let Ok(n) = value.parse::<u32>() {
                    state.max_array = n;
                }
            }
            _ => {
                // Unknown limit clause — silently ignore.
            }
        }
    }
    // Compile the class-name pattern clauses with the existing glob matcher.
    // `SerialFilter::parse` walks the whole spec but only retains the
    // `FilterEntry::Class` rules for matching (limit clauses become inert
    // `FilterEntry::Limit` entries `check()` skips), so we feed it the raw
    // spec instead of re-tokenising. Store the result only when at least one
    // class-name rule is present — a limits-only spec yields no patterns and
    // leaves `patterns` as `None` (no behavioural change for those callers).
    let compiled = SerialFilter::parse(spec);
    if compiled
        .entries
        .iter()
        .any(|e| matches!(e, FilterEntry::Class { .. }))
    {
        state.patterns = Some(compiled);
    }
    state
}

/// A writer's back-reference table. Each written object is filed under its
/// weak lock key ([`stream_key`]), not its address (gc-common w21-c): the key
/// survives a move, and a dead object's key is never minted again, so a new
/// object at a vacated address is written in full rather than as a
/// `TC_REFERENCE` to the dead one.
struct HandleState {
    next_handle: u32,
    key_to_handle: HashMap<usize, u32>,
    handle_to_key: HashMap<u32, usize>,
    /// Maximum nesting depth allowed (ObjectInputFilter enforcement)
    max_depth: i32,
    /// Maximum reference count allowed
    max_references: i32,
}

impl HandleState {
    fn new() -> Self {
        HandleState {
            next_handle: BASE_WIRE_HANDLE,
            key_to_handle: HashMap::new(),
            handle_to_key: HashMap::new(),
            max_depth: 256,
            max_references: 10000,
        }
    }

    fn assign_handle(&mut self, obj_key: usize) -> u32 {
        let h = self.next_handle;
        self.next_handle += 1;
        self.key_to_handle.insert(obj_key, h);
        self.handle_to_key.insert(h, obj_key);
        h
    }

    fn lookup_handle(&self, obj_key: usize) -> Option<u32> {
        self.key_to_handle.get(&obj_key).copied()
    }

    /// Drop the rows of written objects whose keys were freed (they died).
    fn forget_keys(&mut self, freed: &std::collections::HashSet<usize>) {
        if self.key_to_handle.is_empty() {
            return;
        }
        self.key_to_handle.retain(|k, _| !freed.contains(k));
        self.handle_to_key.retain(|_, k| !freed.contains(k));
    }
}

/// Get (or create) the write buffer for an ObjectOutputStream.
fn oos_buf_write(addr: usize, data: &[u8]) {
    let mut map = oos_buffers().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(addr).or_default().extend_from_slice(data);
}

/// Snapshot the current buffer for an OOS.
fn oos_buf_snapshot(addr: usize) -> Vec<u8> {
    let map = oos_buffers().lock().unwrap_or_else(|e| e.into_inner());
    map.get(&addr).cloned().unwrap_or_default()
}

/// Reset the OOS buffer (used by reset()).
fn oos_buf_reset(addr: usize) {
    let mut map = oos_buffers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(buf) = map.get_mut(&addr) {
        buf.clear();
    }
}

// ---------------------------------------------------------------------------
// "Current object" context (mirrors the JDK's curObj / curDesc fields).
//
// When `writeObject` / `readObject` dispatch into a class's *custom*
// `writeObject(ObjectOutputStream)` / `readObject(ObjectInputStream)` hook,
// that hook may call `defaultWriteObject()` / `defaultReadObject()`. Those
// no-arg natives have no direct reference to the object being serialized —
// the JDK resolves it through the stream's `curObj` / `curDesc` registers.
//
// We model the same thing: a per-stream stack of (object, class_id) frames.
// The OOS/OIS write/read paths push a frame before invoking the hook and pop
// it afterwards; `defaultWriteObject` / `defaultReadObject` consult the top
// frame.
//
// gc-common w21-c: the frames were rows of a process-wide table keyed by the
// stream's raw address, holding the object raw across the hook -- Java that
// can move it. A hook runs on the thread that pushed its frame, so the stack
// is now per THREAD (entries tagged with the stream key), and a frame names
// the object through the pushing native's pin ([`CurFrame::current_obj`]),
// which stays valid while that native -- and so the hook -- is running.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct CurFrame {
    /// The pushing native's pin of the object (see [`CurFrame::current_obj`]).
    obj_pin: usize,
    /// The object as pinned; the pin's fallback only.
    obj: ObjectRef,
    class_id: ClassId,
    /// On the read side, the wire field descriptor of the object currently
    /// being deserialized. `defaultReadObject()` consults this to know which
    /// field bytes to consume. `None` on the write side (the live class
    /// layout is authoritative there).
    read_desc: Option<ClassDescriptor>,
}

impl CurFrame {
    /// The frame's object at its current address.
    fn current_obj(&self, ctx: &dyn NativeContext) -> ObjectRef {
        ctx.read_native_pin(self.obj_pin, self.obj)
    }
}

thread_local! {
    /// This thread's curObj frames, innermost last, each tagged with its
    /// stream's key.
    static CUR_FRAMES: std::cell::RefCell<Vec<(usize, CurFrame)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// The serialization natives running on this thread, innermost last:
    /// (stream key, pin of the stream, the stream as pinned). See
    /// [`enter_stream`].
    static ACTIVE_STREAMS: std::cell::RefCell<Vec<(usize, usize, ObjectRef)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn cur_push(key: usize, frame: CurFrame) {
    CUR_FRAMES.with(|f| f.borrow_mut().push((key, frame)));
}

fn cur_pop(key: usize) {
    CUR_FRAMES.with(|f| {
        let mut f = f.borrow_mut();
        if let Some(i) = f.iter().rposition(|(k, _)| *k == key) {
            f.remove(i);
        }
    });
}

fn cur_top(key: usize) -> Option<CurFrame> {
    CUR_FRAMES.with(|f| {
        f.borrow()
            .iter()
            .rev()
            .find(|(k, _)| *k == key)
            .map(|(_, frame)| frame.clone())
    })
}

fn cur_clear(key: usize) {
    CUR_FRAMES.with(|f| f.borrow_mut().retain(|(k, _)| *k != key));
}

/// A serialization native's registration as the active user of its stream
/// on this thread (see [`enter_stream`]). Dropping it (any return path)
/// unregisters; [`ActiveStream::exit`] also releases its pin.
struct ActiveStream {
    depth: usize,
    pin: usize,
}

impl ActiveStream {
    /// Release the stream's pin (and every pin taken after it) and
    /// unregister.
    fn exit(self, ctx: &mut dyn NativeContext) {
        ctx.unpin_native_roots(self.pin);
    }
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        let depth = self.depth;
        ACTIVE_STREAMS.with(|s| s.borrow_mut().truncate(depth));
    }
}

/// Pin `stream` and register it as the stream keyed `key` for the nested
/// dispatch this native may run (`writeExternal` / `readExternal` / a custom
/// `readObject` hook need the stream object; [`active_stream`] answers it at
/// its current address). Replaces the `oos_stream_refs` /
/// `ois_stream_refs_map` rows, which held the stream raw and were keyed by
/// its address (gc-common w21-c).
fn enter_stream(ctx: &mut dyn NativeContext, key: usize, stream: ObjectRef) -> ActiveStream {
    let pin = ctx.pin_native_root(stream);
    let depth = ACTIVE_STREAMS.with(|s| {
        let mut s = s.borrow_mut();
        let depth = s.len();
        s.push((key, pin, stream));
        depth
    });
    ActiveStream { depth, pin }
}

/// The stream keyed `key` that an active serialization native on this thread
/// registered, at its current address.
fn active_stream(ctx: &dyn NativeContext, key: usize) -> Option<ObjectRef> {
    let (pin, stream) = ACTIVE_STREAMS.with(|s| {
        s.borrow()
            .iter()
            .rev()
            .find(|(k, _, _)| *k == key)
            .map(|&(_, pin, stream)| (pin, stream))
    })?;
    Some(ctx.read_native_pin(pin, stream))
}

/// Load bytes into an OIS read buffer.
fn ois_buf_load(addr: usize, data: Vec<u8>) {
    let mut map = ois_buffers().lock().unwrap_or_else(|e| e.into_inner());
    map.insert(addr, (data, 0));
}

/// Read `n` bytes from an OIS buffer.  Returns empty vec if not enough data.
///
/// JEP-290 wiring: every successful (or attempted) read advances the
/// per-stream `bytes` counter via `filter_account_bytes`. If
/// `max_bytes` has already been tripped the read still returns zeros
/// — the sticky `rejected` flag is what `readObject` consults at the
/// end of a frame to raise `IOException("filter status: REJECTED")`.
///
/// SECURITY (deserialization DoS via unbounded allocation, HIGH): the
/// short-read fall-back previously allocated `vec![0u8; n]` for an
/// arbitrary attacker-controlled `n` (e.g. a `TC_LONGSTRING` length of
/// `0x7FFF_FFFF_FFFF_FFFF` → ~9.2 EB → OOM abort). We now never allocate
/// more than the bytes actually remaining in the buffer, and use checked
/// arithmetic for the `pos + n` bound so a near-`usize::MAX` length can
/// never overflow into a passing comparison.
fn ois_buf_read(addr: usize, n: usize) -> Vec<u8> {
    // Account first so that an exactly-at-limit final read still
    // surfaces the reject (otherwise short-reads near the cap would
    // slip past).
    let _ok = filter_account_bytes(addr, n);
    let mut map = ois_buffers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((buf, pos)) = map.get_mut(&addr) {
        // Checked add: a malicious `n` close to `usize::MAX` must not
        // wrap past `buf.len()` and pass the bounds check.
        if let Some(end) = pos.checked_add(n) {
            if end <= buf.len() {
                let slice = buf[*pos..end].to_vec();
                *pos = end;
                return slice;
            }
        }
    }
    // Short read / no data for this stream. The historical contract is to
    // return an `n`-length zero pad so fixed-width primitive readers
    // (`readInt` etc., which index `bytes[0..n]`) keep working at
    // end-of-stream. We preserve that contract but CAP the synthesised
    // allocation: an attacker-controlled `n` (e.g. a `TC_LONGSTRING`
    // length of `0x7FFF_FFFF_FFFF_FFFF`) must never let this allocate
    // multiple exabytes. Length-prefixed readers (strings, arrays) clamp
    // `n` to the bytes actually remaining *before* calling, so they never
    // depend on the pad past this cap, and short reads still decode to an
    // empty/truncated value rather than an OOM abort.
    vec![0u8; n.min(MAX_SERIAL_BUF_READ_PAD)]
}

/// Upper bound on the zero-padding `ois_buf_read` will synthesise on a
/// short read. Large enough to satisfy any legitimate fixed-width
/// primitive read (8 bytes) with head-room; small enough that an
/// attacker-controlled length can never weaponise the short-read
/// fall-back into an out-of-memory abort.
const MAX_SERIAL_BUF_READ_PAD: usize = 64;

/// Peek at remaining bytes in an OIS buffer.
fn ois_buf_remaining(addr: usize) -> usize {
    let map = ois_buffers().lock().unwrap_or_else(|e| e.into_inner());
    if let Some((buf, pos)) = map.get(&addr) {
        buf.len().saturating_sub(*pos)
    } else {
        0
    }
}

/// Write the Java serialization stream header: 0xACED 0005.
fn write_stream_header(addr: usize) {
    oos_buf_write(addr, &STREAM_MAGIC.to_be_bytes());
    oos_buf_write(addr, &STREAM_VERSION.to_be_bytes());
}

/// Write a class descriptor with real field information.
fn write_class_desc(addr: usize, class_name: &str, svuid: i64, flags: u8, fields: &[(char, &str)]) {
    oos_buf_write(addr, &[TC_CLASSDESC]);
    // Class name as modified UTF-8 (length as u16, then bytes)
    let name_bytes = class_name.as_bytes();
    oos_buf_write(addr, &(name_bytes.len() as u16).to_be_bytes());
    oos_buf_write(addr, name_bytes);
    // serialVersionUID as i64 big-endian
    oos_buf_write(addr, &svuid.to_be_bytes());
    // classDescFlags
    oos_buf_write(addr, &[flags]);
    // fields count
    oos_buf_write(addr, &(fields.len() as u16).to_be_bytes());
    // Write field descriptors: type_code (u8) + field_name (modified UTF-8)
    for &(type_code, field_name) in fields {
        oos_buf_write(addr, &[type_code as u8]);
        let fname_bytes = field_name.as_bytes();
        oos_buf_write(addr, &(fname_bytes.len() as u16).to_be_bytes());
        oos_buf_write(addr, fname_bytes);
        // For object/array type codes, write a type string
        if type_code == 'L' || type_code == '[' {
            // Write TC_STRING with the class name as type string
            write_string_token(addr, "Ljava/lang/Object;");
        }
    }
    // classAnnotation: TC_ENDBLOCKDATA
    oos_buf_write(addr, &[TC_ENDBLOCKDATA]);
    // superClassDesc: TC_NULL (no super for simplified protocol)
    oos_buf_write(addr, &[TC_NULL]);
}

/// Write a TC_OBJECT token with a class descriptor.
/// `svuid` is the caller's ([`stream_class_svuid`] needs a context).
fn write_object_header(addr: usize, class_name: &str, svuid: i64) {
    oos_buf_write(addr, &[TC_OBJECT]);
    // Default: no field descriptors in header (fields written separately by writeObject)
    write_class_desc(addr, class_name, svuid, SC_SERIALIZABLE, &[]);
}

/// Write a TC_STRING with modified UTF-8 encoding.
fn write_string_token(addr: usize, s: &str) {
    oos_buf_write(addr, &[TC_STRING]);
    let bytes = s.as_bytes();
    oos_buf_write(addr, &(bytes.len() as u16).to_be_bytes());
    oos_buf_write(addr, bytes);
}

/// Write TC_NULL.
fn write_null_token(addr: usize) {
    oos_buf_write(addr, &[TC_NULL]);
}

/// Validate Java serialization stream header on an OIS buffer.
fn validate_stream_header(addr: usize) -> bool {
    let data = ois_buf_read(addr, 4);
    if data.len() < 4 {
        return false;
    }
    let magic = u16::from_be_bytes([data[0], data[1]]);
    let version = u16::from_be_bytes([data[2], data[3]]);
    magic == STREAM_MAGIC && version == STREAM_VERSION
}

/// Skip a class descriptor in the OIS buffer (TC_CLASSDESC format).
/// Consumes: TC_CLASSDESC + u16 name len + name bytes + i64 SVUID + u8 flags
///           + u16 field count + TC_ENDBLOCKDATA + TC_NULL
fn skip_class_desc(addr: usize) {
    let tc = ois_buf_read(addr, 1);
    if tc[0] != TC_CLASSDESC {
        return;
    }
    // class name (u16 len + bytes)
    let len_bytes = ois_buf_read(addr, 2);
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let _ = ois_buf_read(addr, len); // name bytes
    let _ = ois_buf_read(addr, 8); // serialVersionUID
    let _ = ois_buf_read(addr, 1); // flags
    let field_count_bytes = ois_buf_read(addr, 2);
    let field_count = u16::from_be_bytes([field_count_bytes[0], field_count_bytes[1]]) as usize;
    // Skip field descriptors
    for _ in 0..field_count {
        let tc_byte = ois_buf_read(addr, 1); // type code
        let fname_len_bytes = ois_buf_read(addr, 2);
        let fname_len = u16::from_be_bytes([fname_len_bytes[0], fname_len_bytes[1]]) as usize;
        let _ = ois_buf_read(addr, fname_len); // field name
                                               // For object/array type codes, there's a TC_STRING type descriptor
        let type_code = tc_byte[0] as char;
        if type_code == 'L' || type_code == '[' {
            let peek = ois_buf_read(addr, 1);
            if peek[0] == TC_STRING {
                let ts_len_bytes = ois_buf_read(addr, 2);
                let ts_len = u16::from_be_bytes([ts_len_bytes[0], ts_len_bytes[1]]) as usize;
                let _ = ois_buf_read(addr, ts_len);
            }
        }
    }
    let _ = ois_buf_read(addr, 1); // TC_ENDBLOCKDATA
    let _ = ois_buf_read(addr, 1); // TC_NULL (super class desc)
}

/// Read a class descriptor from OIS buffer and return the class name.
/// Also consumes the descriptor bytes.
fn read_class_desc_name(addr: usize) -> String {
    let tc = ois_buf_read(addr, 1);
    if tc[0] != TC_CLASSDESC {
        return "java/lang/Object".to_string();
    }
    // class name (u16 len + bytes)
    let len_bytes = ois_buf_read(addr, 2);
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let name_bytes = ois_buf_read(addr, len);
    let class_name = String::from_utf8_lossy(&name_bytes).to_string();
    let _ = ois_buf_read(addr, 8); // serialVersionUID
    let _ = ois_buf_read(addr, 1); // flags
    let field_count_bytes = ois_buf_read(addr, 2);
    let field_count = u16::from_be_bytes([field_count_bytes[0], field_count_bytes[1]]) as usize;
    // Skip field descriptors
    for _ in 0..field_count {
        let tc_byte = ois_buf_read(addr, 1); // type code
        let fname_len_bytes = ois_buf_read(addr, 2);
        let fname_len = u16::from_be_bytes([fname_len_bytes[0], fname_len_bytes[1]]) as usize;
        let _ = ois_buf_read(addr, fname_len); // field name
                                               // For object/array type codes, there's a TC_STRING type descriptor
        let type_code = tc_byte[0] as char;
        if type_code == 'L' || type_code == '[' {
            let peek = ois_buf_read(addr, 1);
            if peek[0] == TC_STRING {
                let ts_len_bytes = ois_buf_read(addr, 2);
                let ts_len = u16::from_be_bytes([ts_len_bytes[0], ts_len_bytes[1]]) as usize;
                let _ = ois_buf_read(addr, ts_len);
            }
        }
    }
    let _ = ois_buf_read(addr, 1); // TC_ENDBLOCKDATA
    let _ = ois_buf_read(addr, 1); // TC_NULL (super class desc)
    class_name
}

/// Parsed class descriptor from the serialization stream.
#[derive(Clone)]
struct ClassDescriptor {
    class_name: String,
    serial_version_uid: i64,
    /// Field type codes: 'I', 'J', 'F', 'D', 'Z', 'B', 'S', 'C', 'L'
    field_types: Vec<char>,
    field_names: Vec<String>,
}

/// Read a full class descriptor from the OIS buffer, returning structured info.
fn read_class_descriptor(addr: usize) -> Option<ClassDescriptor> {
    let tc = ois_buf_read(addr, 1);
    if tc[0] != TC_CLASSDESC {
        return None;
    }
    // class name
    let len_bytes = ois_buf_read(addr, 2);
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let name_bytes = ois_buf_read(addr, len);
    let class_name = String::from_utf8_lossy(&name_bytes).to_string();
    // serialVersionUID
    let svuid_bytes = ois_buf_read(addr, 8);
    let serial_version_uid = i64::from_be_bytes([
        svuid_bytes[0],
        svuid_bytes[1],
        svuid_bytes[2],
        svuid_bytes[3],
        svuid_bytes[4],
        svuid_bytes[5],
        svuid_bytes[6],
        svuid_bytes[7],
    ]);
    let _ = ois_buf_read(addr, 1); // flags
    let field_count_bytes = ois_buf_read(addr, 2);
    let field_count = u16::from_be_bytes([field_count_bytes[0], field_count_bytes[1]]) as usize;
    let mut field_types = Vec::with_capacity(field_count);
    let mut field_names = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let tc_byte = ois_buf_read(addr, 1);
        let type_code = tc_byte[0] as char;
        let fname_len_bytes = ois_buf_read(addr, 2);
        let fname_len = u16::from_be_bytes([fname_len_bytes[0], fname_len_bytes[1]]) as usize;
        let fname_bytes = ois_buf_read(addr, fname_len);
        let fname = String::from_utf8_lossy(&fname_bytes).to_string();
        // For object/array type codes, consume TC_STRING type descriptor
        if type_code == 'L' || type_code == '[' {
            let peek = ois_buf_read(addr, 1);
            if peek[0] == TC_STRING {
                let ts_len_bytes = ois_buf_read(addr, 2);
                let ts_len = u16::from_be_bytes([ts_len_bytes[0], ts_len_bytes[1]]) as usize;
                let _ = ois_buf_read(addr, ts_len);
            }
        }
        field_types.push(type_code);
        field_names.push(fname);
    }
    let _ = ois_buf_read(addr, 1); // TC_ENDBLOCKDATA
    let _ = ois_buf_read(addr, 1); // TC_NULL (super class desc)
    Some(ClassDescriptor {
        class_name,
        serial_version_uid,
        field_types,
        field_names,
    })
}

/// Reset the process-global serialization state belonging to ONE stream
/// address, plus the single piece that really is process-wide.
///
/// This replaces `reset_serialization_globals()`, which `clear()`ed the whole
/// of `oos_buffers` / `ois_buffers` / `handle_registry` / the JEP-290
/// filter-state map. Every one of those maps is keyed by stream address and is
/// shared by ALL tests in the binary — `cargo test` runs the tests of one
/// binary in threads of ONE process — so a test that cleared them wiped the
/// entries a concurrently-running sibling had just installed, between that
/// sibling's write and its read.
///
/// That is the measured cause of `jep290_unbounded_defaults_do_not_reject`
/// failing inside the 4408-test suite and passing alone. Running
/// `serialization::` with `--test-threads=16` in a loop reproduced it in
/// 60/200 runs before this change and 0/200 after, and the same loop showed
/// the fault is not confined to that one test: `jep290_maxbytes_not_tripped_-
/// when_under_limit` (filter state) and `m24_int_roundtrip_via_buffers`
/// (stream buffers) fail through the identical window.
///
/// Scoping the reset to the caller's own address removes the interference at
/// its source, rather than relying on every future test remembering to take
/// `serialization_test_guard()` — a guard only helps when BOTH sides hold it,
/// and the jep290 filter tests never did. The one entry that is not per-stream,
/// the process-wide `jdk.serialFilter`, is still cleared globally; every caller
/// of this function holds the guard while it does so.
#[cfg(test)]
pub fn reset_serialization_state_for(addr: usize) {
    oos_buffers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    ois_buffers()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    handle_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    // PERF: through the helper so `ois_filter_state_count` stays in sync.
    filter_state_remove(addr);
    // The per-stream JEP-290 filter for this address only (the synthetic
    // read-path consults it — see C3).
    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&addr);
    // Per VM since gc-common w10-a; the unit tests' mock contexts are VM 0,
    // and the callers' shared `serialization_test_guard()` is what keeps
    // resetting that row safe.
    PROCESS_SERIAL_FILTER.with(0, |st| {
        *st = ProcessSerialFilter {
            seeded: true,
            ..ProcessSerialFilter::default()
        }
    });
}

/// Test-only serialization guard. The (de)serialization machinery keeps its
/// buffers, handle registry, and JEP-290 filter state in process-global
/// `Mutex`-protected maps. cargo runs unit tests in parallel by default, so a
/// test that calls `reset_serialization_globals()` (which clears those *global*
/// maps) can wipe a concurrently-running test's buffer/filter mid-read. Every
/// test that touches the global serialization state acquires this guard first
/// so those tests run serially with respect to one another. (Poisoning is
/// ignored — a panicked test must not wedge the rest.)
#[cfg(test)]
pub fn serialization_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Serialization Protocol Constants
// ---------------------------------------------------------------------------

const STREAM_MAGIC: u16 = 0xACED;
const STREAM_VERSION: u16 = 5;

/// Hard ceiling on the byte length of a single `TC_LONGSTRING` we will
/// allocate, independent of any configured JEP-290 `maxarray` filter.
/// `TC_LONGSTRING` carries an attacker-controlled u64 length; this cap is
/// the last line of defence against a deserialization DoS (unbounded
/// allocation) when no `maxarray` clause is installed. 256 MiB comfortably
/// exceeds any legitimate serialized string while staying far below the
/// out-of-memory regime an attacker is fishing for.
const MAX_SERIAL_STRING_BYTES: usize = 256 * 1024 * 1024;

/// Hard ceiling on the declared element count of a single `TC_ARRAY` we will
/// allocate, independent of any configured JEP-290 `maxarray` filter.
///
/// `TC_ARRAY` carries an attacker-controlled 32-bit length; with no `maxarray`
/// clause installed (the default), `filter_check_array` returns "unbounded",
/// so a hostile stream declaring ~2.1 billion elements would otherwise drive a
/// multi-GB allocation before any object is read. This is the filter-
/// independent last line of defence (the sibling of `MAX_SERIAL_STRING_BYTES`
/// for `TC_LONGSTRING`). 64 Mi elements comfortably exceeds any legitimate
/// serialized array while bounding the worst-case allocation: even an 8-byte
/// element type (`long`/`double`) caps at 512 MiB, and reference arrays at
/// `64 Mi * sizeof(ObjectRef)`, far below the out-of-memory regime an attacker
/// is fishing for. Beyond this ceiling we additionally reject any length the
/// stream cannot possibly back with at least one byte per element.
const MAX_SERIAL_ARRAY_ELEMS: usize = 64 * 1024 * 1024;

// Type codes (TC_*)
const TC_NULL: u8 = 0x70;
const TC_REFERENCE: u8 = 0x71;
const TC_CLASSDESC: u8 = 0x72;
const TC_OBJECT: u8 = 0x73;
const TC_STRING: u8 = 0x74;
const TC_ARRAY: u8 = 0x75;
const TC_CLASS: u8 = 0x76;
const TC_BLOCKDATA: u8 = 0x77;
const TC_ENDBLOCKDATA: u8 = 0x78;
const TC_RESET: u8 = 0x79;
const TC_BLOCKDATALONG: u8 = 0x7A;
const TC_EXCEPTION: u8 = 0x7B;
const TC_LONGSTRING: u8 = 0x7C;
const TC_PROXYCLASSDESC: u8 = 0x7D;
const TC_ENUM: u8 = 0x7E;
const BASE_WIRE_HANDLE: u32 = 0x7E0000;

// Object stream flags
const SC_WRITE_METHOD: u8 = 0x01;
const SC_SERIALIZABLE: u8 = 0x02;

// Field access flags
const ACC_TRANSIENT: u16 = 0x0080;
const SC_EXTERNALIZABLE: u8 = 0x04;
const SC_BLOCK_DATA: u8 = 0x08;
const SC_ENUM: u8 = 0x10;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple cycle detector for object graph serialization.
fn detect_cycle(written_objects: &[usize], obj_addr: usize) -> bool {
    written_objects.contains(&obj_addr)
}

/// The members `java.io.ObjectStreamClass.computeDefaultSUID` hashes, as the
/// class declares them (raw `access_flags`; the masks are applied by
/// [`default_svuid_of_shape`], exactly where the JDK applies them).
/// Interpreter round i4 wave 96, lane L2
/// (`interpreter-i86-L2-synthetic-serialization-stand-in-is-not-hotspots-RETIRED-20261015.md`).
struct SuidShape {
    /// `Class.getName()`: the binary name with `.` (an array's descriptor
    /// form, `[Ljava.lang.String;`).
    name: String,
    /// `Class.getModifiers()` (the InnerClasses flags of a member class).
    class_mods: i32,
    /// `Class.getInterfaces()` names (`.`-separated); unused for an array.
    interfaces: Vec<String>,
    /// `getDeclaredFields()`: (name, modifiers, descriptor).
    fields: Vec<(String, i32, String)>,
    /// `hasStaticInitializer`: the class declares `<clinit>`.
    has_clinit: bool,
    /// `getDeclaredConstructors()`: (modifiers, descriptor).
    ctors: Vec<(i32, String)>,
    /// `getDeclaredMethods()` (no `<init>` / `<clinit>`): (name, modifiers,
    /// descriptor).
    methods: Vec<(String, i32, String)>,
}

/// `DataOutputStream.writeUTF`: a u16 byte count, then modified UTF-8 over
/// the UTF-16 code units (U+0000 as two bytes, a supplementary character as
/// its two surrogates, three bytes each). A string past 65535 encoded bytes
/// (the JDK throws `UTFDataFormatException`, which `computeDefaultSUID` turns
/// into an `InternalError`) cannot name a class member; it is truncated.
fn java_write_utf(out: &mut Vec<u8>, s: &str) {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.encode_utf16() {
        if (0x0001..=0x007F).contains(&c) {
            bytes.push(c as u8);
        } else if c > 0x07FF {
            bytes.push(0xE0 | ((c >> 12) & 0x0F) as u8);
            bytes.push(0x80 | ((c >> 6) & 0x3F) as u8);
            bytes.push(0x80 | (c & 0x3F) as u8);
        } else {
            bytes.push(0xC0 | ((c >> 6) & 0x1F) as u8);
            bytes.push(0x80 | (c & 0x3F) as u8);
        }
    }
    bytes.truncate(usize::from(u16::MAX));
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(&bytes);
}

/// `String.compareTo`: lexicographic over UTF-16 code units.
fn java_string_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

/// `java.io.ObjectStreamClass.computeDefaultSUID` over `shape` (JDK 25):
/// SHA-1 of, in order, the class name; its modifiers masked to
/// `PUBLIC|FINAL|INTERFACE|ABSTRACT` (an interface's ABSTRACT set iff it
/// declares methods); the sorted interface names (not for an array); the
/// name-sorted fields that are not private static / private transient (name,
/// modifiers masked to `PUBLIC|PRIVATE|PROTECTED|STATIC|FINAL|VOLATILE|TRANSIENT`,
/// descriptor as is); `<clinit>` / `STATIC` / `()V` when present; the
/// signature-sorted non-private constructors and the (name, signature)-sorted
/// non-private methods (modifiers masked to
/// `PUBLIC|PRIVATE|PROTECTED|STATIC|FINAL|SYNCHRONIZED|NATIVE|ABSTRACT|STRICT`,
/// descriptors with `/` turned into `.`). The value is the digest's first
/// eight bytes read little-endian. The sorts are stable (`Arrays.sort` on
/// objects), so two fields of one name keep their declaration order.
fn default_svuid_of_shape(shape: &SuidShape) -> i64 {
    use sha1::Digest as _;
    const PUBLIC: i32 = 0x0001;
    const PRIVATE: i32 = 0x0002;
    const PROTECTED: i32 = 0x0004;
    const STATIC: i32 = 0x0008;
    const FINAL: i32 = 0x0010;
    const SYNCHRONIZED: i32 = 0x0020;
    const VOLATILE: i32 = 0x0040;
    const TRANSIENT: i32 = 0x0080;
    const NATIVE: i32 = 0x0100;
    const INTERFACE: i32 = 0x0200;
    const ABSTRACT: i32 = 0x0400;
    const STRICT: i32 = 0x0800;
    const METHOD_MASK: i32 =
        PUBLIC | PRIVATE | PROTECTED | STATIC | FINAL | SYNCHRONIZED | NATIVE | ABSTRACT | STRICT;

    let mut out: Vec<u8> = Vec::new();
    java_write_utf(&mut out, &shape.name);

    let mut class_mods = shape.class_mods & (PUBLIC | FINAL | INTERFACE | ABSTRACT);
    if (class_mods & INTERFACE) != 0 {
        class_mods = if shape.methods.is_empty() {
            class_mods & !ABSTRACT
        } else {
            class_mods | ABSTRACT
        };
    }
    out.extend_from_slice(&class_mods.to_be_bytes());

    if !shape.name.starts_with('[') {
        let mut names: Vec<&str> = shape.interfaces.iter().map(String::as_str).collect();
        names.sort_by(|a, b| java_string_cmp(a, b));
        for name in names {
            java_write_utf(&mut out, name);
        }
    }

    let mut fields: Vec<&(String, i32, String)> = shape.fields.iter().collect();
    fields.sort_by(|a, b| java_string_cmp(&a.0, &b.0));
    for (name, mods, desc) in fields {
        let mods = mods & (PUBLIC | PRIVATE | PROTECTED | STATIC | FINAL | VOLATILE | TRANSIENT);
        if (mods & PRIVATE) == 0 || (mods & (STATIC | TRANSIENT)) == 0 {
            java_write_utf(&mut out, name);
            out.extend_from_slice(&mods.to_be_bytes());
            java_write_utf(&mut out, desc);
        }
    }

    if shape.has_clinit {
        java_write_utf(&mut out, "<clinit>");
        out.extend_from_slice(&STATIC.to_be_bytes());
        java_write_utf(&mut out, "()V");
    }

    let mut ctors: Vec<&(i32, String)> = shape.ctors.iter().collect();
    ctors.sort_by(|a, b| java_string_cmp(&a.1, &b.1));
    for (mods, desc) in ctors {
        let mods = mods & METHOD_MASK;
        if (mods & PRIVATE) == 0 {
            java_write_utf(&mut out, "<init>");
            out.extend_from_slice(&mods.to_be_bytes());
            java_write_utf(&mut out, &desc.replace('/', "."));
        }
    }

    let mut methods: Vec<&(String, i32, String)> = shape.methods.iter().collect();
    methods.sort_by(|a, b| java_string_cmp(&a.0, &b.0).then_with(|| java_string_cmp(&a.2, &b.2)));
    for (name, mods, desc) in methods {
        let mods = mods & METHOD_MASK;
        if (mods & PRIVATE) == 0 {
            java_write_utf(&mut out, name);
            out.extend_from_slice(&mods.to_be_bytes());
            java_write_utf(&mut out, &desc.replace('/', "."));
        }
    }

    let digest = sha1::Sha1::digest(out.as_slice());
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest[..8]);
    i64::from_le_bytes(first)
}

/// The serialVersionUID a stream written by this stand-in carries for the
/// class named `class_name` (internal form): what
/// `ObjectStreamClass.lookup(cl).getSerialVersionUID()` answers on HotSpot
/// ([`stream_class_svuid_of_id`]). An array is computed from its name (it
/// has no members). A name the VM has no class for -- only this module's own
/// wire sentinels, e.g. [`SERIALIZED_LAMBDA_CLASS`], which no other JVM reads
/// -- keeps the stand-in's old name hash ([`sentinel_name_svuid`]).
fn stream_class_svuid(ctx: &mut dyn NativeContext, class_name: &str) -> i64 {
    if class_name.starts_with('[') {
        return array_default_svuid(ctx, class_name);
    }
    match ctx.class_id_by_name(class_name) {
        Some(class_id) => stream_class_svuid_of_id(ctx, class_id),
        None => sentinel_name_svuid(class_name),
    }
}

/// `ObjectStreamClass.lookup(cl).getSerialVersionUID()` for `class_id`:
/// `0` for a class that is not `Serializable` (`lookupAny`), for an enum
/// (`Enum.class.isAssignableFrom(cl)`), and -- when it declares none -- for a
/// record or a `java.lang.reflect.Proxy` class; else the declared
/// `static final serialVersionUID` (`getDeclaredSUID`, read as `getLong`,
/// so an `int`/`short`/`byte`/`char` one widens); else
/// [`default_svuid_of_shape`] over the class's declared members.
///
/// Does not initialize the class. HotSpot's `hasStaticInitializer` does
/// (JNI `GetStaticMethodID`), but the classes this module writes already
/// have instances, and the descriptor builder computes the value eagerly
/// where the JDK computes it in `getSerialVersionUID()`, so initializing here
/// would run a static initializer `lookup` does not run.
fn stream_class_svuid_of_id(ctx: &mut dyn NativeContext, class_id: ClassId) -> i64 {
    let Some(name) = ctx.class_name_of_id(class_id) else {
        return 0;
    };
    if name.starts_with('[') {
        return array_default_svuid(ctx, &name);
    }
    if !class_is_serializable(ctx, class_id) {
        return 0;
    }
    if class_extends(ctx, class_id, "java/lang/Enum") {
        return 0;
    }
    if let Some(declared) = declared_svuid(ctx, class_id) {
        return declared;
    }
    let direct_super = ctx
        .superclass_of(class_id)
        .and_then(|s| ctx.class_name_of_id(s));
    if matches!(
        direct_super.as_deref(),
        Some("java/lang/Record" | "java/lang/reflect/Proxy")
    ) {
        return 0;
    }
    default_svuid_of_class(ctx, class_id, &name)
}

/// Does `class_id` have `super_name` as a (transitive) superclass?
fn class_extends(ctx: &dyn NativeContext, class_id: ClassId, super_name: &str) -> bool {
    let mut cur = ctx.superclass_of(class_id);
    let mut guard = 0;
    while let Some(id) = cur {
        if ctx.class_name_of_id(id).as_deref() == Some(super_name) {
            return true;
        }
        guard += 1;
        if guard > 256 {
            return false;
        }
        cur = ctx.superclass_of(id);
    }
    false
}

/// `ObjectStreamClass.getDeclaredSUID`: a declared `serialVersionUID` that is
/// both `static` and `final`, read with `Field.getLong` (a non-integral type
/// fails that read, and the JDK falls back to the computed value).
fn declared_svuid(ctx: &dyn NativeContext, class_id: ClassId) -> Option<i64> {
    const ACC_FINAL: u16 = 0x0010;
    let field = ctx
        .declared_fields(class_id)
        .into_iter()
        .find(|f| f.name == "serialVersionUID")?;
    if (field.access_flags & (ACC_STATIC | ACC_FINAL)) != (ACC_STATIC | ACC_FINAL) {
        return None;
    }
    if !matches!(field.descriptor.as_str(), "J" | "I" | "S" | "B" | "C") {
        return None;
    }
    let index = ctx.static_field_index_by_name(class_id, "serialVersionUID")?;
    match ctx.get_static_field(class_id, index) {
        Value::Long(v) => Some(v),
        Value::Int(v) => Some(i64::from(v)),
        _ => None,
    }
}

/// [`default_svuid_of_shape`] over `class_id`'s declared members (`name` is
/// its internal name).
fn default_svuid_of_class(ctx: &mut dyn NativeContext, class_id: ClassId, name: &str) -> i64 {
    let class_mods = crate::lang_class::effective_class_modifiers(ctx, class_id);
    let interfaces: Vec<String> = ctx
        .class_interfaces(class_id)
        .into_iter()
        .filter_map(|id| ctx.class_name_of_id(id))
        .map(|n| n.replace('/', "."))
        .collect();
    let fields: Vec<(String, i32, String)> = ctx
        .declared_fields(class_id)
        .into_iter()
        .map(|f| (f.name, i32::from(f.access_flags), f.descriptor))
        .collect();
    let mut has_clinit = false;
    let mut ctors: Vec<(i32, String)> = Vec::new();
    let mut methods: Vec<(String, i32, String)> = Vec::new();
    for m in ctx.declared_methods(class_id) {
        let mods = i32::from(m.access_flags);
        if m.name == "<clinit>" {
            has_clinit = true;
        } else if m.name == "<init>" {
            ctors.push((mods, m.descriptor));
        } else {
            methods.push((m.name, mods, m.descriptor));
        }
    }
    default_svuid_of_shape(&SuidShape {
        name: name.replace('/', "."),
        class_mods,
        interfaces,
        fields,
        has_clinit,
        ctors,
        methods,
    })
}

/// The default serialVersionUID of the array class `array_name` (descriptor
/// form, `/` or `.`): `getModifiers()` of an array is
/// `FINAL|ABSTRACT|` its element class's access bits (`PUBLIC` for a
/// primitive element, as `Class.getModifiers` answers); an array declares
/// nothing and its interfaces are not hashed.
fn array_default_svuid(ctx: &mut dyn NativeContext, array_name: &str) -> i64 {
    const PUBLIC: i32 = 0x0001;
    const FINAL: i32 = 0x0010;
    const ABSTRACT: i32 = 0x0400;
    let internal = array_name.replace('.', "/");
    let elem = internal.trim_start_matches('[');
    let elem_access = match elem.strip_prefix('L').and_then(|e| e.strip_suffix(';')) {
        Some(class) => match ctx.class_id_by_name(class) {
            Some(id) => crate::lang_class::effective_class_modifiers(ctx, id) & 0x0007,
            None => PUBLIC,
        },
        None => PUBLIC,
    };
    default_svuid_of_shape(&SuidShape {
        name: internal.replace('/', "."),
        class_mods: FINAL | ABSTRACT | elem_access,
        interfaces: Vec::new(),
        fields: Vec::new(),
        has_clinit: false,
        ctors: Vec::new(),
        methods: Vec::new(),
    })
}

/// The stand-in's name hash, kept ONLY for a name no class answers to: this
/// module's private wire sentinels (`cratonvm/internal/...`), which only its
/// own reader reads and which carry no JDK class to compute a value for.
fn sentinel_name_svuid(class_name: &str) -> i64 {
    let mut hash: i64 = 0;
    for b in class_name.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(b as i64);
    }
    hash
}

/// Type-code character to descriptor mapping helper.
fn type_code_is_primitive(code: i32) -> bool {
    matches!(
        code as u8 as char,
        'B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z'
    )
}

/// Read a single field value from the OIS buffer based on its type code.
fn read_field_value(ctx: &mut dyn NativeContext, addr: usize, type_code: char) -> Value {
    match type_code {
        'I' => {
            let bytes = ois_buf_read(addr, 4);
            Value::Int(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        'J' => {
            let bytes = ois_buf_read(addr, 8);
            Value::Long(i64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        }
        'F' => {
            let bytes = ois_buf_read(addr, 4);
            Value::Float(f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
        'D' => {
            let bytes = ois_buf_read(addr, 8);
            Value::Double(f64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        }
        'Z' => {
            let bytes = ois_buf_read(addr, 4); // written as int
            Value::Int(if bytes[3] != 0 { 1 } else { 0 })
        }
        'B' => {
            let bytes = ois_buf_read(addr, 4); // written as int
            Value::Int(bytes[3] as i8 as i32)
        }
        'S' => {
            let bytes = ois_buf_read(addr, 4); // written as int
            Value::Int(i16::from_be_bytes([bytes[2], bytes[3]]) as i32)
        }
        'C' => {
            let bytes = ois_buf_read(addr, 4); // written as int
            Value::Int(u16::from_be_bytes([bytes[2], bytes[3]]) as i32)
        }
        'L' | '[' => {
            // Object (or array) field — dispatch to the recursive reader so
            // that nested objects, arrays, and back-references all work.
            ois_read_value(ctx, addr)
        }
        _ => {
            let bytes = ois_buf_read(addr, 4);
            Value::Int(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        }
    }
}

/// Allocate a stub ObjectStreamClass with default values.
fn alloc_stream_class_stub(
    ctx: &mut dyn NativeContext,
    class_name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let svuid = stream_class_svuid(ctx, class_name);
    // gc-common w20-c: the name first, pinned across the descriptor's
    // allocation (allocated second, it could leave `desc` stale).
    let name = ctx.create_string(class_name);
    let name_pin = ctx.pin_native_root(name);
    let desc: ObjectRef = try_alloc_concurrent_synthetic(ctx, "java/io/ObjectStreamClass", 6)?;
    let name: ObjectRef = ctx.read_native_pin(name_pin, name);
    ctx.unpin_native_roots(name_pin);
    ctx.set_field(desc, 0, Value::Object(Some(name)));
    ctx.set_field(desc, 1, Value::Long(svuid));
    ctx.set_field(desc, 2, Value::Int(0));
    ctx.set_field(desc, 3, Value::Int(SC_SERIALIZABLE as i32));
    ctx.set_field(desc, 4, Value::Int(0));
    ctx.set_field(desc, 5, Value::Int(0));
    Ok(desc)
}

/// Map a JVM field descriptor to its serialization type code.
fn field_type_code(descriptor: &str) -> char {
    match descriptor {
        "I" => 'I',
        "J" => 'J',
        "F" => 'F',
        "D" => 'D',
        "Z" => 'Z',
        "B" => 'B',
        "S" => 'S',
        "C" => 'C',
        s if s.starts_with('[') => '[',
        s if s.starts_with('L') => 'L',
        _ => 'I',
    }
}

/// True iff `class_id` (transitively) implements `java/io/Externalizable`.
fn class_is_externalizable(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    ctx.class_id_by_name("java/io/Externalizable")
        .map(|eid| ctx.is_subclass(class_id, eid))
        .unwrap_or(false)
}

/// True iff `class_id` (transitively) implements `java/io/Serializable`.
fn class_is_serializable(ctx: &dyn NativeContext, class_id: ClassId) -> bool {
    ctx.class_id_by_name("java/io/Serializable")
        .map(|sid| ctx.is_subclass(class_id, sid) || class_id == sid)
        .unwrap_or(false)
}

/// Write a single primitive field value's raw big-endian bytes. Primitives
/// are written inline (no type/handle bytes) exactly as the JDK packs them
/// into the field data block. `bool`/`byte` occupy one byte, `char`/`short`
/// two, `int`/`float` four, `long`/`double` eight.
fn oos_write_primitive(addr: usize, type_code: char, val: &Value) {
    match (type_code, val) {
        ('Z', Value::Int(v)) => oos_buf_write(addr, &[if *v != 0 { 1 } else { 0 }]),
        ('B', Value::Int(v)) => oos_buf_write(addr, &[*v as u8]),
        ('C', Value::Int(v)) => oos_buf_write(addr, &(*v as u16).to_be_bytes()),
        ('S', Value::Int(v)) => oos_buf_write(addr, &(*v as i16).to_be_bytes()),
        ('I', Value::Int(v)) => oos_buf_write(addr, &v.to_be_bytes()),
        ('J', Value::Long(v)) => oos_buf_write(addr, &v.to_be_bytes()),
        ('F', Value::Float(v)) => oos_buf_write(addr, &v.to_be_bytes()),
        ('D', Value::Double(v)) => oos_buf_write(addr, &v.to_be_bytes()),
        // Defensive: a primitive slot holding an unexpected Value kind.
        ('J', _) => oos_buf_write(addr, &0i64.to_be_bytes()),
        ('D', _) => oos_buf_write(addr, &0f64.to_be_bytes()),
        ('F', _) => oos_buf_write(addr, &0f32.to_be_bytes()),
        ('Z' | 'B', _) => oos_buf_write(addr, &[0]),
        ('C' | 'S', _) => oos_buf_write(addr, &0u16.to_be_bytes()),
        _ => oos_buf_write(addr, &0i32.to_be_bytes()),
    }
}

/// Sentinel class name marking a serialized lambda record (our equivalent of
/// `java.lang.invoke.SerializedLambda`). Written by [`oos_write_value`] when it
/// encounters a synthetic `$$Lambda` proxy and recognised by [`ois_read_object`]
/// to drive lambda reconstruction. Not a real loadable class.
const SERIALIZED_LAMBDA_CLASS: &str = "cratonvm/internal/SerializedLambda";

/// Write a length-prefixed (u16) UTF-8 string as raw bytes — NOT a `TC_STRING`
/// token, so it consumes no wire handle. Used for the fixed metadata fields of
/// a serialized-lambda record, read back by [`read_lp_string`].
fn write_lp_string(addr: usize, s: &str) {
    let bytes = s.as_bytes();
    oos_buf_write(addr, &(bytes.len() as u16).to_be_bytes());
    oos_buf_write(addr, bytes);
}

/// Read a length-prefixed (u16) UTF-8 string written by [`write_lp_string`].
fn read_lp_string(addr: usize) -> String {
    let len_bytes = ois_buf_read(addr, 2);
    if len_bytes.len() < 2 {
        return String::new();
    }
    let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
    let bytes = ois_buf_read(addr, len);
    String::from_utf8_lossy(&bytes).to_string()
}

/// Set in a serialized-lambda record's reference-kind byte (a JVMS
/// `reference_kind`, 1..=9, so the high bit is free) when the record carries
/// the proxy's `altMetafactory` marker interfaces and bridge descriptors right
/// after that byte. A record without them is byte-for-byte the older format,
/// and a record of the older format reads back as "none".
const LAMBDA_RECORD_HAS_ALT_LISTS: u8 = 0x80;

/// Write a u16 count followed by that many [`write_lp_string`] strings.
fn write_lp_string_list(addr: usize, items: &[String]) {
    let count = items.len().min(u16::MAX as usize);
    oos_buf_write(addr, &(count as u16).to_be_bytes());
    for item in &items[..count] {
        write_lp_string(addr, item);
    }
}

/// Read a list written by [`write_lp_string_list`]. A truncated stream yields
/// empty strings (as [`read_lp_string`] does), which are dropped.
fn read_lp_string_list(addr: usize) -> Vec<String> {
    let count_bytes = ois_buf_read(addr, 2);
    if count_bytes.len() < 2 {
        return Vec::new();
    }
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]) as usize;
    (0..count)
        .map(|_| read_lp_string(addr))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Write a single lambda-capture value. Object/array captures recurse through
/// the shared reference writer (handles + nested graphs); primitives are
/// packed in their *wide* form (`Z`/`B`/`S`/`C` as a 4-byte int, like the
/// JDK's field block read by [`read_field_value`]) so the read side can decode
/// each capture with `read_field_value` symmetrically.
fn oos_write_capture(
    ctx: &mut dyn NativeContext,
    addr: usize,
    type_code: char,
    val: &Value,
) -> MethodCallResult {
    match type_code {
        'L' | '[' => {
            oos_write_value(ctx, addr, val)?;
        }
        'J' => oos_buf_write(addr, &val.as_long().unwrap_or(0).to_be_bytes()),
        'D' => {
            let d = match val {
                Value::Double(d) => *d,
                _ => 0.0,
            };
            oos_buf_write(addr, &d.to_be_bytes());
        }
        'F' => {
            let f = match val {
                Value::Float(f) => *f,
                _ => 0.0,
            };
            oos_buf_write(addr, &f.to_be_bytes());
        }
        // 'I', 'Z', 'B', 'S', 'C' and any unexpected code: 4-byte int.
        _ => oos_buf_write(addr, &val.as_int().unwrap_or(0).to_be_bytes()),
    }
    Ok(None)
}

/// Reconstruct a serialized lambda (the `SERIALIZED_LAMBDA_CLASS` record whose
/// descriptor has just been consumed by [`read_class_descriptor`]). Re-registers
/// an equivalent lambda proxy via [`NativeContext::register_lambda_proxy`] and
/// allocates a proxy instance with the captured values, so the deserialized
/// SAM dispatches to the same implementation method as the original.
fn reconstruct_serialized_lambda(ctx: &mut dyn NativeContext, addr: usize) -> Value {
    let functional_interface = read_lp_string(addr);
    let sam_method_name = read_lp_string(addr);
    let sam_descriptor = read_lp_string(addr);
    let impl_class = read_lp_string(addr);
    let impl_member = read_lp_string(addr);
    let impl_descriptor = read_lp_string(addr);
    let instantiated_descriptor = read_lp_string(addr);
    let capture_types = read_lp_string(addr);
    let ref_byte = ois_buf_read(addr, 1).first().copied().unwrap_or(6);
    let ref_kind = ref_byte & !LAMBDA_RECORD_HAS_ALT_LISTS;
    // `altMetafactory` markers and bridges, when the writer had any. HotSpot
    // replays them by re-running the capturing class's `$deserializeLambda$`
    // (the same bootstrap, same flags); dropping them lost the deserialized
    // lambda's marker `instanceof` and its bridge descriptors.
    let (marker_interfaces, bridges) = if ref_byte & LAMBDA_RECORD_HAS_ALT_LISTS != 0 {
        let markers = read_lp_string_list(addr);
        (markers, read_lp_string_list(addr))
    } else {
        (Vec::new(), Vec::new())
    };
    let count_bytes = ois_buf_read(addr, 4);
    let count = if count_bytes.len() == 4 {
        i32::from_be_bytes([
            count_bytes[0],
            count_bytes[1],
            count_bytes[2],
            count_bytes[3],
        ])
        .max(0) as usize
    } else {
        0
    };
    let capture_chars: Vec<char> = capture_types.chars().collect();

    let proxy_raw = ctx.register_lambda_proxy(
        &functional_interface,
        &sam_method_name,
        &sam_descriptor,
        &impl_class,
        &impl_member,
        &impl_descriptor,
        ref_kind,
        &instantiated_descriptor,
        &capture_types,
        // Serializable by construction: this proxy is being rebuilt FROM a
        // `SerializedLambda` record, so the original call site must have been
        // serializable for that record to exist at all. Without this the
        // round-tripped lambda would come back with no `writeReplace()` and
        // could not be serialized a second time.
        true,
    );

    // Proxy registration failed (test mock / table full): still consume the
    // capture bytes so the stream stays aligned, then yield null.
    if proxy_raw == 0 {
        ois_push_handle(ctx, addr, None);
        for i in 0..count {
            let tc = capture_chars.get(i).copied().unwrap_or('L');
            let _ = read_field_value(ctx, addr, tc);
        }
        return Value::Object(None);
    }
    // No-ops for the (usual) empty lists.
    ctx.register_lambda_proxy_markers(proxy_raw, &marker_interfaces);
    ctx.register_lambda_proxy_bridges(proxy_raw, &bridges);

    let proxy_cid = ClassId::new(proxy_raw);
    let obj = ctx.alloc_object(proxy_cid, count);
    // Assign the wire handle before decoding captures (mirrors the writer,
    // which assigned the lambda's handle ahead of its payload).
    ois_push_handle(ctx, addr, Some(obj));
    // gc-common w21-c: an object capture is a full nested read (allocations,
    // `readObject` hooks), so the proxy is re-read from a pin for every
    // capture (it was written through its first address).
    let obj_pin = ctx.pin_native_root(obj);
    for i in 0..count {
        let tc = capture_chars.get(i).copied().unwrap_or('L');
        let v = read_field_value(ctx, addr, tc);
        let cur = ctx.read_native_pin(obj_pin, obj);
        ctx.set_field(cur, i, v);
    }
    let obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Value::Object(Some(obj))
}

/// Recursively serialize one reference value (`null`, `String`, array, or a
/// nested Serializable/Externalizable object), handling back-reference
/// (cycle) detection via the per-stream wire-handle table. This is shared by
/// `writeObject`, the object-field marshaller, and `defaultWriteObject`, so
/// nested object graphs round-trip instead of degrading to `TC_NULL`.
fn oos_write_value(ctx: &mut dyn NativeContext, addr: usize, val: &Value) -> MethodCallResult {
    let obj = match val {
        Value::Object(None) => {
            write_null_token(addr);
            return Ok(None);
        }
        Value::Object(Some(o)) => *o,
        // A bare primitive reaching the reference writer means the caller
        // mis-classified the field; encode defensively as TC_NULL.
        _ => {
            write_null_token(addr);
            return Ok(None);
        }
    };

    // gc-common w21-c: the back-reference table keys the object by its weak
    // lock key, which survives a move (it was the raw address).
    let obj_key = stream_key(ctx, obj);

    // Back-reference: emit TC_REFERENCE for an already-written instance.
    {
        let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
        let state = registry.entry(addr).or_insert_with(HandleState::new);
        if let Some(handle) = state.lookup_handle(obj_key) {
            oos_buf_write(addr, &[TC_REFERENCE]);
            oos_buf_write(addr, &handle.to_be_bytes());
            return Ok(None);
        }
    }

    let class_id = ctx.class_id_of_object(obj);
    let class_name = ctx
        .class_name_of_id(class_id)
        .unwrap_or_else(|| "java/lang/Object".to_string());

    if class_name == "java/lang/String" {
        let s = ctx.read_string(obj).unwrap_or_default();
        // Assign a handle (strings are shareable, hence TC_REFERENCE-able).
        {
            let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            registry
                .entry(addr)
                .or_insert_with(HandleState::new)
                .assign_handle(obj_key);
        }
        write_string_token(addr, &s);
        return Ok(None);
    }

    // Arrays: write TC_ARRAY + element-class descriptor + length + elements.
    // Arrays are *always* Serializable in Java, so this must precede the
    // serializability check below. Detect by heap kind (robust against a VM
    // or mock whose `class_name_of_id` does not surface a `[`-prefixed array
    // class name), falling back to the JVM array-descriptor shape.
    if class_name.starts_with('[') || ctx.heap_kind_of(obj) == ObjectKind::Array {
        // gc-common w21-c: an array takes a wire handle too, as in the JDK
        // (`writeArray` -> `handles.assign`). The reader always pushed one for
        // every `TC_ARRAY` (`ois_read_array`), so without it every handle the
        // writer assigned after an array named the wrong object on read (a
        // `TC_REFERENCE` to a `Point` written after an `int[]` field came back
        // as the array), and an array shared twice was written twice.
        {
            let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            registry
                .entry(addr)
                .or_insert_with(HandleState::new)
                .assign_handle(obj_key);
        }
        return oos_write_array(ctx, addr, obj, &class_name);
    }

    // Serializable lambdas. A synthetic `$$Lambda` proxy class is not loadable
    // by name, so the real JDK replaces a serializable lambda with a
    // `SerializedLambda` on write (a compiler-generated `writeReplace`) and
    // reconstructs it on read. We do the equivalent: emit a self-describing
    // record carrying the lambda call-site metadata plus the captured values,
    // keyed by `SERIALIZED_LAMBDA_CLASS` so the read path can rebuild it.
    // Without this, serializing an object that holds a serializable lambda
    // (e.g. Spring's `TypeDescriptor`, whose `annotatedElementSupplier` field
    // is a `() -> ...` lambda) fails on read with
    // `ClassNotFoundException: <host>$$Lambda/0x...`. Must precede the
    // serializability check — our proxy class carries no `Serializable` stamp.
    if let Some(meta) = ctx.lambda_proxy_serial_metadata(class_id) {
        // Assign the wire handle before the payload (cyclic-graph safety).
        {
            let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            registry
                .entry(addr)
                .or_insert_with(HandleState::new)
                .assign_handle(obj_key);
        }
        oos_buf_write(addr, &[TC_OBJECT]);
        let svuid = sentinel_name_svuid(SERIALIZED_LAMBDA_CLASS);
        write_class_desc(addr, SERIALIZED_LAMBDA_CLASS, svuid, SC_SERIALIZABLE, &[]);
        for s in [
            meta.functional_interface.as_str(),
            meta.sam_method_name.as_str(),
            meta.sam_descriptor.as_str(),
            meta.impl_class.as_str(),
            meta.impl_member.as_str(),
            meta.impl_descriptor.as_str(),
            meta.instantiated_descriptor.as_str(),
            meta.capture_types.as_str(),
        ] {
            write_lp_string(addr, s);
        }
        let has_alt_lists = !meta.marker_interfaces.is_empty() || !meta.bridges.is_empty();
        if has_alt_lists {
            oos_buf_write(addr, &[meta.impl_ref_kind | LAMBDA_RECORD_HAS_ALT_LISTS]);
            write_lp_string_list(addr, &meta.marker_interfaces);
            write_lp_string_list(addr, &meta.bridges);
        } else {
            oos_buf_write(addr, &[meta.impl_ref_kind]);
        }
        let capture_chars: Vec<char> = meta.capture_types.chars().collect();
        oos_buf_write(addr, &(capture_chars.len() as i32).to_be_bytes());
        // gc-common w21-c: an object capture is a nested write that can run
        // Java and allocate, so the lambda is re-read from a pin per capture.
        let obj_pin = ctx.pin_native_root(obj);
        for (i, tc) in capture_chars.iter().enumerate() {
            let cur = ctx.read_native_pin(obj_pin, obj);
            let v = ctx.get_field(cur, i);
            oos_write_capture(ctx, addr, *tc, &v)?;
        }
        ctx.unpin_native_roots(obj_pin);
        return Ok(None);
    }

    // Reject non-Serializable classes the way the JDK does — as a real
    // `java.io.NotSerializableException` whose message is the offending class
    // name, not as a plain `IOException` that merely NAMES that class in its
    // text. `catch (NotSerializableException)` and any `instanceof` test are
    // written against the class, and both answered "no" to the message form.
    // `java/io/NotSerializableException` extends `ObjectStreamException`
    // extends `IOException` in `jdk_superclass`, so the coarser handlers still
    // match.
    if !class_is_serializable(ctx, class_id) {
        return Err(not_serializable_exception(ctx, &class_name));
    }

    // Assign the wire handle *before* writing fields so a self-referential
    // field decodes back to the same instance.
    {
        let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
        registry
            .entry(addr)
            .or_insert_with(HandleState::new)
            .assign_handle(obj_key);
    }

    // Synthetic unmodifiable / immutable collection wrappers
    // (`Collections.unmodifiable*`, `List.of`/`Set.of`/`Map.of`). These are
    // `Serializable` in the real JDK but carry no real bytecode fields, so the
    // generic declared-fields marshaller below would emit an empty object and
    // drop the backing collection. Emit an explicit two-field record — slot 0
    // (`L` backing collection) and slot 1 (`I` immutable marker) — which the
    // read path restores positionally onto a freshly-stamped wrapper. The
    // iterator wrappers (`UnmodifiableItr`/`ListItr`) are not `Serializable`
    // and correctly fall through to the rejection above.
    if class_name.starts_with("cratonvm/internal/Unmodifiable") {
        let backing = ctx.get_field(obj, 0);
        let marker = match ctx.get_field(obj, 1) {
            Value::Int(i) => i,
            _ => 0,
        };
        let svuid = stream_class_svuid_of_id(ctx, class_id);
        oos_buf_write(addr, &[TC_OBJECT]);
        write_class_desc(
            addr,
            &class_name,
            svuid,
            SC_SERIALIZABLE,
            &[('L', "backing"), ('I', "marker")],
        );
        oos_write_value(ctx, addr, &backing)?;
        oos_write_primitive(addr, 'I', &Value::Int(marker));
        return Ok(None);
    }

    // Externalizable: emit the header, then let the class's writeExternal
    // append its own data via the ObjectOutput contract.
    if class_is_externalizable(ctx, class_id) {
        oos_buf_write(addr, &[TC_OBJECT]);
        let svuid = stream_class_svuid_of_id(ctx, class_id);
        write_class_desc(
            addr,
            &class_name,
            svuid,
            SC_EXTERNALIZABLE | SC_BLOCK_DATA,
            &[],
        );
        // Find the OOS "this" so writeExternal can call writeInt/etc. on it:
        // the entry native that is writing through the stream keyed `addr`
        // registered it ([`enter_stream`]); if none did, skip (header still
        // written).
        if let Some(stream) = active_stream(ctx, addr) {
            ctx.invoke_virtual(
                obj,
                "writeExternal",
                "(Ljava/io/ObjectOutput;)V",
                &[Value::Object(Some(stream))],
            )?;
        }
        return Ok(None);
    }

    // Plain Serializable object: header with real field descriptors, then
    // the default field data (recursing for object fields).
    let fields = ctx.declared_fields(class_id);
    let serializable_fields: Vec<_> = fields
        .iter()
        .filter(|f| !f.is_static && (f.access_flags & ACC_TRANSIENT) == 0)
        .collect();
    let field_descs: Vec<(char, String)> = serializable_fields
        .iter()
        .map(|f| (field_type_code(&f.descriptor), f.name.clone()))
        .collect();
    let field_refs: Vec<(char, &str)> = field_descs
        .iter()
        .map(|(tc, name)| (*tc, name.as_str()))
        .collect();
    let svuid = stream_class_svuid_of_id(ctx, class_id);
    oos_buf_write(addr, &[TC_OBJECT]);
    write_class_desc(addr, &class_name, svuid, SC_SERIALIZABLE, &field_refs);

    // gc-common w20-c: a nested write can run Java (`writeExternal`) and
    // allocate, so `obj` is read back from a pin for every field (it was read
    // through its first address after each nested object). The dispatcher
    // truncates the pin stack on the `?` returns.
    let obj_pin = ctx.pin_native_root(obj);
    for (idx, f) in serializable_fields.iter().enumerate() {
        let tc = field_descs[idx].0;
        let obj_now = ctx.read_native_pin(obj_pin, obj);
        let v = ctx.get_field(obj_now, f.slot_index);
        if tc == 'L' || tc == '[' {
            oos_write_value(ctx, addr, &v)?;
        } else {
            oos_write_primitive(addr, tc, &v);
        }
    }
    ctx.unpin_native_roots(obj_pin);
    Ok(None)
}

/// Serialize an array object: TC_ARRAY + class descriptor (carrying the
/// array's JVM class name, e.g. `[I`) + i32 length + packed elements.
fn oos_write_array(
    ctx: &mut dyn NativeContext,
    addr: usize,
    arr: ObjectRef,
    class_name: &str,
) -> MethodCallResult {
    oos_buf_write(addr, &[TC_ARRAY]);
    // Prefer the real `[`-prefixed JVM array class name; if the context did
    // not surface one (e.g. the test mock returns "java/lang/Object"),
    // synthesize the descriptor from the runtime element type so the wire
    // form and the element packing below agree.
    let arr_class_name: String = if class_name.starts_with('[') {
        class_name.to_string()
    } else {
        format!(
            "[{}",
            array_element_descriptor_char(ctx.heap_element_type_of(arr))
        )
    };
    let svuid = stream_class_svuid(ctx, &arr_class_name);
    write_class_desc(addr, &arr_class_name, svuid, SC_SERIALIZABLE, &[]);
    let len = ctx.array_length(arr);
    oos_buf_write(addr, &(len as i32).to_be_bytes());
    let elem_char: char = arr_class_name
        .as_bytes()
        .get(1)
        .map(|b| *b as char)
        .unwrap_or('L');
    // gc-common w21-c: a reference element is a nested write that can run
    // Java and allocate, so the array is re-read from a pin per element (the
    // elements after the first nested object were read through the array's
    // first address).
    let arr_pin = ctx.pin_native_root(arr);
    for i in 0..len {
        let cur = ctx.read_native_pin(arr_pin, arr);
        let v = ctx.get_array_element(cur, i);
        match elem_char {
            'B' => oos_buf_write(addr, &[v.as_int().unwrap_or(0) as u8]),
            'Z' => oos_buf_write(addr, &[if v.as_int().unwrap_or(0) != 0 { 1 } else { 0 }]),
            'C' => oos_buf_write(addr, &(v.as_int().unwrap_or(0) as u16).to_be_bytes()),
            'S' => oos_buf_write(addr, &(v.as_int().unwrap_or(0) as i16).to_be_bytes()),
            'I' => oos_buf_write(addr, &(v.as_int().unwrap_or(0)).to_be_bytes()),
            'J' => oos_buf_write(addr, &(v.as_long().unwrap_or(0)).to_be_bytes()),
            'F' => {
                let f = match v {
                    Value::Float(f) => f,
                    _ => 0.0,
                };
                oos_buf_write(addr, &f.to_be_bytes());
            }
            'D' => {
                let d = match v {
                    Value::Double(d) => d,
                    _ => 0.0,
                };
                oos_buf_write(addr, &d.to_be_bytes());
            }
            _ => {
                oos_write_value(ctx, addr, &v)?;
            }
        }
    }
    ctx.unpin_native_roots(arr_pin);
    Ok(None)
}

/// Emit the default field block for the object on top of the `curObj` stack.
///
/// This is the byte-for-byte counterpart of the class descriptor that
/// `writeObject` emitted for a custom-`writeObject` class: the same filter
/// (non-static, non-transient declared fields) in the same order, which is
/// exactly what the reader's `defaultReadObject` consumes. Shared by
/// `defaultWriteObject()` and `writeFields()` so the two cannot drift apart
/// and leave the stream desynchronised.
fn write_default_field_block(
    ctx: &mut dyn NativeContext,
    addr: usize,
    frame: &CurFrame,
) -> MethodCallResult {
    let fields = ctx.declared_fields(frame.class_id);
    let serializable_fields: Vec<_> = fields
        .iter()
        .filter(|f| !f.is_static && (f.access_flags & ACC_TRANSIENT) == 0)
        .collect();
    // gc-common w21-c: the object is read through the frame's pin per field
    // (a nested write can move it; it was read through the frame's raw copy).
    for f in &serializable_fields {
        let tc = field_type_code(&f.descriptor);
        let obj = frame.current_obj(ctx);
        let v = ctx.get_field(obj, f.slot_index);
        if tc == 'L' || tc == '[' {
            oos_write_value(ctx, addr, &v)?;
        } else {
            oos_write_primitive(addr, tc, &v);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// ObjectOutputStream  (6-field synthetic)
//   0: stream_ref, 1: protocol_version, 2: depth,
//   3: objects_written, 4: block_mode, 5: enable_replace
// ---------------------------------------------------------------------------

fn register_object_output_stream(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectOutputStream";

    // <init>(OutputStream)V
    r.register(cls, "<init>", "(Ljava/io/OutputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream = match args.get(1) {
            Some(Value::Object(Some(stream))) => *stream,
            _ => {
                // The JDK constructor writes the stream header to `out`
                // before returning, so `new ObjectOutputStream(null)` throws
                // `NullPointerException` from inside the constructor itself
                // rather than "later, at the first delegated write" (see
                // `object_output_stream_p70`'s own comment, corrected here).
                // The javadoc says so explicitly: "NullPointerException - if
                // out is null".
                return Err(RuntimeError::NullPointerException { message: None }.into());
            }
        };
        ctx.set_field(this, 0, Value::Object(Some(stream)));
        ctx.set_field(this, 1, Value::Int(2)); // protocol version 2
        ctx.set_field(this, 2, Value::Int(0)); // depth
        ctx.set_field(this, 3, Value::Int(0)); // objects_written
        ctx.set_field(this, 4, Value::Int(1)); // block_mode on
        ctx.set_field(this, 5, Value::Int(0)); // enable_replace off
        let addr = stream_key(ctx, this);
        // A new stream's key is fresh (a dead stream's key is never minted
        // again, gc-common w21-c), so these resets are defensive only: they
        // used to guard a stream constructed at the recycled ADDRESS of a
        // collected one, which emitted that stream's bytes ahead of its own
        // magic.
        oos_buf_reset(addr);
        write_stream_header(addr);
        // Initialize handle tracking for this stream
        {
            let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            registry.insert(addr, HandleState::new());
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // writeObject(Object)V — writes TC_OBJECT / TC_NULL / TC_STRING with field values
    r.register(cls, "writeObject", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        // Cross-call GC-safety fix (2026-07-07, same shape as
        // ois_read_object): `this` (the ObjectOutputStream instance) is a
        // bare Rust local read again *after* the custom writeObject hook /
        // default oos_write_value recursion below, both of which can run
        // arbitrary Java bytecode and trigger a moving GC. Pin it so the
        // post-dispatch depth-counter reset reads the live address.
        // gc-common w21-c: the pin also registers the stream for the nested
        // `writeExternal` dispatch ([`active_stream`]).
        let entry = enter_stream(ctx, addr, this);
        let this_pin = entry.pin;
        let depth = match ctx.get_field(this, 2) {
            Value::Int(d) => d,
            _ => 0,
        };

        // Enforce max depth limit
        {
            let registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = registry.get(&addr) {
                if depth + 1 > state.max_depth {
                    return Err(RuntimeError::IOException {
                        message: format!(
                            "Serialization depth {} exceeds maximum {}",
                            depth + 1,
                            state.max_depth
                        ),
                    }
                    .into());
                }
            }
        }

        ctx.set_field(this, 2, Value::Int(depth + 1));
        let count = match ctx.get_field(this, 3) {
            Value::Int(c) => c,
            _ => 0,
        };

        // Enforce max references limit
        {
            let registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = registry.get(&addr) {
                if count + 1 > state.max_references {
                    return Err(RuntimeError::IOException {
                        message: format!(
                            "Serialization reference count {} exceeds maximum {}",
                            count + 1,
                            state.max_references
                        ),
                    }
                    .into());
                }
            }
        }

        ctx.set_field(this, 3, Value::Int(count + 1));

        // A class that declares a custom `private void writeObject(
        // ObjectOutputStream)` controls its own field layout. We honour
        // that hook: write the object header, push the curObj frame, then
        // dispatch the hook (which may call defaultWriteObject() and the
        // primitive writers). For all other objects fall back to the shared
        // default marshaller (`oos_write_value`).
        let result = (|| -> MethodCallResult {
            let obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    write_null_token(addr);
                    return Ok(None);
                }
            };
            let class_id = ctx.class_id_of_object(obj);
            let class_name = ctx
                .class_name_of_id(class_id)
                .unwrap_or_else(|| "java/lang/Object".to_string());

            // Strings / arrays / Externalizable / non-Serializable are all
            // handled uniformly by the shared writer.
            let has_custom = class_name != "java/lang/String"
                && !class_name.starts_with('[')
                && class_is_serializable(ctx, class_id)
                && !class_is_externalizable(ctx, class_id)
                && find_private_method(
                    ctx,
                    class_id,
                    "writeObject",
                    "(Ljava/io/ObjectOutputStream;)V",
                )
                .is_some();

            if !has_custom {
                return oos_write_value(ctx, addr, &Value::Object(Some(obj)));
            }

            // Custom writeObject path. Back-reference check first.
            let obj_key = stream_key(ctx, obj);
            {
                let mut registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
                let state = registry.entry(addr).or_insert_with(HandleState::new);
                if let Some(handle) = state.lookup_handle(obj_key) {
                    oos_buf_write(addr, &[TC_REFERENCE]);
                    oos_buf_write(addr, &handle.to_be_bytes());
                    return Ok(None);
                }
                state.assign_handle(obj_key);
            }
            let fields = ctx.declared_fields(class_id);
            let field_descs: Vec<(char, String)> = fields
                .iter()
                .filter(|f| !f.is_static && (f.access_flags & ACC_TRANSIENT) == 0)
                .map(|f| (field_type_code(&f.descriptor), f.name.clone()))
                .collect();
            let field_refs: Vec<(char, &str)> = field_descs
                .iter()
                .map(|(tc, name)| (*tc, name.as_str()))
                .collect();
            let svuid = stream_class_svuid_of_id(ctx, class_id);
            oos_buf_write(addr, &[TC_OBJECT]);
            write_class_desc(
                addr,
                &class_name,
                svuid,
                SC_SERIALIZABLE | SC_WRITE_METHOD,
                &field_refs,
            );
            // gc-common w21-c: the frame names `obj` through this pin, which
            // outlives the hook (its `defaultWriteObject` reads the object
            // after its own nested writes).
            let obj_pin = ctx.pin_native_root(obj);
            cur_push(
                addr,
                CurFrame {
                    obj_pin,
                    obj,
                    class_id,
                    read_desc: None,
                },
            );
            let this_now = ctx.read_native_pin(this_pin, this);
            let hook = ctx.invoke_special(
                &class_name,
                "writeObject",
                "(Ljava/io/ObjectOutputStream;)V",
                &[Value::Object(Some(obj)), Value::Object(Some(this_now))],
            );
            cur_pop(addr);
            ctx.unpin_native_roots(obj_pin);
            hook.map(|_| None)
        })();

        let this_cur = ctx.read_native_pin(this_pin, this);
        entry.exit(ctx);
        ctx.set_field(this_cur, 2, Value::Int(depth));
        result
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // writeUnshared(Object)V — like writeObject but the JDK guarantees the
    // object is never shared via a back-reference. We emit the full object
    // (fields included) via the shared marshaller; sharing/back-references
    // only ever point at *shared* writes, so not registering a handle is the
    // correct unshared semantics.
    r.register(
        cls,
        "writeUnshared",
        "(Ljava/lang/Object;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = stream_key(ctx, this);
            let count = match ctx.get_field(this, 3) {
                Value::Int(c) => c,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(count + 1));
            let val = args.get(1).copied().unwrap_or(Value::Object(None));
            let entry = enter_stream(ctx, addr, this);
            let result = oos_write_value(ctx, addr, &val);
            entry.exit(ctx);
            result
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Primitive/data writers — write actual big-endian bytes to the buffer.
    r.register(cls, "writeInt", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        oos_buf_write(stream_key(ctx, this), &val.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeLong", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        oos_buf_write(stream_key(ctx, this), &val.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeFloat", "(F)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Float(v)) => *v,
            _ => 0.0,
        };
        // `floatToIntBits`: the canonical NaN (round 15 wave 5, lane jdkonly7).
        let bits = crate::lang_math::java_float_to_int_bits(val);
        oos_buf_write(stream_key(ctx, this), &bits.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeDouble", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Double(v)) => *v,
            _ => 0.0,
        };
        // `doubleToLongBits`: the canonical NaN.
        let bits = crate::lang_math::java_double_to_long_bits(val);
        oos_buf_write(stream_key(ctx, this), &bits.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeBoolean", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val: u8 = match args.get(1) {
            Some(Value::Int(v)) if *v != 0 => 1,
            _ => 0,
        };
        oos_buf_write(stream_key(ctx, this), &[val]);
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeByte", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Int(v)) => *v as u8,
            _ => 0u8,
        };
        oos_buf_write(stream_key(ctx, this), &[val]);
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeShort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Int(v)) => *v as i16,
            _ => 0i16,
        };
        oos_buf_write(stream_key(ctx, this), &val.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeChar", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = match args.get(1) {
            Some(Value::Int(v)) => *v as u16,
            _ => 0u16,
        };
        oos_buf_write(stream_key(ctx, this), &val.to_be_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeUTF", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        let s = match args.get(1) {
            Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
            _ => String::new(),
        };
        let bytes = s.as_bytes();
        oos_buf_write(addr, &(bytes.len() as u16).to_be_bytes());
        oos_buf_write(addr, bytes);
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeBytes", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = match args.get(1) {
            Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
            _ => String::new(),
        };
        oos_buf_write(stream_key(ctx, this), s.as_bytes());
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "writeChars", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        let s = match args.get(1) {
            Some(Value::Object(Some(obj))) => ctx.read_string(*obj).unwrap_or_default(),
            _ => String::new(),
        };
        for ch in s.encode_utf16() {
            oos_buf_write(addr, &ch.to_be_bytes());
        }
        let c = match ctx.get_field(this, 3) {
            Value::Int(v) => v,
            _ => 0,
        };
        ctx.set_field(this, 3, Value::Int(c + 1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "defaultWriteObject", "()V", |ctx, args| {
        // Called from inside a user's custom writeObject() hook. Writes the
        // current object's non-static, non-transient field data using the
        // class descriptor pushed by `writeObject` (the curObj frame).
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        let frame = match cur_top(addr) {
            Some(f) => f,
            // Not invoked from within a writeObject hook — the JDK throws
            // NotActiveException; we no-op to stay lenient for callers that
            // (incorrectly) call it outside the hook.
            None => return Ok(None),
        };
        let entry = enter_stream(ctx, addr, this);
        let result = write_default_field_block(ctx, addr, &frame);
        entry.exit(ctx);
        result
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // flush() — copy internal buffer into underlying ByteArrayOutputStream if present
    r.register(cls, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
            let class_id = ctx.class_id_of_object(stream);
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if class_name == "java/io/ByteArrayOutputStream" {
                let buf = oos_buf_snapshot(addr);
                // gc-common w20-c: the array's allocation can move the
                // stream, which is written after it.
                let stream_pin = ctx.pin_native_root(stream);
                let arr = ctx.new_array(ArrayElementType::Byte, buf.len());
                let stream: ObjectRef = ctx.read_native_pin(stream_pin, stream);
                ctx.unpin_native_roots(stream_pin);
                for (i, &b) in buf.iter().enumerate() {
                    ctx.set_array_element(arr, i, Value::Int(b as i8 as i32));
                }
                ctx.set_field(stream, 0, Value::Object(Some(arr)));
                ctx.set_field(stream, 1, Value::Int(buf.len() as i32));            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "close", "()V", |ctx, args| {
        // Flush internal buffer to underlying stream, then close it.
        //
        // `ObjectOutputStream.close()` is `flush(); clear(); bout.close();`
        // under `throws IOException` with no `catch` anywhere on the chain
        // (`flush()` is a bare `bout.flush()`), so BOTH delegations PROPAGATE.
        // Dropping them turned a failed serialization flush — a full disk, a
        // broken socket — into a clean `try`-with-resources exit over a
        // truncated stream. W7-57-close-flush-swallow-sweep.md
        //
        // The per-stream side-table drop still runs on the failing path: it is
        // our own bookkeeping, not part of the JDK body, and leaving an entry
        // behind on a recycled address is its own defect. The first failure is
        // the one reported, matching the JDK's straight-line order.
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        // gc-common w20-c: `flush` is Java, and `this` is read after it.
        let this_pin = ctx.pin_native_root(this);
        let mut outcome = ctx.invoke_virtual(this, "flush", "()V", &[]).map(|_| ());
        let this: ObjectRef = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        if outcome.is_ok() {
            // Skipped when the flush threw — the JDK body is straight-line, so
            // `bout.close()` is not reached either.
            if let Value::Object(Some(stream)) = ctx.get_field(this, 0) {
                outcome = ctx.invoke_virtual(stream, "close", "()V", &[]).map(|_| ());
            }
        }
        // Drop this thread's curObj frames for the stream. Its buffer and
        // handle table stay (a `flush()` after `close()` must not overwrite
        // the sink with an empty buffer); they go with the stream's key.
        cur_clear(addr);
        outcome?;
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // reset()V
    r.register(cls, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 3, Value::Int(0)); // reset objects_written
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // useProtocolVersion(int)V
    r.register(cls, "useProtocolVersion", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let version = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 2,
        };
        ctx.set_field(this, 1, Value::Int(version));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // writeFields()V — emits the field block for the object currently being
    // written, the `putFields()`-based alternative to `defaultWriteObject()`.
    //
    // The wave-2 note kept this a no-op on the grounds that the `PutField`
    // accumulator is empty by construction (true — no `PutField.put*` native is
    // registered anywhere in the tree, so nothing is ever buffered). But the
    // block is NOT optional: `writeObject` has already emitted a class
    // descriptor naming these fields, and the reader's `defaultReadObject`
    // consumes exactly that many bytes. Writing nothing desynchronises the
    // stream for any class that has non-transient fields AND uses `putFields`.
    // Emit the same block `defaultWriteObject()` does, from the live object.
    //
    // For the one real caller in-tree — `ConcurrentHashMap.writeObject`
    // (native-collections), which uses the `serialPersistentFields` idiom —
    // every declared field is transient, so the block is empty and the emitted
    // bytes are identical to the previous no-op. Reading the values from the
    // live object rather than the accumulator is the remaining approximation;
    // closing it means implementing `PutField.put*`, still an open residual.
    //
    // Wave-4 verification of that residual, since it reads like a silent
    // wrong-answer risk and is not one: `git grep 'ObjectOutputStream$PutField'`
    // over the tree finds only the two `putFields()` factories (here and
    // native-collections) — there is no `put`/`write` native on the class, and
    // the object those factories hand back is a 2-field SYNTHETIC whose class
    // has no bytecode. So a caller that actually uses the accumulator fails
    // loudly at the first `put(...)` and never reaches `writeFields`. The
    // divergence is bounded to "reads live values" for callers that obtain a
    // PutField and never write to it; a caller that writes to it cannot get
    // here at all.
    r.register(cls, "writeFields", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        match cur_top(addr) {
            Some(frame) => {
                let entry = enter_stream(ctx, addr, this);
                let result = write_default_field_block(ctx, addr, &frame);
                entry.exit(ctx);
                result
            }
            // Outside a writeObject hook the JDK throws NotActiveException;
            // stay lenient here, exactly as `defaultWriteObject` above does.
            None => Ok(None),
        }
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // putFields() -> ObjectOutputStream.PutField
    r.register(
        cls,
        "putFields",
        "()Ljava/io/ObjectOutputStream$PutField;",
        |ctx, _args| {
            let obj: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/io/ObjectOutputStream$PutField", 2)?;
            ctx.set_field(obj, 0, Value::Int(0)); // field count
            ctx.set_field(obj, 1, Value::Int(0)); // written flag
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // writeStreamHeader()V — emit STREAM_MAGIC (0xACED) + STREAM_VERSION (5).
    //
    // Was a no-op. `<init>` already calls `write_stream_header` directly, so
    // the constructor path was fine, but this method is `protected` precisely
    // so a subclass (or `reset()`-style re-priming code) can re-emit the
    // header — and those callers silently produced a stream with no magic,
    // which the matching `ObjectInputStream` then rejects as corrupt. Writing
    // the real four bytes costs nothing on the constructor path because that
    // path does not route through here.
    r.register(cls, "writeStreamHeader", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        write_stream_header(stream_key(ctx, this));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---------------------------------------------------------------------------
// ObjectInputStream  (6-field synthetic)
//   0: stream_ref, 1: depth, 2: objects_read,
//   3: enable_resolve, 4: block_mode, 5: closed
// ---------------------------------------------------------------------------

/// Read one serialized value from an `ObjectInputStream`'s byte buffer,
/// recursively. This is the core of `readObject` and is also invoked from
/// `read_field_value` so that a class's object-typed fields can themselves
/// be full `TC_OBJECT` / `TC_REFERENCE` / `TC_ARRAY` tokens.
fn ois_read_value(ctx: &mut dyn NativeContext, addr: usize) -> Value {
    if ois_buf_remaining(addr) == 0 {
        return Value::Object(None);
    }
    // JEP-290 depth gate. `filter_enter_depth` short-circuits on the
    // sticky reject flag too, so once `max_bytes`/`max_refs` have
    // tripped further recursion stops here. The matching `exit_depth`
    // runs at every return point — collected at function end via a
    // small RAII guard below.
    if !filter_enter_depth(addr) {
        return Value::Object(None);
    }
    struct DepthGuard(usize);
    impl Drop for DepthGuard {
        fn drop(&mut self) {
            filter_exit_depth(self.0);
        }
    }
    let _guard = DepthGuard(addr);

    let tc = ois_buf_read(addr, 1);
    match tc[0] {
        TC_NULL => Value::Object(None),
        TC_REFERENCE => {
            // Account the back-reference before resolving it so a
            // maliciously long `TC_REFERENCE` chain fails fast.
            if !filter_account_ref(addr) {
                return Value::Object(None);
            }
            let h = ois_buf_read(addr, 4);
            let handle = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
            match ois_lookup_handle(ctx, addr, handle) {
                Some(o) => Value::Object(Some(o)),
                None => Value::Object(None),
            }
        }
        TC_STRING => {
            let len_bytes = ois_buf_read(addr, 2);
            let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
            let str_bytes = ois_buf_read(addr, len);
            let s = String::from_utf8_lossy(&str_bytes).to_string();
            let obj = ctx.create_string(&s);
            ois_push_handle(ctx, addr, Some(obj));
            Value::Object(Some(obj))
        }
        TC_LONGSTRING => {
            let lb = ois_buf_read(addr, 8);
            let wire_len =
                u64::from_be_bytes([lb[0], lb[1], lb[2], lb[3], lb[4], lb[5], lb[6], lb[7]]);

            // SECURITY (deserialization DoS via unbounded allocation, HIGH):
            // `TC_LONGSTRING` carries a full u64 length straight off the wire.
            // The previous code passed it verbatim to `ois_buf_read`, so a
            // hostile length such as `0x7FFF_FFFF_FFFF_FFFF` requested ~9.2 EB
            // → OOM abort. We mirror `ois_read_array`'s maxarray discipline:
            //   1. clamp to the bytes actually remaining in the stream (a
            //      valid string can never be longer than what's left), and
            //   2. run the clamped length through `filter_check_array` so any
            //      configured JEP-290 `maxarray` cap trips the sticky reject
            //      flag, plus a hard ceiling independent of the filter.
            let remaining = ois_buf_remaining(addr) as u64;
            let clamped = wire_len.min(remaining).min(MAX_SERIAL_STRING_BYTES as u64) as usize;

            // Honour a configured `maxarray` (the JEP-290 cap that bounds
            // attacker-controlled element counts). A reject here trips the
            // sticky flag; `readObject`/`readUnshared` surface it as the
            // `IOException("filter status: REJECTED")` flow.
            if !filter_check_array(addr, clamped) {
                return Value::Object(None);
            }

            let str_bytes = ois_buf_read(addr, clamped);
            let s = String::from_utf8_lossy(&str_bytes).to_string();
            let obj = ctx.create_string(&s);
            ois_push_handle(ctx, addr, Some(obj));
            Value::Object(Some(obj))
        }
        TC_CLASS => {
            skip_class_desc(addr);
            ois_push_handle(ctx, addr, None);
            Value::Object(None)
        }
        TC_ENUM => {
            // SECURITY (JEP-290 deserialization filter bypass, CRITICAL):
            // filter the enum class too. We read (rather than skip) the class
            // descriptor so we have the enum type name to evaluate; a reject
            // trips the sticky flag and aborts before the constant name is
            // materialized or a wire handle is reserved. `read_class_desc_name`
            // consumes the same descriptor bytes `skip_class_desc` would.
            let enum_class = read_class_desc_name(addr);
            if synthetic_read_class_rejected(ctx.vm_identity(), addr, &enum_class) {
                return Value::Object(None);
            }
            let name_val = ois_read_value(ctx, addr);
            ois_push_handle(
                ctx,
                addr,
                match name_val {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                },
            );
            name_val
        }
        TC_ARRAY => ois_read_array(ctx, addr),
        TC_OBJECT => ois_read_object(ctx, addr),
        _ => Value::Object(None),
    }
}

// `latest_user_defined_loader_class` used to live here. It moved to
// `crate::classloader` because this module is `#[cfg(any(feature =
// "experimental-serialization", feature = "synthetic-jdk"))]` and the real
// `jdk/internal/misc/VM.latestUserDefinedLoader0()` native — which every
// real-JDK `ObjectInputStream.readObject()` reaches via `resolveClass()` —
// must be registered in the DEFAULT build, where neither feature is on.
// See fixed-suite-bugs/h2/ for the H2 regression this caused.
pub use crate::classloader::latest_user_defined_loader_class;

/// Materialize a `TC_OBJECT` whose opening tag has already been consumed.
/// Reserves the wire handle **before** reading field values so that a
/// self-referential field decodes back to the same instance.
fn ois_read_object(ctx: &mut dyn NativeContext, addr: usize) -> Value {
    let desc = match read_class_descriptor(addr) {
        Some(d) => d,
        None => return Value::Object(None),
    };
    // SECURITY (JEP-290 deserialization filter bypass, CRITICAL): consult the
    // serial filter BEFORE initializing or instantiating the class. The
    // synthetic read-path does not go through the JDK `resolveClass` native
    // (the only place the filter used to be checked), so without this gate a
    // gadget class would be loaded and instantiated unfiltered. A reject
    // trips the sticky flag and aborts the read; `readObject` surfaces it as
    // `IOException("filter status: REJECTED: ...")`.
    if synthetic_read_class_rejected(ctx.vm_identity(), addr, &desc.class_name) {
        return Value::Object(None);
    }
    // Serialized-lambda record (written by `oos_write_value` for a synthetic
    // `$$Lambda` proxy): reconstruct an equivalent lambda proxy rather than
    // attempting to instantiate the sentinel class.
    if desc.class_name == SERIALIZED_LAMBDA_CLASS {
        return reconstruct_serialized_lambda(ctx, addr);
    }
    // Prefer resolving the stream's class name through whichever loader the
    // deserializing caller's own call stack implies (mirrors
    // `ObjectInputStream.resolveClass()`'s `latestUserDefinedLoader()`
    // fallback — see `latest_user_defined_loader_class` above). Only a
    // genuine user-defined loader (id >= 3; plain "Application", id 2, is
    // the same global default `ensure_class_initialized` already resolves
    // against) can pick out a *different* same-named class, so only bother
    // with the loader-aware lookup in that case, falling back to the
    // loader-oblivious path if the caller's loader never defined this class
    // name (e.g. a JDK class read from a stream written elsewhere).
    let mut loader_aware_class_id = None;
    if let Some(caller_class_id) = latest_user_defined_loader_class(ctx) {
        let loader_id = ctx.loader_id_of_class(caller_class_id);
        if loader_id >= 3 {
            loader_aware_class_id =
                ctx.class_id_by_name_and_loader(&desc.class_name, loader_id as u32);
        }
    }
    let resolved = match loader_aware_class_id {
        Some(cid) => Ok(cid),
        None => ctx.ensure_class_initialized(&desc.class_name),
    };
    let serialized_count = desc.field_types.len().max(2);
    let obj = if let Ok(class_id) = resolved {
        let class_field_count = ctx
            .declared_fields(class_id)
            .iter()
            .filter(|f| !f.is_static)
            .count()
            .max(serialized_count);
        ctx.alloc_object(class_id, class_field_count)
    } else {
        // `ois_read_value`/`ois_read_object`/`ois_read_array` are a mutually
        // recursive `-> Value` family with no error channel, and every other
        // unresolvable-descriptor path in this function answers
        // `Value::Object(None)`. Under `--real-jdk` the fabrication is refused;
        // answer the same null rather than widening the whole family here.
        match try_alloc_concurrent_synthetic(ctx, &desc.class_name, serialized_count) {
            Ok(o) => o,
            Err(_) => return Value::Object(None),
        }
    };

    // Cross-call GC-safety fix (2026-07-07, same shape + pattern as
    // nio_selector.rs's build_set / populate_selected_keys_field): `obj` is a
    // bare Rust local held across several `invoke_virtual`/`invoke_special`
    // dispatches below (readExternal, a custom readObject hook) and across
    // the default field-decode path (`ois_read_descriptor_fields`, whose own
    // recursive nested-object reads can themselves trigger a moving GC via
    // `ois_read_value`/`read_field_value`). Any of those can relocate `obj`,
    // so every use of it below -- including the final return -- must go
    // through the pin rather than the stale captured value. Reproduced under
    // CRATONVM_GC_STRESS amplification as "Stale pointer detected in
    // invokevirtual receiver (all-zero header)" attributed to
    // ObjectInputStream/nested field classes during Tomcat Tribes'
    // GroupChannel.messageReceived deserialization.
    let obj_pin = ctx.pin_native_root(obj);

    ois_push_handle(ctx, addr, Some(obj));

    // Zero-initialize all instance fields to their type defaults.
    if let Ok(class_id) = resolved {
        for f in ctx
            .declared_fields(class_id)
            .iter()
            .filter(|f| !f.is_static)
        {
            let default = match f.descriptor.as_str() {
                "J" => Value::Long(0),
                "F" => Value::Float(0.0),
                "D" => Value::Double(0.0),
                d if d.starts_with('L') || d.starts_with('[') => Value::Object(None),
                _ => Value::Int(0),
            };
            let cur = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(cur, f.slot_index, default);
        }
    }

    // Externalizable: invoke readExternal so the class consumes its own
    // self-describing data from the stream.
    if let Ok(class_id) = resolved {
        if class_is_externalizable(ctx, class_id) {
            if let Some(stream) = active_stream(ctx, addr) {
                cur_push(
                    addr,
                    CurFrame {
                        obj_pin,
                        obj: ctx.read_native_pin(obj_pin, obj),
                        class_id,
                        read_desc: None,
                    },
                );
                let cur = ctx.read_native_pin(obj_pin, obj);
                let _ = ctx.invoke_virtual(
                    cur,
                    "readExternal",
                    "(Ljava/io/ObjectInput;)V",
                    &[Value::Object(Some(stream))],
                );
                cur_pop(addr);
            }
            let result_obj = ctx.read_native_pin(obj_pin, obj);
            ctx.unpin_native_roots(obj_pin);
            return Value::Object(Some(result_obj));
        }
        // Custom readObject hook: push the curObj frame and dispatch so the
        // hook can call defaultReadObject() + the primitive readers.
        if let Some(_m) = find_private_method(
            ctx,
            class_id,
            "readObject",
            "(Ljava/io/ObjectInputStream;)V",
        ) {
            if let Some(stream) = active_stream(ctx, addr) {
                // The descriptor field metadata is stashed so a nested
                // defaultReadObject() can decode it; reuse the curObj frame.
                cur_push(
                    addr,
                    CurFrame {
                        obj_pin,
                        obj: ctx.read_native_pin(obj_pin, obj),
                        class_id,
                        read_desc: Some(desc.clone()),
                    },
                );
                let cur = ctx.read_native_pin(obj_pin, obj);
                let _ = ctx.invoke_special(
                    &desc.class_name,
                    "readObject",
                    "(Ljava/io/ObjectInputStream;)V",
                    &[Value::Object(Some(cur)), Value::Object(Some(stream))],
                );
                cur_pop(addr);
                let result_obj = ctx.read_native_pin(obj_pin, obj);
                ctx.unpin_native_roots(obj_pin);
                return Value::Object(Some(result_obj));
            }
        }
    }

    // Default path: decode the field block straight into the object.
    let cur = ctx.read_native_pin(obj_pin, obj);
    ois_read_descriptor_fields(ctx, addr, cur, &desc, resolved.ok());
    let result_obj = ctx.read_native_pin(obj_pin, obj);
    ctx.unpin_native_roots(obj_pin);
    Value::Object(Some(result_obj))
}

/// Read the field-data block described by `desc` from the stream and store
/// each value into the matching slot of `obj`. When `resolved` is `Some`,
/// fields are matched by name to the live class layout; otherwise they fall
/// back to positional slots on the synthetic object. Shared by the default
/// `ois_read_object` path and `defaultReadObject`.
fn ois_read_descriptor_fields(
    ctx: &mut dyn NativeContext,
    addr: usize,
    obj: ObjectRef,
    desc: &ClassDescriptor,
    resolved: Option<ClassId>,
) {
    // Cross-call GC-safety fix (2026-07-07, see ois_read_object's matching
    // comment): a reference-typed field (`tc` == 'L' or '[') recurses through
    // `read_field_value` -> `ois_read_value` into the full nested-object
    // reader -- allocations, constructors, and custom readObject/readExternal
    // hooks, any of which can trigger a moving GC. `obj` must be re-read via
    // the pin before every `set_field`, not used as the stale value captured
    // at entry.
    let obj_pin = ctx.pin_native_root(obj);
    if let Some(class_id) = resolved {
        let names_snapshot: Vec<(String, usize)> = ctx
            .declared_fields(class_id)
            .iter()
            .filter(|f| !f.is_static)
            .map(|f| (f.name.clone(), f.slot_index))
            .collect();
        for (i, tc) in desc.field_types.clone().iter().enumerate() {
            let val = read_field_value(ctx, addr, *tc);
            let cur = ctx.read_native_pin(obj_pin, obj);
            if let Some((_, slot)) = names_snapshot
                .iter()
                .find(|(n, _)| n == &desc.field_names[i])
            {
                ctx.set_field(cur, *slot, val);
            } else {
                ctx.set_field(cur, i, val);
            }
        }
    } else {
        for (i, tc) in desc.field_types.iter().enumerate() {
            let val = read_field_value(ctx, addr, *tc);
            let cur = ctx.read_native_pin(obj_pin, obj);
            ctx.set_field(cur, i, val);
        }
    }
    ctx.unpin_native_roots(obj_pin);
}

/// Materialize a `TC_ARRAY` whose opening tag has already been consumed.
/// The class descriptor carries the element type (`"[I"`, `"[Ljava/...;"`).
fn ois_read_array(ctx: &mut dyn NativeContext, addr: usize) -> Value {
    let desc = match read_class_descriptor(addr) {
        Some(d) => d,
        None => return Value::Object(None),
    };
    // SECURITY (JEP-290 deserialization filter bypass, CRITICAL): the array
    // class descriptor (e.g. `[Lcom.evil.Gadget;`) must clear the serial
    // filter before we allocate or populate the array — the JDK runs the
    // filter on the array class too. Checked here, before the length is even
    // read, because the synthetic path never reaches the JDK `resolveClass`
    // native where the filter used to be enforced.
    if synthetic_read_class_rejected(ctx.vm_identity(), addr, &desc.class_name) {
        return Value::Object(None);
    }
    let len_bytes = ois_buf_read(addr, 4);
    let length = i32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]).max(0)
        as usize;

    // SECURITY (deserialization DoS, default-reachable): a filter-independent
    // upper bound on the declared element count, applied *before* allocation.
    // The JEP-290 `maxarray` filter below is opt-in — with no filter installed
    // (the default) `filter_check_array` reports "unbounded", so a hostile
    // stream declaring up to ~2.1 billion elements could drive a multi-GB
    // allocation here. Mirror the JDK's defensive sizing: reject any length
    // that exceeds the hard ceiling, or that the stream cannot possibly back
    // with at least one byte per element (every wire element — even a `TC_NULL`
    // reference — consumes >= 1 byte), independent of any configured filter.
    if length > MAX_SERIAL_ARRAY_ELEMS || length > ois_buf_remaining(addr) {
        return Value::Object(None);
    }

    // JEP-290 maxarray: reject *before* allocating to avoid the
    // attacker-controlled length triggering a multi-GB allocation.
    if !filter_check_array(addr, length) {
        return Value::Object(None);
    }

    let elem_type = array_element_type_from_class(&desc.class_name);
    let mut arr = ctx.new_array(elem_type, length);
    ois_push_handle(ctx, addr, Some(arr));
    // gc-common w21-c: a reference element is a full nested read
    // (allocations, `readObject` hooks), so the array is re-read from a pin
    // after each one (the later elements were stored through its first
    // address).
    let arr_pin = ctx.pin_native_root(arr);

    let elem_char: char = match desc.class_name.as_bytes().get(1).copied() {
        Some(b) => b as char,
        None => 'L',
    };
    for i in 0..length {
        let v = match elem_char {
            'B' => {
                let b = ois_buf_read(addr, 1);
                Value::Int(b[0] as i8 as i32)
            }
            'Z' => {
                let b = ois_buf_read(addr, 1);
                Value::Int(if b[0] != 0 { 1 } else { 0 })
            }
            'C' => {
                let b = ois_buf_read(addr, 2);
                Value::Int(u16::from_be_bytes([b[0], b[1]]) as i32)
            }
            'S' => {
                let b = ois_buf_read(addr, 2);
                Value::Int(i16::from_be_bytes([b[0], b[1]]) as i32)
            }
            'I' => {
                let b = ois_buf_read(addr, 4);
                Value::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            }
            'J' => {
                let b = ois_buf_read(addr, 8);
                Value::Long(i64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            'F' => {
                let b = ois_buf_read(addr, 4);
                Value::Float(f32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            }
            'D' => {
                let b = ois_buf_read(addr, 8);
                Value::Double(f64::from_be_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            _ => {
                let v = ois_read_value(ctx, addr);
                arr = ctx.read_native_pin(arr_pin, arr);
                v
            }
        };
        ctx.set_array_element(arr, i, v);
    }
    ctx.unpin_native_roots(arr_pin);
    Value::Object(Some(arr))
}

fn array_element_type_from_class(name: &str) -> ArrayElementType {
    let b = name.as_bytes();
    if b.len() < 2 || b[0] != b'[' {
        return ArrayElementType::Reference;
    }
    match b[1] {
        b'B' => ArrayElementType::Byte,
        b'Z' => ArrayElementType::Boolean,
        b'C' => ArrayElementType::Char,
        b'S' => ArrayElementType::Short,
        b'I' => ArrayElementType::Int,
        b'J' => ArrayElementType::Long,
        b'F' => ArrayElementType::Float,
        b'D' => ArrayElementType::Double,
        _ => ArrayElementType::Reference,
    }
}

/// Inverse of [`array_element_type_from_class`]: the JVM field-descriptor
/// character for an array's element type. Reference elements use `L`, which
/// the array writer/reader route through the generic object value path.
fn array_element_descriptor_char(t: ArrayElementType) -> char {
    match t {
        ArrayElementType::Byte => 'B',
        ArrayElementType::Boolean => 'Z',
        ArrayElementType::Char => 'C',
        ArrayElementType::Short => 'S',
        ArrayElementType::Int => 'I',
        ArrayElementType::Long => 'J',
        ArrayElementType::Float => 'F',
        ArrayElementType::Double => 'D',
        ArrayElementType::Reference => 'L',
    }
}

fn register_object_input_stream(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectInputStream";

    // <init>(InputStream)V — if wrapping a ByteArrayInputStream, pre-load data
    r.register(cls, "<init>", "(Ljava/io/InputStream;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Every side table in this module WAS keyed by the stream object's raw
        // ADDRESS, and an address is recycled: the heap reuses one after the
        // previous `ObjectInputStream` there is collected, and a test process
        // that builds hundreds of VMs recycles whole arenas. So a fresh stream
        // could inherit a DEAD stream's state unless construction wiped it.
        // Since gc-common w21-c the tables are keyed by the stream's weak lock
        // key, which is never reused, so the wipe below is defensive only.
        //
        // The wire-handle table is the one that bites: `ois_handles` maps
        // handle → already-materialised object, so a stale table makes a
        // `TC_REFERENCE` resolve to an object from the previous stream. Seen
        // as `ClassCastException: cratonvm.SerializeBasic$Nested cannot be
        // cast to cratonvm.SerializeBasic` — `SerializeBasic.testNestedObject`
        // ran first, its `Nested` stayed behind under handle 0, and
        // `testSimpleRoundTrip` read it back. Isolated, both pass; only the
        // full corpus recycles the address.
        //
        // Clear on CONSTRUCTION rather than only on `close()`: a stream that
        // is never closed (both of those fixtures, and most real code that
        // wraps a `ByteArrayInputStream`) never reaches the close path at all.
        let addr = stream_key(ctx, this);
        // Release the roots of this VM's streams that died unclosed.
        serial_drain_retired_roots(ctx);
        ois_clear_handles(ctx, addr);
        ois_clear_filter_state(addr);
        ois_buf_load(addr, Vec::new());
        if let Some(Value::Object(Some(stream))) = args.get(1) {
            ctx.set_field(this, 0, Value::Object(Some(*stream)));
            // Bridge: load bytes from ByteArrayInputStream into OIS buffer
            let class_id = ctx.class_id_of_object(*stream);
            let class_name = ctx.class_name_of_id(class_id).unwrap_or_default();
            if class_name == "java/io/ByteArrayInputStream" {
                if let Value::Object(Some(arr)) = ctx.get_field(*stream, 0) {
                    let pos = ctx.get_field(*stream, 1).as_int().unwrap_or(0) as usize;
                    let count = ctx.get_field(*stream, 3).as_int().unwrap_or(0) as usize;
                    let mut bytes = Vec::with_capacity(count.saturating_sub(pos));
                    for i in pos..count {
                        let b = ctx.get_array_element(arr, i).as_int().unwrap_or(0);
                        bytes.push(b as u8);
                    }
                    ois_buf_load(addr, bytes);
                    // Validate stream header
                    if ois_buf_remaining(addr) >= 4 {
                        if !validate_stream_header(addr) {
                            return Err(RuntimeError::IOException {
                                message: "invalid stream header".into(),
                            }
                            .into());
                        }
                    }
                }
            }
        }
        ctx.set_field(this, 1, Value::Int(0)); // depth
        ctx.set_field(this, 2, Value::Int(0)); // objects_read
        ctx.set_field(this, 3, Value::Int(0)); // enable_resolve off
        ctx.set_field(this, 4, Value::Int(1)); // block_mode on
        ctx.set_field(this, 5, Value::Int(0)); // closed = false
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // readObject() -> Object
    //
    // Dispatches to the recursive `ois_read_value` helper which handles
    // `TC_NULL`, `TC_STRING`, `TC_LONGSTRING`, `TC_OBJECT` (recursive, with
    // wire-handle tracking for cycles), `TC_REFERENCE`, `TC_ARRAY`,
    // `TC_CLASS`, and `TC_ENUM`. Depth / reference-count ceilings are
    // enforced here rather than inside the helper so that we fail loudly
    // on adversarial streams before ever allocating.
    r.register(cls, "readObject", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);

        let count = match ctx.get_field(this, 2) {
            Value::Int(c) => c,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(count + 1));

        // Filter limits from ObjectInputStream.setObjectInputFilter(...):
        // enforced here via the shared handle_registry. Missing entry
        // means "no filter installed", so use permissive defaults.
        {
            let registry = handle_registry().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(state) = registry.get(&addr) {
                if count + 1 > state.max_references {
                    return Err(RuntimeError::IOException {
                        message: format!(
                            "Deserialization reference count {} exceeds maximum {}",
                            count + 1,
                            state.max_references
                        ),
                    }
                    .into());
                }
            }
        }

        // gc-common w21-c: registers the stream for the nested
        // `readExternal` / `readObject`-hook dispatch ([`active_stream`]).
        let entry = enter_stream(ctx, addr, this);
        let value = ois_read_value(ctx, addr);
        entry.exit(ctx);

        // JEP-290: surface a sticky filter rejection (depth/refs/bytes/
        // array overflow) as an `IOException` — `InvalidClassException`
        // extends `IOException` so the wire-up matches the JDK's
        // `ObjectInputFilter.Status.REJECTED` flow.
        if let Some(reason) = filter_is_rejected(addr) {
            return Err(RuntimeError::IOException {
                message: format!("filter status: REJECTED: {}", reason),
            }
            .into());
        }

        Ok(Some(value))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // readUnshared() -> Object
    r.register(cls, "readUnshared", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        let count = match ctx.get_field(this, 2) {
            Value::Int(c) => c,
            _ => 0,
        };
        ctx.set_field(this, 2, Value::Int(count + 1));
        let remaining = ois_buf_remaining(addr);
        if remaining == 0 {
            return Ok(Some(Value::Object(None)));
        }
        let tc = ois_buf_read(addr, 1);
        let value = match tc[0] {
            TC_NULL => Value::Object(None),
            TC_STRING => {
                let len_bytes = ois_buf_read(addr, 2);
                let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
                let str_bytes = ois_buf_read(addr, len);
                let s = String::from_utf8_lossy(&str_bytes).to_string();
                let obj = ctx.create_string(&s);
                Value::Object(Some(obj))
            }
            TC_OBJECT => {
                skip_class_desc(addr);
                let obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Object", 2)?;
                Value::Object(Some(obj))
            }
            _ => Value::Object(None),
        };
        // JEP-290: mirror `readObject` and surface a sticky maxbytes (or
        // other) rejection accumulated by `ois_buf_read` as an
        // `IOException("filter status: REJECTED")`.
        if let Some(reason) = filter_is_rejected(addr) {
            return Err(RuntimeError::IOException {
                message: format!("filter status: REJECTED: {}", reason),
            }
            .into());
        }
        Ok(Some(value))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // Primitive readers — read actual big-endian bytes from OIS buffer
    r.register(cls, "readInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 4);
        let val = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Int(val)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 8);
        let val = i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        Ok(Some(Value::Long(val)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 4);
        let val = f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Ok(Some(Value::Float(val)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 8);
        let val = f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]);
        Ok(Some(Value::Double(val)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readBoolean", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 1);
        let val = if bytes[0] != 0 { 1 } else { 0 };
        Ok(Some(Value::Int(val)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readByte", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 1);
        Ok(Some(Value::Int(bytes[0] as i8 as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 2);
        let val = i16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(val as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = ois_buf_read(stream_key(ctx, this), 2);
        let val = u16::from_be_bytes([bytes[0], bytes[1]]);
        Ok(Some(Value::Int(val as i32)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readUTF", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        let len_bytes = ois_buf_read(addr, 2);
        let len = u16::from_be_bytes([len_bytes[0], len_bytes[1]]) as usize;
        let str_bytes = ois_buf_read(addr, len);
        let s = String::from_utf8_lossy(&str_bytes).to_string();
        let obj = ctx.create_string(&s);
        Ok(Some(Value::Object(Some(obj))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // readFully([B)V — fill the destination byte[] from the stream buffer.
    r.register(cls, "readFully", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        if let Some(Value::Object(Some(dst))) = args.get(1) {
            let n = ctx.array_length(*dst);
            let bytes = ois_buf_read(addr, n);
            for (i, b) in bytes.iter().enumerate().take(n) {
                ctx.set_array_element(*dst, i, Value::Int(*b as i8 as i32));            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // defaultReadObject()V — called from within a custom readObject() hook.
    // Consumes the current object's default field block (recorded in the
    // curObj frame's descriptor) and stores it into the live object.
    r.register(cls, "defaultReadObject", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        if let Some(frame) = cur_top(addr) {
            // gc-common w21-c: the object through the frame's pin (the hook
            // that called us may have moved it; the frame's copy is raw).
            let obj = frame.current_obj(ctx);
            if let Some(desc) = frame.read_desc {
                let entry = enter_stream(ctx, addr, this);
                ois_read_descriptor_fields(ctx, addr, obj, &desc, Some(frame.class_id));
                entry.exit(ctx);
            }
        }
        // Outside a readObject hook the JDK throws NotActiveException; we
        // no-op for leniency.
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // readFields() -> GetField
    r.register(
        cls,
        "readFields",
        "()Ljava/io/ObjectInputStream$GetField;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/io/ObjectInputStream$GetField", 2)?;
            ctx.set_field(obj, 0, Value::Int(0));
            ctx.set_field(obj, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    r.register(cls, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 5, Value::Int(1));
        // Drop the wire-handle table so a reused stream address doesn't
        // observe dangling entries from a prior ObjectInputStream.
        let addr = stream_key(ctx, this);
        // Releases the roots the handle table held (gc-common w21-c).
        ois_clear_handles(ctx, addr);
        // Drop the per-stream JEP-290 filter (and release its root).
        clear_stream_filter(ctx, addr);
        // Drop JEP-290 per-stream counter state (depth/refs/bytes) too.
        ois_clear_filter_state(addr);
        // Drop this thread's curObj frames for the stream.
        cur_clear(addr);
        serial_drain_retired_roots(ctx);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(cls, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let remaining = ois_buf_remaining(stream_key(ctx, this)) as i32;
        Ok(Some(Value::Int(remaining)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "readStreamHeader", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = stream_key(ctx, this);
        if ois_buf_remaining(addr) >= 4 {
            if !validate_stream_header(addr) {
                return Err(RuntimeError::IOException {
                    message: "invalid stream header".into(),                }
                .into());            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(
        cls,
        "readClassDescriptor",
        "()Ljava/io/ObjectStreamClass;",
        |ctx, _args| {
            let desc = alloc_stream_class_stub(ctx, "java/lang/Object")?;
            Ok(Some(Value::Object(Some(desc))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // resolveClass(ObjectStreamClass) -> Class
    //
    // JEP-290 gate: must consult the per-stream filter first, then the
    // process-wide filter. A `REJECTED` decision MUST raise
    // `InvalidClassException("filter status: REJECTED")` — silently
    // returning null (the pre-fix behaviour) opened the door to
    // gadget-chain deserialization because the JDK fall-back path then
    // resolved the class via the system loader.
    r.register(cls, "resolveClass", "(Ljava/io/ObjectStreamClass;)Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = stream_key(ctx, this);
            let desc = match args.get(1) {
                Some(Value::Object(Some(d))) => *d,
                _ => return Ok(Some(Value::Object(None))),
            };
            let class_name = match osc_class_name(ctx, desc) {
                Some(n) => n,
                None => return Ok(Some(Value::Object(None))),
            };
            if evaluate_serial_filters(ctx.vm_identity(), addr, &class_name)
                == FilterStatus::Rejected
            {
                return Err(RuntimeError::IOException {
                    message: format!(
                        "InvalidClassException: filter status: REJECTED: class \"{}\" rejected by ObjectInputFilter",
                        class_name
                    ),
                }
                .into());
            }
            // Allowed / Undecided fall through to the JDK's normal
            // resolution path. We return null so the caller (`readObject`'s
            // resolveClass call site in the JDK) uses its default loader
            // logic; the security gate has already cleared.
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // resolveProxyClass([Ljava/lang/String;) -> Class
    //
    // The JDK contract here is symmetrical: each proxy interface name
    // must clear the filter before the proxy class is materialised.
    //
    // spring-bug-08: register as a `Bridge`, NOT the default `SyntheticStub`.
    // In no-synthetic-stubs / real-JDK mode the registry DROPS every
    // `SyntheticStub` serialization native (so the real OIS bytecode runs) —
    // which is correct for `readObject`/`resolveClass`, but `resolveProxyClass`
    // MUST override the real body (whose default routes the unsupported
    // `Proxy.getProxyClass` dynamic-module path). A `Bridge` survives the drop
    // and is then force-dispatched via `force_native_over_real_jdk_bytecode`.
            r.register(cls, "resolveProxyClass", "([Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = stream_key(ctx, this);
            let arr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr);
            let mut names: Vec<String> = Vec::with_capacity(len);
            for i in 0..len {
                let iface = match ctx.get_array_element(arr, i) {
                    Value::Object(Some(s)) => s,
                    _ => continue,
                };
                if let Some(name) = ctx.read_string(iface) {
                    if evaluate_serial_filters(ctx.vm_identity(), addr, &name)
                        == FilterStatus::Rejected
                    {
                        return Err(RuntimeError::IOException {
                            message: format!(
                                "InvalidClassException: filter status: REJECTED: proxy interface \"{}\" rejected by ObjectInputFilter",
                                name
                            ),
                        }
                        .into());
                    }
                    names.push(name);
                }
            }
            // spring-bug-08: resolve to a CratonVM generated `$ProxyN` class
            // ourselves rather than returning null (which lets the JDK default
            // `resolveProxyClass` run `Proxy.getProxyClass` → the dynamic-module
            // path that the synthetic proxy model can't drive, surfacing as
            // `ClassNotFoundException: null`). The returned class extends the
            // serializable `Proxy$Instance` super, so the proxy's `h` handler
            // field deserializes back into it and the proxy round-trips.
            if let Some(cid) = crate::resolve_serialized_proxy_class(ctx, &names) {
                let mirror = ctx.get_class_mirror(cid);
                return Ok(Some(Value::Object(Some(mirror))));            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    
    // setObjectInputFilter(ObjectInputFilter)V — per-stream filter slot.
    // Must be set before the first object is read (we do NOT enforce that
    // here — the JDK does — but we honour subsequent installs).
    r.register(
        cls,
        "setObjectInputFilter",
        "(Ljava/io/ObjectInputFilter;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = stream_key(ctx, this);
            let filter_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    // null filter => clear.
                    clear_stream_filter(ctx, addr);
                    return Ok(None);
                }
            };
            let status = ctx.get_field(filter_obj, 0).as_int().unwrap_or(0);
            let parsed = match status {
                s if s == FilterStatus::Rejected as i32 => SerialFilter::parse("!*"),
                s if s == FilterStatus::Allowed as i32 => SerialFilter::parse("*"),
                _ => SerialFilter {
                    entries: Vec::new(),
                    pattern: String::new(),
                },
            };
            ois_stream_filters()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(addr, parsed);
            // gc-common w21-c: held as a global root (the stream's
            // `serialFilter` field holds it strongly in the JDK); it was a
            // raw `ObjectRef` nothing kept alive or current.
            let vm = ctx.vm_identity();
            let held = SerialRoot::hold(ctx, filter_obj);
            let replaced = ois_stream_filter_objs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(addr, (vm, held));
            if let Some((owner, old)) = replaced {
                release_serial_roots(ctx, owner, vec![old.root]);
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    r.register(
        cls,
        "getObjectInputFilter",
        "()Ljava/io/ObjectInputFilter;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = stream_key(ctx, this);
            let held = ois_stream_filter_objs()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&addr)
                .map(|(_, held)| *held);
            let obj = match held {
                Some(h) => h.current(ctx),
                None => None,
            };
            Ok(Some(Value::Object(obj)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // KEEP — this is the real body, re-verified against JDK 25 source
    // (`java.base/java/io/ObjectInputStream.java`, `readObjectOverride`):
    //
    //     protected Object readObjectOverride()
    //         throws IOException, ClassNotFoundException
    //     {
    //         return null;
    //     }
    //
    // A hook that exists only to be overridden by a subclass built with the
    // protected no-arg constructor, and `readObject()` only routes to it when
    // `enableOverride` is set — i.e. exactly for such a subclass, which
    // resolves to its own declaring class and never reaches this native.
    r.register(
        cls,
        "readObjectOverride",
        "()Ljava/lang/Object;",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// ObjectStreamClass  (8-field synthetic — WP0.2)
//   0: class_name_ref (String),     — "getName"
//   1: serial_version_uid (Long),   — "getSerialVersionUID"
//   2: field_count (Int),
//   3: flags (Int; SC_* bits),
//   4: has_write_object (Int; 0/1),
//   5: has_read_object (Int; 0/1),
//   6: fields_array (ObjectStreamField[]),   [added WP0.2]
//   7: class_mirror (Class),                  [added WP0.2]
//
// The legacy slots 0..5 match the pre-WP0.2 layout so any call site that
// read via raw slot indices continues to work; the two new slots (6,7)
// are visible only via the new `getFields0` / `forClass0` getters.
//
// In addition — and this is the load-bearing change for WP0.2 — every
// descriptor returned by `lookup` / `lookupAny` has the following real
// JDK-named fields populated via `set_field_by_name`:
//
//   name                      String
//   cl                        Class       (aka "forClass" target)
//   suid                      Long        (boxed)
//   serializable              boolean
//   externalizable            boolean
//   isEnum                    boolean
//   hasWriteObjectData        boolean
//   hasBlockExternalData      boolean
//   fields                    ObjectStreamField[]   (declaration order)
//   cons                      Constructor           (= serializableConstructor)
//   serializableConstructor   Constructor
//   writeObjectMethod         Method  (or null)
//   readObjectMethod          Method  (or null)
//   readObjectNoDataMethod    Method  (or null)
//   readResolveMethod         Method  (or null)
//   writeReplaceMethod        Method  (or null)
//
// Writes to unknown named fields are silently no-ops (see
// `NativeContextImpl::set_field_by_name`), so when the real JDK
// `ObjectStreamClass` class has only a subset of these names the extras
// are ignored. Synthetic-jdk mode ignores all of them and uses the
// indexed slots above.
// ---------------------------------------------------------------------------

/// Bit 0 of OSC-internal flags field: target class implements Serializable.
const OSC_FLAG_SERIALIZABLE: i32 = 0x01;
/// Bit 1: target class implements Externalizable.
const OSC_FLAG_EXTERNALIZABLE: i32 = 0x02;
/// Bit 2: target class is an enum.
const OSC_FLAG_ENUM: i32 = 0x04;

/// Access flags we care about for field / method filtering.
/// (Kept local rather than importing from reader so serialization.rs
/// stays self-contained.)
const ACC_PUBLIC: u16 = 0x0001;
const ACC_STATIC: u16 = 0x0008;
const ACC_PRIVATE: u16 = 0x0002;
const ACC_ENUM: u16 = 0x4000;

/// Find a no-arg constructor in `class_id` with the given access-flag
/// mask (`ACC_PRIVATE` accepted — callers are usually looking for
/// something specific). Returns the matching `MethodMetadata` or None.
fn find_no_arg_constructor(ctx: &dyn NativeContext, class_id: ClassId) -> Option<MethodMetadata> {
    for m in ctx.declared_methods(class_id) {
        if m.name == "<init>" && m.descriptor == "()V" {
            return Some(m);
        }
    }
    None
}

/// Walk the super-class chain from `class_id` upward looking for the
/// closest ancestor that is **not** Serializable. That class's no-arg
/// constructor is the "serializableConstructor" per JLS 17 §1.10 (and
/// real-JDK `ObjectStreamClass#getSerializableConstructor`).
///
/// Returns `None` when:
///  * The class has no non-Serializable ancestor (e.g. `Object` isn't
///    loaded, which shouldn't happen in practice).
///  * The ancestor's no-arg constructor doesn't exist (in the real JDK
///    this surfaces later as `InvalidClassException: no valid
///    constructor`).
fn find_serializable_constructor(
    ctx: &dyn NativeContext,
    class_id: ClassId,
) -> Option<(ClassId, MethodMetadata)> {
    let serializable_id = ctx.class_id_by_name("java/io/Serializable")?;
    // Walk up from `class_id.superclass` to the first ancestor NOT
    // subtyping Serializable. Its no-arg ctor is the one we want.
    let mut cur = ctx.superclass_of(class_id);
    while let Some(cid) = cur {
        if !ctx.is_subclass(cid, serializable_id) {
            if let Some(ctor) = find_no_arg_constructor(ctx, cid) {
                return Some((cid, ctor));
            }
            // Found a non-Serializable ancestor but it has no no-arg
            // ctor — per spec this class isn't really deserializable,
            // but real JDK still populates the field with the Object
            // default ctor if available. Fall back to keep walking —
            // the closest non-Serializable ancestor is what we want,
            // but if its ctor is missing we punt on the whole field.
            return None;
        }
        cur = ctx.superclass_of(cid);
    }
    None
}

/// Find a private instance method with exactly the given name+desc.
/// Returns the matching `MethodMetadata`.  Serialization hook methods
/// (`writeObject`, `readObject`, `readObjectNoData`) are spec-required
/// to be `private`; this matches real-JDK `ObjectStreamClass#
/// getPrivateMethod`.
fn find_private_method(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<MethodMetadata> {
    for m in ctx.declared_methods(class_id) {
        if m.name == name
            && m.descriptor == descriptor
            && (m.access_flags & ACC_PRIVATE) != 0
            && (m.access_flags & ACC_STATIC) == 0
        {
            return Some(m);
        }
    }
    None
}

/// Find an "inheritable" instance method for the serialization hooks
/// `writeReplace` / `readResolve`. Real-JDK semantics: the method can
/// be declared anywhere in the class hierarchy with any accessibility
/// — walking up until we find one. Non-static only.
fn find_inheritable_method(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    name: &str,
    descriptor: &str,
) -> Option<MethodMetadata> {
    let mut cur = Some(class_id);
    while let Some(cid) = cur {
        for m in ctx.declared_methods(cid) {
            if m.name == name && m.descriptor == descriptor && (m.access_flags & ACC_STATIC) == 0 {
                return Some(m);
            }
        }
        cur = ctx.superclass_of(cid);
    }
    None
}

/// Build an `ObjectStreamField` representing one declared field.
/// Real-JDK layout (synthetic 4-slot matching pre-WP0.2):
///   0: name (String)
///   1: type code (char)
///   2: type string (String)
///   3: offset (int)
///
/// Stale-`ObjectRef` fix (see `build_object_stream_class`'s doc comment for
/// the full family writeup): `osf` is allocated first and then reused after
/// `ctx.create_string(field_descriptor)` — a second allocation that can
/// trigger a moving collection and relocate it. `name` is likewise reused
/// (in the trailing `set_field_by_name`) after that same second allocation.
/// Both are pinned across it and re-read afterward, exactly like
/// `tm_binary_search`'s `owner`/`data`/`key`.
fn build_object_stream_field(
    ctx: &mut dyn NativeContext,
    field_name: &str,
    field_descriptor: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let osf = try_alloc_concurrent_synthetic(ctx, "java/io/ObjectStreamField", 4)?;
    let osf_pin = ctx.pin_native_root(osf);
    let mut osf = osf;
    let name = ctx.create_string(field_name);
    osf = ctx.read_native_pin(osf_pin, osf);
    let name_pin = ctx.pin_native_root(name);
    let mut name = name;
    ctx.set_field(osf, 0, Value::Object(Some(name)));
    let tc: char = match field_descriptor.chars().next() {
        Some(c @ ('B' | 'C' | 'D' | 'F' | 'I' | 'J' | 'S' | 'Z')) => c,
        Some('L' | '[') => 'L',
        _ => 'I',
    };
    ctx.set_field(osf, 1, Value::Int(tc as i32));
    let ts = ctx.create_string(field_descriptor);
    osf = ctx.read_native_pin(osf_pin, osf);
    name = ctx.read_native_pin(name_pin, name);
    ctx.set_field(osf, 2, Value::Object(Some(ts)));
    ctx.set_field(osf, 3, Value::Int(0));
    // Also populate the JDK-named slots if the class has them.
    ctx.set_field_by_name(osf, "name", Value::Object(Some(name)));
    ctx.set_field_by_name(osf, "type", Value::Object(None));
    ctx.set_field_by_name(osf, "signature", Value::Object(Some(ts)));
    ctx.set_field_by_name(osf, "offset", Value::Int(0));
    ctx.unpin_native_roots(osf_pin);
    Ok(osf)
}

/// Build a fully-populated `ObjectStreamClass` for `class_id`. This is
/// the heart of WP0.2 — called by both `lookup` and `lookupAny`.
///
/// `treat_nonserial_as_serial` mirrors the real-JDK split:
///   * `lookup(cls)`     — returns null for non-Serializable classes.
///   * `lookupAny(cls)`  — returns a descriptor anyway, with `fields`
///     empty and the Serializable-related flags cleared.
///
/// # Stale-`ObjectRef` fix
///
/// This function captures `desc`, then `name_obj`, then `class_mirror`, then
/// `osf_arr` — and, after each, keeps allocating: the per-field
/// `ObjectStreamField`s (`build_object_stream_field`, itself with the same
/// hazard — see its doc comment), the constructor mirror, and up to five
/// method mirrors. Every one of those allocations is a potential GC
/// safepoint, and a moving collection triggered by a *later* allocation can
/// relocate an *earlier*-captured `ObjectRef`. The original code captured
/// `desc`/`name_obj`/`class_mirror`/`osf_arr` once and reused the same Rust
/// locals throughout — exactly the shape already fixed in
/// `tm_binary_search`/`ts_binary_search`, `pbq_offer_locked`/
/// `pbq_ensure_capacity` (see
/// `docs/feature-designs/native-dispatch-architecture.md
/// bug-h2-priorityblockingqueue-stale-objectref-classcastexception-FIXED.md`)
/// and `ChmSegmentResizeGuard::acquire_gc_safe`. A stale write here does not
/// crash — it silently lands on whatever now occupies the relocated address,
/// which a later real-bytecode reader of that address can observe as a
/// `ClassCastException` between two unrelated types (the H2 PBQ bug's
/// symptom exactly, and this project's own `docs/feature-designs/native-dispatch-architecture.md
/// junit-launcher-init-classcastexception-arrayspliterator-rare-flake-RETIRED-20260921.md`
/// coincidentally). **This function is NOT that page's cause** — it is
/// gated `#[cfg(any(feature = "experimental-serialization", feature =
/// "synthetic-jdk"))]`, neither of which is in the default `--real-jdk`/
/// `--fast-boot` build the page's flake was seen on; under those modes
/// `ObjectStreamClass.lookup` runs as real JDK bytecode and never reaches
/// this native at all (confirmed while investigating that page — see its
/// retirement banner). Fixed anyway because it is a real bug in the modes
/// this function DOES run in.
///
/// Each of the four long-lived refs is pinned the moment it is captured and
/// re-read after every subsequent allocating call (`ctx.pin_native_root` /
/// `ctx.read_native_pin`, released together by one `ctx.unpin_native_roots`
/// at the end — pins are a stack, so unpinning the first handle drops all
/// four). Values that are created and consumed with no allocation in
/// between (the per-descriptor method/constructor mirrors below) need no
/// pin of their own; only the four survivors that cross later allocations
/// do.
fn build_object_stream_class(
    ctx: &mut dyn NativeContext,
    class_id: ClassId,
    include_non_serializable: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    // Fast-path: cached descriptor already exists for this class.
    if let Some(cached) = ctx.osc_cache_get(class_id) {
        return Ok(Some(cached));
    }

    let Some(class_name) = ctx.class_name_of_id(class_id) else {
        return Ok(None);
    };
    let serializable_id = ctx.class_id_by_name("java/io/Serializable");
    let externalizable_id = ctx.class_id_by_name("java/io/Externalizable");
    let is_serializable = serializable_id
        .map(|sid| ctx.is_subclass(class_id, sid) || class_id == sid)
        .unwrap_or(false);
    let is_externalizable = externalizable_id
        .map(|eid| ctx.is_subclass(class_id, eid))
        .unwrap_or(false);
    let is_enum = (ctx.class_access_flags(class_id) & ACC_ENUM) != 0;

    if !is_serializable && !include_non_serializable {
        // lookup(cls) returns null for non-Serializable classes.
        return Ok(None);
    }

    // ----- Allocate the descriptor object -----
    // We request 8 slots minimum (the WP0.2 layout above). In real-JDK
    // mode `alloc_concurrent_synthetic` bumps this to the real-JDK
    // field count if larger, so named-field writes below still hit the
    // right slots either way.
    let desc = try_alloc_concurrent_synthetic(ctx, "java/io/ObjectStreamClass", 8)?;
    let desc_pin = ctx.pin_native_root(desc);
    let mut desc = desc;

    // ----- Legacy indexed-slot population (preserved for back-compat) -----
    let name_obj = ctx.create_string(&class_name);
    desc = ctx.read_native_pin(desc_pin, desc);
    let name_pin = ctx.pin_native_root(name_obj);
    let mut name_obj = name_obj;

    // `getSerialVersionUID()`: the declared value, else the JDK's computed
    // default (interpreter round i4 wave 96, lane L2).
    let svuid = stream_class_svuid_of_id(ctx, class_id);
    let class_mirror = ctx.get_class_mirror(class_id);
    desc = ctx.read_native_pin(desc_pin, desc);
    name_obj = ctx.read_native_pin(name_pin, name_obj);
    let class_mirror_pin = ctx.pin_native_root(class_mirror);
    let mut class_mirror = class_mirror;

    // Collect declared instance fields in declaration order.
    // Note: we keep the original `declared_fields` order — real JDK
    // sorts primitives before objects, but the roadmap explicitly
    // requires *declaration* order (and `serialPersistentFields`
    // overrides anyway for real-JDK sorted semantics; out of scope
    // for WP0.2). Static and transient fields are excluded.
    let owned_osf_fields: Vec<(String, String)> = if is_serializable {
        ctx.declared_fields(class_id)
            .into_iter()
            .filter(|f| !f.is_static && (f.access_flags & ACC_TRANSIENT) == 0)
            .map(|f| (f.name, f.descriptor))
            .collect()
    } else {
        Vec::new()
    };
    let field_count = owned_osf_fields.len() as i32;

    let mut flags = 0;
    if is_serializable {
        flags |= OSC_FLAG_SERIALIZABLE;
    }
    if is_externalizable {
        flags |= OSC_FLAG_EXTERNALIZABLE;
    }
    if is_enum {
        flags |= OSC_FLAG_ENUM;
    }

    let has_write_object = if is_serializable {
        find_private_method(
            ctx,
            class_id,
            "writeObject",
            "(Ljava/io/ObjectOutputStream;)V",
        )
        .is_some() as i32
    } else {
        0
    };
    let has_read_object = if is_serializable {
        find_private_method(
            ctx,
            class_id,
            "readObject",
            "(Ljava/io/ObjectInputStream;)V",
        )
        .is_some() as i32
    } else {
        0
    };

    ctx.set_field(desc, 0, Value::Object(Some(name_obj)));
    ctx.set_field(desc, 1, Value::Long(svuid));
    ctx.set_field(desc, 2, Value::Int(field_count));
    ctx.set_field(desc, 3, Value::Int(flags));
    ctx.set_field(desc, 4, Value::Int(has_write_object));
    ctx.set_field(desc, 5, Value::Int(has_read_object));

    // ----- Slot 6: fields[] (declaration order) -----
    //
    // Allocate the ObjectStreamField[] up-front so the set_field into
    // slot 6 lands before the loop populates the array. An empty array
    // (length 0) is fine for non-Serializable / lookupAny lookups.
    //
    // `ObjectStreamField[]`, the declared type (i8-L2), not `Object[]`. The
    // component lookup may load; `desc`, `name_obj` and `class_mirror` are
    // pinned and re-read below.
    let osf_component = crate::lang_class::loading_component_id(ctx, "java/io/ObjectStreamField");
    let osf_arr = ctx.new_ref_array(osf_component, owned_osf_fields.len());
    desc = ctx.read_native_pin(desc_pin, desc);
    name_obj = ctx.read_native_pin(name_pin, name_obj);
    class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
    let osf_arr_pin = ctx.pin_native_root(osf_arr);
    let mut osf_arr = osf_arr;
    for (i, (fname, fdesc)) in owned_osf_fields.iter().enumerate() {
        let osf = match build_object_stream_field(ctx, fname, fdesc) {
            Ok(osf) => osf,
            Err(e) => {
                ctx.unpin_native_roots(desc_pin);
                return Err(e);
            }
        };
        desc = ctx.read_native_pin(desc_pin, desc);
        name_obj = ctx.read_native_pin(name_pin, name_obj);
        class_mirror = ctx.read_native_pin(class_mirror_pin, class_mirror);
        osf_arr = ctx.read_native_pin(osf_arr_pin, osf_arr);
        ctx.set_array_element(osf_arr, i, Value::Object(Some(osf)));
    }
    ctx.set_field(desc, 6, Value::Object(Some(osf_arr)));

    // ----- Slot 7: class mirror -----
    ctx.set_field(desc, 7, Value::Object(Some(class_mirror)));

    // ----- JDK-named slot population -----
    //
    // `set_field_by_name` is a no-op when the class doesn't have a
    // field by that name, so populating all of the real-JDK field
    // names is always safe — synthetic-jdk mode simply ignores them.
    ctx.set_field_by_name(desc, "name", Value::Object(Some(name_obj)));
    ctx.set_field_by_name(desc, "cl", Value::Object(Some(class_mirror)));
    // In real JDK, `suid` is a boxed `Long` not a primitive long. For
    // synthetic-jdk the field doesn't exist so the write is a no-op;
    // for real-JDK we still populate the primitive form because the
    // bytecode reader handles both paths via the descriptor-aware
    // `get_field_as`.
    ctx.set_field_by_name(desc, "suid", Value::Long(svuid));
    ctx.set_field_by_name(desc, "serializable", Value::Int(is_serializable as i32));
    ctx.set_field_by_name(desc, "externalizable", Value::Int(is_externalizable as i32));
    ctx.set_field_by_name(desc, "isEnum", Value::Int(is_enum as i32));
    ctx.set_field_by_name(desc, "hasWriteObjectData", Value::Int(has_write_object));
    ctx.set_field_by_name(
        desc,
        "hasBlockExternalData",
        Value::Int(is_externalizable as i32),
    );
    ctx.set_field_by_name(desc, "fields", Value::Object(Some(osf_arr)));

    // ----- Reflective method / constructor hooks -----
    //
    // `serializableConstructor` / `cons` is the load-bearing field —
    // JUnit4 `Result.<clinit>` calls this with `getDeclaredConstructor`
    // and throws NPE if it's null. We always populate it (via the
    // non-Serializable ancestor's no-arg ctor, matching real JDK
    // behavior) whenever we can find one.
    if let Some((anc_id, ctor_meta)) = find_serializable_constructor(ctx, class_id) {
        let _ = anc_id;
        let ctor_obj = match create_constructor_object(ctx, &ctor_meta) {
            Ok(o) => o,
            Err(e) => {
                ctx.unpin_native_roots(desc_pin);
                return Err(e);
            }
        };
        desc = ctx.read_native_pin(desc_pin, desc);
        ctx.set_field_by_name(desc, "cons", Value::Object(Some(ctor_obj)));
        ctx.set_field_by_name(
            desc,
            "serializableConstructor",
            Value::Object(Some(ctor_obj)),
        );
        ctx.set_field_by_name(
            desc,
            "deserializationConstructor",
            Value::Object(Some(ctor_obj)),
        );
    } else {
        // Leave as Value::Object(None) — callers already handle null
        // hook (the field defaults to 0/null on allocation). Be
        // explicit so the descriptor's `null` state is unambiguous.
        ctx.set_field_by_name(desc, "cons", Value::Object(None));
        ctx.set_field_by_name(desc, "serializableConstructor", Value::Object(None));
        ctx.set_field_by_name(desc, "deserializationConstructor", Value::Object(None));
    }

    // writeObjectMethod / readObjectMethod / readObjectNoDataMethod —
    // these are all spec-required to be `private`; `findPrivateMethod`
    // matches real-JDK `ObjectStreamClass#getPrivateMethod`. Each mirror is
    // built and immediately stored (no allocation intervenes between the
    // two), so `desc` alone needs a refresh — the mirror itself is fresh.
    let wo = match find_private_method(
        ctx,
        class_id,
        "writeObject",
        "(Ljava/io/ObjectOutputStream;)V",
    )
    .map(|m| create_method_object(ctx, &m))
    .transpose()
    {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(desc_pin);
            return Err(e);
        }
    };
    desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field_by_name(desc, "writeObjectMethod", Value::Object(wo));

    let ro = match find_private_method(
        ctx,
        class_id,
        "readObject",
        "(Ljava/io/ObjectInputStream;)V",
    )
    .map(|m| create_method_object(ctx, &m))
    .transpose()
    {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(desc_pin);
            return Err(e);
        }
    };
    desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field_by_name(desc, "readObjectMethod", Value::Object(ro));

    let rond = match find_private_method(ctx, class_id, "readObjectNoData", "()V")
        .map(|m| create_method_object(ctx, &m))
        .transpose()
    {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(desc_pin);
            return Err(e);
        }
    };
    desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field_by_name(desc, "readObjectNoDataMethod", Value::Object(rond));

    // writeReplaceMethod / readResolveMethod — inheritable, any
    // accessibility. `findInheritableMethod` walks up the hierarchy.
    let wr = match find_inheritable_method(ctx, class_id, "writeReplace", "()Ljava/lang/Object;")
        .map(|m| create_method_object(ctx, &m))
        .transpose()
    {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(desc_pin);
            return Err(e);
        }
    };
    desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field_by_name(desc, "writeReplaceMethod", Value::Object(wr));

    let rr = match find_inheritable_method(ctx, class_id, "readResolve", "()Ljava/lang/Object;")
        .map(|m| create_method_object(ctx, &m))
        .transpose()
    {
        Ok(v) => v,
        Err(e) => {
            ctx.unpin_native_roots(desc_pin);
            return Err(e);
        }
    };
    desc = ctx.read_native_pin(desc_pin, desc);
    ctx.set_field_by_name(desc, "readResolveMethod", Value::Object(rr));

    // Install in the cache so the next call returns this same ref.
    let cached = ctx.osc_cache_put(class_id, desc);
    ctx.unpin_native_roots(desc_pin);
    Ok(Some(cached))
}

/// Helper: resolve `Class` mirror arg to its `ClassId`. Falls back to
/// the contextual class-id-of-object if the mirror isn't in the
/// reverse map (this shouldn't happen for well-formed mirrors but the
/// fallback keeps this defensively correct under synthetic-jdk).
fn class_id_of_mirror(ctx: &dyn NativeContext, mirror: ObjectRef) -> Option<ClassId> {
    if let Some(id) = ctx.class_id_from_mirror(mirror) {
        return Some(id);
    }
    // Synthetic-jdk fallback: Class mirror has the target class name in
    // a field. Read field "name" and look up by name.
    let name_val = ctx.get_field_by_name(mirror, "name");
    if let Value::Object(Some(s)) = name_val {
        if let Some(n) = ctx.read_string(s) {
            return ctx.class_id_by_name(&n.replace('.', "/"));
        }
    }
    None
}

/// True iff the class identified by the mirror in `args[0]` declares a
/// `<clinit>` static initializer. Used by `hasStaticInitializer` to feed the
/// real-JDK `computeDefaultSUID` digest. Fails safe to `false` if the mirror or
/// class can't be resolved (matching the conservative legacy behavior, but only
/// when we genuinely can't tell).
///
/// Initializes the class first, as HotSpot's JNI `GetStaticMethodID` does
/// (interpreter round i4 wave 86, lane L2; the essential twin in `lib.rs`,
/// `essential_class_has_static_initializer`, says why).
pub fn class_has_static_initializer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<bool, MethodCallFailed> {
    let mirror = match obj_arg(args, 0) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    let class_id = match class_id_of_mirror(ctx, mirror) {
        Some(id) => id,
        None => return Ok(false),
    };
    ctx.initialize_class(class_id)?;
    Ok(ctx
        .declared_methods(class_id)
        .iter()
        .any(|m| &*m.name == "<clinit>"))
}

const MH_KIND_SPECIAL_FOR_SERIALIZATION_HOOK: i32 = 2;

fn native_sun_reflection_factory_get(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "sun/reflect/ReflectionFactory", 1)?;
    Ok(Some(Value::Object(Some(obj))))
}

fn native_jdk_reflection_factory_get(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let obj = try_alloc_concurrent_synthetic(ctx, "jdk/internal/reflect/ReflectionFactory", 1)?;
    Ok(Some(Value::Object(Some(obj))))
}

fn reflection_factory_debug_enabled() -> bool {
    crate::nbflags().dbg_reflection_factory
}

fn reflection_factory_class_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(ClassId, ObjectRef)> {
    for idx in [1usize, 0] {
        if let Some(Value::Object(Some(mirror))) = args.get(idx) {
            if let Some(class_id) = class_id_of_mirror(ctx, *mirror) {
                return Some((class_id, *mirror));
            }
        }
    }
    None
}

fn reflection_factory_hook_handle(
    ctx: &mut dyn NativeContext,
    requested_class_id: ClassId,
    method: MethodMetadata,
) -> MethodCallResult {
    let class_name = ctx
        .class_name_of_id(method.declaring_class_id)
        .or_else(|| ctx.class_name_of_id(requested_class_id))
        .unwrap_or_default();
    if class_name.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let mh = alloc_method_handle(
        ctx,
        &class_name,
        &method.name,
        &method.descriptor,
        MH_KIND_SPECIAL_FOR_SERIALIZATION_HOOK,
    )?;
    Ok(Some(Value::Object(Some(mh))))
}

fn reflection_factory_private_hook(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let Some((class_id, _)) = reflection_factory_class_arg(ctx, args) else {
        return Ok(Some(Value::Object(None)));
    };
    let target_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let method = find_private_method(ctx, class_id, name, descriptor);
    if reflection_factory_debug_enabled() {
        match &method {
            Some(m) => eprintln!(
                "[rf-ser] private {}{} target={} cid={} -> handle decl_cid={} decl={} meta_desc={}",
                name,
                descriptor,
                target_name,
                class_id.as_u32(),
                m.declaring_class_id.as_u32(),
                ctx.class_name_of_id(m.declaring_class_id)
                    .unwrap_or_default(),
                m.descriptor
            ),
            None => eprintln!(
                "[rf-ser] private {}{} target={} cid={} -> null",
                name,
                descriptor,
                target_name,
                class_id.as_u32()
            ),
        }
    }
    match method {
        Some(method) => reflection_factory_hook_handle(ctx, class_id, method),
        None => Ok(Some(Value::Object(None))),
    }
}

fn reflection_factory_inheritable_hook(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    name: &str,
    descriptor: &str,
) -> MethodCallResult {
    let Some((class_id, _)) = reflection_factory_class_arg(ctx, args) else {
        return Ok(Some(Value::Object(None)));
    };
    let target_name = ctx.class_name_of_id(class_id).unwrap_or_default();
    let method = find_inheritable_method(ctx, class_id, name, descriptor);
    if reflection_factory_debug_enabled() {
        match &method {
            Some(m) => eprintln!(
                "[rf-ser] inheritable {}{} target={} cid={} -> handle decl_cid={} decl={} meta_desc={}",
                name,
                descriptor,
                target_name,
                class_id.as_u32(),
                m.declaring_class_id.as_u32(),
                ctx.class_name_of_id(m.declaring_class_id).unwrap_or_default(),
                m.descriptor
            ),
            None => eprintln!(
                "[rf-ser] inheritable {}{} target={} cid={} -> null",
                name,
                descriptor,
                target_name,
                class_id.as_u32()
            ),
        }
    }
    match method {
        Some(method) => reflection_factory_hook_handle(ctx, class_id, method),
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_reflection_factory_read_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reflection_factory_private_hook(ctx, args, "readObject", "(Ljava/io/ObjectInputStream;)V")
}

fn native_reflection_factory_read_object_no_data(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reflection_factory_private_hook(ctx, args, "readObjectNoData", "()V")
}

fn native_reflection_factory_write_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reflection_factory_private_hook(ctx, args, "writeObject", "(Ljava/io/ObjectOutputStream;)V")
}

fn native_reflection_factory_read_resolve(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reflection_factory_inheritable_hook(ctx, args, "readResolve", "()Ljava/lang/Object;")
}

fn native_reflection_factory_write_replace(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    reflection_factory_inheritable_hook(ctx, args, "writeReplace", "()Ljava/lang/Object;")
}

fn native_reflection_factory_has_static_initializer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some((class_id, _)) = reflection_factory_class_arg(ctx, args) else {
        return Ok(Some(Value::Int(0)));
    };
    let has_clinit = ctx
        .declared_methods(class_id)
        .iter()
        .any(|m| &*m.name == "<clinit>");
    Ok(Some(Value::Int(has_clinit as i32)))
}

fn install_serialization_constructor_accessor(
    ctx: &mut dyn NativeContext,
    ctor_obj: ObjectRef,
    target_mirror: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let base_pin = ctx.pin_native_root(ctor_obj);
    let target_mirror_pin = ctx.pin_native_root(target_mirror);

    let target =
        try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/DirectMethodHandle$Constructor", 1)?;
    let target_pin = ctx.pin_native_root(target);
    let target_mirror = ctx.read_native_pin(target_mirror_pin, target_mirror);
    ctx.set_field_by_name(target, "instanceClass", Value::Object(Some(target_mirror)));

    let accessor = try_alloc_concurrent_synthetic(
        ctx,
        "jdk/internal/reflect/DirectConstructorHandleAccessor",
        1,
    )?;
    let target = ctx.read_native_pin(target_pin, target);
    let ctor_obj = ctx.read_native_pin(base_pin, ctor_obj);
    ctx.set_field_by_name(accessor, "target", Value::Object(Some(target)));
    ctx.set_field_by_name(
        ctor_obj,
        "constructorAccessor",
        Value::Object(Some(accessor)),
    );
    ctx.unpin_native_roots(base_pin);
    Ok(())
}

fn constructor_meta_from_constructor_object(
    ctx: &dyn NativeContext,
    ctor_obj: ObjectRef,
) -> Option<MethodMetadata> {
    let declaring_mirror = match ctx.get_field_by_name(ctor_obj, "clazz") {
        Value::Object(Some(mirror)) => mirror,
        _ => return None,
    };
    let declaring_class_id = class_id_of_mirror(ctx, declaring_mirror)?;
    let descriptor = read_constructor_descriptor(ctx, ctor_obj)?;
    let access_flags = match ctx.get_field_by_name(ctor_obj, "modifiers") {
        Value::Int(flags) => flags as u16,
        _ => 0,
    };
    if reflection_factory_debug_enabled() {
        eprintln!(
            "[rf-ser] ctor-copy obj={:?} decl_cid={} decl={} desc={} flags=0x{:x}",
            ctor_obj,
            declaring_class_id.as_u32(),
            ctx.class_name_of_id(declaring_class_id).unwrap_or_default(),
            descriptor,
            access_flags
        );
    }
    Some(MethodMetadata {
        name: "<init>".to_string(),
        descriptor,
        access_flags,
        declaring_class_id,
        exceptions: Vec::new(),
        signature: None,
    })
}

fn native_reflection_factory_new_constructor_for_serialization(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some((class_id, target_mirror)) = reflection_factory_class_arg(ctx, args) else {
        return Ok(Some(Value::Object(None)));
    };

    let ctor_obj = match args.get(2) {
        Some(Value::Object(Some(ctor))) => {
            match constructor_meta_from_constructor_object(ctx, *ctor) {
                Some(meta) => create_constructor_object(ctx, &meta)?,
                None => return Ok(Some(Value::Object(None))),
            }
        }
        _ => match find_serializable_constructor(ctx, class_id) {
            Some((_, ctor_meta)) => create_constructor_object(ctx, &ctor_meta)?,
            None => return Ok(Some(Value::Object(None))),
        },
    };
    install_serialization_constructor_accessor(ctx, ctor_obj, target_mirror)?;
    Ok(Some(Value::Object(Some(ctor_obj))))
}

fn native_reflection_factory_new_constructor_for_externalization(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some((class_id, _)) = reflection_factory_class_arg(ctx, args) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(ctor_meta) = find_no_arg_constructor(ctx, class_id) else {
        return Ok(Some(Value::Object(None)));
    };
    if (ctor_meta.access_flags & ACC_PUBLIC) == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let ctor_obj = create_constructor_object(ctx, &ctor_meta)?;
    Ok(Some(Value::Object(Some(ctor_obj))))
}

pub fn register_reflection_factory_serialization(r: &mut NativeMethodRegistry) {
    r.register(
        "sun/reflect/ReflectionFactory",
        "getReflectionFactory",
        "()Lsun/reflect/ReflectionFactory;",
        native_sun_reflection_factory_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        "jdk/internal/reflect/ReflectionFactory",
        "getReflectionFactory",
        "()Ljdk/internal/reflect/ReflectionFactory;",
        native_jdk_reflection_factory_get, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    for cls in [
        "sun/reflect/ReflectionFactory",
        "jdk/internal/reflect/ReflectionFactory",
    ] {
        r.register(
            cls,
            "newConstructorForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            native_reflection_factory_new_constructor_for_serialization, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "newConstructorForSerialization",
            "(Ljava/lang/Class;Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
            native_reflection_factory_new_constructor_for_serialization, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "newConstructorForExternalization",
            "(Ljava/lang/Class;)Ljava/lang/reflect/Constructor;",
            native_reflection_factory_new_constructor_for_externalization, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "readObjectForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            native_reflection_factory_read_object, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "readObjectNoDataForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            native_reflection_factory_read_object_no_data, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "writeObjectForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            native_reflection_factory_write_object, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "readResolveForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            native_reflection_factory_read_resolve, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "writeReplaceForSerialization",
            "(Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
            native_reflection_factory_write_replace, cratonvm_native_api::NativeKind::SyntheticStub,
        );
        r.register(
            cls,
            "hasStaticInitializerForSerialization",
            "(Ljava/lang/Class;)Z",
            native_reflection_factory_has_static_initializer, cratonvm_native_api::NativeKind::SyntheticStub,
        );
    }
}

fn register_object_stream_class(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectStreamClass";

    // KEEP, and the wave-3 phrasing ("no-op in modern OpenJDK") was too loose
    // to check, so here is what the real one does. `ObjectStreamClass.<clinit>`
    // calls `initNative()`, whose JNI body
    // (`java.base/share/native/libjava/ObjectStreamClass.c`) does exactly one
    // thing: cache a JNI global ref to `java.lang.NoSuchMethodError` for the
    // sibling `hasStaticInitializer` native to throw. It establishes NO
    // Java-visible state, and CratonVM's `hasStaticInitializer` (registered
    // immediately below) needs no cached class ref because it raises through
    // `RuntimeError`. So there is nothing a body here could set up that
    // anything later reads — the same shape as `OperatingSystemImpl.initialize0`
    // in `jmx.rs`. It must stay REGISTERED, though: an unregistered native
    // fails `<clinit>` with UnsatisfiedLinkError and takes serialization down.
    r.register(cls, "initNative", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::SyntheticStub);

    // hasStaticInitializer(Class, boolean) -> boolean
    // Returns true iff the class declares a `<clinit>` (static initializer).
    // ObjectStreamClass.computeDefaultSUID uses this to decide whether to fold
    // `<clinit>`/STATIC/`()V` into the SHA-1 digest. Returning a constant false
    // when the class DOES have a static initializer yields a serialVersionUID
    // that disagrees with the real JDK (and HotSpot), breaking cross-VM
    // deserialization (e.g. the Gradle test worker reading a HotSpot-written
    // WorkerConfig stream). The second arg (`checkSuperclass`) is irrelevant for
    // the declared-only check the spec performs here.
    r.register(
        cls,
        "hasStaticInitializer",
        "(Ljava/lang/Class;Z)Z",
        |ctx, args| {
            Ok(Some(Value::Int(
                class_has_static_initializer(ctx, args)? as i32
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Older single-arg form seen in some JDKs.
    r.register(
        cls,
        "hasStaticInitializer",
        "(Ljava/lang/Class;)Z",
        |ctx, args| {
            Ok(Some(Value::Int(
                class_has_static_initializer(ctx, args)? as i32
            )))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // lookup(Class) -> ObjectStreamClass (static)
    // WP0.2: returns a fully-populated descriptor cached per-ClassId.
    r.register(
        cls,
        "lookup",
        "(Ljava/lang/Class;)Ljava/io/ObjectStreamClass;",
        |ctx, args| {
            let mirror = obj_arg(args, 0)?;
            let class_id = match class_id_of_mirror(ctx, mirror) {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            match build_object_stream_class(ctx, class_id, false)? {
                Some(desc) => Ok(Some(Value::Object(Some(desc)))),
                None => Ok(Some(Value::Object(None))), // non-Serializable -> null
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // lookupAny(Class) -> ObjectStreamClass (static)
    // WP0.2: returns a descriptor even for non-Serializable classes —
    // real-JDK uses this during (de)serialization to reason about a
    // superclass that may or may not itself be Serializable.
    r.register(
        cls,
        "lookupAny",
        "(Ljava/lang/Class;)Ljava/io/ObjectStreamClass;",
        |ctx, args| {
            let mirror = obj_arg(args, 0)?;
            let class_id = match class_id_of_mirror(ctx, mirror) {
                Some(id) => id,
                None => return Ok(Some(Value::Object(None))),
            };
            match build_object_stream_class(ctx, class_id, true)? {
                Some(desc) => Ok(Some(Value::Object(Some(desc)))),
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // getName() -> String
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name_ref = ctx.get_field(this, 0);
        Ok(Some(name_ref))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getSerialVersionUID() -> long
    r.register(cls, "getSerialVersionUID", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let uid = ctx.get_field(this, 1);
        Ok(Some(uid))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getField(String) -> ObjectStreamField
    // WP0.2: consult the fields[] slot (6) and linear-scan by name.
    r.register(
        cls,
        "getField",
        "(Ljava/lang/String;)Ljava/io/ObjectStreamField;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fields_val = ctx.get_field(this, 6);
            let arr = match fields_val {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let target_name = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => return Ok(Some(Value::Object(None))),
            };
            let n = ctx.array_length(arr);
            for i in 0..n {
                if let Value::Object(Some(osf)) = ctx.get_array_element(arr, i) {
                    let fname = ctx.get_field(osf, 0);
                    if let Value::Object(Some(fs)) = fname {
                        if ctx.read_string(fs).as_deref() == Some(target_name.as_str()) {
                            return Ok(Some(Value::Object(Some(osf))));                        }                    }                }            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // getFields() -> ObjectStreamField[]
    // WP0.2: returns the cached fields[] array built by `lookup`.
    r.register(
        cls,
        "getFields",
        "()[Ljava/io/ObjectStreamField;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 6)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // forClass() -> Class
    // WP0.2: returns the cached Class mirror (slot 7).
    r.register(cls, "forClass", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 7)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // toString() -> String
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name_val = ctx.get_field(this, 0);
        Ok(Some(name_val))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---------------------------------------------------------------------------
// ObjectStreamField  (4-field synthetic)
//   0: name_ref, 1: type_code, 2: type_string_ref, 3: offset
// ---------------------------------------------------------------------------

fn register_object_stream_field(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectStreamField";

    // getName() -> String
    r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = ctx.get_field(this, 0);
        Ok(Some(name))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getType() -> Class
    //
    // For primitive type codes return the primitive class mirror (int.class,
    // long.class, ...). For object / array fields resolve the type string
    // (slot 2) to its Class mirror, falling back to Object.class.
    r.register(cls, "getType", "()Ljava/lang/Class;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = match ctx.get_field(this, 1) {
            Value::Int(c) => c as u8 as char,
            _ => 'L',
        };
        let prim = match code {
            'I' => Some("int"),
            'J' => Some("long"),
            'F' => Some("float"),
            'D' => Some("double"),
            'Z' => Some("boolean"),
            'B' => Some("byte"),
            'S' => Some("short"),
            'C' => Some("char"),
            _ => None,
        };
        if let Some(p) = prim {
            return Ok(Some(Value::Object(Some(ctx.primitive_class_mirror(p)))));
        }
        // Object/array: resolve the type-string field to a class mirror.
        let type_string = match ctx.get_field(this, 2) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        // Type strings look like "Ljava/lang/String;" or "[I"; strip the
        // leading 'L' and trailing ';' for the object form.
        let class_name = if let Some(inner) = type_string
            .strip_prefix('L')
            .and_then(|s| s.strip_suffix(';'))
        {
            inner.to_string()
        } else if type_string.starts_with('[') {
            type_string.clone()
        } else {
            "java/lang/Object".to_string()
        };
        let mirror = match ctx.class_id_by_name(&class_name) {
            Some(cid) => ctx.get_class_mirror(cid),
            None => match ctx.class_id_by_name("java/lang/Object") {
                Some(cid) => ctx.get_class_mirror(cid),
                None => return Ok(Some(Value::Object(None))),
            },
        };
        Ok(Some(Value::Object(Some(mirror))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getTypeCode() -> char
    r.register(cls, "getTypeCode", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = ctx.get_field(this, 1);
        Ok(Some(code))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getTypeString() -> String
    r.register(cls, "getTypeString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ts = ctx.get_field(this, 2);
        Ok(Some(ts))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // getOffset() -> int
    r.register(cls, "getOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let off = ctx.get_field(this, 3);
        Ok(Some(off))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // isPrimitive() -> boolean
    r.register(cls, "isPrimitive", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let code = match ctx.get_field(this, 1) {
            Value::Int(c) => c,
            _ => 0,
        };
        let prim = if type_code_is_primitive(code) { 1 } else { 0 };
        Ok(Some(Value::Int(prim)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // isUnshared() -> boolean
    //
    // Was a hard `false`. The real `ObjectStreamField` carries a `boolean
    // unshared` set by the `(String, Class, boolean)` constructor and by
    // `ObjectStreamClass`'s `serialPersistentFields` walk; an unconditional
    // `false` tells `writeObject`/`readObject` to use the shared handle table
    // for a field a class explicitly declared unshared, which silently aliases
    // two logically distinct objects. Read the named field so the real-JDK
    // layout answers truthfully; our 4-slot synthetic has no such field and
    // `get_field_by_name` reports `Object(None)` for it, which falls through to
    // `false` — the same answer as before for synthetic-built descriptors.
    r.register(cls, "isUnshared", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let unshared = match ctx.get_field_by_name(this, "unshared") {
            Value::Int(v) => v != 0,
            _ => false,
        };
        Ok(Some(Value::Int(if unshared { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // compareTo(Object) -> int
    //
    // Was a constant 0, i.e. "every field compares equal". `ObjectStreamClass`
    // sorts its `ObjectStreamField[]` with this comparator before writing the
    // class descriptor and before matching a received descriptor against the
    // local class, so a constant 0 left the field order at whatever the
    // reflection walk happened to produce. Two JVMs that enumerate declared
    // fields in different orders then disagree on the stream layout and
    // deserialisation reads each value into the wrong field.
    //
    // Real JDK body: primitives sort before object fields; within a group,
    // by field name.
    r.register(cls, "compareTo", "(Ljava/lang/Object;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            // `compareTo(null)` is a NullPointerException in the JDK; our
            // callers only ever pass real descriptors, so treat anything else
            // as "sorts after" rather than unwinding mid-sort.
            _ => return Ok(Some(Value::Int(-1))),
        };
        let this_prim = type_code_is_primitive(ctx.get_field(this, 1).as_int().unwrap_or(0));
        let other_prim = type_code_is_primitive(ctx.get_field(other, 1).as_int().unwrap_or(0));
        if this_prim != other_prim {
            return Ok(Some(Value::Int(if this_prim { -1 } else { 1 })));
        }
        // No allocation happens between these reads, so the raw refs are safe.
        let this_name = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let other_name = match ctx.get_field(other, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        // Java's String.compareTo is UTF-16 code-unit order. Field names are
        // Java identifiers (ASCII in every practical case), for which Rust's
        // byte-wise Ord agrees exactly.
        let ord = match this_name.cmp(&other_name) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
        Ok(Some(Value::Int(ord)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---------------------------------------------------------------------------
// Serializable / Externalizable marker interfaces
// ---------------------------------------------------------------------------

fn register_serializable(r: &mut NativeMethodRegistry) {
    let cls = "java/io/Serializable";
    // Marker interface: it declares no methods at all, so `registerNatives` has
    // no body to shadow and doing nothing matches HotSpot, where the JVM-side
    // hook is likewise empty.
    r.register(cls, "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::SyntheticStub);
}

// `java/io/Externalizable` deliberately has NO registrations. `writeExternal` /
// `readExternal` are ABSTRACT interface methods whose behaviour is defined
// entirely by the implementing class, so no native can ever be right here. The
// no-op pair that used to live here was unreachable for a real implementor
// (native lookup keys on the resolved method's declaring class — the user class
// — and the ancestor walk follows `superclass` only, never the interface list)
// and actively wrong on the one path that does resolve to the interface: a
// receiver whose class collapses to `java/io/Externalizable` silently
// serialized NOTHING instead of raising AbstractMethodError. No CratonVM
// synthetic implements Externalizable, so there is no synthetic receiver to
// serve either. `oos_write_object` drives the real thing by
// `invoke_virtual`-ing `writeExternal` on the user object.

// ---------------------------------------------------------------------------
// ObjectInputFilter (JEP 290) — 4-field synthetic
//   0: status (0=UNDECIDED, 1=ALLOWED, 2=REJECTED)
//   1: max_depth, 2: max_references, 3: max_bytes
// ---------------------------------------------------------------------------
//
// JEP-290 enforcement layer (HIGH-severity security gate)
// =======================================================
//
// The JDK's `ObjectInputFilter` protects against gadget-chain
// deserialization attacks by letting an application reject unwanted
// classes *before* they are resolved by `ObjectInputStream.resolveClass`.
// Two levels of filtering are defined:
//
//   * **Process-wide filter** — installed exactly once via
//     `ObjectInputFilter.Config.setSerialFilter(...)`. The JEP mandates
//     that a second call must throw `IllegalStateException`. The default
//     value comes from the `jdk.serialFilter` system property (we
//     read it lazily on first access — `env::var`).
//
//   * **Per-stream filter** — installed via
//     `ObjectInputStream.setObjectInputFilter(...)`. Consulted *first*;
//     if it returns `UNDECIDED`, falls back to the process-wide filter.
//
// `resolveClass` and `resolveProxyClass` must invoke this filter
// pipeline. A `REJECTED` decision MUST surface as an
// `InvalidClassException("filter status: REJECTED")` — never a silent
// null return (that was the pre-fix behaviour, which left the door wide
// open for gadget chains).
//
// Pattern grammar (subset of the JDK's ObjectInputFilter syntax):
//
//   pattern_list := pattern (';' pattern)*
//   pattern      := '!' rule        (reject)
//                 | rule             (allow)
//                 | limit '=' value  (maxdepth/maxrefs/maxbytes/maxarray)
//   rule         := '*'              (match any class name — single token)
//                 | '<glob>**'       (match any class whose name STARTS WITH glob)
//                 | '<glob>*'        (match any class in the same package,
//                                     but not in sub-packages)
//                 | '<fqcn>'         (exact class-name match)
//
// All four JEP-290 resource limits (maxdepth/maxrefs/maxbytes/maxarray)
// are now enforced on the deserialization read paths:
//
//   * maxbytes  — `ois_buf_read` accounts every byte consumed via
//     `filter_account_bytes`; once cumulative `bytes` exceeds the
//     configured `max_bytes` the sticky `rejected` flag trips.
//   * maxarray  — `ois_read_array` consults `filter_check_array` against
//     the on-wire declared length *before* allocating the backing array,
//     rejecting lengths greater than `max_array`.
//   * maxdepth  — `ois_read_value` gates recursion through
//     `filter_enter_depth` / `filter_exit_depth`.
//   * maxrefs   — the `TC_REFERENCE` arm of `ois_read_value` bumps `refs`
//     via `filter_account_ref`.
//
// Once any cap trips, `readObject` consults `filter_is_rejected` and
// raises `IOException("filter status: REJECTED: ...")` — the
// `ObjectInputFilter.Status.REJECTED` flow per JEP-290.

/// Filter decision per JEP-290. Matches `ObjectInputFilter.Status`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FilterStatus {
    Undecided = 0,
    Allowed = 1,
    Rejected = 2,
}

/// A single rule in a compiled filter pattern.
#[derive(Clone, Debug)]
enum FilterRule {
    /// `*` — single-token wildcard, matches any class name.
    Any,
    /// `prefix.**` — recursive package match (prefix without trailing `**`).
    Recursive(String),
    /// `prefix.*` — same-package match (prefix is the package).
    SamePackage(String),
    /// Exact fully-qualified class name (slash-or-dot form normalised to dots).
    Exact(String),
}

impl FilterRule {
    fn matches(&self, class_name_dotted: &str) -> bool {
        match self {
            FilterRule::Any => true,
            FilterRule::Recursive(prefix) => class_name_dotted.starts_with(prefix.as_str()),
            FilterRule::SamePackage(pkg_prefix) => {
                if !class_name_dotted.starts_with(pkg_prefix.as_str()) {
                    return false;
                }
                // No further `.` separator beyond the prefix — otherwise it's
                // in a sub-package and a single `*` must NOT match it.
                let tail = &class_name_dotted[pkg_prefix.len()..];
                !tail.contains('.')
            }
            FilterRule::Exact(name) => class_name_dotted == name.as_str(),
        }
    }
}

/// One element of a parsed filter spec.
#[derive(Clone, Debug)]
enum FilterEntry {
    /// A class-name rule. `reject` is true when the source pattern began with `!`.
    Class { rule: FilterRule, reject: bool },
    /// `key=value` — `key` ∈ { maxdepth, maxrefs, maxbytes, maxarray }.
    /// Stored but not yet enforced (TODO above).
    Limit { key: String, value: i64 },
}

/// A compiled JEP-290 filter — a list of entries evaluated left-to-right
/// against the class name. First match wins (per the JDK).
#[derive(Clone, Debug)]
pub struct SerialFilter {
    entries: Vec<FilterEntry>,
    /// Original pattern, kept verbatim for `toString` round-tripping.
    pub pattern: String,
}

impl SerialFilter {
    /// Parse a `jdk.serialFilter`-style pattern string.
    pub fn parse(pattern: &str) -> SerialFilter {
        let mut entries = Vec::new();
        for raw in pattern.split(';') {
            let token = raw.trim();
            if token.is_empty() {
                continue;
            }
            // limit token: contains '='
            if let Some(eq) = token.find('=') {
                let key = token[..eq].trim().to_ascii_lowercase();
                let value_s = token[eq + 1..].trim();
                if matches!(
                    key.as_str(),
                    "maxdepth" | "maxrefs" | "maxbytes" | "maxarray"
                ) {
                    if let Ok(v) = value_s.parse::<i64>() {
                        entries.push(FilterEntry::Limit { key, value: v });
                        continue;
                    }
                }
                // Unknown limit / malformed value — skip silently. The JDK
                // would throw IllegalArgumentException at Config time; we
                // mirror its lenient runtime behaviour to avoid breaking
                // existing JREs that ship odd filter strings.
                continue;
            }

            let (reject, body) = if let Some(stripped) = token.strip_prefix('!') {
                (true, stripped.trim())
            } else {
                (false, token)
            };
            if body.is_empty() {
                continue;
            }
            // Normalise '/'->'.' so callers can pass either form. We keep
            // the rule canonical in dotted form (`a.b.C`) and the
            // `matches` path converts the incoming class name the same way.
            let body = body.replace('/', ".");
            let rule = if body == "*" {
                FilterRule::Any
            } else if let Some(prefix) = body.strip_suffix(".**") {
                FilterRule::Recursive(format!("{}.", prefix))
            } else if body == "**" {
                FilterRule::Recursive(String::new())
            } else if let Some(prefix) = body.strip_suffix(".*") {
                FilterRule::SamePackage(format!("{}.", prefix))
            } else if body.ends_with('*') {
                // Bare-prefix glob without a separator, e.g. "com.foo.*"
                // already handled above; here we treat "Foo*" as a recursive
                // glob since the JDK reads `*` greedily in the same-package
                // form only when preceded by `.`.
                let prefix = &body[..body.len() - 1];
                FilterRule::Recursive(prefix.to_string())
            } else {
                FilterRule::Exact(body)
            };
            entries.push(FilterEntry::Class { rule, reject });
        }
        SerialFilter {
            entries,
            pattern: pattern.to_string(),
        }
    }

    /// Decide whether `class_name` (slash- or dot-form) is allowed.
    pub fn check(&self, class_name: &str) -> FilterStatus {
        let dotted = class_name.replace('/', ".");
        for entry in &self.entries {
            if let FilterEntry::Class { rule, reject } = entry {
                if rule.matches(&dotted) {
                    return if *reject {
                        FilterStatus::Rejected
                    } else {
                        FilterStatus::Allowed
                    };
                }
            }
        }
        FilterStatus::Undecided
    }
}

/// A VM's "process-wide" JEP-290 serial filter (§2.1): the one
/// `ObjectInputFilter.Config` holds, which in a JVM is a static of a class,
/// i.e. one per VM.
///
/// gc-common w10-a (`common-w8a-process-global-object-singletons-in-native-builtins`):
/// this was two process-wide slots (`process_serial_filter` and
/// `process_serial_filter_obj`). VM B's `getSerialFilter()` answered VM A's
/// filter object, B's deserialization was judged by A's filter, and B's
/// `setSerialFilter` threw "can only be set once" because A had set one.
/// Rows are dropped by [`forget_vm_serial_filter`] at VM teardown.
#[derive(Default)]
struct ProcessSerialFilter {
    /// Whether `parsed` has been seeded from `jdk.serialFilter`.
    seeded: bool,
    /// The compiled filter the decision is taken with. `None` means "no filter
    /// installed yet"; once set, the JEP forbids replacement.
    parsed: Option<SerialFilter>,
    /// The Java filter object `setSerialFilter` installed, for
    /// `getSerialFilter` to round-trip.
    ///
    /// GC: a global root of the owning VM ([`SerialRoot`]); every read
    /// resolves the CURRENT address through it. gc-common w29-d: it used to be
    /// `(identity_key, ObjectRef)` rooted through `register_var_handle_root`
    /// and re-read through `read_var_handle_root(identity_key)`. That registry
    /// is keyed by identity hash, so `getSerialFilter()` answered whichever
    /// object had been registered first under the filter's hash -- a
    /// `VarHandle`, a `TypeVariable`, anything a native had rooted there.
    obj: Option<SerialRoot>,
}

static PROCESS_SERIAL_FILTER: cratonvm_native_api::vm_scoped::VmScoped<ProcessSerialFilter> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Run `f` on `vm`'s serial-filter state, seeding it on first use.
///
/// The seed honours the `jdk.serialFilter` setting if present. The real JDK
/// also reads `conf/security/java.security` — out of scope here. `f` runs
/// under `VmScoped`'s lock: it must not allocate or call into Java (parsing
/// and matching a `SerialFilter` is pure Rust).
fn with_process_serial_filter<R>(vm: usize, f: impl FnOnce(&mut ProcessSerialFilter) -> R) -> R {
    PROCESS_SERIAL_FILTER.with(vm, |st| {
        if !st.seeded {
            st.seeded = true;
            st.parsed = cratonvm_types::flags::runtime_var("jdk.serialFilter")
                .ok()
                .or_else(|| cratonvm_types::flags::runtime_var("JDK_SERIAL_FILTER").ok())
                .filter(|s| !s.is_empty())
                .map(|s| SerialFilter::parse(&s));
        }
        f(st)
    })
}

/// Drop `vm`'s serial filter and filter object (VM teardown, from
/// `lib.rs::forget_vm_native_root_stores`). Idempotent.
pub fn forget_vm_serial_filter(vm: usize) {
    PROCESS_SERIAL_FILTER.forget(vm);
}

/// Per-stream filter table keyed by the `ObjectInputStream`'s stream key.
fn ois_stream_filters() -> &'static Mutex<HashMap<usize, SerialFilter>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, SerialFilter>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-stream filter Java objects, as `(vm, root)`. The Java surface lets
/// callers retrieve back the same `ObjectInputFilter` instance they installed
/// via `getObjectInputFilter`, so we hold onto the Java object too -- as a
/// global root of `vm` (gc-common w21-c).
fn ois_stream_filter_objs() -> &'static Mutex<HashMap<usize, (usize, SerialRoot)>> {
    static INSTANCE: std::sync::OnceLock<Mutex<HashMap<usize, (usize, SerialRoot)>>> =
        std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drop the stream keyed `key`'s `setObjectInputFilter` filter and release
/// the root of its filter object.
fn clear_stream_filter(ctx: &mut dyn NativeContext, key: usize) {
    ois_stream_filters()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    let removed = ois_stream_filter_objs()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some((vm, held)) = removed {
        release_serial_roots(ctx, vm, vec![held.root]);
    }
}

/// Apply the per-stream filter first, then `vm`'s process-wide one.
/// Returns the merged decision per JEP-290 (UNDECIDED ⇒ allow at
/// resolveClass time, REJECTED ⇒ throw).
fn evaluate_serial_filters(vm: usize, ois_addr: usize, class_name: &str) -> FilterStatus {
    // Per-stream filter installed via `setObjectInputFilter` (synthetic
    // ALLOW/REJECT objects compiled to `SerialFilter`).
    {
        let map = ois_stream_filters()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(f) = map.get(&ois_addr) {
            match f.check(class_name) {
                FilterStatus::Rejected => return FilterStatus::Rejected,
                FilterStatus::Allowed => return FilterStatus::Allowed,
                FilterStatus::Undecided => { /* fall through */ }
            }
        }
    }
    // Per-stream class-name patterns parsed from a filter *string* (the
    // `parse_serial_filter` path, installed via `ois_set_filter_state`).
    // Still a per-stream filter, so it precedes the process-wide one but
    // follows any explicit `setObjectInputFilter` install above.
    {
        let map = ois_filter_state().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(f) = map.get(&ois_addr).and_then(|s| s.patterns.as_ref()) {
            match f.check(class_name) {
                FilterStatus::Rejected => return FilterStatus::Rejected,
                FilterStatus::Allowed => return FilterStatus::Allowed,
                FilterStatus::Undecided => { /* fall through */ }
            }
        }
    }
    with_process_serial_filter(vm, |st| {
        st.parsed
            .as_ref()
            .map_or(FilterStatus::Undecided, |f| f.check(class_name))
    })
}

/// Read the class name slot of an `ObjectStreamClass` descriptor.
/// Returns `None` when the descriptor or its name field is null.
fn osc_class_name(ctx: &dyn NativeContext, desc: ObjectRef) -> Option<String> {
    match ctx.get_field(desc, 0) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

fn alloc_filter(ctx: &mut dyn NativeContext, status: i32) -> Result<ObjectRef, MethodCallFailed> {
    let filter = try_alloc_concurrent_synthetic(ctx, "java/io/ObjectInputFilter", 4)?;
    ctx.set_field(filter, 0, Value::Int(status));
    ctx.set_field(filter, 1, Value::Int(256));
    ctx.set_field(filter, 2, Value::Int(10000));
    ctx.set_field(filter, 3, Value::Long(0));
    Ok(filter)
}

fn register_object_input_filter(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectInputFilter";

    // checkInput(FilterInfo) -> Status
    r.register(
        cls,
        "checkInput",
        "(Ljava/io/ObjectInputFilter$FilterInfo;)Ljava/io/ObjectInputFilter$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let status = match ctx.get_field(this, 0) {
                Value::Int(s) => s,
                _ => 0,
            };
            let status_obj: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/io/ObjectInputFilter$Status", 1)?;
            ctx.set_field(status_obj, 0, Value::Int(status));
            Ok(Some(Value::Object(Some(status_obj))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // allowFilter (static)
    r.register(
        cls, "allowFilter",
        "(Ljava/util/function/Predicate;Ljava/io/ObjectInputFilter$Status;)Ljava/io/ObjectInputFilter;",
        |ctx, _args| Ok(Some(Value::Object(Some(alloc_filter(ctx, 1)?)))), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // rejectFilter (static)
    r.register(
        cls, "rejectFilter",
        "(Ljava/util/function/Predicate;Ljava/io/ObjectInputFilter$Status;)Ljava/io/ObjectInputFilter;",
        |ctx, _args| Ok(Some(Value::Object(Some(alloc_filter(ctx, 2)?)))), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // merge (static)
    r.register(
        cls,
        "merge",
        "(Ljava/io/ObjectInputFilter;Ljava/io/ObjectInputFilter;)Ljava/io/ObjectInputFilter;",
        |ctx, _args| Ok(Some(Value::Object(Some(alloc_filter(ctx, 0)?)))), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Config.getSerialFilter (static) — returns the previously-installed
    // Java filter object, or null if none was set. The actual filter
    // *decision* is taken via the parsed `SerialFilter` cache (which is
    // populated either from `jdk.serialFilter` or from the call to
    // `setSerialFilter` below).
    r.register(
        "java/io/ObjectInputFilter$Config",
        "getSerialFilter",
        "()Ljava/io/ObjectInputFilter;",
        |ctx, _args| {
            // Resolve the CURRENT (post-GC) address through the filter's
            // global root (gc-common w29-d). Mock contexts without global
            // roots answer the raw ref they stored.
            let obj = with_process_serial_filter(ctx.vm_identity(), |st| st.obj)
                .and_then(|root| root.current(&*ctx));
            Ok(Some(Value::Object(obj)))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // Config.setSerialFilter (static) — JEP-290 §2.1: must throw
    // IllegalStateException if a filter has already been set.
    r.register(
        "java/io/ObjectInputFilter$Config",
        "setSerialFilter",
        "(Ljava/io/ObjectInputFilter;)V",
        |ctx, args| {
            let vm = ctx.vm_identity();
            let already_set = || RuntimeError::IllegalStateException {
                message: "Serial filter can only be set once".into(),
            };
            // Already-set guard, for THIS VM (gc-common w10-a).
            {
                if with_process_serial_filter(vm, |st| st.parsed.is_some() || st.obj.is_some()) {
                    return Err(already_set().into());
                }
                // Accept null only as a no-op (mirrors the JDK behaviour
                // where setSerialFilter(null) is documented as a NPE; we
                // surface that explicitly to keep callers honest).
                let filter_obj = match args.get(0) {
                    Some(Value::Object(Some(o))) => *o,
                    _ => {
                        return Err(RuntimeError::NullPointerException {
                            message: Some("filter".into()),
                        }
                        .into());
                    }
                };
                // Best-effort: extract a pattern string by calling the
                // synthetic filter object's slot-0 status as a stand-in for
                // an explicit pattern. Real filters created via
                // `Config.createFilter(String)` would carry the original
                // pattern in a dedicated field; synthetic ALLOW/REJECT
                // filters carry only a status. We honour both:
                //   – slot 0 == REJECTED ⇒ reject-everything pattern
                //   – slot 0 == ALLOWED  ⇒ allow-everything pattern
                //   – otherwise          ⇒ rely on the system property /
                //                          UNDECIDED (the resolver falls
                //                          back to permissive).
                let status = ctx.get_field(filter_obj, 0).as_int().unwrap_or(0);
                let parsed = match status {
                    s if s == FilterStatus::Rejected as i32 => SerialFilter::parse("!*"),
                    s if s == FilterStatus::Allowed as i32 => SerialFilter::parse("*"),
                    _ => SerialFilter {
                        entries: Vec::new(),
                        pattern: String::new(),
                    },
                };
                // Keep alive and remapped across GC moves: a global root of
                // this VM, unique to this object (gc-common w29-d; it used to
                // be the identity-hash-keyed var-handle-root registry). No
                // allocation since `filter_obj` was read from `args`.
                let root = SerialRoot::hold(ctx, filter_obj);
                // Publish both halves together. A racing setter that won
                // since the check above makes this one the second setter;
                // its global root is released below.
                let installed = with_process_serial_filter(vm, |st| {
                    if st.parsed.is_some() || st.obj.is_some() {
                        return false;
                    }
                    st.parsed = Some(parsed);
                    st.obj = Some(root);
                    true
                });
                if !installed {
                    if root.root != 0 {
                        ctx.remove_global_root(root.root);
                    }
                    return Err(already_set().into());
                }
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// ObjectOutput / ObjectInput  (interface stubs)
// ---------------------------------------------------------------------------

fn register_object_output(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectOutput";
    // Static interface initializer: `ObjectOutput` declares no `registerNatives`
    // body of its own, so there is nothing to shadow and doing nothing matches
    // HotSpot.
    r.register(cls, "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::SyntheticStub);
    // `writeObject` / `flush` / `close` are ABSTRACT interface methods and used
    // to be no-ops here. `ObjectOutputStream` — the only implementor CratonVM
    // ever produces — carries real natives for all three (see
    // `register_object_output_stream`), and dispatch keys on the resolved
    // method's declaring class, so those are what a genuine receiver reaches.
    // The interface entries were therefore dead weight except on a receiver
    // that collapses to the bare interface, where a no-op `writeObject`
    // silently produced an EMPTY serialization stream and a no-op `close`
    // leaked the sink. A loud AbstractMethodError is the better outcome, so
    // nothing is registered for them.
}

fn register_object_input(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ObjectInput";
    // Static interface initializer: `ObjectInput` declares no `registerNatives`
    // body of its own, so there is nothing to shadow and doing nothing matches
    // HotSpot.
    r.register(cls, "registerNatives", "()V", native_noop, cratonvm_native_api::NativeKind::SyntheticStub);
    // Round-7 HIGH-12 fix: previously a blanket UnsupportedOperationException
    // here propagated out of any JNDI / RMI bootstrap path that probes the
    // interface (e.g. `InitialContext` decoding the LDAP environment). The
    // JDK contract allows `readObject` to return null at end-of-stream, and
    // the JNDI bootstrap recovers gracefully from `null` but not from a UOE
    // surfaced through `Object readObject() throws ClassNotFoundException`.
    // Return null so downstream `if (obj == null) { ... fallback ... }`
    // branches fire instead of unwinding through the interface default.
    //
    // Round-9 MED-9 follow-up: a real primitive-aware deserializer lives on
    // `ObjectInputStream` (concrete subclass), where `ois_read_value` /
    // `ois_read_object` decode `TC_STRING`/`TC_OBJECT` for String, Integer,
    // Long and friends — see :860-987 in this file. That path is only
    // reachable when the call lands on the concrete `ObjectInputStream`
    // override; here we are the bare interface default with no backing
    // stream pointer in `args[0]` to drive a decode. Returning null is
    // therefore the only correct behaviour at this layer: any caller that
    // expected real bytes already dispatched virtually to the OIS subclass
    // before reaching this default. Verified safe for JNDI bootstrap which
    // null-checks the return value (InitialContext.getURLOrDefaultInitCtx).
    //
    // Wave-3 reachability check, re-confirmed in wave 4 against
    // `vm/src/runtime/interpreter.rs` (`try_stackless_invoke` step 6) and
    // `vm/src/vm/vm_exec.rs`, so this is not re-litigated: both entries are
    // instance methods on an INTERFACE, and native lookup drops those unless
    // the descriptor is the `()Liface;` / `(Liface;)Liface;` default-method
    // shape (neither is — `()Ljava/lang/Object;` and `()I`) or the triple is
    // force-listed in `should_force_registered_native_over_bytecode` (neither
    // is). So they cannot intercept a user `ObjectInput` implementor at all —
    // the only receiver that reaches them is one whose class collapses to the
    // bare interface, i.e. a CratonVM synthetic with no backing stream. For
    // that receiver `null` (end of stream) and `0` (no bytes readable without
    // blocking) are the JDK's own legal answers, not placeholders: the
    // interface declares both abstract, so there is no real body to match and
    // the contract is all there is. VERDICT: KEEP, on the interface-contract
    // ground — there is no receiver state here to read.
    r.register(cls, "readObject", "()Ljava/lang/Object;", |_ctx, _args| {
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "available", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `close()` is an ABSTRACT interface method; `ObjectInputStream` provides
    // the real one and dispatch keys on the resolved method's declaring class,
    // so the no-op that used to sit here only ever fired for a receiver that
    // collapsed to the bare interface — where it silently leaked the underlying
    // stream. Not registered: let that surface as AbstractMethodError.
    // (`readObject` / `available` above stay because they return a value the
    // JDK contract genuinely permits, not because they do nothing.)
}

// ---------------------------------------------------------------------------
// Exception types  (3-field synthetic each)
//   0: message_ref, 1: classname_ref, 2: detail_ref
// ---------------------------------------------------------------------------

fn register_invalid_class_exception(r: &mut NativeMethodRegistry) {
    let cls = "java/io/InvalidClassException";

    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(msg))) = args.get(1) {
            ctx.set_field(this, 0, Value::Object(Some(*msg)));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(
        cls,
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(Value::Object(Some(cname))) = args.get(1) {
                ctx.set_field(this, 1, Value::Object(Some(*cname)));
            }
            if let Some(Value::Object(Some(msg))) = args.get(2) {
                ctx.set_field(this, 0, Value::Object(Some(*msg)));
            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    r.register(cls, "getMessage", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    r.register(cls, "getClassname", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

fn register_exception_type(r: &mut NativeMethodRegistry, cls: &str) {
    r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(msg))) = args.get(1) {
            ctx.set_field(this, 0, Value::Object(Some(*msg)));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(cls, "getMessage", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

fn register_not_serializable_exception(r: &mut NativeMethodRegistry) {
    register_exception_type(r, "java/io/NotSerializableException");
}

fn register_stream_corrupted_exception(r: &mut NativeMethodRegistry) {
    register_exception_type(r, "java/io/StreamCorruptedException");
    // No-arg constructor. A bare no-op leaves `detailMessage` null (correct)
    // but ALSO skips `fillInStackTrace()`, so `getStackTrace()` came back empty
    // — the same bug `native_exception_init_empty` was written to fix for every
    // other no-arg exception ctor (`register_exception_extras_natives`, which
    // does not list this class). Use that shared helper.
    r.register(
        "java/io/StreamCorruptedException",
        "<init>",
        "()V",
        crate::native_exception_init_empty, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// Registration entry point
// ---------------------------------------------------------------------------

/// REACHABILITY (traced wave 4; the second gate came off 2026-08-11 — read
/// this before judging anything above).
///
/// This function has exactly one call site outside tests,
/// `native-builtins/src/lib.rs`, and it is now gated ONCE: it sits inside
/// `register_synthetic_overrides`, which is `#[cfg(feature =
/// "synthetic-jdk")]`. So every registration below is dead in the default
/// real-JDK build and live in every synthetic-library build.
///
/// It used to carry a second `#[cfg(feature = "experimental-serialization")]`
/// at that call site, and the pair was a hole rather than a policy. In a
/// synthetic build `java/io/ObjectOutputStream` and `ObjectInputStream` are
/// fabricated stubs with no bytecode behind them, so withholding the natives
/// left them present-but-INERT rather than absent: `writeObject` wrote
/// nothing, `readObject` handed back a blank instance whose every field read
/// null, and writing a non-`Serializable` raised nothing. That is precisely
/// what `KNOWN_SYNTHETIC_JDK_GAPS` pinned as four "serialization gaps" in the
/// class library — with the 7.6k lines of implementation they were said to be
/// missing already compiled into the same binary (this module's own `#[cfg]`
/// is `any(experimental-serialization, synthetic-jdk)`).
///
/// This module has NO entry point on the real-JDK path any more. The last one,
/// a call to `register_reflection_factory_serialization` from
/// `register_essential_natives_with_shims` (gated on
/// `experimental-serialization`), was deleted so the CLI and workspace builds
/// converge on the JDK's own bytecode (see the note at that site in `lib.rs`);
/// `phases_late::objectstreamclass_natives`, a caller-less forwarder, went in
/// interpreter wave 99.
///
/// `register_byte_array_output_stream` used to be called a second time from
/// `lib.rs`, WITHOUT the serialization feature gate but still from inside
/// `register_synthetic_overrides` — a mismatch with the comment at that call
/// site claiming it must "always win" for real-JCA DER output, since it is
/// synthetic-only either way. G54-1 NOMINATION 5 removed both calls: the
/// function registers nothing (see its own doc), so removing them changes
/// nothing about what either registry contains.
pub fn register_serialization_natives(r: &mut NativeMethodRegistry) {
    register_object_output_stream(r);
    register_object_input_stream(r);
    register_object_stream_class(r);
    register_object_stream_field(r);
    register_serializable(r);
    register_object_input_filter(r);
    register_object_output(r);
    register_object_input(r);
    register_invalid_class_exception(r);
    register_not_serializable_exception(r);
    register_stream_corrupted_exception(r);
    register_reflection_factory_serialization(r);
}

// ---------------------------------------------------------------------------
// ByteArrayOutputStream — the twelve registrations and their five helpers are
// GONE (lane G50, 2026-08-17). The function is kept, empty, because its two
// call sites are in `lib.rs`, which that lane does not own.
// ---------------------------------------------------------------------------

/// Registers nothing. `java/io/ByteArrayOutputStream` is served in **every**
/// mode by `cratonvm_native_io::register_io_natives`.
///
/// # Why the twelve registrations that used to be here were dead — MEASURED
///
/// This function has exactly two callers — `register_serialization_natives`
/// above, and a direct call from `lib.rs` — and BOTH are reached only from
/// `register_synthetic_overrides`, which is
/// `#[cfg(feature = "synthetic-jdk")]`. In the one VM arm that calls it
/// (`vm/src/vm/vm_init.rs`, the `config.use_synthetic_jdk` branch) the very
/// next statement is `register_io_natives`, and
/// `NativeMethodRegistry::register` is last-registration-wins **on the
/// callback**: re-registering a known key assigns `slot.callback = callback`
/// in place (`native-api/src/registry.rs`, the `Some(idx)` arm). So
/// native-io's thirteen `java/io/ByteArrayOutputStream` registrations
/// (`<init>` ×2, `write` ×3, `toByteArray`, `size`, `reset`, `toString` ×3,
/// `close`, `flush`) replaced all twelve of these before any bytecode ran, in
/// every build that ever compiled them.
///
/// `--dump-native-registry` from `cratonvm.exe` at `9ae371468`, taken
/// 2026-08-17 in BOTH modes, agrees: all thirteen rows read `kind = bridge`,
/// `owns_slot = true`, `overwrote = null`,
/// `registered_by = native-io/src/lib.rs:6871`–`:6898`; and `serialization.rs`
/// owns **zero** of the 12,039 (compatible) / 10,691 (`--real-jdk`) rows.
/// Five rows carry `invocations > 0` — `write([BII)V` 77, `flush()V` 14,
/// `<init>()V` 11, `toByteArray()[B` 11, `close()V` 7 — which is positive
/// proof the native-io bodies are the ones that run. (`invocations == 0` would
/// have proved nothing; four dispatch families bypass the counter, `G33-1`.)
///
/// `F34-1` §5's trap — that dropping a synthetic-only pass can drop triples
/// its shipping twin never registered — does not apply: native-io's set is a
/// strict SUPERSET (it also binds `write([B)V`, which was never here).
///
/// # Why leaving them would have been worse than deleting them
///
/// `close()`/`flush()` carried a KEEP comment arguing that a no-op *is* the
/// real behaviour. That is true of the JDK contract and false of this VM:
/// `native_baos_close` / `native_baos_flush` dispatch `BaosEvent::Close` /
/// `BaosEvent::Flush` and then drive `process_pipe_output_close` /
/// `fd_table().flush(fd)` — the machinery behind a `Process`'s stdin pipe,
/// which a no-op silently skips. A plausible-looking synthetic twin that would
/// be wrong the moment the registration order changed is exactly the shape
/// this branch has already shipped a fix into once (`8c72d23ca`).
///
/// Deleted with the arms: `baos_buffer_bytes`, `baos_charset_key`,
/// `decode_utf16_bytes`, `decode_baos_bytes` and `charset_object_name`. None
/// had any referent outside this file in any of the seven crates (`grep`,
/// excluding `target/` and `scratch*/`); the surviving `decode_utf16_bytes` in
/// `xml_stax.rs` is an unrelated private function that happens to share the
/// name.
///
/// Record: `docs/feature-designs/native-dispatch-architecture.md`.
pub fn register_byte_array_output_stream(_r: &mut NativeMethodRegistry) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod serialization_tests;

// ---------------------------------------------------------------------------
// WP0.2 — build_object_stream_class tests
// ---------------------------------------------------------------------------
//
// These exercise the descriptor builder end-to-end via the shared
// `MockNativeContext` harness (extended with WP0.2 per-class field /
// method / super / OSC-cache overrides in `test_utils.rs`).  That
// keeps the WP0.2 test surface small and routed through the same mock
// every other native-method test in this crate uses.

#[cfg(test)]
mod wp02_tests;

// ---------------------------------------------------------------------------
// Object-graph marshalling round-trip tests
// ---------------------------------------------------------------------------
//
// Exercise the shared `oos_write_value` / `ois_read_value` pair end-to-end
// through `MockNativeContext`. These lock in the WP-after-WP0.2 work:
// nested object fields, arrays, back-references (cycles) and primitive
// field marshalling now actually round-trip instead of degrading to
// `TC_NULL`.

#[cfg(test)]
mod marshal_tests;

/// gc-common w10-a: `ObjectInputFilter.Config`'s filter is one per VM.
#[cfg(test)]
mod w10a_serial_filter_vm_isolation_tests;

/// gc-common w21-c (`common-w20c-synthetic-serialization-tables-keyed-by-raw-address`):
/// the per-stream tables are keyed by the stream's weak lock key, the writer's
/// back-reference table by each object's, the reader's objects and the
/// stream's filter are global roots, and the stream "this" / curObj frames are
/// per-thread pins. The mock's identity hash is the address truncated to
/// `i32`, so an address 4 GiB above another has the same hash, as a moved
/// object keeps its identity hash. Each test has its own VM identity (the
/// lock-key registry and these tables are process-wide) and forgets it in a
/// guard.
#[cfg(test)]
mod w21c_stream_key_tests;
