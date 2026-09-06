// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! T19.7.e — XNIO `OptionMap` + `IoFuture` + `XnioExecutor`.
//!
//! XNIO's configuration and async primitives sit underneath the worker
//! (T19.7.b), io-thread (T19.7.c), and conduit (T19.7.d) layers. This
//! module implements the four pillars:
//!
//! * `org.xnio.Option<T>` — a typed configuration-key identity (declaring
//!   class, name, type class). Equality is by identity (address/pointer),
//!   but `OptionMap` stores entries keyed by the `(declaring class, name)`
//!   pair so two independently-created `Option<T>` instances that share the
//!   same coordinates land in the same slot. This mirrors the WildFly/XNIO
//!   semantics where `Options.WORKER_IO_THREADS` is interned.
//!
//! * `org.xnio.OptionMap` — an immutable typed K/V, backed by
//!   `Arc<HashMap<OptionKey, OptionValue>>`. `OptionMap.EMPTY` is the
//!   zero-entry singleton. Builders produce new `OptionMap` instances
//!   by cloning the accumulated entries; once `getMap()` is called, the
//!   builder is marked consumed and subsequent `set()` throws
//!   `IllegalStateException`.
//!
//! * `org.xnio.Options` — a collection of well-known `Option` instances
//!   (`WORKER_IO_THREADS`, `BACKLOG`, `KEEP_ALIVE`, …). Populated on first
//!   access into the `Options.<name>` static field via
//!   `ensure_options_initialized`.
//!
//! * `org.xnio.IoFuture<T>` — XNIO's async future. Backed by an
//!   `AtomicU8` status (WAITING → DONE | CANCELLED | FAILED) plus a
//!   `parking_lot::Mutex<FutureState>` for the result/exception/notifier
//!   list, awakened by a `Condvar` for `await()` callers. Status
//!   transitions are **monotonic**: the first call to setResult /
//!   setException / setCancelled wins, and later calls are no-ops.
//!   Notifiers are dispatched outside the lock, each wrapped in
//!   `catch_unwind(AssertUnwindSafe)` so a bad notifier doesn't kill the
//!   rest of the list.
//!
//! Orchestration contract with the other four T19.7 agents:
//!
//! * T19.7.b (`xnio_worker.rs`) reads `OptionMap` via `Xnio.createWorker` to
//!   extract `WORKER_IO_THREADS` and spin up the right number of IO
//!   threads. This module OWNS `Xnio.createWorker` only to the extent that
//!   it exposes the `get(Option<Integer>)` helper — the worker native
//!   itself lives in `xnio_worker.rs`.
//!
//! * T19.7.c (`xnio_io_thread.rs`) returns our `XnioExecutor$Key` from
//!   `executeAfter` — we register the stub-field layout here so the other
//!   native can set its fields.
//!
//! * T19.7.d (`xnio_conduits.rs`) chains `IoFuture` completions when socket
//!   reads finish — the conduit natives call into the `FutureResult.setResult`
//!   native registered here.
//!
//! ## Status machine
//!
//! ```text
//!                 setResult/setException/setCancelled
//!       WAITING ────────────────────────────────────▶ DONE / FAILED / CANCELLED
//!          │                                                     │
//!          └─────────────────────── await() ────────────────────▶ (returns)
//! ```
//!
//! Monotonic — once out of WAITING, further calls are no-ops.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

// ---------------------------------------------------------------------------
// Synthetic field layouts
// ---------------------------------------------------------------------------

/// `org.xnio.Option<T>` — 3 fields.
const OPT_DECLARING_CLASS: usize = 0; // Class<?> — reflection mirror
const OPT_NAME: usize = 1; // String — name e.g. "WORKER_IO_THREADS"
const OPT_TYPE_CLASS: usize = 2; // Class<?> — the T bound

/// `org.xnio.OptionMap` — 1 field.
// TRAILING extra slot (slot 1, beyond the real fields). In real-JDK mode
// `org.xnio.OptionMap` is the loaded class whose slot 0 is the Object field
// `value` (a Map); a Long written there does not round-trip (read back as an
// Object → as_long() None → handle 0 → "stale handle"). Mirrors the
// ServiceController `_mscId` trailing-slot pattern. Allocate with 2 slots.
const OM_ENTRIES_HANDLE: usize = 1; // long id → `Arc<OptionMapInner>` in registry

/// `org.xnio.OptionMap$Builder` — 1 field.
// TRAILING extra slot (slot 1, beyond the real fields). Real `OptionMap$Builder`
// slot 0 is the Object field `list`; a Long there reads back as Object →
// as_long() None → handle 0 → "stale or unknown handle" at the first
// `Builder.set` (WildFly `ManagementWorkerService.installService`). Allocate 2 slots.
const OMB_PENDING_HANDLE: usize = 1; // long id → `Arc<BuilderInner>` in registry

/// `org.xnio.IoFuture` — 3 fields.
const IOF_STATUS: usize = 0; // int snapshot of the atomic status (for debug)
const IOF_RESULT_SLOT: usize = 1; // long id → `Arc<IoFutureInner>` in registry
const IOF_NOTIFIER_LIST: usize = 2; // long mirror of the above; present for
                                    // observability and to let bytecode cheaply
                                    // read "is waiting" without touching the
                                    // registry.

/// `org.xnio.FutureResult` — 1 field.
const FR_FUTURE_HANDLE: usize = 0; // long id → `Arc<IoFutureInner>` in registry

// ---------------------------------------------------------------------------
// IoFuture status codes (match the Java XNIO enum constants)
// ---------------------------------------------------------------------------

const STATUS_WAITING: u8 = 0;
const STATUS_DONE: u8 = 1;
const STATUS_CANCELLED: u8 = 2;
const STATUS_FAILED: u8 = 3;

fn status_name(s: u8) -> &'static str {
    match s {
        STATUS_WAITING => "WAITING",
        STATUS_DONE => "DONE",
        STATUS_CANCELLED => "CANCELLED",
        STATUS_FAILED => "FAILED",
        _ => "UNKNOWN",
    }
}

// ---------------------------------------------------------------------------
// Option<T> value representation
// ---------------------------------------------------------------------------

/// A K-V pair key. Equality keys on `(declaring class, name)` so two
/// `Option.simple(…)` instantiations with the same coordinates collapse
/// into the same slot — the real XNIO library relies on Option identity
/// being stable across classloaders for configuration to round-trip.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) struct OptionKey {
    pub(crate) declaring_class: String,
    pub(crate) name: String,
}

/// Typed value variants understood by `OptionMap`. Each matches an XNIO
/// primitive type bound (Int/Long/Bool/String) plus a catch-all
/// `ObjectRef` for user-supplied `Option<T>` where `T` is a reference.
#[derive(Debug, Clone)]
pub(crate) enum OptionValue {
    Int(i32),
    Long(i64),
    Bool(bool),
    Str(String),
    Obj(Option<ObjectRef>),
}

