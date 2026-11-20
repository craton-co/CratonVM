// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
pub struct OptionKey {
    pub declaring_class: String,
    pub name: String,
}

/// Typed value variants understood by `OptionMap`. Each matches an XNIO
/// primitive type bound (Int/Long/Bool/String) plus a catch-all
/// `ObjectRef` for user-supplied `Option<T>` where `T` is a reference.
#[derive(Debug, Clone)]
pub enum OptionValue {
    Int(i32),
    Long(i64),
    Bool(bool),
    Str(String),
    Obj(Option<ObjectRef>),
}

impl OptionValue {
    pub fn as_int(&self) -> Option<i32> {
        match self {
            OptionValue::Int(v) => Some(*v),
            OptionValue::Long(v) => i32::try_from(*v).ok(),
            OptionValue::Bool(b) => Some(if *b { 1 } else { 0 }),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
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
pub struct OptionMapInner {
    /// Family-1 fix (cce0079 tree-key tail): `OptionValue::Obj` variants hold
    /// raw heap `ObjectRef`s in this Rust-side registry — invisible to field
    /// tracing. They must be rooted AND remapped by every moving GC (see
    /// `gc_scan_xnio_future_roots`/`gc_update_xnio_future_refs`), which
    /// requires interior mutability — hence the leaf Mutex (same pattern as
    /// `FutureState`/`BuilderInner.pending`). Readers must COPY the entry out
    /// and drop the guard before any GC-capable call (allocation/boxing):
    /// the GC's own scan takes this lock, so holding it across a safepoint
    /// deadlocks the collection.
    pub entries: Mutex<HashMap<OptionKey, OptionValue>>,
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
pub struct BuilderInner {
    pub pending: Mutex<HashMap<OptionKey, OptionValue>>,
    pub consumed: AtomicBool,
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
pub struct IoFutureInner {
    pub status: AtomicU8,
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
    pub fn new_waiting(vm: usize) -> Arc<Self> {
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
        // on the next scan. See `gc_scan_xnio_future_roots`. `vm` is the
        // creating VM (`ctx.vm_identity()`): only its collections root and
        // remap this future's refs (gc-common w8-a).
        register_live_future(vm, &inner);
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
    ///
    /// `future_ref` and the entry's refs must be current; this runs Java, so
    /// none of them is after it returns.
    fn fire_notifier(ctx: &mut dyn NativeContext, future_ref: ObjectRef, entry: &NotifierEntry) {
        let notifier = entry.notifier;
        let att = Value::Object(entry.attachment);
        let fut_v = Value::Object(Some(future_ref));
        let result = catch_unwind(AssertUnwindSafe(|| {
            // org.xnio.IoFuture$Notifier.notify(IoFuture, A)V -- an instance
            // method of the notifier. gc-common w23-b: this was
            // `ctx.invoke("org/xnio/IoFuture$Notifier", ..., &[fut, att])`,
            // which never passed the notifier at all: the future was taken as
            // the receiver of the interface's abstract method, the call
            // failed, and the error was dropped -- no notifier ever ran.
            let _ = ctx.invoke_virtual(
                notifier,
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

/// Fire notifiers `drain_notifiers` took out of `inner` against the future's
/// JVM face: `future` when the caller has one (current at the call), else a
/// fresh `IoFuture` built here. Returns that face, current.
///
/// gc-common w23-b: a drained entry is no longer in `state.notifiers`, so the
/// `xnio-futures` root scan no longer sees it -- and the callers then
/// allocated the future object and ran one notifier after another (each one
/// Java) with the rest of the entries, and the future, held as raw addresses.
/// Every entry and the future are pinned before the first GC point here and
/// read back before each dispatch.
fn fire_drained_notifiers(
    ctx: &mut dyn NativeContext,
    inner: &Arc<IoFutureInner>,
    future: Option<ObjectRef>,
    notifiers: &[NotifierEntry],
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    if notifiers.is_empty() {
        // Nothing to fire: no face to build (the callers used to allocate one
        // regardless, and file a `futures` row for it).
        return Ok(future);
    }
    let mut base: Option<usize> = None;
    let mut pins: Vec<(usize, Option<usize>)> = Vec::with_capacity(notifiers.len());
    for entry in notifiers {
        let notifier_pin = ctx.pin_native_root(entry.notifier);
        if base.is_none() {
            base = Some(notifier_pin);
        }
        let attachment_pin = entry.attachment.map(|a| ctx.pin_native_root(a));
        pins.push((notifier_pin, attachment_pin));
    }
    let future = match future {
        Some(future) => Ok(future),
        None => alloc_io_future(ctx, inner.clone()),
    };
    let fired = future.map(|future| {
        let future_pin = ctx.pin_native_root(future);
        for (entry, &(notifier_pin, attachment_pin)) in notifiers.iter().zip(pins.iter()) {
            let notifier = ctx.read_native_pin(notifier_pin, entry.notifier);
            let attachment = match (attachment_pin, entry.attachment) {
                (Some(pin), Some(a)) => Some(ctx.read_native_pin(pin, a)),
                _ => None,
            };
            let future = ctx.read_native_pin(future_pin, future);
            IoFutureInner::fire_notifier(
                ctx,
                future,
                &NotifierEntry {
                    notifier,
                    attachment,
                },
            );
        }
        ctx.read_native_pin(future_pin, future)
    });
    // `base` is the oldest pin taken here: releasing it releases them all.
    if let Some(base) = base {
        ctx.unpin_native_roots(base);
    }
    fired.map(Some)
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

pub struct Registries {
    /// `handle -> (owning vm_identity, map)`. The owner is what lets the
    /// `xnio-futures` GC root row scan and remap only the collecting VM's
    /// `OptionValue::Obj` refs (gc-common w8-a); handles are process-unique.
    pub maps: Mutex<HashMap<i64, (usize, Arc<OptionMapInner>)>>,
    /// `handle -> (owning vm_identity, builder)`, as `maps`.
    pub builders: Mutex<HashMap<i64, (usize, Arc<BuilderInner>)>>,
    pub futures: Mutex<HashMap<i64, Arc<IoFutureInner>>>,
    map_handles_by_obj: Mutex<HashMap<usize, i64>>,
    builder_handles_by_obj: Mutex<HashMap<usize, i64>>,
    /// Lock key of an `IoFuture` / `FutureResult` object -> the `futures`
    /// handle in its slot (gc-common w17-b). Only [`forget_xnio_obj_keys`]
    /// reads it: it is how a dead future object's `futures` row is found.
    future_handles_by_obj: Mutex<HashMap<usize, i64>>,
    pub next_handle: AtomicU8,
}

fn registries() -> &'static Registries {
    static R: OnceLock<Registries> = OnceLock::new();
    R.get_or_init(|| Registries {
        maps: Mutex::new(HashMap::new()),
        builders: Mutex::new(HashMap::new()),
        futures: Mutex::new(HashMap::new()),
        map_handles_by_obj: Mutex::new(HashMap::new()),
        builder_handles_by_obj: Mutex::new(HashMap::new()),
        future_handles_by_obj: Mutex::new(HashMap::new()),
        next_handle: AtomicU8::new(0),
    })
}

// Real XNIO classes loaded from the JDK/WildFly classpath can have too few
// instance fields for our synthetic trailing handle slot. Key the fallback by
// GC-stable object identity.
//
// gc-common w17-b (`common-w16b-charset-bom-and-xnio-identity-key-registries`):
// the key comes from the lib.rs lock-key registry (`gc_stable_weak_lock_key`),
// as `properties_sidetable::key_for` (w15-b) and `jca::key_factory`'s
// `kpg_obj_key_for` (w16-b) already do. This used to be a private copy of that
// registry's identity-hash + generation scheme. It was VM-scoped (gc-common
// w9-b), but it was pruned only at VM teardown, so it kept one slot per
// OptionMap / Builder ever keyed -- and with each slot a `maps` / `builders`
// row whose `OptionValue::Obj` refs the `xnio-futures` root row kept alive --
// and it rebound the lone slot of a hash, with no hash-0 guard, to whatever
// pointer the caller passed: a dead object's slot to a new object, or a live
// object's slot to a stale pre-move copy of its address. The shared registry
// is VM-scoped too, its keys are unique across VMs and never re-minted, it
// re-addresses its slots after every moving collection and never rebinds
// them, and it frees a key at the first sweep that finds the object dead; the
// sweep then calls [`forget_xnio_obj_keys`].
fn xnio_obj_key_for(ctx: &dyn NativeContext, obj: ObjectRef) -> usize {
    crate::gc_stable_weak_lock_key(ctx, obj)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// Drop the `map_handles_by_obj` / `builder_handles_by_obj` /
/// `future_handles_by_obj` rows filed under `keys`, which the lock-key
/// registry has just freed (the OptionMap, Builder, IoFuture or FutureResult
/// died, or its VM is being torn down) and will never mint again, and the
/// `maps` / `builders` / `futures` rows those rows name.
///
/// A handle names exactly one object: every `register_map` /
/// `register_builder` / `register_future` site mints a fresh handle for the
/// one object it then files it under (`remember_option_map_inner`,
/// `native_option_map_builder`, `alloc_io_future`,
/// `native_future_result_init`), and nothing copies a handle into a second
/// object. So once the object is dead, nothing reads the handle again, and its
/// row can go too: a map's or builder's `OptionValue::Obj` refs, and a
/// future's result / notifier / attachment refs (through the `live_futures`
/// `Weak`, which stops upgrading once no handle holds the `Arc`), were rooted
/// by `gc_scan_xnio_future_roots` until VM teardown, and a future's forever.
/// Two handles may share one future `Arc` (a `FutureResult` and each
/// `IoFuture` `getIoFuture` built for it); it goes with the last. A caller
/// that already looked a row up keeps its own `Arc`.
///
/// Called by `lib.rs::sweep_lock_keys` and `lib.rs::forget_vm_lock_keys` with
/// the registry lock released; each table lock is a leaf, taken on its own,
/// and the removed rows are dropped after the last guard.
pub fn forget_xnio_obj_keys(keys: &[usize]) {
    fn take_handles(t: &Mutex<HashMap<usize, i64>>, keys: &[usize]) -> Vec<i64> {
        let mut t = t.lock();
        if t.is_empty() {
            return Vec::new();
        }
        keys.iter().filter_map(|k| t.remove(k)).collect()
    }
    if keys.is_empty() {
        return;
    }
    let future_handles = take_handles(&registries().future_handles_by_obj, keys);
    let dropped_futures: Vec<Arc<IoFutureInner>> = if future_handles.is_empty() {
        Vec::new()
    } else {
        let mut futures = registries().futures.lock();
        future_handles
            .iter()
            .filter_map(|h| futures.remove(h))
            .collect()
    };
    drop(dropped_futures);
    let map_handles = take_handles(&registries().map_handles_by_obj, keys);
    let builder_handles = take_handles(&registries().builder_handles_by_obj, keys);
    let dropped_maps: Vec<(usize, Arc<OptionMapInner>)> = if map_handles.is_empty() {
        Vec::new()
    } else {
        let mut maps = registries().maps.lock();
        map_handles.iter().filter_map(|h| maps.remove(h)).collect()
    };
    let dropped_builders: Vec<(usize, Arc<BuilderInner>)> = if builder_handles.is_empty() {
        Vec::new()
    } else {
        let mut builders = registries().builders.lock();
        builder_handles
            .iter()
            .filter_map(|h| builders.remove(h))
            .collect()
    };
    drop(dropped_maps);
    drop(dropped_builders);
}

/// Drop the `map_handles_by_obj` / `builder_handles_by_obj` rows that name a
/// store VM `vm` registered (VM teardown). `lib.rs::forget_vm_lock_keys`
/// already dropped the rows filed under `vm`'s weak keys; this catches a row
/// whose key some other caller also minted HELD (`gc_stable_lock_key`), which
/// teardown drops without reporting it freed. Call before the stores go.
fn forget_vm_xnio_obj_keys(vm: usize) {
    let owned_maps: std::collections::HashSet<i64> = registries()
        .maps
        .lock()
        .iter()
        .filter(|(_, (owner, _))| *owner == vm)
        .map(|(h, _)| *h)
        .collect();
    let owned_builders: std::collections::HashSet<i64> = registries()
        .builders
        .lock()
        .iter()
        .filter(|(_, (owner, _))| *owner == vm)
        .map(|(h, _)| *h)
        .collect();
    if !owned_maps.is_empty() {
        registries()
            .map_handles_by_obj
            .lock()
            .retain(|_, h| !owned_maps.contains(&*h));
    }
    if !owned_builders.is_empty() {
        registries()
            .builder_handles_by_obj
            .lock()
            .retain(|_, h| !owned_builders.contains(&*h));
    }
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

/// File the `futures` handle `handle` under the lock key of the `IoFuture` /
/// `FutureResult` object `obj` that carries it in its slot, so the row goes
/// when `obj` dies ([`forget_xnio_obj_keys`]). `obj` must be current.
fn remember_future_handle(ctx: &dyn NativeContext, obj: ObjectRef, handle: i64) {
    let key = xnio_obj_key_for(ctx, obj);
    registries()
        .future_handles_by_obj
        .lock()
        .insert(key, handle);
}

fn remember_option_map_inner(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    inner: Arc<OptionMapInner>,
) -> Arc<OptionMapInner> {
    let h = register_map(ctx.vm_identity(), inner.clone());
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

/// Register `inner` as a map owned by VM `vm` (`ctx.vm_identity()`).
pub fn register_map(vm: usize, inner: Arc<OptionMapInner>) -> i64 {
    let h = next_handle();
    registries().maps.lock().insert(h, (vm, inner));
    h
}

pub fn lookup_map(handle: i64) -> Option<Arc<OptionMapInner>> {
    registries()
        .maps
        .lock()
        .get(&handle)
        .map(|(_, inner)| inner.clone())
}

/// Register `inner` as a builder owned by VM `vm` (`ctx.vm_identity()`).
pub fn register_builder(vm: usize, inner: Arc<BuilderInner>) -> i64 {
    let h = next_handle();
    registries().builders.lock().insert(h, (vm, inner));
    h
}

pub fn lookup_builder(handle: i64) -> Option<Arc<BuilderInner>> {
    registries()
        .builders
        .lock()
        .get(&handle)
        .map(|(_, inner)| inner.clone())
}

pub fn register_future(inner: Arc<IoFutureInner>) -> i64 {
    let h = next_handle();
    registries().futures.lock().insert(h, inner);
    h
}

pub fn lookup_future(handle: i64) -> Option<Arc<IoFutureInner>> {
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
//
// gc-common w17-b: that bound held only in name, since the handle map was
// never pruned either -- every future ever created stayed upgradeable and
// kept its refs rooted for the VM's life (and its `Arc` past it). A handle row
// now goes when the IoFuture / FutureResult object carrying it dies
// (`future_handles_by_obj`, [`forget_xnio_obj_keys`]).

//
// gc-common w8-a (`common-b-process-global-root-sources`, row `xnio-futures`):
// every entry carries the `vm_identity` of the VM that created it -- the
// registry, like the map/builder registries, is shared by every VM in the
// process -- and each VM's collection roots and remaps only its own entries.

fn live_futures() -> &'static Mutex<Vec<(usize, Weak<IoFutureInner>)>> {
    static LIVE: OnceLock<Mutex<Vec<(usize, Weak<IoFutureInner>)>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Add a freshly-created future of VM `vm` to the GC-scan registry. Called
/// once from `IoFutureInner::new_waiting` (the sole construction site).
/// Opportunistically drops already-dead `Weak`s so the vector cannot grow
/// without bound across a long-lived process that never triggers a moving GC.
fn register_live_future(vm: usize, inner: &Arc<IoFutureInner>) {
    let mut live = live_futures().lock();
    live.retain(|(_, w)| w.strong_count() > 0);
    live.push((vm, Arc::downgrade(inner)));
}

/// The OptionMaps VM `vm` registered, cloned out so the registry lock is not
/// held while an entry lock is taken.
fn owned_option_maps(vm: usize) -> Vec<Arc<OptionMapInner>> {
    registries()
        .maps
        .lock()
        .values()
        .filter(|(owner, _)| *owner == vm)
        .map(|(_, inner)| inner.clone())
        .collect()
}

/// The Builders VM `vm` registered; as [`owned_option_maps`].
fn owned_option_map_builders(vm: usize) -> Vec<Arc<BuilderInner>> {
    registries()
        .builders
        .lock()
        .values()
        .filter(|(owner, _)| *owner == vm)
        .map(|(_, inner)| inner.clone())
        .collect()
}

/// Per-VM teardown for the `xnio-futures` row: drop `vm`'s entries from the
/// future, OptionMap and Builder registries. Idempotent. Another VM's
/// collection never visits them, so this returns memory and retires raw
/// addresses into a heap that is going away (gc-common w8-a).
pub fn forget_vm_xnio_roots(vm: usize) {
    forget_vm_xnio_obj_keys(vm);
    forget_vm_xnio_options(vm);
    live_futures().lock().retain(|(owner, _)| *owner != vm);
    registries()
        .maps
        .lock()
        .retain(|_, (owner, _)| *owner != vm);
    registries()
        .builders
        .lock()
        .retain(|_, (owner, _)| *owner != vm);
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
pub fn gc_scan_xnio_future_roots(vm: usize, roots: &mut Vec<cratonvm_types::ObjectRef>) {
    let mut live = live_futures().lock();
    live.retain(|(owner, weak)| {
        let Some(inner) = weak.upgrade() else {
            return false; // future is gone — drop the dead Weak
        };
        if *owner != vm {
            return true; // another VM's future: its collection's to visit
        }
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
    // locks are taken. Only `vm`'s entries, as above.
    let maps = owned_option_maps(vm);
    for m in maps {
        for v in m.entries.lock().values() {
            if let OptionValue::Obj(Some(r)) = v {
                if !r.as_ptr().is_null() {
                    roots.push(*r);
                }
            }
        }
    }
    let builders = owned_option_map_builders(vm);
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
pub fn gc_update_xnio_future_refs(vm: usize, map: &cratonvm_types::PointerMap) {
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
    live.retain(|(owner, weak)| {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        if *owner != vm {
            return true;
        }
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
    let maps = owned_option_maps(vm);
    for m in maps {
        for v in m.entries.lock().values_mut() {
            if let OptionValue::Obj(Some(r)) = v {
                remap(r);
            }
        }
    }
    let builders = owned_option_map_builders(vm);
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
    let h = register_builder(ctx.vm_identity(), inner);
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
    // gc-common w17-b: a real OptionMap `source` is walked through Java, so
    // `this` (returned for chaining) is pinned across it and re-read.
    let this_pin = ctx.pin_native_root(this);
    let source_inner = inner_from_map_any(ctx, source);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.unpin_native_roots(this_pin);
    let source_inner = source_inner?;

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

/// The entries of a REAL `org.xnio.OptionMap`'s backing `Map`, read through its
/// own `entrySet()` iterator.
///
/// gc-common w17-b: every call below runs Java and can collect. The map, the
/// iterator and each entry were used at their pre-call addresses (`hasNext`,
/// `next`, `getValue` on stale receivers), and an `OptionValue::Obj` value was
/// a raw copy that no root covered until the caller registered the map --
/// every later entry's calls could move or free it. Now the map, the
/// iterator, each entry and its key are pinned and re-read before each use,
/// and each object value is pinned until every entry is read, then re-read.
/// The pins are released before returning, so the caller must register the
/// result before it next allocates (`inner_from_map_any` does: `register_map`
/// then roots and remaps the values through the `xnio-futures` row).
fn collect_real_option_map_entries(
    ctx: &mut dyn NativeContext,
    map: ObjectRef,
) -> HashMap<OptionKey, OptionValue> {
    let base = ctx.pin_native_root(map);
    let mut rows: Vec<(OptionKey, OptionValue, Option<usize>)> = Vec::new();
    collect_real_option_map_rows(ctx, base, map, &mut rows);
    let mut out = HashMap::with_capacity(rows.len());
    for (key, value, pin) in rows {
        let value = match (value, pin) {
            (OptionValue::Obj(Some(o)), Some(p)) => {
                OptionValue::Obj(Some(ctx.read_native_pin(p, o)))
            }
            (v, _) => v,
        };
        out.insert(key, value);
    }
    // Releases every pin taken below `base` too.
    ctx.unpin_native_roots(base);
    out
}

/// [`collect_real_option_map_entries`]'s loop. `map` is pinned at `map_pin`.
/// Pins pushed here are never released here (the pin stack is released from
/// a base onward, and the value pins must outlive the loop): the caller's
/// `unpin_native_roots(map_pin)` releases them all. `pin` in a row is the pin
/// of its `OptionValue::Obj` value.
fn collect_real_option_map_rows(
    ctx: &mut dyn NativeContext,
    map_pin: usize,
    map: ObjectRef,
    rows: &mut Vec<(OptionKey, OptionValue, Option<usize>)>,
) {
    let map = ctx.read_native_pin(map_pin, map);
    let Some(set) = invoke_object(ctx, map, "entrySet", "()Ljava/util/Set;", &[]) else {
        return;
    };
    let Some(it) = invoke_object(ctx, set, "iterator", "()Ljava/util/Iterator;", &[]) else {
        return;
    };
    let it_pin = ctx.pin_native_root(it);
    loop {
        let it_now = ctx.read_native_pin(it_pin, it);
        let has_next = matches!(
            ctx.invoke_virtual(it_now, "hasNext", "()Z", &[]),
            Ok(Some(Value::Int(v))) if v != 0
        );
        if !has_next {
            break;
        }
        let it_now = ctx.read_native_pin(it_pin, it);
        let Some(entry) = invoke_object(ctx, it_now, "next", "()Ljava/lang/Object;", &[]) else {
            break;
        };
        let entry_pin = ctx.pin_native_root(entry);
        let Some(opt) = invoke_object(ctx, entry, "getKey", "()Ljava/lang/Object;", &[]) else {
            continue;
        };
        let opt_pin = ctx.pin_native_root(opt);
        // `read_option_coords` only reads fields and strings.
        let Some((declaring_class, name)) = read_option_coords(ctx, opt) else {
            continue;
        };
        let entry = ctx.read_native_pin(entry_pin, entry);
        let value = ctx
            .invoke_virtual(entry, "getValue", "()Ljava/lang/Object;", &[])
            .ok()
            .flatten()
            .unwrap_or(Value::Object(None));
        let opt = ctx.read_native_pin(opt_pin, opt);
        // `option_value_from_java` may unbox through Java and then fall back
        // to the raw value, so the value is pinned across it and re-read by
        // the caller once the loop is done.
        let value_pin = match value {
            Value::Object(Some(v)) => Some(ctx.pin_native_root(v)),
            _ => None,
        };
        let converted = option_value_from_java(ctx, opt, value);
        let pin = match converted {
            OptionValue::Obj(Some(_)) => value_pin,
            _ => None,
        };
        rows.push((
            OptionKey {
                declaring_class,
                name,
            },
            converted,
            pin,
        ));
    }
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
        // gc-common w17-b: the walk runs Java; `this` is re-read after it for
        // the handle-slot write and the key below.
        let this_pin = ctx.pin_native_root(this);
        let entries = collect_real_option_map_entries(ctx, map);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
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
    // gc-common w17-b: a real OptionMap is walked through Java; `opt` is read
    // after it, so it is pinned across it.
    let opt_pin = ctx.pin_native_root(opt);
    let inner = inner_from_map_any(ctx, this);
    let opt = ctx.read_native_pin(opt_pin, opt);
    ctx.unpin_native_roots(opt_pin);
    let inner = inner?;
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
    // gc-common w17-b: as `option_map_get_key`.
    let opt_pin = ctx.pin_native_root(opt);
    let inner = inner_from_map_any(ctx, this);
    let opt = ctx.read_native_pin(opt_pin, opt);
    ctx.unpin_native_roots(opt_pin);
    let inner = inner?;
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
    // gc-common w17-b: every allocation below can collect. Each `Option` was
    // written at its pre-`create_string` address, and the finished ones sat
    // in a plain `Vec` that no root covered until the array was filled, so a
    // collection during a later key's allocations freed or moved them. Each
    // option is pinned from its allocation until the array holds it (its
    // declaring-class string across the second `create_string`), and re-read
    // before every store. Unpinning the first option's pin releases them all.
    let mut opt_pins: Vec<(usize, ObjectRef)> = Vec::with_capacity(keys.len());
    for key in &keys {
        let opt = match try_alloc_concurrent_synthetic(ctx, "org/xnio/Option", 3) {
            Ok(o) => o,
            Err(e) => {
                if let Some(&(base, _)) = opt_pins.first() {
                    ctx.unpin_native_roots(base);
                }
                return Err(e);
            }
        };
        let opt_pin = ctx.pin_native_root(opt);
        let declaring = ctx.create_string(&key.declaring_class);
        let declaring_pin = ctx.pin_native_root(declaring);
        let name = ctx.create_string(&key.name);
        let declaring = ctx.read_native_pin(declaring_pin, declaring);
        let opt = ctx.read_native_pin(opt_pin, opt);
        ctx.set_field(opt, OPT_DECLARING_CLASS, Value::Object(Some(declaring)));
        ctx.set_field(opt, OPT_NAME, Value::Object(Some(name)));
        ctx.set_field(opt, OPT_TYPE_CLASS, Value::Object(None));
        opt_pins.push((opt_pin, opt));
    }
    let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), opt_pins.len());
    for (i, &(pin, opt)) in opt_pins.iter().enumerate() {
        let opt = ctx.read_native_pin(pin, opt);
        ctx.set_array_element(arr, i, Value::Object(Some(opt)));
    }
    if let Some(&(base, _)) = opt_pins.first() {
        ctx.unpin_native_roots(base);
    }
    cratonvm_native_collections::make_iterator_from_array(ctx, arr, opt_pins.len())
}

// ---------------------------------------------------------------------------
// Natives: Options (well-known constants)
// ---------------------------------------------------------------------------

/// Build and populate the Options static fields on first access.
fn ensure_options_initialized(ctx: &mut dyn NativeContext) -> MethodCallResult {
    // If the class isn't loaded yet, load it — allocating the singletons
    // requires a class id.
    let _ = ctx.ensure_class_initialized("org/xnio/Options")?;
    // Per VM (gc-common w9-b). This latch was one per process, so only the
    // FIRST VM ever ran the fill below: every later VM's `Options` statics
    // stayed null and its accessors answered the first VM's `Option` objects.
    let vm = ctx.vm_identity();
    if OPTIONS_FILLED.peek(vm, |done| *done).unwrap_or(false) {
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
    // gc-common w17-b: every allocation below can collect. `declaring` was
    // written into every option from its pre-loop address, and each `opt`
    // got its name, its type and then its var-handle root (the one the
    // accessors read back) at the address it had before the two
    // `create_string`s. `declaring` is pinned for the loop; each `opt` and
    // its name string are pinned across the allocations that follow them, and
    // every reference is re-read before it is stored.
    let declaring = ctx.create_string("org/xnio/Options");
    let declaring_pin = ctx.pin_native_root(declaring);
    for (name, ty) in specs {
        let opt = match try_alloc_concurrent_synthetic(ctx, "org/xnio/Option", 3) {
            Ok(o) => o,
            Err(e) => {
                ctx.unpin_native_roots(declaring_pin);
                return Err(e);
            }
        };
        let opt_pin = ctx.pin_native_root(opt);
        let name_s = ctx.create_string(name);
        let name_pin = ctx.pin_native_root(name_s);
        let ty_s = ctx.create_string(ty);
        let name_s = ctx.read_native_pin(name_pin, name_s);
        let opt = ctx.read_native_pin(opt_pin, opt);
        let declaring = ctx.read_native_pin(declaring_pin, declaring);
        // Releases `opt_pin` and `name_pin`; nothing allocates from here to
        // the end of the iteration.
        ctx.unpin_native_roots(opt_pin);
        ctx.set_field(opt, OPT_DECLARING_CLASS, Value::Object(Some(declaring)));
        ctx.set_field(opt, OPT_NAME, Value::Object(Some(name_s)));
        ctx.set_field(opt, OPT_TYPE_CLASS, Value::Object(Some(ty_s)));
        // Store in a global map for retrieval (reflective getField + the
        // synthetic getXXX accessors below read from here). Keep each Option
        // alive and remapped across GC moves through a global root of this VM
        // (gc-common w29-d; it used to be the identity-hash-keyed var-handle
        // registry, whose read answers the FIRST object registered under a
        // hash). (The static-field write below keeps the object alive too,
        // but the FIELD is remapped by the statics scan — this raw map copy is
        // not, hence the root indirection for reads.)
        let oroot = ctx.add_global_root(opt);
        let replaced = OPTIONS_STORE.with(vm, |store| store.insert(name.to_string(), (oroot, opt)));
        // A racing fill of the same VM replaced a row: release its root.
        if let Some((old_root, _)) = replaced {
            if old_root != 0 {
                ctx.remove_global_root(old_root);
            }
        }
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
    ctx.unpin_native_roots(declaring_pin);
    OPTIONS_FILLED.with(vm, |done| *done = true);
    Ok(None)
}

/// Option-name → `(global_root, ObjectRef)`.
///
/// GC (gc-followups-20260706): every lookup resolves the CURRENT address
/// through the row's root — the GC remaps the root after a move, never this
/// raw static copy. Before that fix `lookup_options_field` handed Java a bare
/// cached `ObjectRef` that went stale after any moving GC. Bounded by the
/// well-known option set.
///
/// gc-common w29-d: the root is a JNI global root of the owning VM (handle 0,
/// and the raw copy as the answer, only in a context without global roots:
/// the mocks). It used to be `register_var_handle_root` plus an identity-hash
/// key read back through `read_var_handle_root`, a registry keyed by identity
/// hash whose read answers the FIRST object registered under that hash.
///
/// PER VM (gc-common w9-b,
/// `common-w8a-process-global-object-singletons-in-native-builtins`): a root
/// resolves in the CALLING VM, so a process-wide store handed VM B either an
/// unrelated object of its own or VM A's raw heap address.
static OPTIONS_STORE: cratonvm_native_api::vm_scoped::VmScoped<
    HashMap<String, (usize, ObjectRef)>,
> = cratonvm_native_api::vm_scoped::VmScoped::new();

/// Whether this VM's `Options` fill has completed (see
/// `ensure_options_initialized`).
static OPTIONS_FILLED: cratonvm_native_api::vm_scoped::VmScoped<bool> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Drop `vm`'s `Options` store and fill latch (VM teardown). Idempotent.
fn forget_vm_xnio_options(vm: usize) {
    OPTIONS_STORE.forget(vm);
    OPTIONS_FILLED.forget(vm);
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
    // Copy the row out before touching `ctx` (`VmScoped` lock discipline).
    let found = OPTIONS_STORE
        .peek(ctx.vm_identity(), |store| store.get(name).copied())
        .flatten();
    match found {
        Some((root, cached)) => {
            // Resolve the CURRENT (post-GC) address — the global root is
            // remapped after a move, this raw copy is not.
            let current = if root == 0 {
                cached
            } else {
                ctx.resolve_global_root(root).unwrap_or(cached)
            };
            Ok(Some(Value::Object(Some(current))))
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
    // gc-common w17-b: the row goes with `obj` (just allocated: current).
    remember_future_handle(ctx, obj, h);
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
    //
    // Interpreter round i3 wave 83, lane L6: xnio-api 3.8's
    // `AbstractIoFuture.await()` waits in `LockSupport.park(this)`: WAITING
    // (parking) on HotSpot, so the region says `Parked`.
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
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
    //
    // Interpreter round i3 wave 83, lane L6: xnio-api 3.8's
    // `AbstractIoFuture.await(long, TimeUnit)` waits in
    // `LockSupport.parkNanos(this, ..)`: TIMED_WAITING (parking).
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::TimedParked);
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
    let mut this = this;
    if inner.try_transition(STATUS_CANCELLED) {
        ctx.set_field(this, IOF_STATUS, Value::Int(STATUS_CANCELLED as i32));
        let notifiers = inner.drain_notifiers();
        inner.cv.notify_all();
        // gc-common w23-b: the notifiers run Java, and `this` was returned as
        // the address it had before them; the drained entries after the first
        // were fired unrooted (`fire_drained_notifiers`).
        if let Some(current) = fire_drained_notifiers(ctx, &inner, Some(this), &notifiers)? {
            this = current;
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
    let entry = NotifierEntry {
        notifier,
        attachment,
    };

    // gc-common w23-b: the status is read under the state lock. It was read
    // before the lock, so a future settled between that read and the push
    // had already drained its list, and the pushed notifier never fired. A
    // settle CASes the status first and drains under this lock after, so
    // under the lock "still WAITING" means the drain is yet to come.
    let settled = {
        let mut state = inner.state.lock();
        if inner.status.load(Ordering::SeqCst) == STATUS_WAITING {
            state.notifiers.push(entry);
            false
        } else {
            true
        }
    };
    // Already settled: fire now, outside the lock.
    if settled {
        IoFutureInner::fire_notifier(ctx, this, &entry);
    }
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
    //
    // Interpreter round i3 wave 83, lane L6: xnio-api 3.8's
    // `AbstractIoFuture.get()` is `await()` first, so `Parked` as there.
    {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::Parked);
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

    let inner = IoFutureInner::new_waiting(ctx.vm_identity());
    let h = register_future(inner.clone());
    ctx.set_field(this, FR_FUTURE_HANDLE, Value::Long(h));
    // gc-common w17-b: the row goes with `this` (nothing allocated since the
    // native entry: current).
    remember_future_handle(ctx, this, h);
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

    // Rebuild a JVM-facing IoFuture object for the fire path (gc-common
    // w23-b: with the drained entries rooted across it).
    fire_drained_notifiers(ctx, &inner, None, &notifiers)?;
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
    fire_drained_notifiers(ctx, &inner, None, &notifiers)?;
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
    fire_drained_notifiers(ctx, &inner, None, &notifiers)?;
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn ioex<S: Into<String>>(m: S) -> MethodCallFailed {
    RuntimeError::IOException { message: m.into() }.into()
}
pub fn iae<S: Into<String>>(m: S) -> MethodCallFailed {
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
        native_option_map_clinit, cratonvm_native_api::NativeKind::Bridge,
    );
    // EMPTY is a public static field — bytecode reading it via
    // `GETSTATIC Lorg/xnio/OptionMap;` goes through the field-access
    // path.  We still register a getter so native callers can cheaply
    // materialise the singleton.
    registry.register(
        "org/xnio/OptionMap",
        "empty",
        "()Lorg/xnio/OptionMap;",
        native_option_map_empty_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "builder",
        "()Lorg/xnio/OptionMap$Builder;",
        native_option_map_builder, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;)Ljava/lang/Object;",
        native_option_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Ljava/lang/Object;",
        native_option_map_get, cratonvm_native_api::NativeKind::Bridge,
    );
    // Typed-overload variants all delegate to the same implementation;
    // the NativeContext's raw Value machinery takes care of coercion
    // on the return.
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;I)I",
        native_option_map_get_int, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;J)J",
        native_option_map_get_long, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "get",
        "(Lorg/xnio/Option;Z)Z",
        native_option_map_get_bool, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap",
        "contains",
        "(Lorg/xnio/Option;)Z",
        native_option_map_contains, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register("org/xnio/OptionMap", "size", "()I", native_option_map_size, cratonvm_native_api::NativeKind::Bridge);
    registry.register(
        "org/xnio/OptionMap",
        "iterator",
        "()Ljava/util/Iterator;",
        native_option_map_iterator, cratonvm_native_api::NativeKind::Bridge,
    );

    // Builder
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;Ljava/lang/Object;)Lorg/xnio/OptionMap$Builder;",
        native_builder_set, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;I)Lorg/xnio/OptionMap$Builder;",
        native_builder_set, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;J)Lorg/xnio/OptionMap$Builder;",
        native_builder_set, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "set",
        "(Lorg/xnio/Option;Z)Lorg/xnio/OptionMap$Builder;",
        native_builder_set_bool, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "addAll",
        "(Lorg/xnio/OptionMap;)Lorg/xnio/OptionMap$Builder;",
        native_builder_add_all, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/OptionMap$Builder",
        "getMap",
        "()Lorg/xnio/OptionMap;",
        native_builder_get_map, cratonvm_native_api::NativeKind::Bridge,
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
        native_options_get_worker_io_threads, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_TASK_CORE_THREADS",
        "()Lorg/xnio/Option;",
        native_options_get_worker_task_core_threads, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_TASK_MAX_THREADS",
        "()Lorg/xnio/Option;",
        native_options_get_worker_task_max_threads, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getWORKER_NAME",
        "()Lorg/xnio/Option;",
        native_options_get_worker_name, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getBACKLOG",
        "()Lorg/xnio/Option;",
        native_options_get_backlog, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getKEEP_ALIVE",
        "()Lorg/xnio/Option;",
        native_options_get_keep_alive, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getTCP_NODELAY",
        "()Lorg/xnio/Option;",
        native_options_get_tcp_nodelay, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getREUSE_ADDRESSES",
        "()Lorg/xnio/Option;",
        native_options_get_reuse_addresses, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getREAD_TIMEOUT",
        "()Lorg/xnio/Option;",
        native_options_get_read_timeout, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getWRITE_TIMEOUT",
        "()Lorg/xnio/Option;",
        native_options_get_write_timeout, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/Options",
        "getSSL_ENABLED",
        "()Lorg/xnio/Option;",
        native_options_get_ssl_enabled, cratonvm_native_api::NativeKind::Bridge,
    );

    // IoFuture
    registry.register(
        "org/xnio/IoFuture",
        "getStatus",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_get_status, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "await",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_await, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "awaitInterruptibly",
        "()Lorg/xnio/IoFuture$Status;",
        native_iof_await, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "await",
        "(JLjava/util/concurrent/TimeUnit;)Lorg/xnio/IoFuture$Status;",
        native_iof_await_timed, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "get",
        "()Ljava/lang/Object;",
        native_iof_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "getInterruptibly",
        "()Ljava/lang/Object;",
        native_iof_get, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "cancel",
        "()Lorg/xnio/IoFuture;",
        native_iof_cancel, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/IoFuture",
        "addNotifier",
        "(Lorg/xnio/IoFuture$Notifier;Ljava/lang/Object;)V",
        native_iof_add_notifier, cratonvm_native_api::NativeKind::Bridge,
    );

    // FutureResult
    registry.register(
        "org/xnio/FutureResult",
        "<init>",
        "()V",
        native_future_result_init, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/FutureResult",
        "getIoFuture",
        "()Lorg/xnio/IoFuture;",
        native_future_result_get_future, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setResult",
        "(Ljava/lang/Object;)Z",
        native_future_result_set_result, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setException",
        "(Ljava/io/IOException;)Z",
        native_future_result_set_exception, cratonvm_native_api::NativeKind::Bridge,
    );
    registry.register(
        "org/xnio/FutureResult",
        "setCancelled",
        "()Z",
        native_future_result_set_cancelled, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// gc-common w9-b (`common-w8a-identity-hash-keyed-registries-are-shared-across-vms`,
/// `common-w8a-process-global-object-singletons-in-native-builtins`).
#[cfg(test)]
mod w9b_xnio_vm_isolation_tests;

/// gc-common w17-b (`common-w16b-charset-bom-and-xnio-identity-key-registries`).
/// The mock's identity hash is the address truncated to `i32`, so addresses
/// 4 GiB apart share a hash. Fake addresses end in 0 and are never
/// dereferenced (only the object-key side table is exercised, never the
/// handle slot). Each test has its own VM identities: the lock-key registry
/// and these tables are process-wide.
#[cfg(test)]
mod w17b_xnio_key_tests;