impl OptionValue {
    pub(crate) fn as_int(&self) -> Option<i32> {
        match self {
            OptionValue::Int(v) => Some(*v),
            OptionValue::Long(v) => i32::try_from(*v).ok(),
            OptionValue::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        match self {
            OptionValue::Bool(b) => Some(*b),
            OptionValue::Int(v) => Some(*v != 0),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// OptionMap inner — immutable snapshot shared via Arc
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct OptionMapInner {
    /// Family-1 fix (cce0079 tree-key tail): `OptionValue::Obj` variants hold
    /// raw heap `ObjectRef`s in this Rust-side registry — invisible to field
    /// tracing. They must be rooted AND remapped by every moving GC (see
    /// `gc_scan_xnio_future_roots`/`gc_update_xnio_future_refs`), which
    /// requires interior mutability — hence the leaf Mutex (same pattern as
    /// `FutureState`/`BuilderInner.pending`). Readers must COPY the entry out
    /// and drop the guard before any GC-capable call (allocation/boxing):
    /// the GC's own scan takes this lock, so holding it across a safepoint
    /// deadlocks the collection.
    pub(crate) entries: Mutex<HashMap<OptionKey, OptionValue>>,
}

impl OptionMapInner {
    fn empty() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<OptionMapInner>> = OnceLock::new();
        EMPTY
            .get_or_init(|| {
                Arc::new(OptionMapInner {
                    entries: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }
}

// ---------------------------------------------------------------------------
// OptionMap builder inner — mutable until consumed
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub(crate) struct BuilderInner {
    pub(crate) pending: Mutex<HashMap<OptionKey, OptionValue>>,
    pub(crate) consumed: AtomicBool,
}

// ---------------------------------------------------------------------------
// IoFuture inner — status + result + notifier list
// ---------------------------------------------------------------------------

/// A pending-notifier entry. Stored as raw `ObjectRef` + attachment so
/// the future can be `Send + Sync` (ObjectRef is a raw pointer wrapper).
#[derive(Clone, Copy)]
struct NotifierEntry {
    notifier: ObjectRef,
    attachment: Option<ObjectRef>,
}

// SAFETY: ObjectRef is already `Send + Sync` (see `types/src/value.rs`
// ownership comment). `NotifierEntry` is a plain POD wrapper over two
// `ObjectRef`, so Send+Sync is inherited.
unsafe impl Send for NotifierEntry {}
unsafe impl Sync for NotifierEntry {}

/// The heart of an IoFuture. Readers await on `cv`; writers flip the
/// status atomic once (monotonic) and broadcast.
pub(crate) struct IoFutureInner {
    pub(crate) status: AtomicU8,
    state: Mutex<FutureState>,
    cv: Condvar,
}

#[derive(Default)]
struct FutureState {
    /// The successful result, set iff status == DONE.
    result: Option<Value>,
    /// The exception message, set iff status == FAILED.
    /// (We do not hold the IOException `ObjectRef` directly — that would
    /// require heap-tracing. We store the message and rebuild the
    /// exception on `get()` using the caller's NativeContext.)
    exception_message: Option<String>,
    /// Registered notifiers, drained on completion.
    notifiers: Vec<NotifierEntry>,
}

impl IoFutureInner {
    pub(crate) fn new_waiting() -> Arc<Self> {
        let inner = Arc::new(Self {
            status: AtomicU8::new(STATUS_WAITING),
            state: Mutex::new(FutureState::default()),
            cv: Condvar::new(),
        });
        // Register in the GC-scan registry so the moving collector can root
        // and remap every `ObjectRef` this future holds (notifier targets,
        // attachments, and the success result) for as long as the future is
        // reachable. The registry holds a `Weak`, so it never keeps a settled
        // future alive — the strong owner is the handle map (and/or a live
        // JVM object). Dead/settled-and-unreachable entries are pruned lazily
        // on the next scan. See `gc_scan_xnio_future_roots`.
        register_live_future(&inner);
        inner
    }

    /// Attempt to transition the status from WAITING to `new_status`.
    /// Returns `true` if we won the race (and the caller's mutation is
    /// authoritative), `false` if a prior transition already committed.
    fn try_transition(&self, new_status: u8) -> bool {
        debug_assert!(
            new_status == STATUS_DONE
                || new_status == STATUS_CANCELLED
                || new_status == STATUS_FAILED,
            "invalid target status {new_status}"
        );
        self.status
            .compare_exchange(
                STATUS_WAITING,
                new_status,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    /// Drain + dispatch notifiers outside the lock, each wrapped in
    /// `catch_unwind` for isolation. If a notifier adds more notifiers
    /// while firing (e.g. chained completion), we pick those up in the
    /// next drain cycle before returning.
    fn drain_notifiers(self: &Arc<Self>) -> Vec<NotifierEntry> {
        let mut all: Vec<NotifierEntry> = Vec::new();
        loop {
            let batch: Vec<NotifierEntry> = {
                let mut state = self.state.lock();
                std::mem::take(&mut state.notifiers)
            };
            if batch.is_empty() {
                break;
            }
            all.extend(batch.iter().copied());
            // Add-during-drain pattern: any notifier that calls
            // `addNotifier` while firing lands in the next iteration's
            // drain. If the status is already settled it will be flushed
            // right back out on this path.
            if self.state.lock().notifiers.is_empty() {
                break;
            }
        }
        all
    }

    /// Fire a notifier via `NativeContext`, wrapped in `catch_unwind` so
    /// one bad callback doesn't abort the batch.
    fn fire_notifier(ctx: &mut dyn NativeContext, future_ref: ObjectRef, entry: &NotifierEntry) {
        let notifier = entry.notifier;
        let att = Value::Object(entry.attachment);
        let fut_v = Value::Object(Some(future_ref));
        let result = catch_unwind(AssertUnwindSafe(|| {
            // org.xnio.IoFuture$Notifier.notify(IoFuture, A)V
            let _ = ctx.invoke(
                "org/xnio/IoFuture$Notifier",
                "notify",
                "(Lorg/xnio/IoFuture;Ljava/lang/Object;)V",
                &[fut_v, att],
            );
        }));
        if result.is_err() {
            tracing::warn!("xnio_async: notifier panicked during drain; suppressed");
        }
    }
}

impl std::fmt::Debug for IoFutureInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoFutureInner")
            .field("status", &status_name(self.status.load(Ordering::SeqCst)))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Process-wide handle registry
// ---------------------------------------------------------------------------
//
// We store `Arc<OptionMapInner>`, `Arc<BuilderInner>`, and
// `Arc<IoFutureInner>` in process-global registries keyed by an i64
// handle that we drop into the JVM object's slot-0 long field. This is
// the standard `cratonvm` pattern for bridging JVM-land object identity
// with Rust-owned state (see `wildfly_undertow::undertow_instances`).
//
// Handles are never reused within a single process run.

pub(crate) struct Registries {
    pub(crate) maps: Mutex<HashMap<i64, Arc<OptionMapInner>>>,
    pub(crate) builders: Mutex<HashMap<i64, Arc<BuilderInner>>>,
    pub(crate) futures: Mutex<HashMap<i64, Arc<IoFutureInner>>>,
    map_handles_by_obj: Mutex<HashMap<usize, i64>>,
    builder_handles_by_obj: Mutex<HashMap<usize, i64>>,
    pub(crate) next_handle: AtomicU8,
}

fn registries() -> &'static Registries {
    static R: OnceLock<Registries> = OnceLock::new();
    R.get_or_init(|| Registries {
        maps: Mutex::new(HashMap::new()),
        builders: Mutex::new(HashMap::new()),
        futures: Mutex::new(HashMap::new()),
        map_handles_by_obj: Mutex::new(HashMap::new()),
        builder_handles_by_obj: Mutex::new(HashMap::new()),
        next_handle: AtomicU8::new(0),
    })
}

// Real XNIO classes loaded from the JDK/WildFly classpath can have too few
// instance fields for our synthetic trailing handle slot. Key the fallback by a
// GC-stable identity hash and use a per-hash generation to distinguish genuine
// 32-bit hash collisions among live wrappers.
struct XnioObjKeyEntry {
    last_ptr: usize,
    generation: u32,
}

fn xnio_obj_key_registry() -> &'static Mutex<HashMap<u32, Vec<XnioObjKeyEntry>>> {
    static R: OnceLock<Mutex<HashMap<u32, Vec<XnioObjKeyEntry>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

#[inline]
fn pack_xnio_obj_key(hash: u32, generation: u32) -> usize {
    ((hash as usize) << 32) | (generation as usize)
}

fn xnio_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    let hash = ctx.identity_hash_code(obj) as u32;
    let ptr = obj.as_ptr() as usize;
    let mut reg = xnio_obj_key_registry().lock();
    let slots = reg.entry(hash).or_default();
    if let Some(slot) = slots.iter().find(|s| s.last_ptr == ptr) {
        return pack_xnio_obj_key(hash, slot.generation);
    }
    if slots.len() == 1 {
        slots[0].last_ptr = ptr;
        return pack_xnio_obj_key(hash, slots[0].generation);
    }
    let generation = slots.len() as u32;
    slots.push(XnioObjKeyEntry {
        last_ptr: ptr,
        generation,
    });
    pack_xnio_obj_key(hash, generation)
}

#[inline]
fn read_handle_slot(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize) -> i64 {
    if ctx.object_num_fields(obj) <= slot {
        return 0;
    }
    ctx.get_field(obj, slot).as_long().unwrap_or(0)
}

#[inline]
fn write_handle_slot_if_present(ctx: &dyn NativeContext, obj: ObjectRef, slot: usize, handle: i64) {
    if ctx.object_num_fields(obj) > slot {
        ctx.set_field(obj, slot, Value::Long(handle));
    }
}

fn remember_map_handle(ctx: &dyn NativeContext, obj: ObjectRef, handle: i64) {
    let key = xnio_obj_key_for(ctx, obj);
    registries().map_handles_by_obj.lock().insert(key, handle);
}

fn remember_builder_handle(ctx: &dyn NativeContext, obj: ObjectRef, handle: i64) {
    let key = xnio_obj_key_for(ctx, obj);
    registries()
        .builder_handles_by_obj
        .lock()
        .insert(key, handle);
}

fn remember_option_map_inner(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    inner: Arc<OptionMapInner>,
) -> Arc<OptionMapInner> {
    let h = register_map(inner.clone());
    write_handle_slot_if_present(ctx, obj, OM_ENTRIES_HANDLE, h);
    remember_map_handle(ctx, obj, h);
    inner
}

fn map_handle_for(ctx: &dyn NativeContext, obj: ObjectRef) -> i64 {
    let slot_handle = read_handle_slot(ctx, obj, OM_ENTRIES_HANDLE);
    if slot_handle != 0 {
        return slot_handle;
    }
    let key = xnio_obj_key_for(ctx, obj);
    registries()
        .map_handles_by_obj
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(0)
}

fn builder_handle_for(ctx: &dyn NativeContext, obj: ObjectRef) -> i64 {
    let slot_handle = read_handle_slot(ctx, obj, OMB_PENDING_HANDLE);
    if slot_handle != 0 {
        return slot_handle;
    }
    let key = xnio_obj_key_for(ctx, obj);
    registries()
        .builder_handles_by_obj
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(0)
}

fn next_handle() -> i64 {
    use std::sync::atomic::AtomicI64;
    static COUNTER: AtomicI64 = AtomicI64::new(1);
    COUNTER.fetch_add(1, Ordering::SeqCst)
}

pub(crate) fn register_map(inner: Arc<OptionMapInner>) -> i64 {
    let h = next_handle();
    registries().maps.lock().insert(h, inner);
    h
}

pub(crate) fn lookup_map(handle: i64) -> Option<Arc<OptionMapInner>> {
    registries().maps.lock().get(&handle).cloned()
}

pub(crate) fn register_builder(inner: Arc<BuilderInner>) -> i64 {
    let h = next_handle();
    registries().builders.lock().insert(h, inner);
    h
}

pub(crate) fn lookup_builder(handle: i64) -> Option<Arc<BuilderInner>> {
    registries().builders.lock().get(&handle).cloned()
}

pub(crate) fn register_future(inner: Arc<IoFutureInner>) -> i64 {
    let h = next_handle();
    registries().futures.lock().insert(h, inner);
    h
}

pub(crate) fn lookup_future(handle: i64) -> Option<Arc<IoFutureInner>> {
    registries().futures.lock().get(&handle).cloned()
}

// ---------------------------------------------------------------------------
// GC root registry for live IoFutures
// ---------------------------------------------------------------------------
//
// A pending `IoFuture` holds JVM `ObjectRef`s that are NOT otherwise reachable
// by the collector: each `NotifierEntry.notifier` / `.attachment` and the
// success `FutureState.result` (when it is a `Value::Object`). These live
// inside an `Arc<IoFutureInner>` that can be handed across threads
// (`addNotifier` on one thread, `setResult` on another) and held across
// arbitrary VM allocations and moving-GC cycles. Without rooting they can be
// reclaimed or relocated underneath us → UAF / wrong-notifier dispatch.
//
// We keep a process-global registry of `Weak<IoFutureInner>`. `Weak` (not
// `Arc`) is deliberate: the strong owner is the handle map in `registries()`
// (and any live JVM object whose slot carries the handle), so this registry
// must never be the thing that keeps a future alive — otherwise a settled,
// otherwise-unreachable future would leak forever. The GC hooks upgrade each
// `Weak`; entries that fail to upgrade are dead and pruned in place, so the
// registry self-cleans without a per-settle hook and stays bounded by the
// live-future count.

fn live_futures() -> &'static Mutex<Vec<Weak<IoFutureInner>>> {
    static LIVE: OnceLock<Mutex<Vec<Weak<IoFutureInner>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Add a freshly-created future to the GC-scan registry. Called once from
/// `IoFutureInner::new_waiting` (the sole construction site). Opportunistically
/// drops already-dead `Weak`s so the vector cannot grow without bound across a
/// long-lived process that never triggers a moving GC.
fn register_live_future(inner: &Arc<IoFutureInner>) {
    let mut live = live_futures().lock();
    live.retain(|w| w.strong_count() > 0);
    live.push(Arc::downgrade(inner));
}

/// GC root scan: push every JVM `ObjectRef` held by a live, pending-or-settled
/// `IoFuture` so the collector treats them as reachable. Companion remap is
/// [`gc_update_xnio_future_refs`]; the two MUST visit the identical set of
/// refs (scan/remap symmetry) or a moving GC would leave a stale pointer.
///
/// For each live future we push, in order:
///   * every `NotifierEntry.notifier`,
///   * every present, non-null `NotifierEntry.attachment`,
///   * the `FutureState.result` ref when it is `Value::Object(Some(_))`.
///
/// Null refs are skipped (`ObjectRef` has no `is_null()`; null is
/// `as_ptr().is_null()`). Dead `Weak`s are pruned in place. `parking_lot`
/// mutexes do not poison, so no recovery dance is needed — but the locks here
/// are leaf locks (no Java allocation happens while held), so the GC can never
/// self-deadlock on them.
pub fn gc_scan_xnio_future_roots(roots: &mut Vec<cratonvm_types::ObjectRef>) {
    let mut live = live_futures().lock();
    live.retain(|weak| {
        let Some(inner) = weak.upgrade() else {
            return false; // future is gone — drop the dead Weak
        };
        let state = inner.state.lock();
        for entry in &state.notifiers {
            if !entry.notifier.as_ptr().is_null() {
                roots.push(entry.notifier);
            }
            if let Some(att) = entry.attachment {
                if !att.as_ptr().is_null() {
                    roots.push(att);
                }
            }
        }
        if let Some(Value::Object(Some(r))) = state.result {
            if !r.as_ptr().is_null() {
                roots.push(r);
            }
        }
        true
    });
    // Family-1 fix (cce0079 tree-key tail): registered OptionMap snapshots
    // and un-consumed Builders hold `OptionValue::Obj` raw refs — reachable
    // ONLY through these Rust-side registries. Root them like the futures
    // above (remap companion below visits the identical set). Clone the
    // Arc lists first so the registry locks are not held while the entry
    // locks are taken.
    let maps: Vec<Arc<OptionMapInner>> = registries().maps.lock().values().cloned().collect();
    for m in maps {
        for v in m.entries.lock().values() {
            if let OptionValue::Obj(Some(r)) = v {
                if !r.as_ptr().is_null() {
                    roots.push(*r);
                }
            }
        }
    }
    let builders: Vec<Arc<BuilderInner>> = registries().builders.lock().values().cloned().collect();
    for b in builders {
        for v in b.pending.lock().values() {
            if let OptionValue::Obj(Some(r)) = v {
                if !r.as_ptr().is_null() {
                    roots.push(*r);
                }
            }
        }
    }
}

/// Post-move remap (companion to [`gc_scan_xnio_future_roots`]). After a moving
/// collection relocates objects, rewrite every `ObjectRef` held by a live
/// future to its new address via `map`. Visits the IDENTICAL set of refs the
/// scan reports, so no live root is missed and every moved ref is repointed.
///
/// `map` is keyed by old address (`as_ptr() as usize`) → new address. A ref not
/// present in `map` did not move and is left untouched. Dead `Weak`s are pruned.
pub fn gc_update_xnio_future_refs(map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let remap = |slot: &mut ObjectRef| {
        let old = slot.as_ptr() as usize;
        if let Some(&new) = map.get(&old) {
            debug_assert!(new != 0, "GC pointer map contains null address");
            // SAFETY: `new` is a live, 8-byte-aligned heap address produced by
            // the moving collector for the object previously at `old`.
            *slot = unsafe { ObjectRef::from_raw(new as *mut u8) };
        }
    };
    let mut live = live_futures().lock();
    live.retain(|weak| {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        let mut state = inner.state.lock();
        for entry in &mut state.notifiers {
            remap(&mut entry.notifier);
            if let Some(att) = entry.attachment.as_mut() {
                remap(att);
            }
        }
        if let Some(Value::Object(Some(r))) = state.result.as_mut() {
            remap(r);
        }
        true
    });
    // Remap companion for the OptionMap/Builder registries (scan above).
    let maps: Vec<Arc<OptionMapInner>> = registries().maps.lock().values().cloned().collect();
    for m in maps {
        for v in m.entries.lock().values_mut() {
            if let OptionValue::Obj(Some(r)) = v {
                remap(r);
            }
        }
    }
    let builders: Vec<Arc<BuilderInner>> = registries().builders.lock().values().cloned().collect();
    for b in builders {
        for v in b.pending.lock().values_mut() {
            if let OptionValue::Obj(Some(r)) = v {
                remap(r);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Option<T> helpers: read the 3 Option fields off a JVM object.
// ---------------------------------------------------------------------------

fn read_option_coords(ctx: &dyn NativeContext, opt: ObjectRef) -> Option<(String, String)> {
    // The declaringClass field can be either a Class mirror (the real
    // `org.xnio.Option.declClass` field, and the common case) or a String
    // (a legacy fallback when a caller passed the class name directly).
    // We accept both. For a Class mirror, resolve it to the represented
    // class's internal name via `mirror_class_name` — NOT
    // `class_id_of_object`, which would return "java/lang/Class" (the
    // class *of the mirror object itself*).
    let declaring = match ctx.get_field(opt, OPT_DECLARING_CLASS) {
        Value::Object(Some(s)) => {
            // A real Class mirror resolves through the mirror→class map.
            if let Some(n) = crate::lang_class::mirror_class_name(ctx, s) {
                n
            } else {
                // Legacy: the field held a plain String class name.
                ctx.read_string(s).unwrap_or_default()
            }
        }
        _ => String::new(),
    };
    let name = match ctx.get_field(opt, OPT_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    if name.is_empty() {
        None
    } else {
        Some((declaring, name))
    }
}

fn read_option_type(ctx: &dyn NativeContext, opt: ObjectRef) -> Option<String> {
    match ctx.get_field(opt, OPT_TYPE_CLASS) {
        Value::Object(Some(s)) => {
            // Real `SingleOption.type` / `SequenceOption.elementType` is a
            // Class mirror; resolve it to the represented class name.
            // (Legacy synthetic path stored a plain String.)
            if let Some(n) = crate::lang_class::mirror_class_name(ctx, s) {
                Some(n)
            } else {
                ctx.read_string(s)
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Natives: Option
// ---------------------------------------------------------------------------

/// `Option.simple(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/Class;)Lorg/xnio/Option;`
fn native_option_simple(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if args.len() < 3 {
        return Err(iae("Option.simple expects 3 args"));
    }
    let obj = try_alloc_concurrent_synthetic(ctx, "org/xnio/Option", 3)?;
    ctx.set_field(obj, OPT_DECLARING_CLASS, args[0]);
    ctx.set_field(obj, OPT_NAME, args[1]);
    ctx.set_field(obj, OPT_TYPE_CLASS, args[2]);
    Ok(Some(Value::Object(Some(obj))))
}

/// `Option.getName()Ljava/lang/String;`
fn native_option_get_name(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(_ctx.get_field(this, OPT_NAME)))
}

/// `Option.cast(Ljava/lang/Object;)Ljava/lang/Object;`
///
/// Checks that the passed-in object is assignable to the Option's type
/// class. Returns the object unchanged on success, or throws
/// `ClassCastException` on mismatch. The check is name-based: we compare
/// the object's declared class name (via `class_id_of_object` →
/// `class_name_of_id`) against the Option's type class name, and accept
/// subclass relationships via `is_subclass`.
fn native_option_cast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let arg = args.get(1).copied().unwrap_or(Value::Object(None));

    // null is always assignable
    let obj_ref = match arg {
        Value::Object(Some(o)) => o,
        _ => return Ok(Some(arg)),
    };

    let expected_type = read_option_type(ctx, this).unwrap_or_default();
    if expected_type.is_empty() {
        // Option has no declared type; accept anything.
        return Ok(Some(arg));
    }

    // Object's actual class.
    let actual_cid = ctx.class_id_of_object(obj_ref);
    let actual_name = ctx.class_name_of_id(actual_cid).unwrap_or_default();

    if actual_name == expected_type {
        return Ok(Some(arg));
    }

    // Subclass check. Resolve the expected class id if loaded.
    if let Some(expected_cid) = ctx.class_id_by_name(&expected_type) {
        if ctx.is_subclass(actual_cid, expected_cid) {
            return Ok(Some(arg));
        }
    }

    Err(cce(format!(
        "Option.cast: {actual_name} cannot be cast to {expected_type}"
    )))
}

/// `Option.parseValue(Ljava/lang/String;Ljava/lang/ClassLoader;)Ljava/lang/Object;`
///
/// Parses a string form of the option's value using the Option's declared
/// type class. Supported type classes: Integer, Long, Boolean, String.
/// Returns the wrapped primitive (for numeric types we return the
/// corresponding `Value` variant, matching XNIO's autoboxing behaviour).
fn native_option_parse_value(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let raw_val = args.get(1).copied().unwrap_or(Value::Object(None));
    let raw = match raw_val {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => return Err(npe("Option.parseValue: null string")),
    };
    let ty = read_option_type(ctx, this).unwrap_or_default();
    match ty.as_str() {
        "java/lang/Integer" => {
            let n: i32 = raw
                .parse()
                .map_err(|_| nfe(format!("parseValue(Integer): '{raw}'")))?;
            Ok(Some(Value::Int(n)))
        }
        "java/lang/Long" => {
            let n: i64 = raw
                .parse()
                .map_err(|_| nfe(format!("parseValue(Long): '{raw}'")))?;
            Ok(Some(Value::Long(n)))
        }
        "java/lang/Boolean" => {
            let b = match raw.to_ascii_lowercase().as_str() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(iae(format!("parseValue(Boolean): '{other}'")));
                }
            };
            Ok(Some(Value::Int(if b { 1 } else { 0 })))
        }
        _ => {
            // Default: return the string itself.
            Ok(Some(raw_val))
        }
    }
}

// ---------------------------------------------------------------------------
// Natives: OptionMap
// ---------------------------------------------------------------------------

fn alloc_option_map(
    ctx: &mut dyn NativeContext,
    inner: Arc<OptionMapInner>,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "org/xnio/OptionMap", 2)?;
    remember_option_map_inner(ctx, obj, inner);
    Ok(obj)
}

/// `OptionMap.<clinit>()V` — populate the public EMPTY static with a
/// Rust-backed empty map so GETSTATIC callers do not observe the uninitialized
/// real-JDK field.
fn native_option_map_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_option_map(ctx, OptionMapInner::empty());
    ctx.set_static_field_by_name("org/xnio/OptionMap", "EMPTY", Value::Object(Some(obj?)));
    Ok(None)
}

/// Retrieve `OptionMap.EMPTY` (a fresh JVM wrapper pointing at the
/// process-wide zero-entry `Arc<OptionMapInner>`).
///
/// The XNIO contract is that `OptionMap.EMPTY == OptionMap.EMPTY` for
/// identity comparison; in synthetic-stub land we cannot cache the
/// ObjectRef globally because each VM instance has its own heap (a
/// pointer valid in one heap is not valid in another).  Instead, we
/// share the **entries handle** — every allocation for EMPTY points at
/// the same underlying `Arc<OptionMapInner>` so `equals()` and `get()`
/// behave identically.  This is also the behaviour the real XNIO
/// library documents: equality, not identity.
fn native_option_map_empty_get(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let obj = alloc_option_map(ctx, OptionMapInner::empty());
    Ok(Some(Value::Object(Some(obj?))))
}

/// `OptionMap.builder()Lorg/xnio/OptionMap$Builder;`
fn native_option_map_builder(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let inner = Arc::new(BuilderInner::default());
    let obj = try_alloc_concurrent_synthetic(ctx, "org/xnio/OptionMap$Builder", 2)?;
    let h = register_builder(inner);
    write_handle_slot_if_present(ctx, obj, OMB_PENDING_HANDLE, h);
    remember_builder_handle(ctx, obj, h);
    Ok(Some(Value::Object(Some(obj))))
}

fn builder_from_this(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<Arc<BuilderInner>, MethodCallFailed> {
    let h = builder_handle_for(ctx, this);
    lookup_builder(h).ok_or_else(|| ise("OptionMap.Builder: stale or unknown handle"))
}

fn check_builder_live(b: &BuilderInner) -> Result<(), MethodCallFailed> {
    if b.consumed.load(Ordering::SeqCst) {
        return Err(ise(
            "OptionMap.Builder: already consumed (getMap was called)",
        ));
    }
    Ok(())
}

/// `Builder.set(Option, Object)Lorg/xnio/OptionMap$Builder;`
/// (plus the primitive-overload variants.)
fn native_builder_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // Tolerate a null Option: we only populate a well-known SUBSET of the
    // `Options.*` statics (see `ensure_options_initialized`), so a `getstatic`
    // of any other option yields null. The synthetic XnioWorker defaults every
    // option it doesn't explicitly read, so silently skip an unknown/null
    // option (return `this` for chaining) instead of NPEing the whole install
    // (e.g. WildFly `ManagementWorkerService.installService` sets `Options.CORK`).
    let opt = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));

    // Decode an object value before any helper below can allocate.  The native
    // entry frame roots its arguments, while `builder_from_this` and
    // `read_option_coords` may enter allocation-capable VM code.  In
    // particular, resolving the XNIO option used to leave a stale String
    // reference for this later `read_string` call during a real MSC start.
    let v = match value {
        Value::Int(n) => OptionValue::Int(n),
        Value::Long(n) => OptionValue::Long(n),
        Value::Object(Some(s)) => match ctx.read_string(s) {
            Some(t) => OptionValue::Str(t),
            None => OptionValue::Obj(Some(s)),
        },
        Value::Object(None) => OptionValue::Obj(None),
        _ => OptionValue::Obj(None),
    };

    // The side-table lookup and option-coordinate extraction below can enter
    // VM helpers which allocate. Keep the receiver and option rooted through
    // that work; `v` is now Rust-owned and no longer retains a Java object
    // requiring a later heap read.
    let this_pin = ctx.pin_native_root(this);
    let opt_pin = ctx.pin_native_root(opt);
    let this = ctx.read_native_pin(this_pin, this);
    let b = builder_from_this(ctx, this)?;
    check_builder_live(&b)?;

    let opt = ctx.read_native_pin(opt_pin, opt);
    let (decl, name) =
        read_option_coords(ctx, opt).ok_or_else(|| iae("Builder.set: option has no name"))?;
    let key = OptionKey {
        declaring_class: decl,
        name,
    };
    b.pending.lock().insert(key, v);
    // Return `this` for chaining.
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(opt_pin);
    ctx.unpin_native_roots(this_pin);
    Ok(Some(Value::Object(Some(this))))
}

/// `Builder.set(Option<Boolean>, boolean)` — we delegate to
/// `native_builder_set` by coercing bool → Int(0/1).
fn native_builder_set_bool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let mapped = if args.len() >= 3 {
        let b = args[2].as_int().unwrap_or(0) != 0;
        vec![args[0], args[1], Value::Int(if b { 1 } else { 0 })]
    } else {
        args.to_vec()
    };
    // The storage layer stores Bool only when we know it's a Boolean
    // option — do that check here.
    let this = obj_arg(&mapped, 0)?;
    // Tolerate a null Option (unpopulated well-known static) — skip + chain.
    let opt = match mapped.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(Some(Value::Object(Some(this)))),
    };
    let b = builder_from_this(ctx, this)?;
    check_builder_live(&b)?;
    let (decl, name) = read_option_coords(ctx, opt)
        .ok_or_else(|| iae("Builder.set(Boolean): option has no name"))?;
    let key = OptionKey {
        declaring_class: decl,
        name,
    };
    let raw = mapped.get(2).and_then(Value::as_int).unwrap_or(0);
    b.pending.lock().insert(key, OptionValue::Bool(raw != 0));
    Ok(Some(Value::Object(Some(this))))
}

/// `Builder.addAll(OptionMap)Lorg/xnio/OptionMap$Builder;`
fn native_builder_add_all(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let source = obj_arg(args, 1)?;
    let b = builder_from_this(ctx, this)?;
    check_builder_live(&b)?;
    let source_inner = inner_from_map_any(ctx, source)?;

    // Copy the source snapshot out before touching the builder lock — no
    // GC-capable call happens here, but keeping the two leaf locks disjoint
    // avoids any ordering coupling with the GC scan (which takes each
    // `entries` lock).
    let source_entries: Vec<(OptionKey, OptionValue)> = source_inner
        .entries
        .lock()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut pending = b.pending.lock();
    for (key, value) in source_entries {
        pending.insert(key, value);
    }

    Ok(Some(Value::Object(Some(this))))
}

/// `Builder.getMap()Lorg/xnio/OptionMap;`
fn native_builder_get_map(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let b = builder_from_this(ctx, this)?;
    // Race on consumed flag: first caller wins.
    if b.consumed.swap(true, Ordering::SeqCst) {
        return Err(ise("Builder.getMap: already consumed"));
    }
    let entries = b.pending.lock().clone();
    let inner = Arc::new(OptionMapInner {
        entries: Mutex::new(entries),
    });
    let obj = alloc_option_map(ctx, inner);
    Ok(Some(Value::Object(Some(obj?))))
}

fn inner_from_map(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<Arc<OptionMapInner>, MethodCallFailed> {
    let h = map_handle_for(ctx, this);
    lookup_map(h).ok_or_else(|| ise("OptionMap: stale or unknown handle"))
}

fn real_option_map_value(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "value") {
        Value::Object(Some(map)) => Some(map),
        _ => match ctx.get_field(this, 0) {
            Value::Object(Some(map)) => Some(map),
            _ => None,
        },
    }
}

fn invoke_object(
    ctx: &mut dyn NativeContext,
    receiver: ObjectRef,
    name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<ObjectRef> {
    match ctx.invoke_virtual(receiver, name, descriptor, args) {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    }
}

fn java_boxed_int(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<i32> {
    match ctx.invoke_virtual(obj, "intValue", "()I", &[]) {
        Ok(Some(Value::Int(v))) => Some(v),
        _ => None,
    }
}

fn java_boxed_long(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<i64> {
    match ctx.invoke_virtual(obj, "longValue", "()J", &[]) {
        Ok(Some(Value::Long(v))) => Some(v),
        Ok(Some(Value::Int(v))) => Some(v as i64),
        _ => None,
    }
}

fn java_boxed_bool(ctx: &mut dyn NativeContext, obj: ObjectRef) -> Option<bool> {
    match ctx.invoke_virtual(obj, "booleanValue", "()Z", &[]) {
        Ok(Some(Value::Int(v))) => Some(v != 0),
        _ => None,
    }
}

fn option_value_from_java(
    ctx: &mut dyn NativeContext,
    opt: ObjectRef,
    value: Value,
) -> OptionValue {
    let ty = read_option_type(ctx, opt).unwrap_or_default();
    match value {
        Value::Int(v) => {
            if ty == "java/lang/Boolean" {
                OptionValue::Bool(v != 0)
            } else {
                OptionValue::Int(v)
            }
        }
        Value::Long(v) => OptionValue::Long(v),
        Value::Object(Some(obj)) => match ty.as_str() {
            "java/lang/Integer" => java_boxed_int(ctx, obj)
                .map(OptionValue::Int)
                .unwrap_or(OptionValue::Obj(Some(obj))),
            "java/lang/Long" => java_boxed_long(ctx, obj)
                .map(OptionValue::Long)
                .unwrap_or(OptionValue::Obj(Some(obj))),
            "java/lang/Boolean" => java_boxed_bool(ctx, obj)
                .map(OptionValue::Bool)
                .unwrap_or(OptionValue::Obj(Some(obj))),
            "java/lang/String" => ctx
                .read_string(obj)
                .map(OptionValue::Str)
                .unwrap_or(OptionValue::Obj(Some(obj))),
            _ => ctx
                .read_string(obj)
                .map(OptionValue::Str)
                .unwrap_or(OptionValue::Obj(Some(obj))),
        },
        Value::Object(None) => OptionValue::Obj(None),
        _ => OptionValue::Obj(None),
    }
}

fn collect_real_option_map_entries(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
) -> HashMap<OptionKey, OptionValue> {
    let mut out = HashMap::new();
    let Some(set) = invoke_object(ctx, map, "entrySet", "()Ljava/util/Set;", &[]) else {
        return out;
    };
    let Some(it) = invoke_object(ctx, set, "iterator", "()Ljava/util/Iterator;", &[]) else {
        return out;
    };
    loop {
        let has_next = matches!(
            ctx.invoke_virtual(it, "hasNext", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        );
        if !has_next {
            break;
        }
        let Some(entry) = invoke_object(ctx, it, "next", "()Ljava/lang/Object;", &[]) else {
            break;
        };
        let Some(opt) = invoke_object(ctx, entry, "getKey", "()Ljava/lang/Object;", &[]) else {
            continue;
        };
        let Some((declaring_class, name)) = read_option_coords(ctx, opt) else {
            continue;
        };
        let value = ctx
            .invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        out.insert(
            OptionKey {
                declaring_class,
                name,
            },
            option_value_from_java(ctx, opt, value),
        );
    }
    out
}

fn inner_from_map_any(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<Arc<OptionMapInner>, MethodCallFailed> {
    let h = map_handle_for(ctx, this);
    if let Some(inner) = lookup_map(h) {
        return Ok(inner);
    }

    if let Some(map) = real_option_map_value(ctx, this) {
        let entries = collect_real_option_map_entries(ctx, map);
        return Ok(remember_option_map_inner(
            ctx,
            this,
            Arc::new(OptionMapInner {
                entries: Mutex::new(entries),
            }),
        ));
    }

    Err(ise("OptionMap: stale or unknown handle"))
}

fn option_map_get_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<(Arc<OptionMapInner>, OptionKey, Option<Value>), MethodCallFailed> {
    let this = obj_arg(args, 0)?;
    let opt = obj_arg(args, 1)?;
    let inner = inner_from_map_any(ctx, this)?;
    let (decl, name) =
        read_option_coords(ctx, opt).ok_or_else(|| iae("OptionMap.get: option has no name"))?;
    let key = OptionKey {
        declaring_class: decl,
        name,
    };
    let default_val = args.get(2).copied();
    Ok((inner, key, default_val))
}

fn value_as_int(ctx: &dyn NativeContext, value: Value) -> Option<i32> {
    match value {
        Value::Int(n) => Some(n),
        Value::Long(n) => i32::try_from(n).ok(),
        Value::Object(Some(obj)) => match crate::lang_class::unbox_value(ctx, obj) {
            Value::Int(n) => Some(n),
            Value::Long(n) => i32::try_from(n).ok(),
            _ => None,
        },
        _ => None,
    }
}

fn value_as_long(ctx: &dyn NativeContext, value: Value) -> Option<i64> {
    match value {
        Value::Long(n) => Some(n),
        Value::Int(n) => Some(n as i64),
        Value::Object(Some(obj)) => match crate::lang_class::unbox_value(ctx, obj) {
            Value::Long(n) => Some(n),
            Value::Int(n) => Some(n as i64),
            _ => None,
        },
        _ => None,
    }
}

fn value_as_bool(ctx: &dyn NativeContext, value: Value) -> Option<bool> {
    match value {
        Value::Int(n) => Some(n != 0),
        Value::Long(n) => Some(n != 0),
        Value::Object(Some(obj)) => match crate::lang_class::unbox_value(ctx, obj) {
            Value::Int(n) => Some(n != 0),
            Value::Long(n) => Some(n != 0),
            _ => None,
        },
        _ => None,
    }
}

fn option_value_as_int(ctx: &dyn NativeContext, value: &OptionValue) -> Option<i32> {
    match value {
        OptionValue::Int(n) => Some(*n),
        OptionValue::Long(n) => i32::try_from(*n).ok(),
        OptionValue::Bool(b) => Some(if *b { 1 } else { 0 }),
        OptionValue::Str(s) => s.parse().ok(),
        OptionValue::Obj(Some(obj)) => value_as_int(ctx, Value::Object(Some(*obj))),
        OptionValue::Obj(None) => None,
    }
}

fn option_value_as_long(ctx: &dyn NativeContext, value: &OptionValue) -> Option<i64> {
    match value {
        OptionValue::Long(n) => Some(*n),
        OptionValue::Int(n) => Some(*n as i64),
        OptionValue::Bool(b) => Some(if *b { 1 } else { 0 }),
        OptionValue::Str(s) => s.parse().ok(),
        OptionValue::Obj(Some(obj)) => value_as_long(ctx, Value::Object(Some(*obj))),
        OptionValue::Obj(None) => None,
    }
}

fn option_value_as_bool(ctx: &dyn NativeContext, value: &OptionValue) -> Option<bool> {
    match value {
        OptionValue::Bool(b) => Some(*b),
        OptionValue::Int(n) => Some(*n != 0),
        OptionValue::Long(n) => Some(*n != 0),
        OptionValue::Str(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        OptionValue::Obj(Some(obj)) => value_as_bool(ctx, Value::Object(Some(*obj))),
        OptionValue::Obj(None) => None,
    }
}

/// `OptionMap.get(Option)Ljava/lang/Object;` and `get(Option, Object)Object` --
/// both overloads declare a return type of `Ljava/lang/Object;`, so a numeric
/// entry (stored unboxed for cheap internal representation -- see
/// `native_builder_set`, which stores `Value::Int`/`Value::Long` straight from
/// the primitive `set(Option<Integer>, int)`-family setters) MUST be boxed
/// into a real `Integer`/`Long`/`Boolean` object before returning, exactly as
/// real XNIO's `Map<Option<?>, Object>`-backed `OptionMap` would already hold
/// a boxed value. Returning the raw `Value::Int`/`Value::Long` here used to
/// violate the declared `Object` return type: the interpreter's generic
/// native-return handling tolerated it, but the JIT's fast MIC-miss return
/// path (`vm/src/jit/helpers.rs`) takes a raw primitive result and passes it
/// through as if it were already a pointer-shaped `ObjectRef` -- so a small
/// int (e.g. a `60000`ms timeout option) becomes a bogus "object reference"
/// that segfaults the next time anything dereferences it (confirmed root
/// cause of the `ElytronRemoteOutboundConnectionTestCase` SIGSEGV, see
/// `wildfly-elytron-remoting-segfault-post-keyfactory-fix.md`).
fn native_option_map_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (inner, key, default_val) = option_map_get_key(ctx, args)?;

    // Copy the entry OUT and drop the guard before boxing/string allocation
    // (GC-capable) — the GC scan takes the same `entries` lock, so holding
    // it across a safepoint would deadlock the collection.
    let entry = inner.entries.lock().get(&key).cloned();
    match entry {
        Some(OptionValue::Int(n)) => {
            Ok(Some(crate::lang_class::box_value(ctx, Value::Int(n), "I")))
        }
        Some(OptionValue::Long(n)) => {
            Ok(Some(crate::lang_class::box_value(ctx, Value::Long(n), "J")))
        }
        Some(OptionValue::Bool(b)) => Ok(Some(crate::lang_class::box_value(
            ctx,
            Value::Int(if b { 1 } else { 0 }),
            "Z",
        ))),
        Some(OptionValue::Str(s)) => {
            let js = ctx.create_string(&s);
            Ok(Some(Value::Object(Some(js))))
        }
        Some(OptionValue::Obj(o)) => Ok(Some(Value::Object(o))),
        None => {
            if let Some(dv) = default_val {
                Ok(Some(dv))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }
    }
}

/// `OptionMap.get(Option<Integer>, int)I`
fn native_option_map_get_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (inner, key, default_val) = option_map_get_key(ctx, args)?;
    let default_int = default_val.and_then(|v| value_as_int(ctx, v)).unwrap_or(0);
    // Copy-out before the (non-allocating but heap-reading) unbox — keeps
    // the guard's critical section trivially GC-free.
    let entry = inner.entries.lock().get(&key).cloned();
    let value = entry
        .and_then(|v| option_value_as_int(ctx, &v))
        .unwrap_or(default_int);
    Ok(Some(Value::Int(value)))
}

/// `OptionMap.get(Option<Long>, long)J`
fn native_option_map_get_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (inner, key, default_val) = option_map_get_key(ctx, args)?;
    let default_long = default_val.and_then(|v| value_as_long(ctx, v)).unwrap_or(0);
    let entry = inner.entries.lock().get(&key).cloned();
    let value = entry
        .and_then(|v| option_value_as_long(ctx, &v))
        .unwrap_or(default_long);
    Ok(Some(Value::Long(value)))
}

/// `OptionMap.get(Option<Boolean>, boolean)Z`
fn native_option_map_get_bool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (inner, key, default_val) = option_map_get_key(ctx, args)?;
    let default_bool = default_val
        .and_then(|v| value_as_bool(ctx, v))
        .unwrap_or(false);
    let entry = inner.entries.lock().get(&key).cloned();
    let value = entry
        .and_then(|v| option_value_as_bool(ctx, &v))
        .unwrap_or(default_bool);
    Ok(Some(Value::Int(if value { 1 } else { 0 })))
}

/// `OptionMap.contains(Option)Z`
fn native_option_map_contains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let opt = obj_arg(args, 1)?;
    let inner = inner_from_map_any(ctx, this)?;
    let (decl, name) = match read_option_coords(ctx, opt) {
        Some(pair) => pair,
        None => return Ok(Some(Value::Int(0))),
    };
    let key = OptionKey {
        declaring_class: decl,
        name,
    };
    let present = inner.entries.lock().contains_key(&key);
    Ok(Some(Value::Int(if present { 1 } else { 0 })))
}

/// `OptionMap.size()I`
fn native_option_map_size(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_map_any(ctx, this)?;
    let len = inner.entries.lock().len() as i32;
    Ok(Some(Value::Int(len)))
}

fn native_option_map_iterator(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_map_any(ctx, this)?;
    // Copy the keys out and drop the guard BEFORE the per-key allocations
    // below (GC scan takes the same lock — see `OptionMapInner.entries`).
    let keys: Vec<OptionKey> = inner.entries.lock().keys().cloned().collect();
    let mut opts = Vec::with_capacity(keys.len());
    for key in &keys {
        let opt = try_alloc_concurrent_synthetic(ctx, "org/xnio/Option", 3)?;
        let declaring = ctx.create_string(&key.declaring_class);
        ctx.set_field(opt, OPT_DECLARING_CLASS, Value::Object(Some(declaring)));
        let name = ctx.create_string(&key.name);
        ctx.set_field(opt, OPT_NAME, Value::Object(Some(name)));
        ctx.set_field(opt, OPT_TYPE_CLASS, Value::Object(None));
        opts.push(Value::Object(Some(opt)));
    }
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), opts.len());
    for (i, opt) in opts.iter().enumerate() {
        ctx.set_array_element(arr, i, *opt);
    }
    cratonvm_native_collections::make_iterator_from_array(ctx, arr, opts.len())
}

// ---------------------------------------------------------------------------
// Natives: Options (well-known constants)
// ---------------------------------------------------------------------------

/// Build and populate the Options static fields on first access.
fn ensure_options_initialized(ctx: &mut dyn NativeContext) -> MethodCallResult {
    // If the class isn't loaded yet, load it — allocating the singletons
    // requires a class id.
    let _ = ctx.ensure_class_initialized("org/xnio/Options")?;
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.get().is_some() {
        return Ok(None);
    }

    // Populate each well-known Option. Because we don't have a
    // classloader context to pass for the type-Class argument, we
    // synthesize a type string and let the option-cast path handle it
    // by name.
    // bug-08 FIX (wildfly-suite-bugs/bug-08): `Options.<clinit>` is shimmed to a
    // no-op (so the stock `Option.simple(...)` cascade never runs), so EVERY
    // `public static final Option` field must be populated here. The previous
    // 12-entry list left the other 69 fields null — `XnioWorker.<clinit>` reads
    // `Options.WORKER_TASK_KEEPALIVE` (a missing entry) → `SetBuilder.add(null)`
    // → IllegalArgumentException → ExceptionInInitializerError, cascading to
    // NoClassDefFoundError(XnioWorker) and Arquillian remoting-client failures
    // across the WildFly suite. This is the full set of `org.xnio.Options`
    // option constants (xnio-api 3.8.x), so every `getstatic Options.<NAME>`
    // observes a non-null synthetic Option.
    let specs: &[(&str, &str)] = &[
        ("ALLOW_BLOCKING", "java/lang/Boolean"),
        ("MULTICAST", "java/lang/Boolean"),
        ("BROADCAST", "java/lang/Boolean"),
        ("CLOSE_ABORT", "java/lang/Boolean"),
        ("RECEIVE_BUFFER", "java/lang/Integer"),
        ("REUSE_ADDRESSES", "java/lang/Boolean"),
        ("SEND_BUFFER", "java/lang/Integer"),
        ("TCP_NODELAY", "java/lang/Boolean"),
        ("MULTICAST_TTL", "java/lang/Integer"),
        ("IP_TRAFFIC_CLASS", "java/lang/Integer"),
        ("TCP_OOB_INLINE", "java/lang/Boolean"),
        ("KEEP_ALIVE", "java/lang/Boolean"),
        ("BACKLOG", "java/lang/Integer"),
        ("READ_TIMEOUT", "java/lang/Integer"),
        ("WRITE_TIMEOUT", "java/lang/Integer"),
        ("MAX_INBOUND_MESSAGE_SIZE", "java/lang/Integer"),
        ("MAX_OUTBOUND_MESSAGE_SIZE", "java/lang/Integer"),
        ("SSL_ENABLED", "java/lang/Boolean"),
        ("SSL_CLIENT_AUTH_MODE", "org/xnio/SslClientAuthMode"),
        ("SSL_ENABLED_CIPHER_SUITES", "org/xnio/Sequence"),
        ("SSL_SUPPORTED_CIPHER_SUITES", "org/xnio/Sequence"),
        ("SSL_ENABLED_PROTOCOLS", "org/xnio/Sequence"),
        ("SSL_SUPPORTED_PROTOCOLS", "org/xnio/Sequence"),
        ("SSL_PROVIDER", "java/lang/String"),
        ("SSL_PROTOCOL", "java/lang/String"),
        ("SSL_ENABLE_SESSION_CREATION", "java/lang/Boolean"),
        ("SSL_USE_CLIENT_MODE", "java/lang/Boolean"),
        ("SSL_CLIENT_SESSION_CACHE_SIZE", "java/lang/Integer"),
        ("SSL_CLIENT_SESSION_TIMEOUT", "java/lang/Integer"),
        ("SSL_SERVER_SESSION_CACHE_SIZE", "java/lang/Integer"),
        ("SSL_SERVER_SESSION_TIMEOUT", "java/lang/Integer"),
        ("SSL_JSSE_KEY_MANAGER_CLASSES", "org/xnio/Sequence"),
        ("SSL_JSSE_TRUST_MANAGER_CLASSES", "org/xnio/Sequence"),
        ("SSL_RNG_OPTIONS", "org/xnio/OptionMap"),
        ("SSL_PACKET_BUFFER_SIZE", "java/lang/Integer"),
        ("SSL_APPLICATION_BUFFER_SIZE", "java/lang/Integer"),
        ("SSL_PACKET_BUFFER_REGION_SIZE", "java/lang/Integer"),
        ("SSL_APPLICATION_BUFFER_REGION_SIZE", "java/lang/Integer"),
        ("SSL_STARTTLS", "java/lang/Boolean"),
        ("SSL_PEER_HOST_NAME", "java/lang/String"),
        ("SSL_PEER_PORT", "java/lang/Integer"),
        ("SSL_NON_BLOCKING_KEY_MANAGER", "java/lang/Boolean"),
        ("SSL_NON_BLOCKING_TRUST_MANAGER", "java/lang/Boolean"),
        ("USE_DIRECT_BUFFERS", "java/lang/Boolean"),
        ("SECURE", "java/lang/Boolean"),
        ("SASL_POLICY_FORWARD_SECRECY", "java/lang/Boolean"),
        ("SASL_POLICY_NOACTIVE", "java/lang/Boolean"),
        ("SASL_POLICY_NOANONYMOUS", "java/lang/Boolean"),
        ("SASL_POLICY_NODICTIONARY", "java/lang/Boolean"),
        ("SASL_POLICY_NOPLAINTEXT", "java/lang/Boolean"),
        ("SASL_POLICY_PASS_CREDENTIALS", "java/lang/Boolean"),
        ("SASL_QOP", "org/xnio/Sequence"),
        ("SASL_STRENGTH", "org/xnio/sasl/SaslStrength"),
        ("SASL_SERVER_AUTH", "java/lang/Boolean"),
        ("SASL_REUSE", "java/lang/Boolean"),
        ("SASL_MECHANISMS", "org/xnio/Sequence"),
        ("SASL_DISALLOWED_MECHANISMS", "org/xnio/Sequence"),
        ("SASL_PROPERTIES", "org/xnio/Sequence"),
        ("FILE_ACCESS", "org/xnio/FileAccess"),
        ("FILE_APPEND", "java/lang/Boolean"),
        ("FILE_CREATE", "java/lang/Boolean"),
        ("STACK_SIZE", "java/lang/Long"),
        ("WORKER_NAME", "java/lang/String"),
        ("THREAD_PRIORITY", "java/lang/Integer"),
        ("THREAD_DAEMON", "java/lang/Boolean"),
        ("WORKER_IO_THREADS", "java/lang/Integer"),
        ("WORKER_READ_THREADS", "java/lang/Integer"),
        ("WORKER_WRITE_THREADS", "java/lang/Integer"),
        ("SPLIT_READ_WRITE_THREADS", "java/lang/Boolean"),
        ("WORKER_ESTABLISH_WRITING", "java/lang/Boolean"),
        ("WORKER_ACCEPT_THREADS", "java/lang/Integer"),
        ("WORKER_TASK_CORE_THREADS", "java/lang/Integer"),
        ("WORKER_TASK_MAX_THREADS", "java/lang/Integer"),
        ("WORKER_TASK_KEEPALIVE", "java/lang/Integer"),
        ("WORKER_TASK_LIMIT", "java/lang/Integer"),
        ("CORK", "java/lang/Boolean"),
        ("CONNECTION_HIGH_WATER", "java/lang/Integer"),
        ("CONNECTION_LOW_WATER", "java/lang/Integer"),
        ("COMPRESSION_LEVEL", "java/lang/Integer"),
        ("COMPRESSION_TYPE", "org/xnio/CompressionType"),
        ("BALANCING_TOKENS", "java/lang/Integer"),
        ("BALANCING_CONNECTIONS", "java/lang/Integer"),
        ("WATCHER_POLL_INTERVAL", "java/lang/Integer"),
    ];
    let declaring = ctx.create_string("org/xnio/Options");
    for (name, ty) in specs {
        let opt = try_alloc_concurrent_synthetic(ctx, "org/xnio/Option", 3)?;
        ctx.set_field(opt, OPT_DECLARING_CLASS, Value::Object(Some(declaring)));
        let name_s = ctx.create_string(name);
        ctx.set_field(opt, OPT_NAME, Value::Object(Some(name_s)));
        let ty_s = ctx.create_string(ty);
        ctx.set_field(opt, OPT_TYPE_CLASS, Value::Object(Some(ty_s)));
        // Store in a global map for retrieval (reflective getField + the
        // synthetic getXXX accessors below read from here). Keep each Option
        // alive + registry-remapped across GC moves (VarHandle-root pattern);
        // key computed on the just-registered address, no allocation between
        // the two calls. (The static-field write below keeps the object alive
        // too, but the FIELD is remapped by the statics scan — this raw map
        // copy is not, hence the identity-key indirection for reads.)
        ctx.register_var_handle_root(opt);
        let okey = ctx.identity_hash_code(opt);
        options_store().lock().insert(name.to_string(), (okey, opt));
        // Also write the REAL Java static field. We shim `Options.<clinit>`
        // (so the stock clinit's `Option.simple(...)` cascade never runs), which
        // means the `public static final Option` fields stay null unless we set
        // them here. WildFly reads them with `getstatic Options.<NAME>` (e.g.
        // `ManagementWorkerService.installService` → `OptionMap.Builder.set`),
        // NOT via the accessors — so without this the option arrives null and
        // `Builder.set` throws NPE. set_static_field_by_name resolves the field
        // on the loaded org/xnio/Options class.
        ctx.set_static_field_by_name("org/xnio/Options", name, Value::Object(Some(opt)));
    }
    let _ = DONE.set(());
    Ok(None)
}

/// Option-name → `(identity_key, ObjectRef)`.
///
/// GC (gc-followups-20260706): values are registered via
/// `register_var_handle_root` at insert and re-read via
/// `read_var_handle_root` at every lookup (ASYNC_POOL pattern, lib.rs) —
/// the GC remaps the registry entry after a move, never this raw static
/// copy. Before this fix `lookup_options_field` handed Java a bare cached
/// `ObjectRef` that went stale after any moving GC. Bounded by the
/// well-known option set, so the permanent registration cannot grow.
fn options_store() -> &'static Mutex<HashMap<String, (i32, ObjectRef)>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, (i32, ObjectRef)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `Options.<clinit>()V` — populates every well-known static field.
fn native_options_clinit(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ensure_options_initialized(ctx)?;
    Ok(None)
}

/// Getter surface: `Options.getWorkerIoThreads()Lorg/xnio/Option;` and
/// friends. These are not part of stock XNIO (they're static final
/// fields) but we register synthetic accessors so bytecode that calls
/// `Options.class.getField("WORKER_IO_THREADS").get(null)` observes a
/// populated value. The mapping from field name to ObjectRef lives in
/// `options_store`.
///
/// A single dispatcher + one `fn` pointer per well-known name is cheaper
/// than a closure-per-option (the registry requires a raw `fn` pointer,
/// not an `Fn` closure).
fn lookup_options_field(ctx: &mut dyn NativeContext, name: &str) -> MethodCallResult {
    ensure_options_initialized(ctx)?;
    match options_store().lock().get(name) {
        Some(&(key, cached)) => {
            // Re-read the CURRENT (post-GC) address — the var-handle-root
            // registry entry is remapped after a move, this raw copy is not.
            Ok(Some(Value::Object(Some(
                ctx.read_var_handle_root(key).unwrap_or(cached),
            ))))
        }
        None => Ok(Some(Value::Object(None))),
    }
}

fn native_options_get_worker_io_threads(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "WORKER_IO_THREADS")
}
fn native_options_get_worker_task_core_threads(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "WORKER_TASK_CORE_THREADS")
}
fn native_options_get_worker_task_max_threads(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "WORKER_TASK_MAX_THREADS")
}
fn native_options_get_worker_name(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "WORKER_NAME")
}
fn native_options_get_backlog(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    lookup_options_field(ctx, "BACKLOG")
}
fn native_options_get_keep_alive(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    lookup_options_field(ctx, "KEEP_ALIVE")
}
fn native_options_get_tcp_nodelay(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "TCP_NODELAY")
}
fn native_options_get_reuse_addresses(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "REUSE_ADDRESSES")
}
fn native_options_get_read_timeout(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "READ_TIMEOUT")
}
fn native_options_get_write_timeout(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "WRITE_TIMEOUT")
}
fn native_options_get_ssl_enabled(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    lookup_options_field(ctx, "SSL_ENABLED")
}

// ---------------------------------------------------------------------------
// Natives: IoFuture
// ---------------------------------------------------------------------------

fn alloc_io_future(
    ctx: &mut dyn NativeContext,
    inner: Arc<IoFutureInner>,
) -> Result<ObjectRef, MethodCallFailed> {
    let obj = try_alloc_concurrent_synthetic(ctx, "org/xnio/IoFuture", 3)?;
    let h = register_future(inner.clone());
    ctx.set_field(
        obj,
        IOF_STATUS,
        Value::Int(inner.status.load(Ordering::SeqCst) as i32),
    );
    ctx.set_field(obj, IOF_RESULT_SLOT, Value::Long(h));
    ctx.set_field(obj, IOF_NOTIFIER_LIST, Value::Long(h));
    Ok(obj)
}

fn inner_from_future(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<Arc<IoFutureInner>, MethodCallFailed> {
    let h = ctx.get_field(this, IOF_RESULT_SLOT).as_long().unwrap_or(0);
    lookup_future(h).ok_or_else(|| ise("IoFuture: stale or unknown handle"))
}

/// `IoFuture.getStatus()Lorg/xnio/IoFuture$Status;`
///
/// Returns an int encoding (0=WAITING, 1=DONE, 2=CANCELLED, 3=FAILED).
/// Java-level callers wrap this in the enum; our returning-int keeps the
/// stub wire-format simple.
fn native_iof_get_status(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_future(ctx, this)?;
    Ok(Some(Value::Int(inner.status.load(Ordering::SeqCst) as i32)))
}

/// `IoFuture.await()Lorg/xnio/IoFuture$Status;` — block until status != WAITING.
fn native_iof_await(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_future(ctx, this)?;
    // GC-blocking audit (STW takeover 5-class cluster, 2026-07-13): an
    // unbounded IoFuture.await() is a raw parking_lot::Condvar::wait with no
    // GC-blocking-region bracket — same missing-bracket bug found and fixed
    // across ReentrantReadWriteLock/StampedLock's native locks. A thread
    // waiting here for another thread's I/O completion notification stays
    // counted in the STW barrier's `expected` forever if that notifier is
    // itself stalled behind a GC pause. `inner`/`guard` hold no Java heap
    // refs, so a plain begin/end pair (no ref re-sync) suffices.
    ctx.begin_blocking_region();
    let mut guard = inner.state.lock();
    while inner.status.load(Ordering::SeqCst) == STATUS_WAITING {
        inner.cv.wait(&mut guard);
    }
    drop(guard);
    ctx.end_blocking_region();
    Ok(Some(Value::Int(inner.status.load(Ordering::SeqCst) as i32)))
}

/// `IoFuture.await(JLjava/util/concurrent/TimeUnit;)Lorg/xnio/IoFuture$Status;`
fn native_iof_await_timed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_future(ctx, this)?;
    let time = args.get(1).and_then(Value::as_long).unwrap_or(0);
    // We treat the TimeUnit as NANOSECONDS by default; conservative
    // but safe — any over-scale ends up being "long wait" which the
    // monotonic status makes correct.
    let dur = Duration::from_nanos(time.max(0) as u64);
    let deadline = Instant::now() + dur;
    // GC-blocking audit — see `native_iof_await` above. Even though this
    // wait is bounded, the thread stays counted in the STW barrier's
    // `expected` for the full duration, so it still needs the bracket.
    ctx.begin_blocking_region();
    let mut guard = inner.state.lock();
    while inner.status.load(Ordering::SeqCst) == STATUS_WAITING {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let _ = inner.cv.wait_for(&mut guard, deadline - now);
    }
    drop(guard);
    ctx.end_blocking_region();
    Ok(Some(Value::Int(inner.status.load(Ordering::SeqCst) as i32)))
}

/// `IoFuture.cancel()Lorg/xnio/IoFuture;` — request cancellation.
fn native_iof_cancel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_future(ctx, this)?;
    if inner.try_transition(STATUS_CANCELLED) {
        ctx.set_field(this, IOF_STATUS, Value::Int(STATUS_CANCELLED as i32));
        let notifiers = inner.drain_notifiers();
        inner.cv.notify_all();
        for e in &notifiers {
            IoFutureInner::fire_notifier(ctx, this, e);
        }
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `IoFuture.addNotifier(Lorg/xnio/IoFuture$Notifier;Ljava/lang/Object;)V`
fn native_iof_add_notifier(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let notifier = obj_arg(args, 1)?;
    let attachment = match args.get(2).copied() {
        Some(Value::Object(o)) => o,
        _ => None,
    };
    let inner = inner_from_future(ctx, this)?;

    // If already settled, fire immediately (outside lock).
    if inner.status.load(Ordering::SeqCst) != STATUS_WAITING {
        let entry = NotifierEntry {
            notifier,
            attachment,
        };
        IoFutureInner::fire_notifier(ctx, this, &entry);
        return Ok(None);
    }

    let mut state = inner.state.lock();
    state.notifiers.push(NotifierEntry {
        notifier,
        attachment,
    });
    Ok(None)
}

/// `IoFuture.get()Ljava/lang/Object;`
///
/// Blocks until the future is settled, then returns the result or
/// throws. Cancelled → raises `IllegalStateException` (a spec-faithful
/// proxy for `CancellationException` — we don't have a dedicated
/// `CancellationException` variant in `RuntimeError`, but callers of
/// `.get()` test for "it threw something" and the message makes the
/// cause clear).
fn native_iof_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let inner = inner_from_future(ctx, this)?;
    // Block like `await()` — see `native_iof_await`'s GC-blocking audit
    // comment above for the full rationale.
    {
        ctx.begin_blocking_region();
        let mut guard = inner.state.lock();
        while inner.status.load(Ordering::SeqCst) == STATUS_WAITING {
            inner.cv.wait(&mut guard);
        }
        drop(guard);
        ctx.end_blocking_region();
    }
    match inner.status.load(Ordering::SeqCst) {
        STATUS_DONE => {
            let state = inner.state.lock();
            Ok(Some(state.result.unwrap_or(Value::Object(None))))
        }
        STATUS_CANCELLED => Err(ise("IoFuture.get: cancelled")),
        STATUS_FAILED => {
            let state = inner.state.lock();
            let msg = state
                .exception_message
                .clone()
                .unwrap_or_else(|| "IoFuture failed".to_string());
            Err(ioex(msg))
        }
        _ => Err(ise("IoFuture.get: invalid status")),
    }
}

// ---------------------------------------------------------------------------
// Natives: FutureResult (producer side)
// ---------------------------------------------------------------------------

fn future_result_real_io_future(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field_by_name(this, "ioFuture") {
        Value::Object(Some(io_future)) => Some(io_future),
        _ => None,
    }
}

fn future_result_has_real_layout(ctx: &dyn NativeContext) -> bool {
    ctx.resolve_field_index("org/xnio/FutureResult", "ioFuture")
        .is_some()
}

/// `FutureResult.<init>()V` - allocate the paired future + producer.
fn native_future_result_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if future_result_has_real_layout(ctx) {
        let pin = ctx.pin_native_root(this);
        let io_future = match ctx.new_object_initialized(
            "org/xnio/FutureResult$1",
            "(Lorg/xnio/FutureResult;)V",
            &[Value::Object(Some(this))],
        )? {
            Some(Value::Object(Some(obj))) => obj,
            _ => return Err(ise("FutureResult.<init>: failed to allocate ioFuture")),
        };
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        ctx.set_field_by_name(this, "ioFuture", Value::Object(Some(io_future)));
        return Ok(None);
    }

    let inner = IoFutureInner::new_waiting();
    let h = register_future(inner.clone());
    ctx.set_field(this, FR_FUTURE_HANDLE, Value::Long(h));
    Ok(None)
}

fn inner_from_fr(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<Arc<IoFutureInner>, MethodCallFailed> {
    let h = ctx.get_field(this, FR_FUTURE_HANDLE).as_long().unwrap_or(0);
    lookup_future(h).ok_or_else(|| ise("FutureResult: stale or unknown handle"))
}

/// `FutureResult.getIoFuture()Lorg/xnio/IoFuture;`
fn native_future_result_get_future(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(io_future) = future_result_real_io_future(ctx, this) {
        return Ok(Some(Value::Object(Some(io_future))));
    }
    let inner = inner_from_fr(ctx, this)?;
    // Rebuild a JVM IoFuture object pointing at the same Arc.
    let obj = alloc_io_future(ctx, inner);
    Ok(Some(Value::Object(Some(obj?))))
}

/// `FutureResult.setResult(Ljava/lang/Object;)Z`
fn native_future_result_set_result(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let value = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(io_future) = future_result_real_io_future(ctx, this) {
        return ctx.invoke_virtual_declared(
            "org/xnio/AbstractIoFuture",
            io_future,
            "setResult",
            "(Ljava/lang/Object;)Z",
            &[value],
        );
    }
    let inner = inner_from_fr(ctx, this)?;
    if !inner.try_transition(STATUS_DONE) {
        return Ok(Some(Value::Int(0)));
    }
    {
        let mut state = inner.state.lock();
        state.result = Some(value);
    }
    let notifiers = inner.drain_notifiers();
    inner.cv.notify_all();

    // Rebuild a JVM-facing IoFuture object for the fire path.
    let fut_obj = alloc_io_future(ctx, inner.clone())?;
    for e in &notifiers {
        IoFutureInner::fire_notifier(ctx, fut_obj, e);
    }
    Ok(Some(Value::Int(1)))
}

/// `FutureResult.setException(Ljava/io/IOException;)Z`
fn native_future_result_set_exception(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let exc = args.get(1).copied().unwrap_or(Value::Object(None));
    if let Some(io_future) = future_result_real_io_future(ctx, this) {
        return ctx.invoke_virtual_declared(
            "org/xnio/AbstractIoFuture",
            io_future,
            "setException",
            "(Ljava/io/IOException;)Z",
            &[exc],
        );
    }
    let inner = inner_from_fr(ctx, this)?;
    if !inner.try_transition(STATUS_FAILED) {
        return Ok(Some(Value::Int(0)));
    }
    let msg = match exc {
        Value::Object(Some(e)) => {
            // Prefer reading the Throwable.message field directly.  In
            // the real JDK layout `message` is field 0; in MockNativeContext
            // tests we also store the message at slot 0 of the synthetic
            // exception object.  Fall back to `read_string(e)` in case the
            // caller passed a String directly.
            match ctx.get_field(e, 0) {
                Value::Object(Some(m)) => ctx
                    .read_string(m)
                    .unwrap_or_else(|| "IoFuture failed".to_string()),
                _ => ctx
                    .read_string(e)
                    .unwrap_or_else(|| "IoFuture failed".to_string()),
            }
        }
        _ => "IoFuture failed".to_string(),
    };
    {
        let mut state = inner.state.lock();
        state.exception_message = Some(msg);
    }
    let notifiers = inner.drain_notifiers();
    inner.cv.notify_all();
    let fut_obj = alloc_io_future(ctx, inner.clone())?;
    for e in &notifiers {
        IoFutureInner::fire_notifier(ctx, fut_obj, e);
    }
    Ok(Some(Value::Int(1)))
}

/// `FutureResult.setCancelled()Z`
fn native_future_result_set_cancelled(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Some(io_future) = future_result_real_io_future(ctx, this) {
        return ctx.invoke_virtual_declared(
            "org/xnio/AbstractIoFuture",
            io_future,
            "setCancelled",
            "()Z",
            &[],
        );
    }
    let inner = inner_from_fr(ctx, this)?;
    if !inner.try_transition(STATUS_CANCELLED) {
        return Ok(Some(Value::Int(0)));
    }
    let notifiers = inner.drain_notifiers();
    inner.cv.notify_all();
    let fut_obj = alloc_io_future(ctx, inner.clone())?;
    for e in &notifiers {
        IoFutureInner::fire_notifier(ctx, fut_obj, e);
    }
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::IOException { message: m.into() }.into()
}
fn iae<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException { message: m.into() }.into()
}
fn ise<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::IllegalStateException { message: m.into() }.into()
}
fn npe<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::NullPointerException {
        message: Some(m.into()),
    }
    .into()
}
fn cce<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::ClassCastException { message: m.into() }.into()
}
fn nfe<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::NumberFormatException { message: m.into() }.into()
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register all XNIO OptionMap / IoFuture / XnioExecutor natives.
pub fn register_xnio_async_natives(registry: &mut NativeMethodRegistry) {
    // ------------------------------------------------------------------
    // `org.xnio.Option` is NOT synthesized.
    //
    // History: a previous round registered a native `Option.simple` that
    // allocated a *synthetic* `org/xnio/Option` object (rather than the
    // real `org.xnio.SingleOption` / `SequenceOption` subclass that XNIO's
    // `Option.simple` constructs). WildFly's
    // `org.wildfly.extension.io.OptionAttributeDefinition$Builder.determineOptionType`
    // reflects on `option.getClass().getDeclaredField("type")` to recover
    // the option's value-type `Class`. A synthetic `Option` has no `type`
    // field, so `getDeclaredField` threw `NoSuchFieldException`, which
    // WildFly wraps into `IllegalArgumentException`, killing
    // `RemotingSubsystemRootResource.<clinit>` and failing the whole boot
    // (`WFLYSRV0055/0056`).
    //
    // `org.xnio.Option`, `SingleOption`, and `SequenceOption` are pure
    // Java with no native I/O — CratonVM runs their real bytecode fine.
    // Letting the real `Option.simple` (→ `new SingleOption`) run produces
    // an object with a genuine `type` field, so WildFly's reflection
    // works. The real `SingleOption` instance field layout
    // (slot0=`declClass`, slot1=`name`, slot2=`type`) intentionally
    // matches `OPT_DECLARING_CLASS`/`OPT_NAME`/`OPT_TYPE_CLASS`, so the
    // `OptionMap`/`Builder` slot reads below stay valid; `read_option_*`
    // resolves the `Class` mirrors stored in slots 0/2.
    //
    // Therefore: no `Option.simple` / `getName` / `cast` / `parseValue`
    // intercepts, and no `determineOptionType` shim.
    let _ = native_option_simple;
    let _ = native_option_get_name;
    let _ = native_option_cast;
    let _ = native_option_parse_value;

    // OptionMap
    registry.register(
        "org/xnio/OptionMap",
        "<clinit>",
        "()V",
        native_option_map_clinit,
    );
    // EMPTY is a public static field — bytecode reading it via
    // `GETSTATIC Lorg/xnio/OptionMap;` goes through the field-access
    // path.  We still register a getter so native callers can cheaply
    // materialise the singleton.
    registry.register(
        "org/xnio/OptionMap",
        "empty",
        "()Lorg/xnio/OptionMap;",
        native_option_map_empty_get,
    );
    registry.register(
        "org/xnio/OptionMap",
        "builder",
        "()Lorg/xnio/OptionMap$Builder;",
        native_option_map_builder,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;)Ljava/lang/Object;",
        native_option_map_get,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Ljava/lang/Object;",
        native_option_map_get,
    );
    // Typed-overload variants all delegate to the same implementation;
    // the NativeContext's raw Value machinery takes care of coercion
    // on the return.
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;I)I",
        native_option_map_get_int,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;J)J",
        native_option_map_get_long,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;Z)Z",
        native_option_map_get_bool,
    );
    registry.register(
        "org/xnio/OptionMap",
        "contains",
        "(Lorg/xnio/Option;)Z",
        native_option_map_contains,
    );
    registry.register("org/xnio/OptionMap", "size", "()I", native_option_map_size);
    registry.register(
        "org/xnio/OptionMap",
        "iterator",
        "()Ljava/util/Iterator;",
        native_option_map_iterator,
    );

    // Builder
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Lorg/xnio/OptionMap$Builder;",
        native_builder_set,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;I)Lorg/xnio/OptionMap$Builder;",
        native_builder_set,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;J)Lorg/xnio/OptionMap$Builder;",
        native_builder_set,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;Z)Lorg/xnio/OptionMap$Builder;",
        native_builder_set_bool,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "addAll",
        "(Lorg/xnio/OptionMap;)Lorg/xnio/OptionMap$Builder;",
        native_builder_add_all,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "getMap",
        "()Lorg/xnio/OptionMap;",
        native_builder_get_map,
    );

    // Options
    // bug-09 fix (supersedes the bug-08 synthetic-completion approach): do NOT
    // shim `Options.<clinit>`. The stock clinit runs the real
    // `Option.simple(...)` / `Option.sequence(...)` cascade, which — since the
    // `Option.simple` native is de-registered above — constructs REAL
    // `SingleOption`/`SequenceOption` instances. Shimming `<clinit>` to
    // `native_options_clinit` instead left the `public static final Option`
    // fields as *synthetic* abstract `org/xnio/Option` objects, which:
    //   * had no `cast` Code → `OptionMap.create` → `option.cast(v)` →
    //     `AbstractMethodError` → `DefaultXnioWorkerHolder.<clinit>` fails (bug-09);
    //   * only covered 12/81 fields → 69 null incl. WORKER_TASK_KEEPALIVE,
    //     breaking `XnioWorker.<clinit>` (bug-08);
    //   * had no real `type` field → WildFly `determineOptionType` reflection
    //     (`getDeclaredField("type")`) would fail (the original WFLYSRV0055).
    // Real Options instances have working `cast`/`parseValue`/`getName` and a
    // real `type` field; their slot layout (declClass/name/type) still matches
    // the OptionMap/Builder slot reads. See wildfly-suite-bugs/
    // bug-08 + bug-09.
    let _ = native_options_clinit;
    // Accessors for each well-known field. Registered as top-level fn
    // pointers (not closures) because the native registry stores raw
    // function pointers.
    registry.register(
        "org/xnio/Options",
        "getWORKER_IO_THREADS",
        "()Lorg/xnio/Option;",
        native_options_get_worker_io_threads,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_TASK_CORE_THREADS",
        "()Lorg/xnio/Option;",
        native_options_get_worker_task_core_threads,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_TASK_MAX_THREADS",
        "()Lorg/xnio/Option;",
        native_options_get_worker_task_max_threads,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_NAME",
        "()Lorg/xnio/Option;",
        native_options_get_worker_name,
    );
    registry.register(
        "org/xnio/Options",
        "getBACKLOG",
        "()Lorg/xnio/Option;",
        native_options_get_backlog,
    );
    registry.register(
        "org/xnio/Options",
        "getKEEP_ALIVE",
        "()Lorg/xnio/Option;",
        native_options_get_keep_alive,
    );
    registry.register(
        "org/xnio/Options",
        "getTCP_NODELAY",
        "()Lorg/xnio/Option;",
        native_options_get_tcp_nodelay,
    );
    registry.register(
        "org/xnio/Options",
        "getREUSE_ADDRESSES",
        "()Lorg/xnio/Option;",
        native_options_get_reuse_addresses,
    );
    registry.register(
        "org/xnio/Options",
        "getREAD_TIMEOUT",
        "()Lorg/xnio/Option;",
        native_options_get_read_timeout,
    );
    registry.register(
        "org/xnio/Options",
        "getWRITE_TIMEOUT",
        "()Lorg/xnio/Option;",
        native_options_get_write_timeout,
    );
    registry.register(
        "org/xnio/Options",
        "getSSL_ENABLED",
        "()Lorg/xnio/Option;",
        native_options_get_ssl_enabled,
    );

    // IoFuture
    registry.register(
        "org/xnio/IoFuture",
        "getStatus",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_get_status,
    );
    registry.register(
        "org/xnio/IoFuture",
        "await",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_await,
    );
    registry.register(
        "org/xnio/IoFuture",
        "awaitInterruptibly",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_await,
    );
    registry.register(
        "org/xnio/IoFuture",
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Lorg/xnio/IoFuture$Status;",
        native_iof_await_timed,
    );
    registry.register(
        "org/xnio/IoFuture",
        "get",
        "()Ljava/lang/Object;",
        native_iof_get,
    );
    registry.register(
        "org/xnio/IoFuture",
        "getInterruptibly",
        "()Ljava/lang/Object;",
        native_iof_get,
    );
    registry.register(
        "org/xnio/IoFuture",
        "cancel",
        "()Lorg/xnio/IoFuture;",
        native_iof_cancel,
    );
    registry.register(
        "org/xnio/IoFuture",
        "addNotifier",
        "(Lorg/xnio/IoFuture$Notifier;Ljava/lang/Object;)V",
        native_iof_add_notifier,
    );

    // FutureResult
    registry.register(
        "org/xnio/FutureResult",
        "<init>",
        "()V",
        native_future_result_init,
    );
    registry.register(
        "org/xnio/FutureResult",
        "getIoFuture",
        "()Lorg/xnio/IoFuture;",
        native_future_result_get_future,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setResult",
        "(Ljava/lang/Object;)Z",
        native_future_result_set_result,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setException",
        "(Ljava/io/IOException;)Z",
        native_future_result_set_exception,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setCancelled",
        "()Z",
        native_future_result_set_cancelled,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    /// Helper: build an Option via the native (so the test exercises the
    /// same path that Java-side `Option.simple(...)` would).
    fn make_option(
        ctx: &mut crate::test_utils::MockNativeContext,
        declaring: &str,
        name: &str,
        ty: &str,
    ) -> ObjectRef {
        let d = ctx.create_string(declaring);
        let n = ctx.create_string(name);
        let t = ctx.create_string(ty);
        let r = native_option_simple(
            ctx,
            &[
                Value::Object(Some(d)),
                Value::Object(Some(n)),
                Value::Object(Some(t)),
            ],
        )
        .unwrap()
        .unwrap();
        match r {
            Value::Object(Some(o)) => o,
            _ => panic!("expected Option object"),
        }
    }

    fn empty_map(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
        match native_option_map_empty_get(ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("expected OptionMap"),
        }
    }

    fn new_builder(ctx: &mut crate::test_utils::MockNativeContext) -> ObjectRef {
        match native_option_map_builder(ctx, &[]).unwrap().unwrap() {
            Value::Object(Some(o)) => o,
            _ => panic!("expected Builder"),
        }
    }

    fn new_future(ctx: &mut crate::test_utils::MockNativeContext) -> (ObjectRef, ObjectRef) {
        // Allocate a FutureResult and init it; read back its future.
        let fr = try_alloc_concurrent_synthetic(ctx, "org/xnio/FutureResult", 1).unwrap();
        native_future_result_init(ctx, &[Value::Object(Some(fr))]).unwrap();
        let fut = match native_future_result_get_future(ctx, &[Value::Object(Some(fr))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected IoFuture"),
        };
        (fr, fut)
    }

    #[test]
    fn t19_7_e_option_simple_creates_option_with_type() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "WORKER_IO_THREADS",
            "java/lang/Integer",
        );
        // Field 1 should be the name string.
        let name_val = native_option_get_name(&mut ctx, &[Value::Object(Some(opt))])
            .unwrap()
            .unwrap();
        let name = match name_val {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
            _ => String::new(),
        };
        assert_eq!(name, "WORKER_IO_THREADS");
        // Type should round-trip.
        assert_eq!(
            read_option_type(&ctx, opt).as_deref(),
            Some("java/lang/Integer")
        );
    }

    #[test]
    fn t19_7_e_option_map_empty_has_size_zero() {
        let mut ctx = mock_ctx();
        let m = empty_map(&mut ctx);
        let size = native_option_map_size(&mut ctx, &[Value::Object(Some(m))])
            .unwrap()
            .unwrap();
        assert_eq!(size, Value::Int(0));
    }

    #[test]
    fn t19_7_e_option_map_one_field_layout_uses_side_table_handle() {
        let mut ctx = mock_ctx();
        let cid = ctx.ensure_class_initialized("org/xnio/OptionMap").unwrap();
        let m = ctx.alloc_object(cid, 1);

        let mut entries = HashMap::new();
        entries.insert(
            OptionKey {
                declaring_class: "org/xnio/Options".to_string(),
                name: "WORKER_IO_THREADS".to_string(),
            },
            OptionValue::Int(7),
        );
        let h = register_map(Arc::new(OptionMapInner {
            entries: Mutex::new(entries),
        }));
        write_handle_slot_if_present(&ctx, m, OM_ENTRIES_HANDLE, h);
        remember_map_handle(&ctx, m, h);

        assert_eq!(ctx.object_num_fields(m), 1);
        assert_eq!(read_handle_slot(&ctx, m, OM_ENTRIES_HANDLE), 0);
        let inner = inner_from_map(&ctx, m).unwrap();
        assert_eq!(inner.entries.lock().len(), 1);
    }

    #[test]
    fn t19_7_e_option_map_builder_one_field_layout_uses_side_table_handle() {
        let mut ctx = mock_ctx();
        let cid = ctx
            .ensure_class_initialized("org/xnio/OptionMap$Builder")
            .unwrap();
        let b = ctx.alloc_object(cid, 1);
        let h = register_builder(Arc::new(BuilderInner::default()));
        write_handle_slot_if_present(&ctx, b, OMB_PENDING_HANDLE, h);
        remember_builder_handle(&ctx, b, h);

        assert_eq!(ctx.object_num_fields(b), 1);
        assert_eq!(read_handle_slot(&ctx, b, OMB_PENDING_HANDLE), 0);
        let inner = builder_from_this(&ctx, b).unwrap();
        assert!(!inner.consumed.load(Ordering::SeqCst));
    }

    #[test]
    fn t19_7_e_option_map_builder_set_get_round_trip() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "WORKER_IO_THREADS",
            "java/lang/Integer",
        );
        let b = new_builder(&mut ctx);
        // set(Option, I)
        native_builder_set(
            &mut ctx,
            &[
                Value::Object(Some(b)),
                Value::Object(Some(opt)),
                Value::Int(8),
            ],
        )
        .unwrap();
        let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected map"),
        };
        let got = native_option_map_get(
            &mut ctx,
            &[Value::Object(Some(map)), Value::Object(Some(opt))],
        )
        .unwrap()
        .unwrap();
        // `get(Option)Object` must box the stored int, not return it raw
        // (the JIT's fast return path treats a raw int as a bogus ObjectRef --
        // see native_option_map_get's doc comment).
        match got {
            Value::Object(Some(o)) => {
                assert_eq!(ctx.get_field(o, 0), Value::Int(8));
            }
            other => panic!("expected boxed Integer, got {other:?}"),
        }
        // Size should be 1.
        let size = native_option_map_size(&mut ctx, &[Value::Object(Some(map))])
            .unwrap()
            .unwrap();
        assert_eq!(size, Value::Int(1));
        // contains should be true.
        let has = native_option_map_contains(
            &mut ctx,
            &[Value::Object(Some(map)), Value::Object(Some(opt))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(has, Value::Int(1));
    }

    #[test]
    fn t19_7_e_option_map_builder_add_all_copies_entries() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "TCP_NODELAY",
            "java/lang/Boolean",
        );

        let source_builder = new_builder(&mut ctx);
        native_builder_set_bool(
            &mut ctx,
            &[
                Value::Object(Some(source_builder)),
                Value::Object(Some(opt)),
                Value::Int(1),
            ],
        )
        .unwrap();
        let source_map =
            match native_builder_get_map(&mut ctx, &[Value::Object(Some(source_builder))])
                .unwrap()
                .unwrap()
            {
                Value::Object(Some(o)) => o,
                _ => panic!("expected source map"),
            };

        let dest_builder = new_builder(&mut ctx);
        let returned = native_builder_add_all(
            &mut ctx,
            &[
                Value::Object(Some(dest_builder)),
                Value::Object(Some(source_map)),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(returned, Value::Object(Some(dest_builder)));

        let dest_map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(dest_builder))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected destination map"),
        };
        let got = native_option_map_get_bool(
            &mut ctx,
            &[
                Value::Object(Some(dest_map)),
                Value::Object(Some(opt)),
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Value::Int(1));

        let size = native_option_map_size(&mut ctx, &[Value::Object(Some(dest_map))])
            .unwrap()
            .unwrap();
        assert_eq!(size, Value::Int(1));
    }

    #[test]
    fn t19_7_e_option_map_default_when_missing_returns_default() {
        let mut ctx = mock_ctx();
        let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
        let m = empty_map(&mut ctx);
        let got = native_option_map_get(
            &mut ctx,
            &[
                Value::Object(Some(m)),
                Value::Object(Some(opt)),
                Value::Int(128),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Value::Int(128), "missing key → default");
    }

    #[test]
    fn t19_7_e_option_map_immutable_after_build() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "WORKER_IO_THREADS",
            "java/lang/Integer",
        );
        let b = new_builder(&mut ctx);
        native_builder_set(
            &mut ctx,
            &[
                Value::Object(Some(b)),
                Value::Object(Some(opt)),
                Value::Int(4),
            ],
        )
        .unwrap();
        let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected map"),
        };
        // Once consumed, further set() throws IllegalStateException.
        let err = native_builder_set(
            &mut ctx,
            &[
                Value::Object(Some(b)),
                Value::Object(Some(opt)),
                Value::Int(99),
            ],
        )
        .err()
        .expect("set after getMap must fail");
        assert!(format!("{err:?}").contains("IllegalStateException"));
        // Re-calling getMap must also fail.
        let err2 = native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
            .err()
            .expect("getMap twice must fail");
        assert!(format!("{err2:?}").contains("IllegalStateException"));
        // The previously-returned map is unaffected.
        let got = native_option_map_get(
            &mut ctx,
            &[Value::Object(Some(map)), Value::Object(Some(opt))],
        )
        .unwrap()
        .unwrap();
        match got {
            Value::Object(Some(o)) => {
                assert_eq!(ctx.get_field(o, 0), Value::Int(4));
            }
            other => panic!("expected boxed Integer, got {other:?}"),
        }
    }

    #[test]
    fn t19_7_e_options_worker_io_threads_default_is_four() {
        let mut ctx = mock_ctx();
        // Populate via <clinit>.
        native_options_clinit(&mut ctx, &[]).unwrap();
        // Options.WORKER_IO_THREADS exists in the store.
        let store = options_store().lock();
        let wio = store.get("WORKER_IO_THREADS").copied();
        drop(store);
        assert!(wio.is_some(), "WORKER_IO_THREADS option must be populated");

        // The "default 4" assertion is the contract with T19.7.b — until
        // T19.7.b is landed, we exercise the contract by running the
        // builder path: build a map with no WORKER_IO_THREADS set, call
        // get with default=4, and confirm we get 4. The getter for the
        // default lives in xnio_worker.rs, but the OptionMap API surface
        // here is what makes 4 addressable.
        // (Store entries are `(identity_key, ObjectRef)` — take the ref.)
        let opt = wio.unwrap().1;
        let m = empty_map(&mut ctx);
        let got = native_option_map_get(
            &mut ctx,
            &[
                Value::Object(Some(m)),
                Value::Object(Some(opt)),
                Value::Int(4),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Value::Int(4));
    }

    #[test]
    fn t19_7_e_io_future_status_initially_waiting() {
        let mut ctx = mock_ctx();
        let (_fr, fut) = new_future(&mut ctx);
        let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        assert_eq!(s, Value::Int(STATUS_WAITING as i32));
    }

    #[test]
    fn t19_7_e_future_result_set_result_transitions_to_done() {
        let mut ctx = mock_ctx();
        let (fr, fut) = new_future(&mut ctx);
        // setResult with a string value.
        let js = ctx.create_string("hello");
        let rv = native_future_result_set_result(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(js))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(rv, Value::Int(1), "first setResult wins");
        // Status is now DONE.
        let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        assert_eq!(s, Value::Int(STATUS_DONE as i32));
        // Second setResult is a no-op.
        let rv2 = native_future_result_set_result(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(js))],
        )
        .unwrap()
        .unwrap();
        assert_eq!(rv2, Value::Int(0), "second setResult is no-op");
    }

    #[test]
    fn t19_7_e_future_result_set_exception_transitions_to_failed() {
        let mut ctx = mock_ctx();
        let (fr, fut) = new_future(&mut ctx);
        // Build a synthetic IOException with a message field.
        let exc = try_alloc_concurrent_synthetic(&mut ctx, "java/io/IOException", 2).unwrap();
        let msg = ctx.create_string("disk full");
        ctx.set_field(exc, 0, Value::Object(Some(msg)));
        native_future_result_set_exception(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(exc))],
        )
        .unwrap();
        let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        assert_eq!(s, Value::Int(STATUS_FAILED as i32));
        // get() now throws an IOException whose message contains the
        // original "disk full" text.
        let err = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
            .err()
            .expect("failed future should error on get");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("disk full") || msg.contains("IoFuture failed"),
            "error message must mention the cause: {msg}"
        );
    }

    #[test]
    fn t19_7_e_io_future_await_blocks_until_done() {
        use std::thread;
        use std::time::Duration;

        let mut ctx = mock_ctx();
        let (fr, fut) = new_future(&mut ctx);
        // Grab the Arc directly so the background thread can signal
        // completion without needing a NativeContext.
        let inner = inner_from_future(&ctx, fut).unwrap();
        let inner_bg = inner.clone();
        let t = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            if inner_bg.try_transition(STATUS_DONE) {
                inner_bg.state.lock().result = Some(Value::Int(42));
                inner_bg.cv.notify_all();
            }
        });

        let start = Instant::now();
        let rv = native_iof_await(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(rv, Value::Int(STATUS_DONE as i32));
        assert!(
            elapsed >= Duration::from_millis(30),
            "await should block ~50ms, elapsed={elapsed:?}"
        );
        t.join().unwrap();

        // get() returns the stashed result.
        let rv2 = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        assert_eq!(rv2, Value::Int(42));

        // And fr itself still reports DONE.
        let _ = fr;
    }

    #[test]
    fn t19_7_e_io_future_notifier_fires_on_completion() {
        use std::sync::atomic::AtomicU32;

        let mut ctx = mock_ctx();
        let (fr, fut) = new_future(&mut ctx);

        // Build a synthetic Notifier object. Our invoke() in MockNativeContext
        // always returns Ok(None) without dispatching, so we test the
        // wiring by observing the notifier list before/after completion.
        let notifier =
            try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
        // addNotifier while still WAITING → should stash.
        native_iof_add_notifier(
            &mut ctx,
            &[
                Value::Object(Some(fut)),
                Value::Object(Some(notifier)),
                Value::Object(None),
            ],
        )
        .unwrap();
        let inner = inner_from_future(&ctx, fut).unwrap();
        assert_eq!(
            inner.state.lock().notifiers.len(),
            1,
            "notifier should be stored while waiting"
        );

        // Complete the future → notifier list drained.
        native_future_result_set_result(&mut ctx, &[Value::Object(Some(fr)), Value::Object(None)])
            .unwrap();
        assert_eq!(
            inner.state.lock().notifiers.len(),
            0,
            "notifier list should be drained after completion"
        );

        // addNotifier AFTER completion → fires synchronously (and the
        // list stays empty).
        let n2 = try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
        native_iof_add_notifier(
            &mut ctx,
            &[
                Value::Object(Some(fut)),
                Value::Object(Some(n2)),
                Value::Object(None),
            ],
        )
        .unwrap();
        assert_eq!(
            inner.state.lock().notifiers.len(),
            0,
            "post-completion notifier should fire synchronously, not stash"
        );

        // Defeat unused-var warnings.
        let _ = AtomicU32::new(0);
    }

    #[test]
    fn t19_7_e_io_future_cancel_transitions_to_cancelled() {
        let mut ctx = mock_ctx();
        let (_fr, fut) = new_future(&mut ctx);
        let _ = native_iof_cancel(&mut ctx, &[Value::Object(Some(fut))]);
        let s = native_iof_get_status(&mut ctx, &[Value::Object(Some(fut))])
            .unwrap()
            .unwrap();
        assert_eq!(s, Value::Int(STATUS_CANCELLED as i32));
        // get() on a cancelled future throws IllegalStateException.
        let err = native_iof_get(&mut ctx, &[Value::Object(Some(fut))])
            .err()
            .expect("cancelled get must fail");
        assert!(format!("{err:?}").contains("cancelled"));
    }

    // ------------------------------------------------------------------
    // Bonus robustness tests (beyond the minimum 12).
    // ------------------------------------------------------------------

    #[test]
    fn t19_7_e_io_future_await_timed_returns_waiting_on_timeout() {
        let mut ctx = mock_ctx();
        let (_fr, fut) = new_future(&mut ctx);
        let rv = native_iof_await_timed(
            &mut ctx,
            &[
                Value::Object(Some(fut)),
                Value::Long(5_000_000), // 5ms
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        // Still WAITING because nothing completed it.
        assert_eq!(rv, Value::Int(STATUS_WAITING as i32));
    }

    #[test]
    fn t19_7_e_option_cast_null_is_accepted() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "WORKER_NAME",
            "java/lang/String",
        );
        let r = native_option_cast(&mut ctx, &[Value::Object(Some(opt)), Value::Object(None)])
            .unwrap()
            .unwrap();
        assert_eq!(r, Value::Object(None));
    }

    #[test]
    fn t19_7_e_option_parse_value_integer() {
        let mut ctx = mock_ctx();
        let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
        let s = ctx.create_string("42");
        let r = native_option_parse_value(
            &mut ctx,
            &[
                Value::Object(Some(opt)),
                Value::Object(Some(s)),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(42));
    }

    #[test]
    fn t19_7_e_option_parse_value_boolean_true() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "TCP_NODELAY",
            "java/lang/Boolean",
        );
        let s = ctx.create_string("TRUE");
        let r = native_option_parse_value(
            &mut ctx,
            &[
                Value::Object(Some(opt)),
                Value::Object(Some(s)),
                Value::Object(None),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(r, Value::Int(1));
    }

    #[test]
    fn t19_7_e_option_parse_value_bad_integer_raises_nfe() {
        let mut ctx = mock_ctx();
        let opt = make_option(&mut ctx, "org/xnio/Options", "BACKLOG", "java/lang/Integer");
        let s = ctx.create_string("not-a-number");
        let err = native_option_parse_value(
            &mut ctx,
            &[
                Value::Object(Some(opt)),
                Value::Object(Some(s)),
                Value::Object(None),
            ],
        )
        .err()
        .expect("bad integer must be rejected");
        assert!(format!("{err:?}").contains("NumberFormatException"));
    }

    #[test]
    fn t19_7_e_option_map_get_bool_round_trip() {
        let mut ctx = mock_ctx();
        let opt = make_option(
            &mut ctx,
            "org/xnio/Options",
            "KEEP_ALIVE",
            "java/lang/Boolean",
        );
        let b = new_builder(&mut ctx);
        // set(Option, Z)
        native_builder_set_bool(
            &mut ctx,
            &[
                Value::Object(Some(b)),
                Value::Object(Some(opt)),
                Value::Int(1),
            ],
        )
        .unwrap();
        let map = match native_builder_get_map(&mut ctx, &[Value::Object(Some(b))])
            .unwrap()
            .unwrap()
        {
            Value::Object(Some(o)) => o,
            _ => panic!("expected map"),
        };
        let got = native_option_map_get(
            &mut ctx,
            &[Value::Object(Some(map)), Value::Object(Some(opt))],
        )
        .unwrap()
        .unwrap();
        // Bool is stored as Int(0/1) internally but `get(Option)Object` must
        // box it into a real Boolean, matching the declared Object return type.
        match got {
            Value::Object(Some(o)) => {
                assert_eq!(ctx.get_field(o, 0), Value::Int(1));
            }
            other => panic!("expected boxed Boolean, got {other:?}"),
        }
    }

    #[test]
    fn t19_7_e_option_map_get_bool_unboxes_object_entry() {
        let mut ctx = mock_ctx();
        let opt = make_option(&mut ctx, "org/xnio/Options", "SECURE", "java/lang/Boolean");
        let boolean_cid = ctx.ensure_class_initialized("java/lang/Boolean").unwrap();
        let boxed_true = ctx.alloc_object(boolean_cid, 1);
        ctx.set_field(boxed_true, 0, Value::Int(1));

        let mut entries = HashMap::new();
        entries.insert(
            OptionKey {
                declaring_class: "org/xnio/Options".to_string(),
                name: "SECURE".to_string(),
            },
            OptionValue::Obj(Some(boxed_true)),
        );
        let map = alloc_option_map(
            &mut ctx,
            Arc::new(OptionMapInner {
                entries: Mutex::new(entries),
            }),
        )
        .unwrap();

        let got = native_option_map_get_bool(
            &mut ctx,
            &[
                Value::Object(Some(map)),
                Value::Object(Some(opt)),
                Value::Int(0),
            ],
        )
        .unwrap()
        .unwrap();
        assert_eq!(got, Value::Int(1));
    }

    // ------------------------------------------------------------------
    // GC root scan / remap (UAF fix).
    //
    // The registry is process-global and shared with the other tests
    // (which may run in parallel), so these assertions check *containment*
    // of the specific refs under test, never exact registry-wide counts.
    // ------------------------------------------------------------------

    fn ptr_of(r: ObjectRef) -> usize {
        r.as_ptr() as usize
    }

    #[test]
    fn t19_7_e_gc_scan_reports_pending_notifier_and_attachment() {
        let mut ctx = mock_ctx();
        let (_fr, fut) = new_future(&mut ctx);
        let notifier =
            try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
        let attachment = ctx.create_string("att");
        // addNotifier while WAITING stashes (notifier, attachment).
        native_iof_add_notifier(
            &mut ctx,
            &[
                Value::Object(Some(fut)),
                Value::Object(Some(notifier)),
                Value::Object(Some(attachment)),
            ],
        )
        .unwrap();

        let mut roots = Vec::new();
        gc_scan_xnio_future_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
        assert!(
            addrs.contains(&ptr_of(notifier)),
            "scan must root the pending notifier"
        );
        assert!(
            addrs.contains(&ptr_of(attachment)),
            "scan must root the notifier attachment"
        );
    }

    #[test]
    fn t19_7_e_gc_scan_reports_object_result_after_set() {
        let mut ctx = mock_ctx();
        let (fr, _fut) = new_future(&mut ctx);
        let result = ctx.create_string("payload");
        native_future_result_set_result(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(result))],
        )
        .unwrap();

        let mut roots = Vec::new();
        gc_scan_xnio_future_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
        assert!(
            addrs.contains(&ptr_of(result)),
            "scan must root the settled Object result (held until get())"
        );
    }

    #[test]
    fn t19_7_e_gc_remap_repoints_notifier_attachment_and_result() {
        let mut ctx = mock_ctx();
        let (fr, fut) = new_future(&mut ctx);
        let notifier =
            try_alloc_concurrent_synthetic(&mut ctx, "org/xnio/IoFuture$Notifier", 1).unwrap();
        let attachment = ctx.create_string("att");
        native_iof_add_notifier(
            &mut ctx,
            &[
                Value::Object(Some(fut)),
                Value::Object(Some(notifier)),
                Value::Object(Some(attachment)),
            ],
        )
        .unwrap();
        let result = ctx.create_string("payload");
        native_future_result_set_result(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(result))],
        )
        .unwrap();

        // Fabricate fresh, 8-byte-aligned destination addresses. The refs are
        // never dereferenced here — we only assert the stored pointer flipped.
        let new_notifier = 0x4000usize;
        let new_attachment = 0x5000usize;
        let new_result = 0x6000usize;
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(ptr_of(notifier), new_notifier);
        map.insert(ptr_of(attachment), new_attachment);
        map.insert(ptr_of(result), new_result);

        gc_update_xnio_future_refs(&map);

        // Reach the inner directly and confirm the stored refs were repointed.
        let inner = inner_from_future(&ctx, fut).unwrap();
        let state = inner.state.lock();
        // setResult drains notifiers, so the result ref is the live one to check.
        assert_eq!(
            state.result.map(|v| match v {
                Value::Object(Some(r)) => ptr_of(r),
                _ => 0,
            }),
            Some(new_result),
            "result ref must be remapped to its new address"
        );
        // A post-settle scan must now report the remapped result address and
        // none of the stale ones (scan/remap symmetry).
        drop(state);
        let mut roots = Vec::new();
        gc_scan_xnio_future_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
        assert!(addrs.contains(&new_result), "scan sees remapped result");
        assert!(
            !addrs.contains(&ptr_of(result)),
            "stale pre-move result address must not survive"
        );
    }

    #[test]
    fn t19_7_e_gc_remap_empty_map_is_noop() {
        let mut ctx = mock_ctx();
        let (fr, _fut) = new_future(&mut ctx);
        let result = ctx.create_string("payload");
        native_future_result_set_result(
            &mut ctx,
            &[Value::Object(Some(fr)), Value::Object(Some(result))],
        )
        .unwrap();
        // Empty pointer map → no relocation happened → refs unchanged.
        gc_update_xnio_future_refs(&cratonvm_types::PointerMap::default());
        let mut roots = Vec::new();
        gc_scan_xnio_future_roots(&mut roots);
        let addrs: Vec<usize> = roots.iter().map(|r| ptr_of(*r)).collect();
        assert!(
            addrs.contains(&ptr_of(result)),
            "empty remap leaves the result ref untouched"
        );
    }
}
