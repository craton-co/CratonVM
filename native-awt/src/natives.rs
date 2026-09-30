// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! AWT/Swing/Java2D native method registration.
//!
//! Registers native callbacks for `java/awt/*`, `javax/swing/*`, and
//! `sun/java2d/*` classes. Each callback maps Java-side API calls to
//! the Rust peer/renderer/EDT infrastructure in this crate.

use std::sync::{Arc, OnceLock};

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::edt;
use crate::edt::InvokeAndWaitError;
use crate::edt::VmRoot;
use crate::event::{event_id, AwtEvent, PeerId};
use crate::graphics2d::Graphics2DState;
use crate::image::{self, ImageId, ImageType};
use crate::peer::{self, ComponentType};
use crate::swing;

// ---------------------------------------------------------------------------
// InvocationEvent ↔ callback_id side-table
// ---------------------------------------------------------------------------
//
// `EventQueue.getNextEvent` allocates a Java `java/awt/event/InvocationEvent`
// for each dequeued `AwtEventData::Invocation`.  We bind that Java object's
// identity hash to the `callback_id` here so the
// `InvocationEvent.dispatch()V` native (which only receives the event
// object as `this`) can find the matching Runnable via
// `edt::edt_for_vm(vm).take_runnable_root(callback_id)` (the dispatch native
// resolves the root only in its own VM -- see the dispatch native).
//
// A separate side-table (rather than a synthetic field on the event) avoids
// having to teach the JDK class layout about an extra slot — synthetic
// fields require coordinating with class-loader synthesis and break when
// the real-JDK class is loaded.

/// Maximum number of pending event-hash → callback_id mappings. If an
/// `InvocationEvent` is never `dispatch`'d (GC'd before the EDT picks it up,
/// for example), the entry would leak forever. Cap with FIFO eviction so a
/// misbehaving app can't grow this map without bound.
const MAX_INVOCATION_CALLBACKS: usize = 10_000;

/// `(vm_identity, InvocationEvent identity hash)`: the BUCKET of a binding.
/// gc-common w22-a: the key used to be the bare hash, so VM B's
/// `InvocationEvent.dispatch()` on an event whose hash collided with a
/// pending VM A event took A's callback id (and with it A's Runnable row).
///
/// gc-common w29-c (`common-w28b-remaining-identity-hash-keyed-side-tables`
/// rank 30, route R2): a bucket holds one row per pending EVENT, each naming
/// its event by a WEAK global root (`NativeContext::add_weak_global_root`, the
/// [`KeyedRows`] scheme of the peer / Graphics / image tables). Keyed by the
/// hash alone, two undispatched events of one VM with equal identity hashes
/// shared one row: the second `getNextEvent` REPLACED the first's callback id,
/// so the first event's `dispatch()` ran the second's Runnable on the EDT and
/// the second found nothing (its `invokeAndWait` caller waited for a
/// completion that was signalled for the wrong event). A dispatch now takes
/// only the row whose weak root resolves to its own receiver. A weak root
/// follows its event through every moving collection, so nothing here has to
/// be re-addressed; a row with no weak root (0: a context with no collector,
/// e.g. a test mock) matches any event of its bucket, the pre-w29 behaviour.
type InvocationKey = (usize, i32);

/// One pending binding: the event's weak root (0: none) and its callback id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InvocationRow {
    weak: usize,
    callback_id: u64,
}

/// Does `row` belong to `event`? A row without a weak root belongs to any
/// event of its bucket; a cleared one (its event was collected) to none.
fn invocation_row_is(
    row: InvocationRow,
    event: ObjectRef,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> bool {
    row.weak == 0 || resolve(row.weak) == Some(event)
}

struct InvocationCallbackTable {
    map: FxHashMap<InvocationKey, Vec<InvocationRow>>,
    /// Every row once, oldest first, for the cap's FIFO eviction:
    /// `(bucket, callback id)` (callback ids are unique per VM queue).
    order: std::collections::VecDeque<(InvocationKey, u64)>,
}

impl InvocationCallbackTable {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            order: std::collections::VecDeque::new(),
        }
    }

    /// Drop `vm`'s rows (VM teardown). Their weak roots index the dying VM's
    /// own table, which goes with it, so they are not released.
    fn forget_vm(&mut self, vm: usize) {
        self.map.retain(|&(owner, _), _| owner != vm);
        self.order.retain(|&((owner, _), _)| owner != vm);
    }

    /// Take the row of `key` with `callback_id` out of the bucket (not out of
    /// `order`).
    fn take_row(&mut self, key: InvocationKey, callback_id: u64) -> Option<InvocationRow> {
        let rows = self.map.get_mut(&key)?;
        let at = rows.iter().position(|row| row.callback_id == callback_id)?;
        let row = rows.remove(at);
        if rows.is_empty() {
            self.map.remove(&key);
        }
        Some(row)
    }

    fn forget_order(&mut self, key: InvocationKey, callback_id: u64) {
        if let Some(pos) = self.order.iter().position(|&o| o == (key, callback_id)) {
            self.order.remove(pos);
        }
    }

    /// File `event`'s binding. Answers the rows it took out, whose weak roots
    /// the caller releases (through their own VM, `release_vm_roots`): rows of
    /// the bucket whose event was collected or that already named `event`
    /// (a rebind), a row without a weak root when the new one has none either
    /// (the pre-w29 one-row-per-bucket rule of a context with no collector),
    /// and the oldest rows past [`MAX_INVOCATION_CALLBACKS`]. A row naming
    /// another LIVE event of the bucket is kept.
    fn insert(
        &mut self,
        key: InvocationKey,
        event: ObjectRef,
        row: InvocationRow,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Vec<VmRoot> {
        let mut taken: Vec<InvocationRow> = Vec::new();
        if let Some(rows) = self.map.get_mut(&key) {
            let mut i = 0;
            while i < rows.len() {
                let old = rows[i];
                let stale = match old.weak {
                    0 => row.weak == 0,
                    weak => match resolve(weak) {
                        None => true,
                        Some(current) => current == event,
                    },
                };
                if stale {
                    taken.push(rows.remove(i));
                } else {
                    i += 1;
                }
            }
            if rows.is_empty() {
                self.map.remove(&key);
            }
        }
        for old in &taken {
            self.forget_order(key, old.callback_id);
        }
        let mut released: Vec<VmRoot> = taken
            .into_iter()
            .map(|old| VmRoot {
                vm: key.0,
                handle: old.weak,
            })
            .collect();
        while self.order.len() >= MAX_INVOCATION_CALLBACKS {
            let Some((old_key, old_id)) = self.order.pop_front() else {
                break;
            };
            if let Some(old) = self.take_row(old_key, old_id) {
                released.push(VmRoot {
                    vm: old_key.0,
                    handle: old.weak,
                });
            }
        }
        self.map.entry(key).or_default().push(row);
        self.order.push_back((key, row.callback_id));
        released
    }

    /// Take `event`'s binding out of `key`'s bucket.
    fn remove(
        &mut self,
        key: InvocationKey,
        event: ObjectRef,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Option<InvocationRow> {
        let callback_id = self
            .map
            .get(&key)?
            .iter()
            .find(|row| invocation_row_is(**row, event, resolve))?
            .callback_id;
        let row = self.take_row(key, callback_id)?;
        self.forget_order(key, callback_id);
        Some(row)
    }
}

fn invocation_event_callbacks() -> &'static Mutex<InvocationCallbackTable> {
    static INSTANCE: OnceLock<Mutex<InvocationCallbackTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(InvocationCallbackTable::new()))
}

/// File `event` (identity hash `event_hash` in `vm`, weak root `weak`) as the
/// carrier of `callback_id`. Answers the weak roots to release (see
/// [`InvocationCallbackTable::insert`]).
fn bind_invocation_event(
    vm: usize,
    event_hash: i32,
    event: ObjectRef,
    weak: usize,
    callback_id: u64,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> Vec<VmRoot> {
    invocation_event_callbacks().lock().insert(
        (vm, event_hash),
        event,
        InvocationRow { weak, callback_id },
        resolve,
    )
}

/// Take `event`'s binding (identity hash `event_hash` in `vm`): its callback
/// id and its weak root, which the caller releases.
fn take_invocation_event_callback(
    vm: usize,
    event_hash: i32,
    event: ObjectRef,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> Option<InvocationRow> {
    invocation_event_callbacks()
        .lock()
        .remove((vm, event_hash), event, resolve)
}

/// Take a global root for one of this crate's process-wide tables, tagged
/// with the calling VM. Releases the calling VM's retired roots first (see
/// [`release_vm_roots`]): every path that files a root here is also a point
/// where the owning VM is known to be running.
fn add_global_root_or_oom(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    label: &str,
) -> Result<VmRoot, cratonvm_types::error::MethodCallFailed> {
    drain_retired_awt_roots(ctx);
    let handle = ctx.add_global_root(obj);
    if handle == 0 {
        return Err(RuntimeError::OutOfMemoryError {
            message: format!("failed to create global root for {label}"),
        }
        .into());
    }
    Ok(VmRoot {
        vm: ctx.vm_identity(),
        handle,
    })
}

// ---------------------------------------------------------------------------
// Releasing a root through ITS VM (gc-common w22-a)
// ---------------------------------------------------------------------------
//
// The peer-source table and the EDT's Runnable table are process-wide, so a
// size-cap eviction, a replacement, or a cross-VM dispatch can hand one VM's
// call a root that ANOTHER VM created. Releasing that handle through the
// current `ctx` frees whatever unrelated root sits at the same index of the
// current VM's table (and leaks the real one). Such a root is parked here, on
// a retired list its own VM drains the next time it files a root
// (`add_global_root_or_oom`) or dispatches an `InvocationEvent`; the VM's
// teardown drops what is left, unreleased (`forget_vm_awt_roots`). Same
// scheme as the synthetic serialization tables' `serial_retired_roots`
// (gc-common w21-c).

/// Roots parked for their owning VM. Not compatibility state: rows are
/// transient (drained by the owner) and per-VM by content.
fn retired_awt_roots() -> &'static Mutex<Vec<VmRoot>> {
    static INSTANCE: OnceLock<Mutex<Vec<VmRoot>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Number of parked roots (written under the list's lock): the common drain,
/// with nothing parked, is one atomic load and takes no lock.
static RETIRED_AWT_ROOTS_PENDING: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Split `roots` into the handles `current_vm` owns (release them now) and the
/// roots of other VMs (park them). Zero handles are dropped.
fn partition_vm_roots<I>(current_vm: usize, roots: I) -> (Vec<usize>, Vec<VmRoot>)
where
    I: IntoIterator<Item = VmRoot>,
{
    let mut own = Vec::new();
    let mut foreign = Vec::new();
    for root in roots {
        if root.handle == 0 {
            continue;
        }
        if root.vm == current_vm {
            own.push(root.handle);
        } else {
            foreign.push(root);
        }
    }
    (own, foreign)
}

fn park_retired_awt_roots(roots: Vec<VmRoot>) {
    if roots.is_empty() {
        return;
    }
    let mut retired = retired_awt_roots().lock();
    retired.extend(roots);
    RETIRED_AWT_ROOTS_PENDING.store(retired.len(), std::sync::atomic::Ordering::Release);
}

/// Remove and return `vm`'s parked handles.
fn take_retired_awt_roots(vm: usize) -> Vec<usize> {
    if RETIRED_AWT_ROOTS_PENDING.load(std::sync::atomic::Ordering::Acquire) == 0 {
        return Vec::new();
    }
    let mut retired = retired_awt_roots().lock();
    let mut mine = Vec::new();
    retired.retain(|root| {
        if root.vm == vm {
            mine.push(root.handle);
            false
        } else {
            true
        }
    });
    RETIRED_AWT_ROOTS_PENDING.store(retired.len(), std::sync::atomic::Ordering::Release);
    mine
}

/// Release the calling VM's parked roots (lock dropped before any `ctx` call).
fn drain_retired_awt_roots(ctx: &mut dyn NativeContext) {
    for handle in take_retired_awt_roots(ctx.vm_identity()) {
        ctx.remove_global_root(handle);
    }
}

/// Release each root through its own VM: the calling VM's now, every other
/// VM's parked for that VM.
fn release_vm_roots<I>(ctx: &mut dyn NativeContext, roots: I)
where
    I: IntoIterator<Item = VmRoot>,
{
    let (own, foreign) = partition_vm_roots(ctx.vm_identity(), roots);
    park_retired_awt_roots(foreign);
    for handle in own {
        ctx.remove_global_root(handle);
    }
}

/// VM teardown: drop every row `vm` filed in this crate's process-wide tables
/// -- peer sources, pending Runnables, `InvocationEvent` bindings, parked
/// roots -- releasing nothing (the VM's global-ref table dies with it).
/// Idempotent. Called from the VM's `release_vm_native_state`
/// (`vm/src/vm/vm_init.rs`, behind the `awt` feature).
///
/// gc-common w23-a (`common-w22a-awt-state-is-process-wide-and-keyed-by-identity-hash`):
/// also the VM's Rust-side AWT state -- its event queue (closed: a dispatch
/// loop still blocked in `getNextEvent` returns, `invokeAndWait` waiters are
/// released), its component peers (and their Swing dirty / focus rows), its
/// Graphics contexts, and its `BufferedImage` rows with their rasters.
pub fn forget_vm_awt_roots(vm: usize) {
    peer_source_table().lock().forget_vm(vm);
    if let Some(queue) = edt::forget_vm_edt(vm) {
        // Unreleased: the roots index the dying VM's own table.
        let _ = queue.close();
    }
    // Rows `vm` filed in the shared identity-0 queue (none in production: a
    // real VM's identity is never 0 and has its own queue).
    let _ = edt::get_edt().forget_vm_runnables(vm);
    invocation_event_callbacks().lock().forget_vm(vm);
    forget_vm_awt_state(vm);
    awt_sweep_generations().lock().remove(&vm);
    let mut retired = retired_awt_roots().lock();
    retired.retain(|root| root.vm != vm);
    RETIRED_AWT_ROOTS_PENDING.store(retired.len(), std::sync::atomic::Ordering::Release);
}

/// The Rust-side half of [`forget_vm_awt_roots`]: peers, Graphics contexts,
/// `BufferedImage` rows and rasters. One lock at a time (never nested), in the
/// order the natives take them. The rows' weak roots are dropped unreleased,
/// like every other root here: they index the dying VM's own table.
fn forget_vm_awt_state(vm: usize) {
    let peers = peer::peer_registry().lock().forget_vm(vm);
    swing::swing_state().lock().forget_peers(&peers);
    // Bound, so the contexts (and their scratch rasters) are freed after the
    // registry lock is released.
    let graphics = gfx_registry().lock().forget_vm(vm);
    drop(graphics);
    let images = buffered_image_ids().lock().forget_vm(vm);
    if !images.is_empty() {
        let mut reg = image::image_registry();
        for id in images {
            reg.destroy(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Peer → Java source ObjectRef side-table
// ---------------------------------------------------------------------------
//
// `EventQueue.getNextEvent` needs to materialise an `AwtEvent` into a Java
// `MouseEvent` / `KeyEvent` / `WindowEvent` / `PaintEvent` whose `source`
// field points at the actual Java `Component` (Frame, Button, …) the event
// targets. Peers are looked up by *identity hash*, not by `ObjectRef`, so
// going `PeerId -> ObjectRef` is otherwise impossible.
//
// We register the source `ObjectRef` here whenever `ensure_peer` mints a
// peer, and look it up when dispatching events.
//
// gc-common w24-b (`common-w23a-awt-rows-outlive-their-java-objects-within-a-vm`):
// the row holds a WEAK global root of the component, so it no longer keeps a
// dead component (and everything it reaches) alive; the component's
// collection drops the row and destroys the peer (`sweep_dead_awt_rows`). The
// 10,000-row FIFO cap that bounded the strong roots is gone with them. While
// the component is shown (`Component.setVisible(true)`) the row also holds a
// strong root, as a HotSpot native peer keeps its target reachable while it
// is displayed. The weak root is also the row's identity: `peer_of` accepts a
// peer for a receiver only when its source row names that receiver.
//
// History (finding H8): the row used to hold the component's raw address and
// resurrect it only while the collection count was unchanged (a moving
// collection would have made it dangling). A global root replaced that: the
// VM remaps it at every collection, and since w24-b a weak one also answers
// `None` once the component was collected.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PeerSourceEntry {
    /// Opaque handle of the component's root: a WEAK one
    /// (`NativeContext::add_weak_global_root`, gc-common w24-b) when `weak`,
    /// else a strong `add_global_root` one (a context without weak roots).
    root_handle: usize,
    /// Stable VM identity hash of the source component (GC-move
    /// independent). Retained as a consistency tag for the eventual
    /// hash→ObjectRef resolution path.
    java_hash: i32,
    /// `root_handle` is weak: the row neither keeps the component alive nor
    /// outlives it (`sweep_dead_awt_rows`), and it judges which component a
    /// peer belongs to (`peer_of`). A strong row judges nothing: the pre-w24
    /// key-only behaviour.
    weak: bool,
    /// gc-common w24-b: a STRONG root held while the component is shown
    /// (`Component.setVisible(true)`, [`hold_shown_peer_source`]), 0
    /// otherwise.
    shown_root: usize,
}

impl PeerSourceEntry {
    /// Every root this row holds, as its VM's `VmRoot`s.
    fn roots(&self, vm: usize) -> impl Iterator<Item = VmRoot> {
        [self.root_handle, self.shown_root]
            .into_iter()
            .filter(|&handle| handle != 0)
            .map(move |handle| VmRoot { vm, handle })
    }
}

/// `(vm_identity, peer id)`. gc-common w22-a: the key used to be the peer id
/// alone, and the peer registry mints ids per identity hash process-wide, so
/// VM B's `getNextEvent` resolved VM A's handle in B's own global-ref table
/// (an unrelated B object became the event `source`), and an eviction released
/// A's handle against B's table.
type PeerSourceKey = (usize, u64);

struct PeerSourceTable {
    map: FxHashMap<PeerSourceKey, PeerSourceEntry>,
}

impl PeerSourceTable {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
        }
    }

    /// File `entry` under `(vm, peer)`. Returns the roots of the row it
    /// replaced (all `vm`'s own: gc-common w24-b removed the FIFO cap whose
    /// evictions could hand back another VM's roots).
    fn insert(&mut self, vm: usize, peer: u64, entry: PeerSourceEntry) -> Vec<VmRoot> {
        match self.map.insert((vm, peer), entry) {
            Some(previous) => previous.roots(vm).collect(),
            None => Vec::new(),
        }
    }

    fn get(&self, vm: usize, peer: u64) -> Option<PeerSourceEntry> {
        self.map.get(&(vm, peer)).copied()
    }

    /// gc-common w24-b: install `strong` as the shown root of `(vm, peer)` if
    /// the row still holds `root_handle` and no shown root. `false`: the
    /// caller keeps (and releases) `strong`.
    fn set_shown_root(&mut self, vm: usize, peer: u64, root_handle: usize, strong: usize) -> bool {
        match self.map.get_mut(&(vm, peer)) {
            Some(entry) if entry.root_handle == root_handle && entry.shown_root == 0 => {
                entry.shown_root = strong;
                true
            }
            _ => false,
        }
    }

    /// gc-common w24-b: take the shown root of `(vm, peer)` out of its row.
    fn take_shown_root(&mut self, vm: usize, peer: u64) -> Option<usize> {
        let entry = self.map.get_mut(&(vm, peer))?;
        let strong = std::mem::take(&mut entry.shown_root);
        (strong != 0).then_some(strong)
    }

    /// gc-common w24-b: take out `vm`'s rows whose WEAK root cleared (the
    /// component was collected). A shown row holds a strong root too, so its
    /// component cannot have died; a strong row is never judged.
    fn take_dead(
        &mut self,
        vm: usize,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Vec<(u64, PeerSourceEntry)> {
        let dead: Vec<PeerSourceKey> = self
            .map
            .iter()
            .filter(|&(&(owner, _), entry)| {
                owner == vm
                    && entry.weak
                    && entry.shown_root == 0
                    && row_is_dead(entry.root_handle, resolve)
            })
            .map(|(&key, _)| key)
            .collect();
        dead.into_iter()
            .filter_map(|key| self.map.remove(&key).map(|entry| (key.1, entry)))
            .collect()
    }

    /// Drop `vm`'s rows (VM teardown), releasing nothing.
    fn forget_vm(&mut self, vm: usize) {
        self.map.retain(|&(owner, _), _| owner != vm);
    }
}

fn peer_source_table() -> &'static Mutex<PeerSourceTable> {
    static INSTANCE: OnceLock<Mutex<PeerSourceTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(PeerSourceTable::new()))
}

/// Record the Java source component for a peer. `java_hash` is the source's
/// stable VM identity hash (read from an active `NativeContext` by the
/// caller).
///
/// gc-common w24-b: the row holds a WEAK root of `source`; only a context
/// without weak roots (`add_weak_global_root` answers 0) falls back to the
/// strong root every row held before.
fn register_peer_source(
    ctx: &mut dyn NativeContext,
    peer_id: PeerId,
    source: ObjectRef,
    java_hash: i32,
) {
    let vm = ctx.vm_identity();
    // gc-common w22-a: `ensure_peer` runs on every component native, and this
    // used to mint a fresh global root and release the previous one on EVERY
    // call even when the row already named the same component. Keep the row
    // when its root still resolves to `source`.
    let existing = peer_source_table().lock().get(vm, peer_id.0);
    if let Some(entry) = existing {
        if entry.root_handle != 0
            && entry.java_hash == java_hash
            && ctx.resolve_global_root(entry.root_handle) == Some(source)
        {
            return;
        }
    }
    drain_retired_awt_roots(ctx);
    let weak_root = ctx.add_weak_global_root(source);
    let (root_handle, weak) = if weak_root != 0 {
        (weak_root, true)
    } else {
        let Ok(root) = add_global_root_or_oom(ctx, source, "AWT peer source") else {
            tracing::warn!(
                peer_id = peer_id.0,
                "AWT peer source not rooted; events for this peer may use a null source"
            );
            return;
        };
        (root.handle, false)
    };
    let removed_roots = peer_source_table().lock().insert(
        vm,
        peer_id.0,
        PeerSourceEntry {
            root_handle,
            java_hash,
            weak,
            shown_root: 0,
        },
    );
    release_vm_roots(ctx, removed_roots);
}

/// gc-common w24-b: while a component is shown its source row also holds a
/// STRONG root, as a HotSpot native peer keeps its target reachable while it
/// is displayed: a shown frame the program dropped every reference to keeps
/// its peer and its events' `source`. Hiding it releases the strong root; the
/// weak one stays. No-op for a strong row (it already roots) or no row.
fn hold_shown_peer_source(
    ctx: &mut dyn NativeContext,
    peer_id: PeerId,
    source: ObjectRef,
    shown: bool,
) {
    let vm = ctx.vm_identity();
    let Some(entry) = peer_source_table().lock().get(vm, peer_id.0) else {
        return;
    };
    if !entry.weak || (entry.shown_root != 0) == shown {
        return;
    }
    if shown {
        // The row must name `source` itself: `peer_of` found the peer
        // through it, but check rather than root another component.
        if ctx.resolve_global_root(entry.root_handle) != Some(source) {
            return;
        }
        let strong = ctx.add_global_root(source);
        if strong == 0 {
            return;
        }
        let installed =
            peer_source_table()
                .lock()
                .set_shown_root(vm, peer_id.0, entry.root_handle, strong);
        if !installed {
            ctx.remove_global_root(strong);
        }
    } else {
        let strong = peer_source_table().lock().take_shown_root(vm, peer_id.0);
        if let Some(strong) = strong {
            ctx.remove_global_root(strong);
        }
    }
}

/// Resolve the Java source component VM `vm` registered for a peer through
/// `resolve` (the handle -> object step). Returns `None` (fail closed) when
/// the entry is absent, the handle is zero, or `resolve` cannot resolve it.
fn lookup_peer_source_with<F>(vm: usize, peer_id: PeerId, resolve: F) -> Option<ObjectRef>
where
    F: FnOnce(usize) -> Option<ObjectRef>,
{
    let entry = peer_source_table().lock().get(vm, peer_id.0)?;
    if entry.root_handle == 0 {
        // Null is never a valid heap object — `ObjectRef::from_raw` requires
        // non-null (debug_assert) and the VM never represents `null` this way.
        return None;
    }
    // The object is read through its global root (remapped by every
    // collection; a weak one answers `None` once the component was
    // collected), never through a cached pointer.
    let _ = entry.java_hash; // retained for the future hash→ref resolution path
    resolve(entry.root_handle)
}

/// The component the calling VM registered as `peer_id`'s source, for an
/// event's `source` field. gc-common w24-b: `None` once a weak row's
/// component was collected; a weak root resolved for a heap store is kept
/// alive first (`gc_reference_keep_alive`, as `Reference.get()` does), since a
/// concurrent mark may not have reached it.
fn lookup_peer_source(ctx: &mut dyn NativeContext, peer_id: PeerId) -> Option<ObjectRef> {
    let entry = peer_source_table().lock().get(ctx.vm_identity(), peer_id.0)?;
    if entry.root_handle == 0 {
        return None;
    }
    let source = ctx.resolve_global_root(entry.root_handle)?;
    if ctx.identity_hash_code(source) != entry.java_hash {
        return None;
    }
    if entry.weak {
        ctx.gc_reference_keep_alive(source);
    }
    Some(source)
}

/// gc-common w24-b: does peer `peer_id`'s source row name `obj`? A peer with
/// no weak row to judge by (none, a strong row) answers `Same`: the pre-w24
/// key-only answer.
fn peer_row_verdict(
    vm: usize,
    peer_id: PeerId,
    obj: ObjectRef,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> RowVerdict {
    let entry = peer_source_table().lock().get(vm, peer_id.0);
    match entry {
        Some(entry) if entry.weak => row_verdict(entry.root_handle, obj, resolve),
        _ => RowVerdict::Same,
    }
}

/// The peer of component `obj` in the calling VM, or `None`.
///
/// gc-common w24-b: among the peers mapped to `obj`'s identity hash, the one
/// whose source row names `obj` itself. A peer of another live component with
/// the same hash (a within-VM collision), or of a collected one whose row the
/// next sweep drops, is not `obj`'s: it used to be shared.
fn peer_of(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<PeerId> {
    let (vm, hash) = obj_key(ctx, obj);
    let resolve = |handle: usize| ctx.resolve_global_root(handle);
    let primary = peer::peer_registry().lock().peer_for_java(vm, hash)?;
    if peer_row_verdict(vm, primary, obj, &resolve) == RowVerdict::Same {
        return Some(primary);
    }
    let collided = peer::peer_registry()
        .lock()
        .collided_peers_for_java(vm, hash);
    collided
        .into_iter()
        .find(|&pid| peer_row_verdict(vm, pid, obj, &resolve) == RowVerdict::Same)
}

// ---------------------------------------------------------------------------
// Weak rows (gc-common w24-b)
// ---------------------------------------------------------------------------
//
// `common-w23a-awt-rows-outlive-their-java-objects-within-a-vm`: every row of
// the peer-source, Graphics and `BufferedImage` tables carries a WEAK global
// root of the Java object it describes (`NativeContext::add_weak_global_root`).
// The root keeps nothing alive and resolves to `None` once a collection found
// the object unreachable. It serves twice:
//
// * identity: a lookup by `(vm, identity hash)` accepts a row only when its
//   weak root resolves to the receiver itself. A row naming ANOTHER live
//   object of the VM is a within-VM identity-hash collision: the receiver
//   gets a row of its own (`KeyedRows::collided`, the peer registry's
//   collided mappings) instead of sharing one;
// * liveness: [`sweep_dead_awt_rows`], run by the VM's next row-creating AWT
//   native after each collection (a per-VM collection-count gate), drops the
//   rows whose weak root cleared, frees their rasters
//   (`ImageRegistry::destroy`, a peer Graphics' own buffer) and destroys
//   their peers.
//
// A row whose weak root is 0 (a context with no collector, e.g. a test mock)
// matches every receiver of its key and is never swept: the pre-w24 behaviour.
//
// Lock order: an AWT table lock may be held while a weak root is resolved or
// minted (the VM's JNI global-ref table lock, a leaf); never the reverse, and
// no AWT table lock is held across a Java call or an allocation.

/// Whose row is it? (gc-common w24-b)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowVerdict {
    /// The row names the object asked about (or has no weak root to judge by).
    Same,
    /// The row names another live object with the same key.
    Other,
    /// The row's object was collected; the next sweep drops the row.
    Dead,
}

fn row_verdict(
    weak: usize,
    obj: ObjectRef,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> RowVerdict {
    if weak == 0 {
        return RowVerdict::Same;
    }
    match resolve(weak) {
        None => RowVerdict::Dead,
        Some(current) if current == obj => RowVerdict::Same,
        Some(_) => RowVerdict::Other,
    }
}

/// Has the weak root `weak` cleared? A row without one never dies.
fn row_is_dead(weak: usize, resolve: &dyn Fn(usize) -> Option<ObjectRef>) -> bool {
    weak != 0 && resolve(weak).is_none()
}

/// One row of a [`KeyedRows`] table: its value and the weak root (0: none)
/// of the Java object it belongs to.
#[derive(Debug, Clone)]
struct WeakRow<T> {
    weak: usize,
    value: T,
}

/// A row [`KeyedRows::insert`] took out: `dead` when its object was
/// collected, else it named the inserted object itself (superseded).
struct DisplacedRow<T> {
    row: WeakRow<T>,
    dead: bool,
}

/// Rows keyed by `(vm, identity hash)`, one per LIVE object: when the key's
/// row names another live object of the VM, the next object's row goes to
/// `collided` (gc-common w24-b).
struct KeyedRows<T> {
    primary: FxHashMap<ObjKey, WeakRow<T>>,
    /// Further rows of a key whose primary row names another live object.
    /// Almost always empty.
    collided: FxHashMap<ObjKey, Vec<WeakRow<T>>>,
}

impl<T: Clone> KeyedRows<T> {
    fn new() -> Self {
        Self {
            primary: FxHashMap::default(),
            collided: FxHashMap::default(),
        }
    }

    /// `obj`'s row under `key`, if any.
    fn get(
        &self,
        key: ObjKey,
        obj: ObjectRef,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Option<T> {
        let primary = self.primary.get(&key)?;
        if row_verdict(primary.weak, obj, resolve) == RowVerdict::Same {
            return Some(primary.value.clone());
        }
        self.collided
            .get(&key)?
            .iter()
            .find(|row| row_verdict(row.weak, obj, resolve) == RowVerdict::Same)
            .map(|row| row.value.clone())
    }

    /// Is there a row under `key` (any object's)? Test-only.
    #[cfg(test)]
    fn contains_key(&self, key: ObjKey) -> bool {
        self.primary.contains_key(&key)
    }

    /// File `value` as `obj`'s row under `key` with weak root `weak`. Returns
    /// the rows it displaced: `obj`'s previous row, and rows of the key whose
    /// object was collected. The caller releases their weak roots.
    fn insert(
        &mut self,
        key: ObjKey,
        obj: ObjectRef,
        weak: usize,
        value: T,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Vec<DisplacedRow<T>> {
        let mut displaced = Vec::new();
        if let Some(mut rows) = self.collided.remove(&key) {
            let mut kept = Vec::with_capacity(rows.len());
            for row in rows.drain(..) {
                match row_verdict(row.weak, obj, resolve) {
                    RowVerdict::Other => kept.push(row),
                    verdict => displaced.push(DisplacedRow {
                        row,
                        dead: verdict == RowVerdict::Dead,
                    }),
                }
            }
            if !kept.is_empty() {
                self.collided.insert(key, kept);
            }
        }
        let row = WeakRow { weak, value };
        let verdict = self
            .primary
            .get(&key)
            .map(|primary| row_verdict(primary.weak, obj, resolve));
        match verdict {
            None => {
                self.primary.insert(key, row);
            }
            Some(RowVerdict::Other) => self.collided.entry(key).or_default().push(row),
            Some(verdict) => {
                if let Some(old) = self.primary.insert(key, row) {
                    displaced.push(DisplacedRow {
                        row: old,
                        dead: verdict == RowVerdict::Dead,
                    });
                }
            }
        }
        displaced
    }

    /// Take `obj`'s row out from under `key`.
    fn remove(
        &mut self,
        key: ObjKey,
        obj: ObjectRef,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Option<WeakRow<T>> {
        let primary_weak = self.primary.get(&key)?.weak;
        if row_verdict(primary_weak, obj, resolve) == RowVerdict::Same {
            let row = self.primary.remove(&key);
            self.promote(key);
            return row;
        }
        let rows = self.collided.get_mut(&key)?;
        let at = rows
            .iter()
            .position(|row| row_verdict(row.weak, obj, resolve) == RowVerdict::Same)?;
        let row = rows.remove(at);
        if rows.is_empty() {
            self.collided.remove(&key);
        }
        Some(row)
    }

    /// `key`'s primary row went: its first collided row takes its place.
    fn promote(&mut self, key: ObjKey) {
        if self.primary.contains_key(&key) {
            return;
        }
        if let Some(mut rows) = self.collided.remove(&key) {
            if !rows.is_empty() {
                let first = rows.remove(0);
                self.primary.insert(key, first);
            }
            if !rows.is_empty() {
                self.collided.insert(key, rows);
            }
        }
    }

    /// Take out every row of `vm` whose weak root cleared (the sweep).
    fn take_dead(
        &mut self,
        vm: usize,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Vec<WeakRow<T>> {
        let mut dead = Vec::new();
        self.collided.retain(|&(owner, _), rows| {
            if owner != vm {
                return true;
            }
            let mut i = 0;
            while i < rows.len() {
                if row_is_dead(rows[i].weak, resolve) {
                    dead.push(rows.remove(i));
                } else {
                    i += 1;
                }
            }
            !rows.is_empty()
        });
        let dead_keys: Vec<ObjKey> = self
            .primary
            .iter()
            .filter(|&(&(owner, _), row)| owner == vm && row_is_dead(row.weak, resolve))
            .map(|(&key, _)| key)
            .collect();
        for key in dead_keys {
            if let Some(row) = self.primary.remove(&key) {
                dead.push(row);
            }
            self.promote(key);
        }
        dead
    }

    /// Take out every row of `vm` (VM teardown).
    fn forget_vm(&mut self, vm: usize) -> Vec<WeakRow<T>> {
        let mut gone = Vec::new();
        self.primary.retain(|&(owner, _), row| {
            if owner == vm {
                gone.push(row.clone());
                false
            } else {
                true
            }
        });
        self.collided.retain(|&(owner, _), rows| {
            if owner == vm {
                gone.append(rows);
                false
            } else {
                true
            }
        });
        gone
    }

    /// Keep the rows whose value `keep` accepts; take out the others.
    fn retain_values(&mut self, mut keep: impl FnMut(&T) -> bool) -> Vec<WeakRow<T>> {
        let mut gone = Vec::new();
        let mut vacated = Vec::new();
        self.primary.retain(|&key, row| {
            if keep(&row.value) {
                true
            } else {
                gone.push(row.clone());
                vacated.push(key);
                false
            }
        });
        self.collided.retain(|_, rows| {
            let mut i = 0;
            while i < rows.len() {
                if keep(&rows[i].value) {
                    i += 1;
                } else {
                    gone.push(rows.remove(i));
                }
            }
            !rows.is_empty()
        });
        for key in vacated {
            self.promote(key);
        }
        gone
    }
}

/// Release the weak roots of rows an insert displaced (all the calling VM's:
/// every row is filed and displaced by its own VM).
fn release_displaced_weak_roots<T>(ctx: &mut dyn NativeContext, displaced: &[DisplacedRow<T>]) {
    for d in displaced {
        if d.row.weak != 0 {
            ctx.remove_global_root(d.row.weak);
        }
    }
}

/// The collection count at each VM's last sweep (gc-common w24-b). Keyed by
/// VM identity, dropped at the VM's teardown (`forget_vm_awt_roots`).
fn awt_sweep_generations() -> &'static Mutex<FxHashMap<usize, u64>> {
    static INSTANCE: OnceLock<Mutex<FxHashMap<usize, u64>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// Has VM `vm` collected since its last sweep (`gc` = its collection count
/// now)? Records `gc`. A VM's first call records and answers `false`: every
/// row-filing path sweeps first, so no row predates it.
fn awt_sweep_due(vm: usize, gc: u64) -> bool {
    match awt_sweep_generations().lock().insert(vm, gc) {
        Some(last) => last != gc,
        None => false,
    }
}

/// What one sweep took out of the tables (gc-common w24-b).
#[derive(Default)]
struct DeadAwtRows {
    /// The dead rows' weak roots, to release through the sweeping VM.
    weak_roots: Vec<usize>,
    /// Peers whose component was collected.
    peers: Vec<PeerId>,
    /// Rasters whose `BufferedImage` was collected.
    images: Vec<ImageId>,
    /// Graphics contexts whose `Graphics` was collected (their scratch
    /// rasters go when these handles drop).
    graphics: Vec<GfxHandle>,
}

/// Take out every row of `vm` whose weak root cleared, one table lock at a
/// time. Pure over the tables (`resolve` is the VM's
/// `resolve_global_root`), so the tests drive it directly.
fn take_dead_awt_rows(vm: usize, resolve: &dyn Fn(usize) -> Option<ObjectRef>) -> DeadAwtRows {
    let mut dead = DeadAwtRows::default();
    let sources = peer_source_table().lock().take_dead(vm, resolve);
    for (peer, entry) in sources {
        dead.weak_roots.push(entry.root_handle);
        dead.peers.push(PeerId(peer));
    }
    let graphics = gfx_registry().lock().take_dead(vm, resolve);
    for row in graphics {
        dead.weak_roots.push(row.weak);
        dead.graphics.push(row.value);
    }
    let images = buffered_image_ids().lock().by_key.take_dead(vm, resolve);
    for row in images {
        dead.weak_roots.push(row.weak);
        dead.images.push(row.value);
    }
    dead
}

/// Free what [`take_dead_awt_rows`] took out -- destroy the peers (and their
/// Swing rows) and the rasters, drop the Graphics contexts -- and return the
/// weak roots for the caller to release. No table lock is held on entry.
///
/// Dropping a Graphics context here loses nothing: an image's Graphics has
/// drawn straight into the image's raster since gc-common w25-d, and a
/// peer's Graphics buffer is never read back.
fn free_dead_awt_rows(dead: DeadAwtRows) -> Vec<usize> {
    let DeadAwtRows {
        weak_roots,
        peers,
        images,
        graphics,
    } = dead;
    if !peers.is_empty() {
        let destroyed: Vec<PeerId> = {
            let mut reg = peer::peer_registry().lock();
            peers.into_iter().filter(|&p| reg.destroy_one(p)).collect()
        };
        swing::swing_state().lock().forget_peers(&destroyed);
    }
    if !images.is_empty() {
        let mut reg = image::image_registry();
        for id in images {
            reg.destroy(id);
        }
    }
    drop(graphics);
    weak_roots
}

/// gc-common w24-b: after each collection of the calling VM, drop its rows
/// whose Java object was collected (see the section comment). Called by the
/// natives that FILE a row, before they take any table lock, so a VM that
/// keeps creating components, Graphics or images keeps its tables at its
/// live objects plus those dead since its last collection. Costs one lock
/// and one map probe when no collection ran since the last call.
fn sweep_dead_awt_rows(ctx: &mut dyn NativeContext) {
    let vm = ctx.vm_identity();
    if !awt_sweep_due(vm, ctx.gc_collection_count()) {
        return;
    }
    let dead = take_dead_awt_rows(vm, &|handle: usize| ctx.resolve_global_root(handle));
    for weak in free_dead_awt_rows(dead) {
        if weak != 0 {
            ctx.remove_global_root(weak);
        }
    }
}

// ---------------------------------------------------------------------------
// Graphics2D context registry
// ---------------------------------------------------------------------------
//
// Each Java `Graphics2D` object is mapped to a `Graphics2DState` (the
// rasterizer state machine in `graphics2d.rs`) keyed by the Java object's
// identity hash code.  Without this side-table, draw* natives have no way
// to find the renderer state for the receiver and every draw call would
// no-op (the bug being fixed here).
//
// A Graphics2D context may be backed by either a `BufferedImage` (in which
// case every primitive draws straight into the image's raster, gc-common
// w25-d, see `with_gfx_entry`) or a `ComponentPeer` (in which case it draws
// into its own buffer and `dispose` marks the peer dirty for repaint).

#[derive(Debug, Clone, Copy)]
enum GfxTarget {
    Image(ImageId),
    Peer(PeerId),
    Detached,
}

struct GfxEntry {
    state: Graphics2DState,
    target: GfxTarget,
    /// Set once `dispose()` has retired this context; its drawing calls are
    /// then no-ops and it no longer touches its target's raster. A disposed
    /// entry still in the registry is kept solely so that `flush_image` can
    /// reap any disposed contexts still associated with an image id without
    /// racing a live (undisposed) context. See `dispose_gfx` / `flush_image`.
    disposed: bool,
}

/// Each Graphics2D context is wrapped in its own `Arc<Mutex<...>>` so that
/// concurrent draw calls on *different* Graphics2D objects don't serialise
/// through the registry's outer lock — we only hold the outer lock long
/// enough to clone the Arc.
type GfxHandle = Arc<Mutex<GfxEntry>>;

/// `(NativeContext::vm_identity(), identity hash)` of a Java object: the key of
/// this crate's Graphics and `BufferedImage` tables (and, as
/// [`peer::JavaKey`], of the peer registry).
///
/// gc-common w23-a (`common-w22a-awt-state-is-process-wide-and-keyed-by-identity-hash`,
/// item 3): these keys used to be the bare identity hash, so VM B's
/// `drawLine` on a Graphics whose hash collided with one of VM A's drew into
/// A's raster, and B's `BufferedImage.getRGB` read A's pixels.
type ObjKey = peer::JavaKey;

fn obj_key(ctx: &dyn NativeContext, obj: ObjectRef) -> ObjKey {
    (ctx.vm_identity(), ctx.identity_hash_code(obj))
}

/// The Graphics contexts, one row per live Java `Graphics` (gc-common w24-b:
/// a [`KeyedRows`], each row with a weak root of its `Graphics`).
fn gfx_registry() -> &'static Mutex<KeyedRows<GfxHandle>> {
    static INSTANCE: OnceLock<Mutex<KeyedRows<GfxHandle>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(KeyedRows::new()))
}

/// A fresh detached context: the lazy fallback for a draw call on a Graphics2D
/// we never saw `create`/`getGraphics` for (a default-sized buffer so the call
/// doesn't panic), and `create()` of a detached one.
fn detached_gfx_entry() -> GfxEntry {
    GfxEntry {
        state: Graphics2DState::create(1, 1),
        target: GfxTarget::Detached,
        disposed: false,
    }
}

/// Look up (or lazily create) the `GfxHandle` for a Java Graphics2D receiver.
/// Holds the outer registry lock only long enough to clone the `Arc`.
///
/// gc-common w24-b: the row must name `receiver` itself (its weak root), so a
/// Graphics whose identity hash collides with another live one of the VM gets
/// its own context instead of drawing into the other's.
fn gfx_handle_for(ctx: &mut dyn NativeContext, receiver: ObjectRef) -> GfxHandle {
    let key = obj_key(ctx, receiver);
    let found = gfx_registry()
        .lock()
        .get(key, receiver, &|handle: usize| ctx.resolve_global_root(handle));
    if let Some(handle) = found {
        return handle;
    }
    sweep_dead_awt_rows(ctx);
    let weak = ctx.add_weak_global_root(receiver);
    let (handle, displaced, unused) = gfx_handle_for_key(key, receiver, weak, &|handle: usize| {
        ctx.resolve_global_root(handle)
    });
    release_displaced_weak_roots(ctx, &displaced);
    if unused != 0 {
        ctx.remove_global_root(unused);
    }
    handle
}

/// [`gfx_handle_for`]'s table half, once the receiver's `(vm, hash)` key is
/// known: `obj`'s context, or a fresh detached one filed with the weak root
/// `weak`. Returns the context, the rows the filing displaced, and `weak`
/// back when it was not used (another thread filed `obj`'s row first).
fn gfx_handle_for_key(
    key: ObjKey,
    obj: ObjectRef,
    weak: usize,
    resolve: &dyn Fn(usize) -> Option<ObjectRef>,
) -> (GfxHandle, Vec<DisplacedRow<GfxHandle>>, usize) {
    let mut reg = gfx_registry().lock();
    if let Some(handle) = reg.get(key, obj, resolve) {
        return (handle, Vec::new(), weak);
    }
    let handle: GfxHandle = Arc::new(Mutex::new(detached_gfx_entry()));
    let displaced = reg.insert(key, obj, weak, handle.clone(), resolve);
    (handle, displaced, 0)
}

/// The context of `receiver` if one is registered (no lazy creation).
fn existing_gfx_handle(ctx: &dyn NativeContext, receiver: ObjectRef) -> Option<GfxHandle> {
    let key = obj_key(ctx, receiver);
    gfx_registry()
        .lock()
        .get(key, receiver, &|handle: usize| ctx.resolve_global_root(handle))
}

/// Ensure a `Graphics2DState` exists for the given Java Graphics2D receiver,
/// then run `f` against it. Lazily creates a detached context if no
/// associated target has been registered, so misbehaving callers still get
/// drawing semantics (writing into a throwaway buffer) instead of panics.
///
/// The outer registry lock is released before `f` runs, so concurrent draw
/// calls on different Graphics2D objects don't block each other.
fn with_gfx<F, R>(ctx: &mut dyn NativeContext, receiver: ObjectRef, f: F) -> R
where
    F: FnOnce(&mut Graphics2DState) -> R,
    R: Default,
{
    let handle = gfx_handle_for(ctx, receiver);
    let mut entry = handle.lock();
    with_gfx_entry(&mut entry, f)
}

/// Run `f` against `entry`'s state, drawing straight into its target
/// `BufferedImage`'s raster when it has one.
///
/// gc-common w25-d: a Graphics on a `BufferedImage` used to draw into a
/// private scratch raster that started BLANK, and `dispose()` copied the
/// whole scratch over the image: pixels set before `createGraphics()` (by
/// `setRGB`, another Graphics, or `ImageIO.read`) were erased at
/// `dispose()` unless this Graphics repainted them, two Graphics on one
/// image erased each other's work, and nothing drawn was visible to
/// `getRGB` / `getRaster` / `drawImage` / `ImageIO.write` until
/// `dispose()` (never, for code that does not dispose). On HotSpot every
/// primitive writes the image's raster at once. Now the image's pixel `Vec`
/// is swapped into the renderer for the call ([`AttachedRaster`], O(1)) and
/// swapped back after it, so a primitive changes exactly the pixels it
/// covers, in the image, before the native returns.
///
/// Lock order: the Graphics entry, then the image registry (as `dispose` and
/// `drawImage` always took them). The registry lock is held for the call,
/// so draws into images serialise on it the way `getRGB` / `setRGB` /
/// `drawImage` already did.
fn with_gfx_entry<F, R>(entry: &mut GfxEntry, f: F) -> R
where
    F: FnOnce(&mut Graphics2DState) -> R,
{
    if let (GfxTarget::Image(id), false) = (entry.target, entry.disposed) {
        let mut reg = image::image_registry();
        if let Some(img) = reg.get_mut(id) {
            let (w, h) = (img.width(), img.height());
            if let Some(mut attached) =
                AttachedRaster::attach(&mut entry.state, img.pixel_vec_mut(), w, h)
            {
                return f(attached.state());
            }
        }
    }
    // A peer or detached context, or an image whose raster is gone (its
    // `BufferedImage` was collected): the context's own buffer.
    f(&mut entry.state)
}

/// `Graphics.drawImage(source, x, y, observer)` for a `BufferedImage` source:
/// blit `source`'s raster through `entry`'s state.
///
/// Holding the image-registry lock while the source is borrowed as a slice
/// avoids an 8 MiB `.to_vec()` per call (a 1080p frame). gc-common w25-d:
/// when `entry` draws into an image, that image's raster is the destination
/// (see [`with_gfx_entry`]); a Graphics drawing its own image into itself
/// reads a snapshot of it, so an overlapping blit reads the pixels as they
/// were before the call.
fn draw_buffered_image(entry: &mut GfxEntry, source: ImageId, x: i32, y: i32) {
    let mut reg = image::image_registry();
    if let (GfxTarget::Image(target), false) = (entry.target, entry.disposed) {
        if target == source {
            let Some(img) = reg.get_mut(target) else {
                return;
            };
            let (w, h) = (img.width(), img.height());
            let snapshot = img.get_data_buffer().to_vec();
            if let Some(mut attached) =
                AttachedRaster::attach(&mut entry.state, img.pixel_vec_mut(), w, h)
            {
                if !snapshot.is_empty() {
                    attached.state().draw_image(&snapshot, w, h, x, y);
                }
            }
            return;
        }
        if let Some((dst, src)) = reg.get_pair_mut(target, source) {
            let (dw, dh) = (dst.width(), dst.height());
            let (sw, sh) = (src.width(), src.height());
            let pixels: &[u32] = src.get_data_buffer();
            if let Some(mut attached) =
                AttachedRaster::attach(&mut entry.state, dst.pixel_vec_mut(), dw, dh)
            {
                if !pixels.is_empty() {
                    attached.state().draw_image(pixels, sw, sh, x, y);
                }
            }
            return;
        }
        // The target's raster is gone (its image was collected): draw into
        // the context's own buffer, as below.
    }
    if let Some(bimg) = reg.get(source) {
        let pixels: &[u32] = bimg.get_data_buffer();
        let (w, h) = (bimg.width(), bimg.height());
        if !pixels.is_empty() {
            entry.state.draw_image(pixels, w, h, x, y);
        }
    }
}

/// A Graphics context's renderer with its `BufferedImage`'s raster swapped
/// in (gc-common w25-d). Dropping it (also on unwind) swaps the context's
/// own buffer back, so the image never keeps the wrong `Vec`.
struct AttachedRaster<'a> {
    state: &'a mut Graphics2DState,
    raster: &'a mut Vec<u32>,
    /// The context's own buffer dimensions, restored at drop.
    own_dims: (u32, u32),
}

impl<'a> AttachedRaster<'a> {
    fn attach(
        state: &'a mut Graphics2DState,
        raster: &'a mut Vec<u32>,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let own_dims = state.swap_backing(raster, width, height)?;
        Some(Self {
            state,
            raster,
            own_dims,
        })
    }

    fn state(&mut self) -> &mut Graphics2DState {
        self.state
    }
}

impl Drop for AttachedRaster<'_> {
    fn drop(&mut self) {
        let (w, h) = self.own_dims;
        // `raster` holds the context's own buffer (of `own_dims`), so this
        // cannot refuse.
        let restored = self.state.swap_backing(self.raster, w, h);
        debug_assert!(restored.is_some());
    }
}

/// File `entry` as the context of the freshly created Graphics `gfx_obj`
/// (gc-common w24-b: with a weak root of it, after the VM's pending sweep).
fn file_gfx_row(ctx: &mut dyn NativeContext, gfx_obj: ObjectRef, entry: GfxEntry) {
    sweep_dead_awt_rows(ctx);
    let key = obj_key(ctx, gfx_obj);
    let weak = ctx.add_weak_global_root(gfx_obj);
    let handle: GfxHandle = Arc::new(Mutex::new(entry));
    let displaced = gfx_registry().lock().insert(key, gfx_obj, weak, handle, &|h: usize| {
        ctx.resolve_global_root(h)
    });
    release_displaced_weak_roots(ctx, &displaced);
}

/// Register a freshly-created Graphics2D Java object as drawing into the
/// `BufferedImage` raster `image_id`.
///
/// gc-common w25-d: the context's own buffer is 1x1. Every call swaps the
/// image's raster in ([`with_gfx_entry`]); the full-size blank scratch it
/// used to allocate (`w * h * 4` bytes per Graphics) is what made
/// `dispose()` erase the image.
fn register_gfx_for_image(ctx: &mut dyn NativeContext, gfx_obj: ObjectRef, image_id: ImageId) {
    file_gfx_row(
        ctx,
        gfx_obj,
        GfxEntry {
            state: Graphics2DState::create(1, 1),
            target: GfxTarget::Image(image_id),
            disposed: false,
        },
    );
}

/// The context `Graphics.create()` files for its new Graphics: the parent's
/// target and a copy of its state ([`Graphics2DState::derive`]), with the
/// buffer size a fresh context of that target gets (1x1 for an image, whose
/// raster is swapped in per call; the peer's size for a peer). A detached
/// context without a parent row.
fn child_gfx_entry(parent: Option<&GfxHandle>) -> GfxEntry {
    let Some(parent) = parent else {
        return detached_gfx_entry();
    };
    let target = parent.lock().target;
    let (w, h) = match target {
        GfxTarget::Peer(pid) => peer_buffer_dims(pid),
        GfxTarget::Image(_) | GfxTarget::Detached => (1, 1),
    };
    let state = parent.lock().state.derive(w, h);
    GfxEntry {
        state,
        target,
        disposed: false,
    }
}

/// The buffer size of a peer's Graphics: the peer's size, at least 1x1.
fn peer_buffer_dims(peer_id: PeerId) -> (u32, u32) {
    let reg = peer::peer_registry().lock();
    match reg.get(peer_id) {
        Some(p) => (p.width.max(1), p.height.max(1)),
        None => (1, 1),
    }
}

fn register_gfx_for_peer(ctx: &mut dyn NativeContext, gfx_obj: ObjectRef, peer_id: PeerId) {
    let (w, h) = peer_buffer_dims(peer_id);
    file_gfx_row(
        ctx,
        gfx_obj,
        GfxEntry {
            state: Graphics2DState::create(w, h),
            target: GfxTarget::Peer(peer_id),
            disposed: false,
        },
    );
}

/// `Graphics.dispose()`: retire the context and remove its registry row.
/// An image's pixels are already in its raster (gc-common w25-d: every
/// primitive draws there, see [`with_gfx_entry`]), so nothing is copied; a
/// peer is marked dirty for repaint.
///
/// Memory note: the entry is `remove`d from the registry up front and the
/// `GfxHandle` `Arc` is dropped at the end of this function, so the disposed
/// context's own renderer buffer (full-size for a peer's Graphics) is
/// reclaimed immediately on dispose rather than lingering until process exit.
fn dispose_gfx(ctx: &mut dyn NativeContext, receiver: ObjectRef) {
    let key = obj_key(ctx, receiver);
    let row = {
        gfx_registry()
            .lock()
            .remove(key, receiver, &|h: usize| ctx.resolve_global_root(h))
    };
    let Some(row) = row else {
        return;
    };
    // gc-common w24-b: the row's weak root goes with it.
    if row.weak != 0 {
        ctx.remove_global_root(row.weak);
    }
    let handle = row.value;
    let mut entry = handle.lock();
    entry.state.dispose();
    entry.disposed = true;
    match entry.target {
        // gc-common w25-d: nothing to commit. This arm used to copy the whole
        // scratch raster over the image, erasing every pixel the Graphics had
        // not itself drawn.
        GfxTarget::Image(_) => {}
        GfxTarget::Peer(peer_id) => {
            let reg = peer::peer_registry().lock();
            if let Some(peer) = reg.get(peer_id) {
                swing::swing_state()
                    .lock()
                    .mark_dirty(peer_id, 0, 0, peer.width, peer.height);
            }
        }
        GfxTarget::Detached => {}
    }
}

/// Reclaim derived/cached native resources associated with a `BufferedImage`
/// when Java explicitly calls `BufferedImage.flush()`.
///
/// IMPORTANT — what flush does and does NOT free:
///
/// `java.awt.Image.flush()` flushes *reconstructable* resources. A CratonVM
/// `BufferedImage` is memory-backed: its authoritative ARGB raster lives in
/// the `ImageRegistry` and is NOT reconstructable from a producer. The JDK
/// contract is that such a raster survives `flush()` — code may legally do
/// `g = img.createGraphics(); ...; g.dispose(); img.flush(); img.getRGB(..)`
/// and still read the drawn pixels, and may re-`createGraphics()` afterwards.
/// Therefore we deliberately do NOT `ImageRegistry::destroy()` the raster
/// here: doing so would be a correctness bug (subsequent legal reads would
/// silently return 0 / no-op). Reclaiming the raster is only safe at the
/// image's true end-of-life: gc-common w24-b's `sweep_dead_awt_rows` does it
/// once the collector found the image dead (its row's weak root cleared).
///
/// What we CAN safely reclaim is any *disposed* Graphics2D context still
/// associated with this image id. Its pixels are in the raster (every
/// primitive draws there, gc-common w25-d) and the Java `Graphics2D` is, by
/// contract, unusable, so the context is provably dead. `dispose_gfx`
/// normally removes such entries immediately, but the lazy-fallback path in
/// `gfx_handle_for` can mint untracked contexts that are later disposed; this
/// reaper bounds their accumulation. We only touch
/// entries flagged `disposed`, never a live one, so there is no risk of
/// dropping a buffer that an in-flight render still holds.
///
/// Returns the reaped rows' weak roots (gc-common w24-b), for the caller to
/// release through its VM.
fn flush_image(image_id: ImageId) -> Vec<usize> {
    let reaped = gfx_registry().lock().retain_values(|handle| {
        // Only inspect handles we can lock without contention; a currently
        // locked handle is in active use (a draw call holds it), so it is by
        // definition not a dead disposed scratch buffer — keep it.
        let Some(entry) = handle.try_lock() else {
            return true;
        };
        let is_dead_scratch =
            entry.disposed && matches!(entry.target, GfxTarget::Image(id) if id == image_id);
        // Returning `false` takes the entry out; its `Arc`/scratch buffer is
        // dropped below, after the registry lock.
        !is_dead_scratch
    });
    reaped
        .into_iter()
        .map(|row| row.weak)
        .filter(|&weak| weak != 0)
        .collect()
}

/// Upper bound on the up-front capacity reserved from a reported array length.
///
/// `array_length` comes from the heap layout of a (possibly malformed/hostile)
/// array object. A bogus length would otherwise force a giant eager
/// `Vec::with_capacity` allocation before a single element is read. We reserve
/// at most this many slots and let the `Vec` grow on demand for legitimately
/// large arrays — the per-element read loop bounds total memory anyway.
const MAX_INT_ARRAY_PREALLOC: usize = 1 << 20; // 1M ints = 4 MiB

/// Read the first `limit` elements of an int[] array (fewer if the array is
/// shorter) into a Vec<i32>.
///
/// gc-common w23-a (per-call cost): the polygon natives use only their
/// `nPoints` prefix, but this used to read the WHOLE array, one virtual
/// `get_array_element` and one `Value` per element. It now reads just the
/// prefix, in one bulk `read_int_array_into` copy. A malformed receiver (not
/// an `int[]`; the descriptor is `[I`) reads as zeros.
fn read_int_array(ctx: &dyn NativeContext, obj: ObjectRef, limit: usize) -> Vec<i32> {
    let len = ctx.array_length(obj).min(limit);
    // `len` is bounded by the caller's `nPoints` and the array's own length;
    // still refuse a giant eager zero-fill for a bogus reported length.
    if len > MAX_INT_ARRAY_PREALLOC {
        let mut out = Vec::with_capacity(MAX_INT_ARRAY_PREALLOC);
        for i in 0..len {
            out.push(match ctx.get_array_element(obj, i) {
                Value::Int(v) => v,
                _ => 0,
            });
        }
        return out;
    }
    let mut out = vec![0i32; len];
    let _ = ctx.read_int_array_into(obj, 0, &mut out);
    out
}

fn get_double(args: &[Value], idx: usize) -> f64 {
    match args.get(idx) {
        Some(Value::Double(v)) => *v,
        Some(Value::Float(v)) => *v as f64,
        Some(Value::Int(v)) => *v as f64,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: unknown — needs census. Nothing in this file sets a
// category; every registration below inherits whatever ambient category the
// caller left in place. Today that is `Bridge`, supplied wholesale by
// `lib.rs::register_awt_natives`'s `with_category(Bridge, register_all)`.
// Per-group verdicts are annotated on each `register_*_natives` below and were
// derived from `javap -p -s` against JDK 25; they are static evidence only and
// must be confirmed with schema-v2 `invocations` counts before any group is
// retagged. See jdk-only-ambient-category-audit.md.
pub fn register_all(registry: &mut NativeMethodRegistry) {
    // EXPERIMENT (P0 over-tagging, per-group split)
    registry.with_category(NativeKind::Bridge, register_toolkit_natives);
    registry.with_category(NativeKind::Bridge, register_headless_natives);
    register_component_natives(registry);
    register_frame_natives(registry);
    registry.with_category(NativeKind::Bridge, |r| {
        register_graphics_natives(r);
        register_image_natives(r);
    });
    register_event_natives(registry);
    register_font_natives(registry);
    register_swing_natives(registry);
    register_clipboard_natives(registry);
}

// ---------------------------------------------------------------------------
// Helper: extract args
// ---------------------------------------------------------------------------

fn get_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(obj))) => Some(*obj),
        _ => None,
    }
}

fn get_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn get_bool(args: &[Value], idx: usize) -> bool {
    get_int(args, idx) != 0
}

fn void_ok() -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn int_ok(v: i32) -> MethodCallResult {
    Ok(Some(Value::Int(v)))
}

fn bool_ok(v: bool) -> MethodCallResult {
    Ok(Some(Value::Int(if v { 1 } else { 0 })))
}

fn obj_ok(obj: ObjectRef) -> MethodCallResult {
    Ok(Some(Value::Object(Some(obj))))
}

fn null_ok() -> MethodCallResult {
    Ok(Some(Value::Object(None)))
}

fn read_string(ctx: &dyn NativeContext, args: &[Value], idx: usize) -> Option<String> {
    get_obj(args, idx).and_then(|obj| ctx.read_string(obj))
}

/// Get or create peer for a Java component object.
///
/// gc-common w24-b: the peer is `obj`'s own ([`peer_of`]). When the identity
/// hash's peer belongs to another LIVE component of the VM, `obj` gets a peer
/// of its own (a collided mapping) instead of sharing it; when it belongs to
/// a collected one, the new peer takes over the mapping and the next sweep
/// destroys the old peer.
fn ensure_peer(ctx: &mut dyn NativeContext, obj: ObjectRef, ctype: ComponentType) -> PeerId {
    let (vm, hash) = obj_key(ctx, obj);
    if let Some(id) = peer_of(ctx, obj) {
        register_peer_source(ctx, id, obj, hash);
        return id;
    }
    sweep_dead_awt_rows(ctx);
    let primary = peer::peer_registry().lock().peer_for_java(vm, hash);
    let collides = primary.is_some_and(|p| {
        peer_row_verdict(vm, p, obj, &|h: usize| ctx.resolve_global_root(h)) == RowVerdict::Other
    });
    let id = {
        let mut reg = peer::peer_registry().lock();
        let id = reg.create_peer(ctype);
        if collides {
            reg.register_colliding_java_mapping(vm, hash, id);
        } else {
            reg.register_java_mapping(vm, hash, id);
        }
        id
    };
    register_peer_source(ctx, id, obj, hash);
    id
}

// ---------------------------------------------------------------------------
// Toolkit natives
// ---------------------------------------------------------------------------

/// Build a real `sun.awt.HeadlessToolkit` instance.
///
/// This mirrors the JDK's own headless code path: `Toolkit.getDefaultToolkit`
/// wraps the platform toolkit in a `HeadlessToolkit` when `java.awt.headless`
/// is true. On CratonVM the Windows platform toolkit (`sun.awt.windows.WToolkit`)
/// cannot be constructed — its `<init>` starts a native AWT message-pump thread
/// and blocks in `Object.wait()` for that thread to flip an `inited` flag, which
/// never happens without the native `awt.dll` event loop. So we construct the
/// `HeadlessToolkit` directly with a `null` underlying toolkit. `HeadlessToolkit`
/// is a real JDK class whose non-graphical methods are self-contained or throw
/// `HeadlessException`; the underlying-toolkit field is only consulted by the
/// handful of methods that legitimately delegate, and a `null` there matches
/// what a headless host without a display would expose.
fn build_headless_toolkit(ctx: &mut dyn NativeContext) -> MethodCallResult {
    // HeadlessToolkit(Toolkit) — store a null underlying toolkit. The
    // ctor's `instanceof ComponentFactory` check is null-safe.
    //
    // gc-common w19-g: `new_object_initialized`, not `new_object` + an
    // `<init>` invoke: the constructor is Java (a GC point) and the old pair
    // returned the pre-constructor address. The VM's `new_object_initialized`
    // keeps the object pinned across `<init>` and returns it forwarded.
    ctx.new_object_initialized(
        "sun/awt/HeadlessToolkit",
        "(Ljava/awt/Toolkit;)V",
        &[Value::Object(None)],
    )
}

// JDK-ONLY-CLASSIFY: unknown — needs census. Mixed group, must be split before
// any retag. `Toolkit.initIDs()V` and `sun/java2d/Disposer.initIDs()V` ARE
// ACC_NATIVE in JDK 25 and an empty body is the spec-correct implementation for
// a VM that resolves fields by name → bridge. But `Toolkit.getScreenSize`,
// `getScreenResolution`, `sync` and `beep` are *abstract* on `java.awt.Toolkit`,
// so registering natives on them intercepts every Toolkit subclass (the
// `Abstract*`-interception hazard), and `sun/awt/SunToolkit.getDefaultToolkit`
// does not exist in the image at all → compatibility shim. Evidence needed:
// per-triple `invocations` under a headless AWT run.
fn register_toolkit_natives(registry: &mut NativeMethodRegistry) {
    // KEEP (deliberate no-op): `initIDs` caches JNI field/method IDs for the
    // native disposer thread. CratonVM resolves fields by name and has no JNI
    // ID cache, so there is nothing to initialize — same rationale as the
    // `initIDs` block further down this file.
    registry.register_with_kind(
        "sun/java2d/Disposer",
        "initIDs",
        "()V",
        |_ctx, _args| void_ok(),
        NativeKind::Bridge,
    );
    // Toolkit.<clinit> calls this JNI bootstrap before ImageIO and Spring's
    // HTTP image converter can initialize desktop classes. CratonVM keeps the
    // relevant IDs in Rust-side registries, so the HotSpot native is a no-op.
    registry.register_with_kind(
        "java/awt/Toolkit",
        "initIDs",
        "()V",
        |_ctx, _args| void_ok(),
        NativeKind::Bridge,
    );
    // java.awt.Toolkit.getDefaultToolkit — return a real HeadlessToolkit.
    // The previous implementation returned `new java/awt/Toolkit`, but
    // `java.awt.Toolkit` is abstract; instances of it have no concrete
    // method bodies and any virtual call would mis-dispatch.
    registry.register(
        "java/awt/Toolkit",
        "getDefaultToolkit",
        "()Ljava/awt/Toolkit;",
        |ctx, _args| build_headless_toolkit(ctx),
    );
    registry.register(
        "java/awt/Toolkit",
        "getScreenSize",
        "()Ljava/awt/Dimension;",
        |ctx, _args| {
            let dim = ctx.new_object("java/awt/Dimension")?;
            if let Some(Value::Object(Some(obj))) = &dim {
                ctx.set_field_by_name(*obj, "width", Value::Int(1920));
                ctx.set_field_by_name(*obj, "height", Value::Int(1080));
            }
            Ok(dim)
        },
    );
    // KEEP (deliberate constant): 96 dpi is the companion of the fixed
    // 1920x1080 `getScreenSize` above — a stock JDK on Windows reports 96 for
    // an unscaled display, and the two values have to describe the same
    // fictional screen or `Toolkit`-derived layout maths comes out
    // inconsistent. A real headless JDK throws HeadlessException from both;
    // CratonVM deliberately answers instead so headless layout code runs.
    registry.register(
        "java/awt/Toolkit",
        "getScreenResolution",
        "()I",
        |_ctx, _args| int_ok(96),
    );
    registry.register("java/awt/Toolkit", "sync", "()V", |_ctx, _args| void_ok());
    registry.register("java/awt/Toolkit", "beep", "()V", |_ctx, _args| void_ok());
    registry.register(
        "sun/awt/SunToolkit",
        "getDefaultToolkit",
        "()Ljava/awt/Toolkit;",
        |ctx, _args| build_headless_toolkit(ctx),
    );
    // sun.awt.PlatformGraphicsInfo.createToolkit — the seam Toolkit.getDefaultToolkit
    // uses to obtain the platform toolkit. The real Windows path returns a
    // `WToolkit`, which cannot be constructed headlessly on CratonVM (see
    // `build_headless_toolkit`). Returning a `HeadlessToolkit` here means the
    // `instanceof HeadlessToolkit` guard further down `getDefaultToolkit`
    // short-circuits the wrap, which is exactly the headless contract.
    registry.register(
        "sun/awt/PlatformGraphicsInfo",
        "createToolkit",
        "()Ljava/awt/Toolkit;",
        |ctx, _args| build_headless_toolkit(ctx),
    );
}

// ---------------------------------------------------------------------------
// Headless GraphicsEnvironment natives
// ---------------------------------------------------------------------------

/// Register headless `java.awt.GraphicsEnvironment` / `sun.awt.PlatformGraphicsInfo`
/// natives.
///
/// Swing/AWT apps started with `-Djava.awt.headless=true` reach
/// `GraphicsEnvironment.getLocalGraphicsEnvironment()` early during AWT init.
/// In the stock JDK that returns `GraphicsEnvironment$LocalGE.INSTANCE`, built
/// by `LocalGE.createGE()`:
///
/// ```text
/// GraphicsEnvironment ge = PlatformGraphicsInfo.createGE();   // Win32GraphicsEnvironment
/// if (GraphicsEnvironment.isHeadless())
///     ge = new HeadlessGraphicsEnvironment(ge);               // headless wrapper
/// ```
///
/// CratonVM can already construct `Win32GraphicsEnvironment` and
/// `HeadlessGraphicsEnvironment`, but the `LocalGE.<clinit>` that wires them
/// together does not run cleanly under the partial bootstrap. Registering
/// `getLocalGraphicsEnvironment` as a native lets us reproduce the exact JDK
/// headless wiring without depending on the `LocalGE` static initializer.
// JDK-ONLY-CLASSIFY: bridge on Windows/macOS, DEAD on this image — and that
// is the whole finding. This marker used to assert that
// `sun/awt/PlatformGraphicsInfo.hasDisplays0()Z` is ACC_NATIVE in JDK 25. On a
// **Linux** JDK 25 image it is not there at all: `javap -p
// sun.awt.PlatformGraphicsInfo` lists exactly `createGE`, `createToolkit`,
// `getDefaultHeadlessProperty` and `getDefaultHeadlessMessage`, every one of
// them ordinary bytecode, and no `hasDisplays0`. `hasDisplays0` is declared by
// the Windows and macOS variants of the class, which a Unix image does not
// ship. The schema-3 census agrees per row: `declared: false`.
//
// So this registrar states nothing (L5b, 2026-08-05). `hasDisplays0` is a
// genuine OS-boundary bridge on the platforms that have it and a registration
// that can never bind on this one, and a census taken here cannot tell those
// two apart from a defect. The three siblings that ARE declared here have
// concrete bytecode — they restate a policy the real bytecode already derives —
// so they are shadows under §1.4, not §1.5 bridges. The split this marker asked
// for has been made in evidence rather than in code.
//
// The Windows-image census this was waiting on was taken on 2026-08-05 and a
// macOS one on 2026-08-10 (CratonVM adjudicates by parsing class bytes, so an
// unpacked image on any host answers). Both declare `hasDisplays0` ACC_NATIVE,
// and it states its kind. `ABSENT` here meant "not measured on this platform",
// never "dead" — which is why `scripts/jdk-only-dead-sweep.py` now refuses an
// image set that omits one.
//
// See l5-native-io-bridge-residuals-RETIRED-20260810.md.
fn register_headless_natives(registry: &mut NativeMethodRegistry) {
    // sun.awt.PlatformGraphicsInfo.getDefaultHeadlessProperty()Z — the JDK
    // consults this when `java.awt.headless` is unset. On a host with no
    // displays it returns true. CratonVM has no native display, so the
    // headless default is `true`, matching `hasDisplays == false`.
    registry.register(
        "sun/awt/PlatformGraphicsInfo",
        "getDefaultHeadlessProperty",
        "()Z",
        |_ctx, _args| bool_ok(true),
    );
    // sun.awt.PlatformGraphicsInfo.hasDisplays0()Z — native display probe.
    // No native display backend → no displays.
    registry.register_with_kind(
        "sun/awt/PlatformGraphicsInfo",
        "hasDisplays0",
        "()Z",
        |_ctx, _args| bool_ok(false),
        NativeKind::Bridge,
    );
    // sun.awt.PlatformGraphicsInfo.getDefaultHeadlessMessage — diagnostic
    // text shown when a headless app touches a graphics-only API.
    registry.register(
        "sun/awt/PlatformGraphicsInfo",
        "getDefaultHeadlessMessage",
        "()Ljava/lang/String;",
        |ctx, _args| {
            obj_ok(
                ctx.create_string(
                    "\nThis machine does not have a display; running in headless mode.",
                ),
            )
        },
    );

    // java.awt.GraphicsEnvironment.isHeadless()Z / .isHeadlessInstance()Z —
    // honour the `java.awt.headless` system property, defaulting to headless.
    registry.register(
        "java/awt/GraphicsEnvironment",
        "isHeadless",
        "()Z",
        |ctx, _args| bool_ok(headless_property(ctx)),
    );
    registry.register(
        "java/awt/GraphicsEnvironment",
        "isHeadlessInstance",
        "()Z",
        |ctx, _args| bool_ok(headless_property(ctx)),
    );

    // java.awt.GraphicsEnvironment.getLocalGraphicsEnvironment — reproduce
    // LocalGE.createGE(): build the platform GE, then wrap it for headless.
    registry.register(
        "java/awt/GraphicsEnvironment",
        "getLocalGraphicsEnvironment",
        "()Ljava/awt/GraphicsEnvironment;",
        |ctx, _args| build_local_graphics_environment(ctx),
    );

    // Font family enumeration.
    //
    // MEASURED (G81-1): this threw a NullPointerException, because the real
    // implementation walks `sun.font` machinery that has no platform font
    // service behind it here. An NPE is not an acceptable answer under EITHER
    // reading of the closure rule: it is not a working implementation, and it
    // is not a "specification-consistent error" — rule 5 names
    // `UnsupportedOperationException` and friends, not a null dereference from
    // the middle of the JDK.
    //
    // What is returned is the five LOGICAL font families, which the Java
    // specification guarantees every implementation provides
    // (`Font.DIALOG`, `DIALOG_INPUT`, `SANS_SERIF`, `SERIF`, `MONOSPACED`).
    // That is a truthful answer rather than a fabricated one: these are the
    // families this VM can actually name, and HotSpot lists all five too. It
    // is deliberately NOT the complete list HotSpot returns — physical fonts
    // are a platform service CratonVM does not have — and no test can assert
    // the complete list anyway, because it varies by machine.
    fn logical_font_families(ctx: &mut dyn NativeContext) -> MethodCallResult {
        const FAMILIES: [&str; 5] = ["Dialog", "DialogInput", "Monospaced", "SansSerif", "Serif"];
        let Some(string_class) = ctx.class_id_by_name("java/lang/String") else {
            return Ok(Some(Value::Object(None)));
        };
        // gc-common w19-g: the array is rooted across every name's allocation
        // and re-read before each store; it used to be held raw, so a
        // collection at the second `create_string` left the later names in
        // the vacated copy and returned that copy's address.
        let mut scope = NativeHandleScope::new(ctx);
        let arr_obj = scope.new_ref_array(string_class, FAMILIES.len());
        let arr_h = scope.root(arr_obj);
        for (i, name) in FAMILIES.iter().enumerate() {
            let s = scope.create_string(name);
            let arr = scope.get(&arr_h);
            scope.set_array_element(arr, i, Value::Object(Some(s)));
        }
        let arr = scope.get(&arr_h);
        Ok(Some(Value::Object(Some(arr))))
    }
    // Registered on the CONCRETE receiver, not on abstract
    // `java.awt.GraphicsEnvironment`. Measured: a registration on the abstract
    // class is not reached, because `HeadlessGraphicsEnvironment` declares its
    // own method with code and virtual dispatch correctly prefers it. That is
    // the same abstract-class interception this crate does elsewhere and should
    // not — see G79-1 §2 — so it is not repeated here.
    registry.register(
        "sun/java2d/HeadlessGraphicsEnvironment",
        "getAvailableFontFamilyNames",
        "()[Ljava/lang/String;",
        |ctx, _args| logical_font_families(ctx),
    );
    registry.register(
        "sun/java2d/HeadlessGraphicsEnvironment",
        "getAvailableFontFamilyNames",
        "(Ljava/util/Locale;)[Ljava/lang/String;",
        |ctx, _args| logical_font_families(ctx),
    );
}

/// Read the effective `java.awt.headless` value. Unset → default headless
/// (CratonVM has no native display).
fn headless_property(ctx: &dyn NativeContext) -> bool {
    match ctx.get_system_property("java.awt.headless") {
        Some(v) => !v.eq_ignore_ascii_case("false"),
        None => true,
    }
}

/// Build the local `GraphicsEnvironment`, mirroring `LocalGE.createGE()`.
fn build_local_graphics_environment(ctx: &mut dyn NativeContext) -> MethodCallResult {
    // PlatformGraphicsInfo.createGE() — the platform GraphicsEnvironment.
    // On Windows this is `sun.awt.Win32GraphicsEnvironment`, which CratonVM
    // can construct (its native chain is satisfied). Fall back to the
    // headless GE with no delegate if the platform GE cannot be built.
    let platform_ge = match ctx.invoke(
        "sun/awt/PlatformGraphicsInfo",
        "createGE",
        "()Ljava/awt/GraphicsEnvironment;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(obj)))) => Some(obj),
        _ => None,
    };

    if headless_property(ctx) {
        // new HeadlessGraphicsEnvironment(platformGE)
        //
        // gc-common w19-g: one `new_object_initialized` call, which pins the
        // platform GE argument across the allocation and the new object
        // across `<init>` (Java). The old `new_object` + `<init>` pair passed
        // the platform GE at its pre-allocation address and returned the
        // pre-constructor address of the headless one.
        return ctx.new_object_initialized(
            "sun/java2d/HeadlessGraphicsEnvironment",
            "(Ljava/awt/GraphicsEnvironment;)V",
            &[Value::Object(platform_ge)],
        );
    }

    match platform_ge {
        Some(obj) => obj_ok(obj),
        None => null_ok(),
    }
}

// ---------------------------------------------------------------------------
// Component natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — all 11 registrations (`setBounds`, `setVisible`,
// `setEnabled`, `repaint`, `requestFocus`, `getWidth`, `getHeight`, …) target
// `java.awt.Component` methods that have concrete bytecode in JDK 25.
// `Component` declares exactly one ACC_NATIVE method, `initIDs()V`, and this
// group does not register it. These re-implement class-library behaviour over
// the crate's Rust peer model, so under JdkOnly the real bytecode must win.
// They are currently tagged `Bridge` only because `register_all` inherits it.
fn register_component_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/Component", "setBounds", "(IIII)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let (x, y) = (get_int(args, 1), get_int(args, 2));
            let (w, h) = (
                get_int(args, 3).max(0) as u32,
                get_int(args, 4).max(0) as u32,
            );
            let pid = peer_of(ctx, this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get_mut(pid) {
                    peer.x = x;
                    peer.y = y;
                    peer.width = w;
                    peer.height = h;
                }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "setVisible", "(Z)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let visible = get_bool(args, 1);
            if let Some(pid) = peer_of(ctx, this) {
                {
                    let mut reg = peer::peer_registry().lock();
                    if let Some(peer) = reg.get_mut(pid) {
                        peer.visible = visible;
                    }
                }
                // gc-common w24-b: a shown component stays reachable.
                hold_shown_peer_source(ctx, pid, this, visible);
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "setEnabled", "(Z)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let enabled = get_bool(args, 1);
            let pid = peer_of(ctx, this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get_mut(pid) {
                    peer.enabled = enabled;
                }
            }
        }
        void_ok()
    });

    registry.register(
        "java/awt/Component",
        "setBackground",
        "(Ljava/awt/Color;)V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let color_val = get_obj(args, 1)
                    .map(|c| match ctx.get_field_by_name(c, "value") {
                        Value::Int(v) => v as u32,
                        _ => 0xFFFFFFFF,
                    })
                    .unwrap_or(0xFFFFFFFF);
                let pid = peer_of(ctx, this);
                let mut reg = peer::peer_registry().lock();
                if let Some(pid) = pid {
                    if let Some(peer) = reg.get_mut(pid) {
                        peer.background = color_val;
                    }
                }
            }
            void_ok()
        },
    );

    registry.register(
        "java/awt/Component",
        "setForeground",
        "(Ljava/awt/Color;)V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let color_val = get_obj(args, 1)
                    .map(|c| match ctx.get_field_by_name(c, "value") {
                        Value::Int(v) => v as u32,
                        _ => 0xFF000000,
                    })
                    .unwrap_or(0xFF000000);
                let pid = peer_of(ctx, this);
                let mut reg = peer::peer_registry().lock();
                if let Some(pid) = pid {
                    if let Some(peer) = reg.get_mut(pid) {
                        peer.foreground = color_val;
                    }
                }
            }
            void_ok()
        },
    );

    registry.register("java/awt/Component", "repaint", "()V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let pid = peer_of(ctx, this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get(pid) {
                    swing::swing_state()
                        .lock()
                        .mark_dirty(pid, 0, 0, peer.width, peer.height);
                }
            }
        }
        void_ok()
    });

    registry.register(
        "java/awt/Component",
        "getGraphics",
        "()Ljava/awt/Graphics;",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(pid) = peer_of(ctx, this) {
                    let gfx = ctx.new_object("java/awt/Graphics2D")?;
                    if let Some(Value::Object(Some(gfx_obj))) = &gfx {
                        register_gfx_for_peer(ctx, *gfx_obj, pid);
                    }
                    return Ok(gfx);
                }
            }
            null_ok()
        },
    );

    registry.register("java/awt/Component", "requestFocus", "()V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Some(pid) = peer_of(ctx, this) {
                swing::swing_state().lock().request_focus(pid);
            }
        }
        void_ok()
    });

    registry.register(
        "java/awt/Component",
        "setFont",
        "(Ljava/awt/Font;)V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(font_obj) = get_obj(args, 1) {
                    let family = ctx
                        .read_string(font_obj)
                        .unwrap_or_else(|| "Dialog".to_string());
                    let style = match ctx.get_field_by_name(font_obj, "style") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    let size = match ctx.get_field_by_name(font_obj, "size") {
                        Value::Int(v) => v,
                        _ => 12,
                    };
                    let pid = peer_of(ctx, this);
                    let mut reg = peer::peer_registry().lock();
                    if let Some(pid) = pid {
                        if let Some(peer) = reg.get_mut(pid) {
                            peer.font_family = family;
                            peer.font_style = style;
                            peer.font_size = size;
                        }
                    }
                }
            }
            void_ok()
        },
    );

    registry.register("java/awt/Component", "getWidth", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let pid = peer_of(ctx, this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get(pid) {
                    return int_ok(peer.width as i32);
                }
            }
        }
        int_ok(0)
    });

    registry.register("java/awt/Component", "getHeight", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let pid = peer_of(ctx, this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get(pid) {
                    return int_ok(peer.height as i32);
                }
            }
        }
        int_ok(0)
    });
}

// ---------------------------------------------------------------------------
// Frame natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — `java.awt.Frame`'s only ACC_NATIVE method is
// `initIDs()V`, which this group does not register. Four of the six entries
// have concrete bytecode; `toFront()V` and `toBack()V` are declared on
// `java.awt.Window`, not `Frame`, so those two registrations never match a
// method on the real class and are dead against a real JDK image.
fn register_frame_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/awt/Frame",
        "setTitle",
        "(Ljava/lang/String;)V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let title = read_string(ctx, args, 1).unwrap_or_default();
                let pid = ensure_peer(ctx, this, ComponentType::Frame);
                let mut reg = peer::peer_registry().lock();
                if let Some(peer) = reg.get_mut(pid) {
                    peer.title = title;
                }
            }
            void_ok()
        },
    );
    registry.register("java/awt/Frame", "setResizable", "(Z)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let resizable = get_bool(args, 1);
            let pid = peer_of(ctx, this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = pid {
                if let Some(peer) = reg.get_mut(pid) {
                    peer.resizable = resizable;
                }
            }
        }
        void_ok()
    });
    // KEEP (deliberate no-ops): all four are window-manager requests against
    // a peer that is never mapped to a display. `toFront`/`toBack` reorder a
    // stack that does not exist; `setIconImage`/`setMenuBar` decorate a frame
    // nobody can see. The JDK's own headless/offscreen peers do the same
    // thing, and none of the four has a getter whose answer we would be
    // falsifying by dropping the value.
    registry.register("java/awt/Frame", "toFront", "()V", |_ctx, _args| void_ok());
    registry.register("java/awt/Frame", "toBack", "()V", |_ctx, _args| void_ok());
    registry.register(
        "java/awt/Frame",
        "setIconImage",
        "(Ljava/awt/Image;)V",
        |_ctx, _args| void_ok(),
    );
    registry.register(
        "java/awt/Frame",
        "setMenuBar",
        "(Ljava/awt/MenuBar;)V",
        |_ctx, _args| void_ok(),
    );
}

// ---------------------------------------------------------------------------
// Graphics2D natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — 27 drawing primitives registered three times over
// (`java/awt/Graphics2D`, `sun/java2d/SunGraphics2D`, `java/awt/Graphics`).
// `java.awt.Graphics` declares NO native method in JDK 25; these are a software
// rasterizer standing in for the Java2D pipeline, i.e. an incomplete
// re-implementation of class-library behaviour, not a VM/OS boundary. Note the
// loop registers the same (name, descriptor) on an abstract superclass and its
// implementations, so the tag chosen here decides dispatch for every
// `Graphics` subclass a user program defines.
fn register_graphics_natives(registry: &mut NativeMethodRegistry) {
    for class in &[
        "java/awt/Graphics2D",
        "sun/java2d/SunGraphics2D",
        "java/awt/Graphics",
    ] {
        // ── Line / rect / oval / arc ──────────────────────────────
        registry.register(class, "drawLine", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x1, y1, x2, y2) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                with_gfx(ctx, this, |g| g.draw_line(x1, y1, x2, y2));
            }
            void_ok()
        });
        registry.register(class, "drawRect", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                with_gfx(ctx, this, |g| g.draw_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "fillRect", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                with_gfx(ctx, this, |g| g.fill_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "drawOval", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                with_gfx(ctx, this, |g| g.draw_oval(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "fillOval", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                with_gfx(ctx, this, |g| g.fill_oval(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "drawArc", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                let (start, extent) = (get_int(args, 5), get_int(args, 6));
                with_gfx(ctx, this, |g| g.draw_arc(x, y, w, h, start, extent));
            }
            void_ok()
        });
        registry.register(class, "fillArc", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (
                    get_int(args, 1),
                    get_int(args, 2),
                    get_int(args, 3),
                    get_int(args, 4),
                );
                let (start, extent) = (get_int(args, 5), get_int(args, 6));
                with_gfx(ctx, this, |g| g.fill_arc(x, y, w, h, start, extent));
            }
            void_ok()
        });

        // ── Text ──────────────────────────────────────────────────
        registry.register(
            class,
            "drawString",
            "(Ljava/lang/String;II)V",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    let text = read_string(ctx, args, 1).unwrap_or_default();
                    let (x, y) = (get_int(args, 2), get_int(args, 3));
                    with_gfx(ctx, this, |g| g.draw_string(&text, x, y));
                }
                void_ok()
            },
        );

        // ── Polygons / polylines ──────────────────────────────────
        registry.register(class, "drawPolygon", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let n = get_int(args, 3).max(0) as usize;
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.draw_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "fillPolygon", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let n = get_int(args, 3).max(0) as usize;
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.fill_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "drawPolyline", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let n = get_int(args, 3).max(0) as usize;
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a, n))
                    .unwrap_or_default();
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.draw_polyline(&xs[..n], &ys[..n]));
            }
            void_ok()
        });

        // ── Image blit ────────────────────────────────────────────
        //
        // ImageObserver-notification status (round-8/9 deferred → round-10):
        //
        // The Java API's `ImageObserver` is designed for *asynchronous*
        // image loading: when Toolkit.createImage/getImage returns, the
        // pixels may not be available yet, and `imageUpdate(image, flags,
        // x, y, w, h)` fires as progress is made (`WIDTH | HEIGHT`,
        // `SOMEBITS`, `FRAMEBITS`, `ALLBITS`). Once `ALLBITS` arrives, the
        // observer can stop redrawing.
        //
        // In this implementation, image loading is fully *synchronous*:
        //   * `BufferedImage` is constructed with all pixels materialised
        //     on the Java side before any `drawImage` can be called.
        //   * There is no async createImage/getImage path that returns
        //     before the bytes are ready — see `register_toolkit_natives`,
        //     which intentionally has no `createImage` / `getImage` /
        //     `prepareImage` natives.
        //
        // Because every blit here sees a fully-resolved pixel buffer, the
        // `ImageObserver` argument has nothing to observe — by the AWT
        // spec, when the image is already loaded the only meaningful
        // notification is `ALLBITS` and the return value of `drawImage`
        // is `true` (which is what we return below). The asynchronous
        // notification machinery becomes load-bearing only if the
        // Toolkit grows an async loader; this comment is the
        // deliberately-left-in-place record of that path so a future
        // async-loader change knows where to wire `imageUpdate(...)`.
        registry.register(
            class,
            "drawImage",
            "(Ljava/awt/Image;IILjava/awt/image/ImageObserver;)Z",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    if let Some(img) = get_obj(args, 1) {
                        let (x, y) = (get_int(args, 2), get_int(args, 3));
                        if let Some(id) = buffered_image_id(ctx, img) {
                            // Acquire locks in the same order as `with_gfx_entry`
                            // (Gfx entry first, then image registry) so the two
                            // can't deadlock when racing on the same Graphics2D.
                            let handle = gfx_handle_for(ctx, this);
                            let mut entry = handle.lock();
                            draw_buffered_image(&mut entry, id, x, y);
                        }
                    }
                }
                // Sync-load contract: image is fully available, no
                // ImageObserver follow-up needed; return true per AWT
                // spec for "drawing has been completed".
                bool_ok(true)
            },
        );

        // ── Color / paint ─────────────────────────────────────────
        registry.register(class, "setColor", "(Ljava/awt/Color;)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let argb = get_obj(args, 1)
                    .map(|c| match ctx.get_field_by_name(c, "value") {
                        Value::Int(v) => v as u32,
                        _ => 0xFF_000000,
                    })
                    .unwrap_or(0xFF_000000);
                let a = ((argb >> 24) & 0xFF) as u8;
                let r = ((argb >> 16) & 0xFF) as u8;
                let g = ((argb >> 8) & 0xFF) as u8;
                let b = (argb & 0xFF) as u8;
                with_gfx(ctx, this, |gs| gs.set_color(r, g, b, a));
            }
            void_ok()
        });

        // G80-1 N2. Both of these were MISSING, and because the real classes
        // are abstract the call did not fall through to anything — it raised
        // `AbstractMethodError: has no Code attribute`, which a user program
        // cannot work around. `setColor` was registered directly above and
        // `clearRect` consumes the background, so both gaps sat next to their
        // own other half; that adjacency is how they survived.
        registry.register(class, "getColor", "()Ljava/awt/Color;", |ctx, args| {
            let argb = match get_obj(args, 0) {
                Some(this) => with_gfx(ctx, this, |gs| gs.color()),
                None => 0xFF_000000,
            };
            let color_obj = ctx.new_object("java/awt/Color")?;
            if let Some(Value::Object(Some(obj))) = &color_obj {
                ctx.set_field_by_name(*obj, "value", Value::Int(argb as i32));
            }
            Ok(color_obj)
        });
        registry.register(
            class,
            "setBackground",
            "(Ljava/awt/Color;)V",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    let argb = get_obj(args, 1)
                        .map(|c| match ctx.get_field_by_name(c, "value") {
                            Value::Int(v) => v as u32,
                            _ => 0xFF_FFFFFF,
                        })
                        .unwrap_or(0xFF_FFFFFF);
                    with_gfx(ctx, this, |gs| gs.set_background(argb));
                }
                void_ok()
            },
        );
        registry.register(class, "getBackground", "()Ljava/awt/Color;", |ctx, args| {
            let argb = match get_obj(args, 0) {
                Some(this) => with_gfx(ctx, this, |gs| gs.background()),
                None => 0xFF_FFFFFF,
            };
            let color_obj = ctx.new_object("java/awt/Color")?;
            if let Some(Value::Object(Some(obj))) = &color_obj {
                ctx.set_field_by_name(*obj, "value", Value::Int(argb as i32));
            }
            Ok(color_obj)
        });

        // ── Font ──────────────────────────────────────────────────
        registry.register(class, "setFont", "(Ljava/awt/Font;)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(font) = get_obj(args, 1) {
                    let family = ctx
                        .read_string(font)
                        .unwrap_or_else(|| "Dialog".to_string());
                    let style = match ctx.get_field_by_name(font, "style") {
                        Value::Int(v) => v,
                        _ => 0,
                    };
                    let size = match ctx.get_field_by_name(font, "size") {
                        Value::Int(v) => v,
                        _ => 12,
                    };
                    with_gfx(ctx, this, |g| g.set_font(&family, style, size));
                }
            }
            void_ok()
        });

        // ── Clip ──────────────────────────────────────────────────
        registry.register(class, "setClip", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y) = (get_int(args, 1), get_int(args, 2));
                let (w, h) = (
                    get_int(args, 3).max(0) as u32,
                    get_int(args, 4).max(0) as u32,
                );
                with_gfx(ctx, this, |g| g.set_clip_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(
            class,
            "getClipBounds",
            "()Ljava/awt/Rectangle;",
            |ctx, args| {
                let bounds = if let Some(this) = get_obj(args, 0) {
                    let handle = existing_gfx_handle(ctx, this);
                    handle.and_then(|h| h.lock().state.get_clip_bounds())
                } else {
                    None
                };
                let rect = ctx.new_object("java/awt/Rectangle")?;
                if let Some(Value::Object(Some(obj))) = &rect {
                    if let Some(r) = bounds {
                        ctx.set_field_by_name(*obj, "x", Value::Int(r.x));
                        ctx.set_field_by_name(*obj, "y", Value::Int(r.y));
                        ctx.set_field_by_name(*obj, "width", Value::Int(r.width as i32));
                        ctx.set_field_by_name(*obj, "height", Value::Int(r.height as i32));
                    }
                }
                Ok(rect)
            },
        );

        // ── Transform ─────────────────────────────────────────────
        registry.register(
            class,
            "setTransform",
            "(Ljava/awt/geom/AffineTransform;)V",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    if let Some(t) = get_obj(args, 1) {
                        let read = |name: &str| -> f64 {
                            match ctx.get_field_by_name(t, name) {
                                Value::Double(v) => v,
                                Value::Float(v) => v as f64,
                                _ => 0.0,
                            }
                        };
                        let xform = crate::renderer::AffineTransform {
                            m00: read("m00"),
                            m01: read("m01"),
                            m02: read("m02"),
                            m10: read("m10"),
                            m11: read("m11"),
                            m12: read("m12"),
                        };
                        with_gfx(ctx, this, |g| g.set_transform(xform));
                    }
                }
                void_ok()
            },
        );
        registry.register(
            class,
            "getTransform",
            "()Ljava/awt/geom/AffineTransform;",
            |ctx, args| {
                let xform = if let Some(this) = get_obj(args, 0) {
                    let handle = existing_gfx_handle(ctx, this);
                    handle
                        .map(|h| h.lock().state.get_transform())
                        .unwrap_or_else(crate::renderer::AffineTransform::identity)
                } else {
                    crate::renderer::AffineTransform::identity()
                };
                let obj = ctx.new_object("java/awt/geom/AffineTransform")?;
                if let Some(Value::Object(Some(t))) = &obj {
                    ctx.set_field_by_name(*t, "m00", Value::Double(xform.m00));
                    ctx.set_field_by_name(*t, "m01", Value::Double(xform.m01));
                    ctx.set_field_by_name(*t, "m02", Value::Double(xform.m02));
                    ctx.set_field_by_name(*t, "m10", Value::Double(xform.m10));
                    ctx.set_field_by_name(*t, "m11", Value::Double(xform.m11));
                    ctx.set_field_by_name(*t, "m12", Value::Double(xform.m12));
                }
                Ok(obj)
            },
        );
        registry.register(class, "translate", "(II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (dx, dy) = (get_int(args, 1) as f64, get_int(args, 2) as f64);
                with_gfx(ctx, this, |g| g.translate(dx, dy));
            }
            void_ok()
        });
        registry.register(class, "rotate", "(D)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let theta = get_double(args, 1);
                with_gfx(ctx, this, |g| g.rotate(theta));
            }
            void_ok()
        });
        registry.register(class, "scale", "(DD)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (sx, sy) = (get_double(args, 1), get_double(args, 2));
                with_gfx(ctx, this, |g| g.scale(sx, sy));
            }
            void_ok()
        });

        // ── Rendering hints ───────────────────────────────────────
        registry.register(
            class,
            "setRenderingHint",
            "(Ljava/awt/RenderingHints$Key;Ljava/lang/Object;)V",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    use crate::graphics2d::{RenderingHintKey as K, RenderingHintValue as V};
                    // Real JDK encodes hint identity as an int `privatekey` on
                    // `RenderingHints.Key` and on each value singleton. Read
                    // those fields and map the standard `java.awt.RenderingHints`
                    // constants. Without this mapping we'd unconditionally turn
                    // antialiasing on, which silently corrupts apps that
                    // explicitly request AA=OFF (e.g. pixel art renderers).
                    let key_obj = get_obj(args, 1);
                    let val_obj = get_obj(args, 2);
                    let key_id = key_obj
                        .map(|o| match ctx.get_field_by_name(o, "privatekey") {
                            Value::Int(v) => v,
                            _ => -1,
                        })
                        .unwrap_or(-1);
                    let val_id = val_obj
                        .map(|o| match ctx.get_field_by_name(o, "privatekey") {
                            Value::Int(v) => v,
                            _ => -1,
                        })
                        .unwrap_or(-1);

                    // SunHints constant identifiers (mirror real JDK numbering).
                    let key = match key_id {
                        1 => Some(K::Antialiasing),     // KEY_ANTIALIASING
                        9 => Some(K::TextAntialiasing), // KEY_TEXT_ANTIALIASING
                        5 => Some(K::Interpolation),    // KEY_INTERPOLATION
                        _ => None,
                    };
                    let value = match val_id {
                        1 | 9 => Some(V::On),                         // VALUE_*_ON
                        2 | 10 => Some(V::Off),                       // VALUE_*_OFF
                        195 => Some(V::BilinearInterpolation), // VALUE_INTERPOLATION_BILINEAR
                        196 => Some(V::NearestNeighborInterpolation), // VALUE_INTERPOLATION_NEAREST_NEIGHBOR
                        197 => Some(V::BicubicInterpolation),         // VALUE_INTERPOLATION_BICUBIC
                        0 | -1 => None,
                        _ => Some(V::Default),
                    };

                    if let (Some(k), Some(v)) = (key, value) {
                        with_gfx(ctx, this, |g| g.set_rendering_hint(k, v));
                    } else if key_id < 0 {
                        // Fields unreadable (RenderingHints clinit may not have
                        // run): fall back to previous best-effort AA=on so apps
                        // that explicitly request AA still get it.
                        with_gfx(ctx, this, |g| g.set_rendering_hint(K::Antialiasing, V::On));
                    }
                    // else: known key but unrecognised value — leave state alone.
                }
                void_ok()
            },
        );

        // ── Stroke ────────────────────────────────────────────────
        registry.register(class, "setStroke", "(Ljava/awt/Stroke;)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                use crate::graphics2d::{CapStyle, JoinStyle, StrokeSpec};
                if let Some(stroke) = get_obj(args, 1) {
                    let width = match ctx.get_field_by_name(stroke, "width") {
                        Value::Float(v) => v,
                        Value::Double(v) => v as f32,
                        Value::Int(v) => v as f32,
                        _ => 1.0,
                    };
                    let cap = match ctx.get_field_by_name(stroke, "cap") {
                        Value::Int(0) => CapStyle::Butt,
                        Value::Int(1) => CapStyle::Round,
                        _ => CapStyle::Square,
                    };
                    let join = match ctx.get_field_by_name(stroke, "join") {
                        Value::Int(1) => JoinStyle::Round,
                        Value::Int(2) => JoinStyle::Bevel,
                        _ => JoinStyle::Miter,
                    };
                    with_gfx(ctx, this, |g| g.set_stroke(StrokeSpec { width, cap, join }));
                }
            }
            void_ok()
        });

        // ── Create / dispose ──────────────────────────────────────
        registry.register(class, "create", "()Ljava/awt/Graphics;", |ctx, args| {
            // Cloning a Graphics2D returns a new receiver that draws into the
            // same image / peer and starts from a copy of the parent's state
            // (gc-common w25-d: it used to start from the default state).
            // Only the parent's context (a Rust `Arc`) is held across the
            // allocation, never its `ObjectRef`.
            let parent = match get_obj(args, 0) {
                Some(this) => existing_gfx_handle(ctx, this),
                None => None,
            };
            let new_gfx = ctx.new_object("java/awt/Graphics2D")?;
            if let Some(Value::Object(Some(obj))) = &new_gfx {
                file_gfx_row(ctx, *obj, child_gfx_entry(parent.as_ref()));
            }
            Ok(new_gfx)
        });
        registry.register(class, "dispose", "()V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                dispose_gfx(ctx, this);
            }
            void_ok()
        });

        // ── Clear / copy ──────────────────────────────────────────
        registry.register(class, "clearRect", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y) = (get_int(args, 1), get_int(args, 2));
                let (w, h) = (
                    get_int(args, 3).max(0) as u32,
                    get_int(args, 4).max(0) as u32,
                );
                with_gfx(ctx, this, |g| g.clear_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "copyArea", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y) = (get_int(args, 1), get_int(args, 2));
                let (w, h) = (
                    get_int(args, 3).max(0) as u32,
                    get_int(args, 4).max(0) as u32,
                );
                let (dx, dy) = (get_int(args, 5), get_int(args, 6));
                with_gfx(ctx, this, |g| g.copy_area(x, y, w, h, dx, dy));
            }
            void_ok()
        });
    }
}

// ---------------------------------------------------------------------------
// BufferedImage natives
// ---------------------------------------------------------------------------

/// Maximum width/height accepted for a BufferedImage. Mirrors the practical
/// per-dimension cap of the reference JDK's raster (a positive 16-bit value)
/// and bounds the backing allocation to `MAX_IMAGE_DIM^2 * 4` bytes (~4 GiB
/// worst case) — large requests are rejected instead of aborting the process.
const MAX_IMAGE_DIM: i32 = 32767;

const IMAGEIO_STREAM_BUFFER_SIZE: usize = 8192;
const IMAGEIO_MAX_STREAM_BYTES: usize = 128 * 1024 * 1024;

fn imageio_io_error(message: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IOException {
        message: message.into(),
    }
    .into()
}

fn imageio_illegal_arg(message: impl Into<String>) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

fn read_all_from_input_stream(
    ctx: &mut dyn NativeContext,
    input: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let base = ctx.pin_native_root(input);
    let result = (|| {
        let scratch = ctx.new_array(ArrayElementType::Byte, IMAGEIO_STREAM_BUFFER_SIZE);
        let scratch_pin = ctx.pin_native_root(scratch);
        let mut out = Vec::new();
        let mut zero_reads = 0usize;

        loop {
            let input = ctx.read_native_pin(base, input);
            let scratch = ctx.read_native_pin(scratch_pin, scratch);
            let read = ctx.invoke_virtual(
                input,
                "read",
                "([BII)I",
                &[
                    Value::Object(Some(scratch)),
                    Value::Int(0),
                    Value::Int(IMAGEIO_STREAM_BUFFER_SIZE as i32),
                ],
            )?;
            let n = match read {
                Some(Value::Int(n)) => n,
                _ => return Err(imageio_io_error("InputStream.read did not return int")),
            };
            if n < 0 {
                break;
            }
            if n == 0 {
                zero_reads += 1;
                if zero_reads > 16 {
                    return Err(imageio_io_error(
                        "InputStream.read returned repeated zero bytes",
                    ));
                }
                continue;
            }
            zero_reads = 0;
            let n = n as usize;
            if n > IMAGEIO_STREAM_BUFFER_SIZE {
                return Err(imageio_io_error(format!(
                    "InputStream.read returned {n} bytes into {IMAGEIO_STREAM_BUFFER_SIZE}-byte buffer"
                )));
            }
            if out
                .len()
                .checked_add(n)
                .map_or(true, |len| len > IMAGEIO_MAX_STREAM_BYTES)
            {
                return Err(imageio_io_error(format!(
                    "ImageIO input exceeds {IMAGEIO_MAX_STREAM_BYTES} byte native limit"
                )));
            }
            let scratch = ctx.read_native_pin(scratch_pin, scratch);
            let mut chunk = vec![0u8; n];
            let copied = ctx.read_byte_array_into(scratch, 0, &mut chunk);
            if copied != n {
                return Err(imageio_io_error("failed to copy InputStream bytes"));
            }
            out.extend_from_slice(&chunk);
        }

        Ok(out)
    })();
    ctx.unpin_native_roots(base);
    result
}

fn write_all_to_output_stream(
    ctx: &mut dyn NativeContext,
    output: ObjectRef,
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    let base = ctx.pin_native_root(output);
    let result = (|| {
        let scratch_len = bytes.len().min(IMAGEIO_STREAM_BUFFER_SIZE).max(1);
        let scratch = ctx.new_array(ArrayElementType::Byte, scratch_len);
        let scratch_pin = ctx.pin_native_root(scratch);
        let mut offset = 0usize;

        while offset < bytes.len() {
            let n = (bytes.len() - offset).min(scratch_len);
            let scratch = ctx.read_native_pin(scratch_pin, scratch);
            if !ctx.write_byte_array_from(scratch, 0, &bytes[offset..offset + n]) {
                return Err(imageio_io_error(
                    "failed to populate OutputStream write buffer",
                ));
            }
            let output = ctx.read_native_pin(base, output);
            let scratch = ctx.read_native_pin(scratch_pin, scratch);
            ctx.invoke_virtual(
                output,
                "write",
                "([BII)V",
                &[
                    Value::Object(Some(scratch)),
                    Value::Int(0),
                    Value::Int(n as i32),
                ],
            )?;
            offset += n;
        }

        let output = ctx.read_native_pin(base, output);
        ctx.invoke_virtual(output, "flush", "()V", &[])?;
        Ok(())
    })();
    ctx.unpin_native_roots(base);
    result
}

/// `BufferedImage` -> raster id, keyed by the image's `(vm, identity hash)`
/// ([`ObjKey`], gc-common w23-a). gc-common w24-b: one row per live image
/// (a [`KeyedRows`], each row with a weak root of its image); the raster of an
/// image the collector found dead is freed by `sweep_dead_awt_rows`.
struct BufferedImageTable {
    by_key: KeyedRows<ImageId>,
    /// Rasters whose row the SAME image re-bound (a second `<init>` on one
    /// object), or -- without weak roots -- a later image with the same key
    /// displaced (an identity-hash collision inside one VM). The displaced
    /// image may still reach its raster through its `imageId` field, so the
    /// raster is kept; it is remembered here only so its VM's teardown can
    /// free it.
    displaced: Vec<(usize, ImageId)>,
}

/// What [`BufferedImageTable::bind`] leaves for its caller to free.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImageBindLeftovers {
    /// Weak roots of the displaced rows, to release through the VM.
    weak_roots: Vec<usize>,
    /// Rasters of displaced rows whose image was collected.
    rasters: Vec<ImageId>,
}

impl BufferedImageTable {
    fn bind(
        &mut self,
        key: ObjKey,
        obj: ObjectRef,
        weak: usize,
        image_id: ImageId,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> ImageBindLeftovers {
        let mut leftovers = ImageBindLeftovers::default();
        for d in self.by_key.insert(key, obj, weak, image_id, resolve) {
            if d.row.weak != 0 {
                leftovers.weak_roots.push(d.row.weak);
            }
            if d.dead {
                leftovers.rasters.push(d.row.value);
            } else if d.row.value != image_id {
                self.displaced.push((key.0, d.row.value));
            }
        }
        leftovers
    }

    fn get(
        &self,
        key: ObjKey,
        obj: ObjectRef,
        resolve: &dyn Fn(usize) -> Option<ObjectRef>,
    ) -> Option<ImageId> {
        self.by_key.get(key, obj, resolve)
    }

    /// Drop `vm`'s rows (VM teardown) and return the raster ids they named.
    fn forget_vm(&mut self, vm: usize) -> Vec<ImageId> {
        let mut mine: Vec<ImageId> = self
            .by_key
            .forget_vm(vm)
            .into_iter()
            .map(|row| row.value)
            .collect();
        self.displaced.retain(|&(owner, id)| {
            if owner == vm {
                mine.push(id);
                false
            } else {
                true
            }
        });
        mine
    }
}

fn buffered_image_ids() -> &'static Mutex<BufferedImageTable> {
    static INSTANCE: OnceLock<Mutex<BufferedImageTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        Mutex::new(BufferedImageTable {
            by_key: KeyedRows::new(),
            displaced: Vec::new(),
        })
    })
}

/// File `image_id` as the raster of the freshly constructed image `obj`
/// (gc-common w24-b: with a weak root of it, after the VM's pending sweep).
/// No GC point: `obj` stays current for the caller.
fn bind_buffered_image(ctx: &mut dyn NativeContext, obj: ObjectRef, image_id: ImageId) {
    sweep_dead_awt_rows(ctx);
    let key = obj_key(ctx, obj);
    let weak = ctx.add_weak_global_root(obj);
    let leftovers = buffered_image_ids().lock().bind(key, obj, weak, image_id, &|h: usize| {
        ctx.resolve_global_root(h)
    });
    for weak in leftovers.weak_roots {
        ctx.remove_global_root(weak);
    }
    if !leftovers.rasters.is_empty() {
        let mut reg = image::image_registry();
        for id in leftovers.rasters {
            reg.destroy(id);
        }
    }
}

fn buffered_image_id(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<ImageId> {
    if let Value::Long(id) = ctx.get_field_by_name(obj, "imageId") {
        return Some(ImageId(id as u64));
    }
    let key = obj_key(ctx, obj);
    buffered_image_ids()
        .lock()
        .get(key, obj, &|h: usize| ctx.resolve_global_root(h))
}

fn decode_buffered_image(ctx: &mut dyn NativeContext, bytes: &[u8]) -> MethodCallResult {
    let decoded = image::BufferedImageData::decode_encoded(bytes).map_err(imageio_io_error)?;
    let created = ctx.new_object_initialized(
        "java/awt/image/BufferedImage",
        "(III)V",
        &[
            Value::Int(decoded.width() as i32),
            Value::Int(decoded.height() as i32),
            Value::Int(ImageType::IntArgb as i32),
        ],
    )?;
    let Some(Value::Object(Some(obj))) = created else {
        return null_ok();
    };
    let Some(id) = buffered_image_id(ctx, obj) else {
        return null_ok();
    };
    let mut reg = image::image_registry();
    let Some(dst) = reg.get_mut(id) else {
        return null_ok();
    };
    decoded.copy_to(dst);
    obj_ok(obj)
}

fn imageio_reader_input_stream(ctx: &dyn NativeContext, reader: ObjectRef) -> Option<ObjectRef> {
    for field in ["iis", "input"] {
        if let Value::Object(Some(stream)) = ctx.get_field_by_name(reader, field) {
            return Some(stream);
        }
    }
    None
}

fn imageio_writer_output_stream(
    ctx: &mut dyn NativeContext,
    writer: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    for field in ["stream", "output"] {
        if let Value::Object(Some(output)) = ctx.get_field_by_name(writer, field) {
            return Ok(output);
        }
    }
    match ctx.invoke_virtual(writer, "getOutput", "()Ljava/lang/Object;", &[])? {
        Some(Value::Object(Some(output))) => Ok(output),
        _ => Err(imageio_io_error("ImageWriter output is not set")),
    }
}

fn iio_image_rendered_image(
    ctx: &mut dyn NativeContext,
    iio_image: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Value::Object(Some(image)) = ctx.get_field_by_name(iio_image, "image") {
        return Ok(image);
    }
    match ctx.invoke_virtual(
        iio_image,
        "getRenderedImage",
        "()Ljava/awt/image/RenderedImage;",
        &[],
    )? {
        Some(Value::Object(Some(image))) => Ok(image),
        _ => Err(imageio_illegal_arg("image == null!")),
    }
}

fn imageio_reader_read(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    format: image::EncodedImageFormat,
) -> MethodCallResult {
    let _ = format;
    let reader = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("reader == null!"))?;
    let stream = imageio_reader_input_stream(ctx, reader)
        .ok_or_else(|| imageio_io_error("ImageReader input is not set"))?;
    let bytes = read_all_from_input_stream(ctx, stream)?;
    decode_buffered_image(ctx, &bytes)
}

fn imageio_writer_write(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    format: image::EncodedImageFormat,
) -> MethodCallResult {
    let writer = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("writer == null!"))?;
    let iio_image = get_obj(args, 2).ok_or_else(|| imageio_illegal_arg("image == null!"))?;
    // gc-common w23-a: both lookups below may run Java (`getRenderedImage`,
    // `getOutput`) when the field is absent -- GC points. The writer used to
    // be held raw across the first and the image across the second.
    let base = ctx.pin_native_root(writer);
    let result = (|| -> MethodCallResult {
        let image_obj = iio_image_rendered_image(ctx, iio_image)?;
        let image_pin = ctx.pin_native_root(image_obj);
        let writer = ctx.read_native_pin(base, writer);
        let output = imageio_writer_output_stream(ctx, writer)?;
        let image_obj = ctx.read_native_pin(image_pin, image_obj);
        // `encode_rendered_image` reads fields only; `write_all_to_output_stream`
        // pins `output` itself.
        let bytes = encode_rendered_image(ctx, image_obj, format)?;
        write_all_to_output_stream(ctx, output, &bytes)?;
        Ok(None)
    })();
    ctx.unpin_native_roots(base);
    result
}

fn encode_rendered_image(
    ctx: &dyn NativeContext,
    image_obj: ObjectRef,
    format: image::EncodedImageFormat,
) -> Result<Vec<u8>, MethodCallFailed> {
    let Some(id) = buffered_image_id(ctx, image_obj) else {
        return Err(imageio_illegal_arg(
            "ImageIO.write only supports CratonVM BufferedImage-backed RenderedImage instances",
        ));
    };
    let image = {
        let reg = image::image_registry();
        reg.get(id).cloned()
    }
    .ok_or_else(|| imageio_illegal_arg("BufferedImage backing store is missing"))?;
    image.encode(format).map_err(imageio_io_error)
}

// JDK-ONLY-CLASSIFY: unknown — needs census. Mixed group, must be split. Seven
// entries are genuine bridges: `com/sun/imageio/plugins/jpeg/JPEGImageReader`'s
// `initJPEGImageReader`, `setSource`, `resetReader`, `resetLibraryState`,
// `disposeReader` and the `initReaderIDs`/`initWriterIDs` pair are all
// ACC_NATIVE in JDK 25 and back libjpeg, which this VM does not link. The
// remaining 15 (`BufferedImage.createGraphics`, `getRGB`/`setRGB`, …) have
// concrete bytecode and are stubs; `BufferedImage.flush()V` is inherited from
// `java.awt.Image` and does not exist on `BufferedImage` itself.
// ---------------------------------------------------------------------------
// G80-1 N1 option (A) — give BufferedImage a REAL raster and colour model
// ---------------------------------------------------------------------------
//
// `BufferedImage.getRaster()`, `getSampleModel()` and `getColorModel()`
// answered `null` under --jdk-only, because our `<init>` shim shadows the real
// constructor and never populated the fields the real one builds. Returning
// `null` from a method that cannot return `null` is the one behaviour nobody
// would defend, so it is fixed here.
//
// The 4770 lines of renderer/graphics2d/image are deliberately VM-INDEPENDENT
// (zero `NativeContext` references), so the rasterizer cannot draw into a Java
// `int[]`. That rules out making the Java array the single backing store
// without an ownership inversion. What is done instead — measured feasible
// first — is to build the genuine JDK objects, which all work verbatim under
// --jdk-only (`DataBufferInt`, `Raster.createPackedRaster`,
// `SinglePixelPackedSampleModel`, `DirectColorModel` were each verified
// identical to HotSpot before a line of this was written), and to SYNCHRONISE
// the pixels into the data buffer at the point the raster is handed out.
//
// THE LIMIT, stated rather than discovered later: the returned raster is a
// SNAPSHOT, not a view. Pixels written through it do not flow back into the
// rasterizer's buffer. Reads are exact; writes through the raster are lost.
// Option (B) in the record removes that limit and costs the VM-independence of
// four thousand lines.

/// Band masks for the packed int layouts we hand out a raster for.
fn packed_masks(image_type: i32) -> Option<[i32; 4]> {
    match image_type {
        // TYPE_INT_RGB — 3 bands, no alpha.
        1 => Some([0x00FF0000u32 as i32, 0x0000FF00, 0x000000FF, 0]),
        // TYPE_INT_ARGB — 4 bands.
        2 => Some([
            0x00FF0000u32 as i32,
            0x0000FF00,
            0x000000FF,
            0xFF000000u32 as i32,
        ]),
        _ => None,
    }
}

/// Build the real `DataBufferInt` / `WritableRaster` / `ColorModel` trio for an
/// image and stamp them onto the `BufferedImage`'s own fields.
///
/// Every object here is built by REAL JDK bytecode; nothing is fabricated.
///
/// gc-common w19-g: every object is Java-built (`<init>`, `createPackedRaster`),
/// so each is a GC point. `this`, the data buffer and the raster are rooted in
/// one handle scope and re-read after the calls that follow them; before, all
/// three were held raw, so a collection inside the colour model's constructor
/// stamped the fields onto the vacated `BufferedImage`.
fn attach_real_raster(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    w: i32,
    h: i32,
    image_type: i32,
) {
    let Some(masks) = packed_masks(image_type) else {
        return;
    };
    let has_alpha = image_type == 2;
    let nbands = if has_alpha { 4 } else { 3 };

    let size = match w.checked_mul(h) {
        Some(v) if v >= 0 => v,
        _ => return,
    };
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let Ok(Some(Value::Object(Some(db_obj)))) =
        scope.new_object_initialized("java/awt/image/DataBufferInt", "(I)V", &[Value::Int(size)])
    else {
        return;
    };
    let db_h = scope.root(db_obj);

    let mask_arr = scope.new_array(ArrayElementType::Int, nbands);
    for (i, m) in masks.iter().take(nbands).enumerate() {
        scope.set_array_element(mask_arr, i, Value::Int(*m));
    }
    let db = scope.get(&db_h);

    let raster_obj = match scope.invoke(
        "java/awt/image/Raster",
        "createPackedRaster",
        "(Ljava/awt/image/DataBuffer;III[ILjava/awt/Point;)Ljava/awt/image/WritableRaster;",
        &[
            Value::Object(Some(db)),
            Value::Int(w),
            Value::Int(h),
            Value::Int(w),
            Value::Object(Some(mask_arr)),
            Value::Object(None),
        ],
    ) {
        Ok(Some(Value::Object(Some(r)))) => r,
        _ => return,
    };
    let raster_h = scope.root(raster_obj);

    let cm_desc = if has_alpha { "(IIIII)V" } else { "(IIII)V" };
    let cm_args: Vec<Value> = if has_alpha {
        vec![
            Value::Int(32),
            Value::Int(masks[0]),
            Value::Int(masks[1]),
            Value::Int(masks[2]),
            Value::Int(masks[3]),
        ]
    } else {
        vec![
            Value::Int(24),
            Value::Int(masks[0]),
            Value::Int(masks[1]),
            Value::Int(masks[2]),
        ]
    };
    let Ok(Some(Value::Object(Some(cm)))) =
        scope.new_object_initialized("java/awt/image/DirectColorModel", cm_desc, &cm_args)
    else {
        return;
    };

    let raster = scope.get(&raster_h);
    let this = scope.get(&this_h);
    scope.set_field_by_name(this, "raster", Value::Object(Some(raster)));
    scope.set_field_by_name(this, "colorModel", Value::Object(Some(cm)));
}

/// Pins `this` across [`sync_raster_pixels_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
fn sync_raster_pixels(ctx: &mut dyn NativeContext, this: &mut ObjectRef) {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = sync_raster_pixels_body(ctx, *this);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

/// Copy the rasterizer's pixels into the real `DataBufferInt` behind `this`'s
/// raster, so a raster handed to Java reflects what has been drawn.
///
/// Called at the points where a raster (or its data) leaves for Java. See the
/// SNAPSHOT limit in the block comment above.
fn sync_raster_pixels_body(ctx: &mut dyn NativeContext, this: ObjectRef) {
    let Some(id) = buffered_image_id(ctx, this) else {
        return;
    };
    let pixels: Vec<u32> = {
        let reg = image::image_registry();
        match reg.get(id) {
            Some(img) => img.get_rgb_region(0, 0, img.width(), img.height()),
            None => return,
        }
    };
    let Value::Object(Some(raster)) = ctx.get_field_by_name(this, "raster") else {
        return;
    };
    let Ok(Some(Value::Object(Some(db)))) = ctx.invoke_virtual(
        raster,
        "getDataBuffer",
        "()Ljava/awt/image/DataBuffer;",
        &[],
    ) else {
        return;
    };
    let Value::Object(Some(data)) = ctx.get_field_by_name(db, "data") else {
        return;
    };
    let len = ctx.array_length(data).min(pixels.len());
    // gc-common w23-a (per-call cost): one bulk copy instead of `w * h`
    // virtual `set_array_element` calls (two million for a 1080p image, on
    // every `getRaster()`). The per-element loop stays as the fallback for a
    // context without the bulk path or a `data` that is not an `int[]`.
    let words: Vec<i32> = pixels[..len].iter().map(|&p| p as i32).collect();
    if ctx.write_int_array_from(data, 0, &words) {
        return;
    }
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(data, i, Value::Int(word));
    }
}

/// The pixel at `(x, y)` as `BufferedImage.getRGB` answers it: the stored
/// ARGB word, with alpha forced to 0xFF for an OPAQUE image type (the JDK
/// reads through the image's ColorModel, and an opaque one answers 0xFF
/// whatever the backing store holds).
///
/// gc-common w25-d: shared by both `getRGB` natives. Only the single-pixel
/// one forced the alpha, so `setRGB(x, y, 0x00123456)` on a `TYPE_INT_RGB`
/// image read back `0xFF123456` from `getRGB(x, y)` and `0x00123456` from
/// the bulk `getRGB` (HotSpot answers `0xFF123456` from both).
fn java_rgb(img: &image::BufferedImageData, x: u32, y: u32) -> u32 {
    let px = img.get_rgb(x, y);
    if img.image_type().has_alpha() {
        px
    } else {
        px | 0xFF00_0000
    }
}

fn register_image_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/image/BufferedImage", "<init>", "(III)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            // Validate the raw signed dimensions before any `as u32` cast so a
            // negative or absurdly large request cannot reach the allocator.
            let w_raw = get_int(args, 1);
            let h_raw = get_int(args, 2);
            if w_raw <= 0 || h_raw <= 0 || w_raw > MAX_IMAGE_DIM || h_raw > MAX_IMAGE_DIM {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "BufferedImage dimensions {w_raw}x{h_raw} out of range (1..={MAX_IMAGE_DIM})"
                    ),
                }
                .into());
            }
            let w = w_raw as u32;
            let h = h_raw as u32;
            let it = match get_int(args, 3) { 1 => ImageType::IntRgb, 3 => ImageType::IntArgbPre, _ => ImageType::IntArgb };
            // `create` returns `None` when `w * h` overflows the `u32`
            // pixel count (image larger than ~65535x65535). Surface that as
            // a Java `OutOfMemoryError` instead of panicking in the multiply.
            let img_id = match image::image_registry().create(w, h, it) {
                Some(id) => id,
                None => {
                    return Err(RuntimeError::OutOfMemoryError {
                        message: format!(
                            "BufferedImage pixel buffer too large: {w}x{h}"
                        ),
                    }
                    .into());
                }
            };
            ctx.set_field_by_name(this, "imageId", Value::Long(img_id.0 as i64));
            ctx.set_field_by_name(this, "width", Value::Int(w as i32));
            ctx.set_field_by_name(this, "height", Value::Int(h as i32));
            bind_buffered_image(ctx, this, img_id);
            // G80-1 N1(A): give the object the REAL raster and colour model the
            // real constructor would have built, so getRaster()/getSampleModel()/
            // getColorModel() stop answering null. Best-effort: an unsupported
            // image type simply leaves the fields as they were.
            attach_real_raster(ctx, this, w_raw, h_raw, get_int(args, 3));
        }
        void_ok()
    });
    // Registered ONLY to synchronise before the raster leaves for Java — the
    // return value is the field the real constructor's counterpart would have
    // returned. Without this the raster is real but its pixels are whatever the
    // rasterizer had not yet written.
    registry.register(
        "java/awt/image/BufferedImage",
        "getRaster",
        "()Ljava/awt/image/WritableRaster;",
        |ctx, args| {
            if let Some(mut this) = get_obj(args, 0) {
                sync_raster_pixels(ctx, &mut this);
                return Ok(Some(ctx.get_field_by_name(this, "raster")));
            }
            null_ok()
        },
    );
    registry.register(
        "java/awt/image/BufferedImage",
        "getWidth",
        "()I",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Value::Int(w) = ctx.get_field_by_name(this, "width") {
                    return int_ok(w);
                }
                if let Some(id) = buffered_image_id(ctx, this) {
                    let reg = image::image_registry();
                    if let Some(img) = reg.get(id) {
                        return int_ok(img.width() as i32);
                    }
                }
            }
            int_ok(0)
        },
    );
    registry.register(
        "java/awt/image/BufferedImage",
        "getHeight",
        "()I",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Value::Int(h) = ctx.get_field_by_name(this, "height") {
                    return int_ok(h);
                }
                if let Some(id) = buffered_image_id(ctx, this) {
                    let reg = image::image_registry();
                    if let Some(img) = reg.get(id) {
                        return int_ok(img.height() as i32);
                    }
                }
            }
            int_ok(0)
        },
    );

    registry.register(
        "java/awt/image/BufferedImage",
        "getRGB",
        "(II)I",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                // Java `int` coordinates: validate the *signed* values before any
                // `as u32` cast, so a negative coordinate is rejected rather than
                // wrapping to a huge index. Mirrors `BufferedImage.getRGB`'s
                // documented `ArrayIndexOutOfBoundsException` contract.
                let (x, y) = (get_int(args, 1), get_int(args, 2));
                if let Some(id) = buffered_image_id(ctx, this) {
                    let reg = image::image_registry();
                    if let Some(img) = reg.get(id) {
                        let (w, h) = (img.width() as i32, img.height() as i32);
                        if x < 0 || y < 0 || x >= w || y >= h {
                            // MEASURED (Sweep17): the comment that stood here
                            // claimed the JDK reports the linear pixel index. It
                            // does not. `BufferedImage.getRGB` bottoms out in the
                            // raster's own bounds check, which throws
                            // `ArrayIndexOutOfBoundsException("Coordinate out of
                            // bounds!")` — no index at all. HotSpot 25.0.3 was
                            // asked; the index wording was invented.
                            let index = (y as i64) * (w as i64) + (x as i64);
                            return Err(RuntimeError::aioobe_with_message(
                                index as i32,
                                "Coordinate out of bounds!",
                            )
                            .into());
                        }
                        // An OPAQUE image type has no alpha channel to report, so
                        // `getRGB` must set it: the JDK returns the ColorModel's
                        // RGB, and an opaque ColorModel answers 0xFF for alpha
                        // whatever the backing store holds. Ours returned the raw
                        // 24-bit value, so every pixel the RASTERIZER had touched
                        // came back with alpha 0 — while a pristine image read
                        // correctly, because `try_new` fills opaque images with
                        // 0xFF000000. That split is why this looked like a
                        // drawing bug rather than a read bug.
                        return int_ok(java_rgb(img, x as u32, y as u32) as i32);
                    }
                }
            }
            int_ok(0)
        },
    );
    registry.register(
        "java/awt/image/BufferedImage",
        "setRGB",
        "(III)V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                // Validate signed coordinates before casting (see `getRGB`).
                let (x, y, argb) = (get_int(args, 1), get_int(args, 2), get_int(args, 3) as u32);
                if let Some(id) = buffered_image_id(ctx, this) {
                    let mut reg = image::image_registry();
                    if let Some(img) = reg.get_mut(id) {
                        let (w, h) = (img.width() as i32, img.height() as i32);
                        if x < 0 || y < 0 || x >= w || y >= h {
                            // Same correction as `getRGB` above.
                            let index = (y as i64) * (w as i64) + (x as i64);
                            return Err(RuntimeError::aioobe_with_message(
                                index as i32,
                                "Coordinate out of bounds!",
                            )
                            .into());
                        }
                        img.set_rgb(x as u32, y as u32, argb);
                    }
                }
            }
            void_ok()
        },
    );
    registry.register(
        "java/awt/image/BufferedImage",
        "getType",
        "()I",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(id) = buffered_image_id(ctx, this) {
                    let reg = image::image_registry();
                    if let Some(img) = reg.get(id) {
                        return int_ok(img.image_type() as i32);
                    }
                }
            }
            int_ok(0)
        },
    );
    registry.register(
        "java/awt/image/BufferedImage",
        "createGraphics",
        "()Ljava/awt/Graphics2D;",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(image_id) = buffered_image_id(ctx, this) {
                    let gfx = ctx.new_object("java/awt/Graphics2D")?;
                    if let Some(Value::Object(Some(gfx_obj))) = &gfx {
                        register_gfx_for_image(ctx, *gfx_obj, image_id);
                    }
                    return Ok(gfx);
                }
            }
            null_ok()
        },
    );
    // BufferedImage.flush() — release reconstructable/derived native resources.
    // Per the JDK contract (and see `flush_image`), the memory-backed pixel
    // raster is NOT reconstructable and must survive flush, so we reclaim only
    // dead (disposed) Graphics2D scratch buffers tied to this image id. The
    // raster itself is never freed here, so re-fetching pixels or re-creating
    // graphics after flush still works.
    registry.register(
        "java/awt/image/BufferedImage",
        "flush",
        "()V",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(id) = buffered_image_id(ctx, this) {
                    for weak in flush_image(id) {
                        ctx.remove_global_root(weak);
                    }
                }
            }
            void_ok()
        },
    );

    // Bulk getRGB: copy an w*h block of ARGB pixels into an int[].
    // Signature: getRGB(int startX, int startY, int w, int h,
    //                   int[] rgbArray, int offset, int scansize)
    registry.register(
        "java/awt/image/BufferedImage",
        "getRGB",
        "(IIII[III)[I",
        |ctx, args| {
            let this = get_obj(args, 0).ok_or_else(|| RuntimeError::NullPointerException {
                message: Some("BufferedImage.getRGB on null".into()),
            })?;
            let (start_x, start_y) = (get_int(args, 1), get_int(args, 2));
            let (w, h) = (get_int(args, 3), get_int(args, 4));
            let offset = get_int(args, 6);
            let scansize = get_int(args, 7);
            if w < 0 || h < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("getRGB: negative dimension {w}x{h}"),
                }
                .into());
            }
            let Some(id) = buffered_image_id(ctx, this) else {
                return null_ok();
            };
            // gc-common w19-g: the process-wide image registry lock is NOT
            // held across the result array's allocation (a GC point). It used
            // to be, so a collection there waited for a peer thread parked on
            // this same lock in another image native, and that peer could
            // never reach its safepoint.
            let (iw, ih) = {
                let reg = image::image_registry();
                let Some(img) = reg.get(id) else {
                    return null_ok();
                };
                (img.width() as i32, img.height() as i32)
            };
            if start_x < 0
                || start_y < 0
                || start_x.checked_add(w).map_or(true, |e| e > iw)
                || start_y.checked_add(h).map_or(true, |e| e > ih)
            {
                return Err(RuntimeError::aioobe_index_only(start_x).into());
            }
            // Allocate the result array if the caller passed null.
            let needed = if h == 0 {
                0i64
            } else {
                offset as i64 + (h as i64 - 1) * scansize as i64 + w as i64
            };
            let arr: ObjectRef = match get_obj(args, 5) {
                Some(a) => a,
                None => {
                    let len = needed.max(0).min(i32::MAX as i64) as usize;
                    ctx.new_array(cratonvm_types::ArrayElementType::Int, len)
                }
            };
            let arr_len = ctx.array_length(arr) as i64;
            let reg = image::image_registry();
            let Some(img) = reg.get(id) else {
                return null_ok();
            };
            // gc-common w23-a (per-call cost): a row that lies wholly inside
            // the array is copied with ONE bulk `write_int_array_from` instead
            // of `w` virtual `set_array_element` calls. A row that crosses
            // either end takes the per-element path below, which writes the
            // in-range prefix and throws at the first bad index -- exactly
            // what the loop always did (and what the JDK's own loop does).
            //
            // gc-common w25-d: pixels are read through `java_rgb`, so an
            // OPAQUE image type answers alpha 0xFF here as in `getRGB(II)I`.
            let mut row_buf: Vec<i32> = Vec::with_capacity(w as usize);
            for row in 0..h {
                let row_start = offset as i64 + row as i64 * scansize as i64;
                if w > 0 && row_start >= 0 && row_start + w as i64 <= arr_len {
                    row_buf.clear();
                    row_buf.extend((0..w).map(|col| {
                        java_rgb(img, (start_x + col) as u32, (start_y + row) as u32) as i32
                    }));
                    if ctx.write_int_array_from(arr, row_start as usize, &row_buf) {
                        continue;
                    }
                }
                for col in 0..w {
                    let argb = java_rgb(img, (start_x + col) as u32, (start_y + row) as u32) as i32;
                    let idx = offset as i64 + row as i64 * scansize as i64 + col as i64;
                    if idx < 0 || idx >= arr_len {
                        return Err(RuntimeError::aioobe_index_only(
                            idx.clamp(i32::MIN as i64, i32::MAX as i64) as i32,
                        )
                        .into());
                    }
                    ctx.set_array_element(arr, idx as usize, Value::Int(argb));
                }
            }
            obj_ok(arr)
        },
    );

    registry.register(
        "javax/imageio/ImageIO",
        "read",
        "(Ljava/io/InputStream;)Ljava/awt/image/BufferedImage;",
        |ctx, args| {
            let input = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("input == null!"))?;
            let bytes = read_all_from_input_stream(ctx, input)?;
            decode_buffered_image(ctx, &bytes)
        },
    );
    registry.register(
        "javax/imageio/ImageIO",
        "read",
        "(Ljava/io/File;)Ljava/awt/image/BufferedImage;",
        |ctx, args| {
            let file = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("input == null!"))?;
            let created = ctx.new_object_initialized(
                "java/io/FileInputStream",
                "(Ljava/io/File;)V",
                &[Value::Object(Some(file))],
            )?;
            let Some(Value::Object(Some(stream))) = created else {
                return null_ok();
            };
            let base = ctx.pin_native_root(stream);
            let result = (|| {
                let stream = ctx.read_native_pin(base, stream);
                let bytes = read_all_from_input_stream(ctx, stream)?;
                let stream = ctx.read_native_pin(base, stream);
                ctx.invoke_virtual(stream, "close", "()V", &[])?;
                decode_buffered_image(ctx, &bytes)
            })();
            ctx.unpin_native_roots(base);
            result
        },
    );
    registry.register(
        "javax/imageio/ImageIO",
        "write",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/OutputStream;)Z",
        |ctx, args| {
            let image_obj = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("im == null!"))?;
            let format_name = read_string(ctx, args, 1)
                .ok_or_else(|| imageio_illegal_arg("formatName == null!"))?;
            let output = get_obj(args, 2).ok_or_else(|| imageio_illegal_arg("output == null!"))?;
            let Some(format) = image::EncodedImageFormat::parse(&format_name) else {
                return bool_ok(false);
            };
            let bytes = encode_rendered_image(ctx, image_obj, format)?;
            write_all_to_output_stream(ctx, output, &bytes)?;
            bool_ok(true)
        },
    );
    registry.register(
        "javax/imageio/ImageIO",
        "write",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljavax/imageio/stream/ImageOutputStream;)Z",
        |ctx, args| {
            let image_obj = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("im == null!"))?;
            let format_name = read_string(ctx, args, 1)
                .ok_or_else(|| imageio_illegal_arg("formatName == null!"))?;
            let output = get_obj(args, 2).ok_or_else(|| imageio_illegal_arg("output == null!"))?;
            let Some(format) = image::EncodedImageFormat::parse(&format_name) else {
                return bool_ok(false);
            };
            let bytes = encode_rendered_image(ctx, image_obj, format)?;
            write_all_to_output_stream(ctx, output, &bytes)?;
            bool_ok(true)
        },
    );
    registry.register(
        "javax/imageio/ImageIO",
        "write",
        "(Ljava/awt/image/RenderedImage;Ljava/lang/String;Ljava/io/File;)Z",
        |ctx, args| {
            let image_obj = get_obj(args, 0).ok_or_else(|| imageio_illegal_arg("im == null!"))?;
            let format_name = read_string(ctx, args, 1)
                .ok_or_else(|| imageio_illegal_arg("formatName == null!"))?;
            let file = get_obj(args, 2).ok_or_else(|| imageio_illegal_arg("output == null!"))?;
            let Some(format) = image::EncodedImageFormat::parse(&format_name) else {
                return bool_ok(false);
            };
            let bytes = encode_rendered_image(ctx, image_obj, format)?;
            let created = ctx.new_object_initialized(
                "java/io/FileOutputStream",
                "(Ljava/io/File;)V",
                &[Value::Object(Some(file))],
            )?;
            let Some(Value::Object(Some(stream))) = created else {
                return bool_ok(false);
            };
            let base = ctx.pin_native_root(stream);
            let result = (|| {
                let stream = ctx.read_native_pin(base, stream);
                write_all_to_output_stream(ctx, stream, &bytes)?;
                let stream = ctx.read_native_pin(base, stream);
                ctx.invoke_virtual(stream, "close", "()V", &[])?;
                bool_ok(true)
            })();
            ctx.unpin_native_roots(base);
            result
        },
    );

    // `initIDs` natives cache JNI field/method IDs for the real JDK image
    // classes. CratonVM resolves fields by name, so no IDs need caching —
    // register these as no-ops so the real-JDK `<clinit>` of each class can
    // complete (it would otherwise throw UnsatisfiedLinkError). The JPEG plugin
    // uses method-specific bootstrap names rather than `initIDs`.
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "initReaderIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V",
        |_ctx, _args| void_ok(),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageWriter",
        "initWriterIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        |_ctx, _args| void_ok(),
        NativeKind::Bridge,
    );
    // KEEP (deliberate constants) — audited 2026-07-27. Every native below
    // manipulates the libjpeg `jpeg_decompress_struct` that the real JDK
    // allocates in `libjavajpeg`. CratonVM decodes JPEG in Rust instead (see
    // `imageio_reader_read` a few lines down, which reads the stream itself
    // and never consults the handle), so there is no C struct to create,
    // point at a source, reset, or free.
    //
    // `initJPEGImageReader` must still return a NON-ZERO opaque handle: the
    // JDK bytecode stores it in `structPointer` and treats 0 as "native
    // allocation failed", throwing from the constructor. `1` is that
    // never-dereferenced token, not a placeholder value.
    //
    // RE-VERIFIED wave 4 (2026-07-28) — DO NOT convert these to throws:
    //   * `JPEGImageReader.read` (registered ~40 lines below) is bound to
    //     `imageio_reader_read`, which resolves the reader's `input` stream,
    //     drains it and decodes with the Rust `image` crate. It takes no
    //     handle argument and reads no handle field.
    //   * `structPointer` occurs exactly ONCE in the whole repository: in
    //     this comment. Nothing — Rust side or Java side — ever dereferences
    //     the token, and there is no per-handle table to key it into.
    // Throwing from any of the six below would break a JPEG decoder that
    // works today, purely to remove a constant that is load-bearing.
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "initJPEGImageReader",
        "()J",
        |_ctx, _args| Ok(Some(Value::Long(1))),
        NativeKind::Bridge,
    );
    // The remaining five are lifecycle calls against that non-existent
    // struct — setSource/resetReader/resetLibraryState/disposeReader/dispose
    // have nothing to own, point at, or release, so an empty body is the
    // correct implementation rather than a missing one.
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "setSource",
        "(J)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "resetReader",
        "(J)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "resetLibraryState",
        "(J)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register_with_kind(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "disposeReader",
        "(J)V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );
    registry.register(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "dispose",
        "()V",
        |_ctx, _args| Ok(None),
    );
    registry.register(
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "read",
        "(ILjavax/imageio/ImageReadParam;)Ljava/awt/image/BufferedImage;",
        |ctx, args| imageio_reader_read(ctx, args, image::EncodedImageFormat::Jpeg),
    );
    registry.register(
        "com/sun/imageio/plugins/png/PNGImageWriter",
        "write",
        "(Ljavax/imageio/metadata/IIOMetadata;Ljavax/imageio/IIOImage;Ljavax/imageio/ImageWriteParam;)V",
        |ctx, args| imageio_writer_write(ctx, args, image::EncodedImageFormat::Png),
    );
    // Twelve of these thirteen classes declare `initIDs()V` ACC_NATIVE in
    // JDK 25 — they are the JNI field-ID caches the Java2D native library
    // fills in, and an empty body is the spec-correct implementation for a VM
    // that resolves fields by name. Those twelve state `Bridge` at this call
    // site (L5b, 2026-08-05).
    for class in [
        "java/awt/image/BufferedImage",
        "java/awt/image/ColorModel",
        "java/awt/image/IndexColorModel",
        "java/awt/image/Raster",
        "java/awt/image/SampleModel",
        "java/awt/image/SinglePixelPackedSampleModel",
        "java/awt/image/Kernel",
        "sun/awt/image/IntegerComponentRaster",
        "sun/awt/image/ByteComponentRaster",
        "sun/awt/image/ShortComponentRaster",
        "sun/awt/image/BytePackedRaster",
        "sun/awt/image/GifImageDecoder",
    ] {
        registry.register_with_kind(
            class,
            "initIDs",
            "()V",
            |_ctx, _args| void_ok(),
            NativeKind::Bridge,
        );
    }
    // The thirteenth is split out only because the census reports it
    // differently, NOT because it is a different kind of thing — a correction
    // to what L5b wrote here on the strength of `declared: false`.
    // `java.awt.image.ComponentSampleModel` does not declare `initIDs` itself;
    // `InheritedDeclProbe` resolves it up the hierarchy and finds `initIDs()V`
    // **native** on the superclass `java.awt.image.SampleModel`, which is what
    // dispatch binds to. `declared: false` on one class is not the same claim
    // as "no ACC_NATIVE target", and reading it that way is how 1,939 rows
    // tree-wide were mis-filed. So this states its kind too.
    registry.register_with_kind(
        "java/awt/image/ComponentSampleModel",
        "initIDs",
        "()V",
        |_ctx, _args| void_ok(),
        NativeKind::Bridge,
    );
}

// ---------------------------------------------------------------------------
// EventQueue natives
// ---------------------------------------------------------------------------

/// Java AWT event class plus the field assignments required to materialise it
/// from an [`crate::event::AwtEvent`].
///
/// Separating the *plan* (class + field writes) from the actual `new_object`
/// / `set_field_by_name` calls lets the synthesis logic be unit-tested
/// without standing up a full `NativeContext` mock. The `getNextEvent`
/// native walks the plan, allocates the class, and writes every field in
/// order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EventSynthesisPlan {
    /// JVM-internal class name (slash-delimited) to allocate.
    pub class_name: &'static str,
    /// Field writes to perform on the freshly-allocated event object.
    /// Order matches the JDK constructor's assignment order so a future
    /// switch to a real constructor invocation is a drop-in.
    pub fields: Vec<(&'static str, Value)>,
}

/// Build the synthesis plan for a given event.
///
/// Returns `None` for event kinds we don't yet synthesize (component /
/// focus / action — see the TODO in `register_event_natives`).
pub(crate) fn plan_event_synthesis(
    evt: &crate::event::AwtEvent,
    source: Option<ObjectRef>,
) -> Option<EventSynthesisPlan> {
    use crate::event::{event_id, AwtEventData};
    let source_value = Value::Object(source);
    let when = Value::Long(evt.timestamp as i64);

    match &evt.data {
        AwtEventData::Mouse {
            x,
            y,
            button,
            click_count,
            modifiers,
            scroll_amount,
        } => {
            let class_name = if evt.id == event_id::MOUSE_WHEEL {
                "java/awt/event/MouseWheelEvent"
            } else {
                "java/awt/event/MouseEvent"
            };
            let mut fields = vec![
                ("source", source_value),
                ("id", Value::Int(evt.id)),
                ("when", when),
                ("modifiers", Value::Int(*modifiers)),
                ("modifiersEx", Value::Int(*modifiers)),
                ("x", Value::Int(*x)),
                ("y", Value::Int(*y)),
                ("xAbs", Value::Int(*x)),
                ("yAbs", Value::Int(*y)),
                ("clickCount", Value::Int(*click_count)),
                ("button", Value::Int(*button)),
                ("popupTrigger", Value::Int(0)),
                ("consumed", Value::Int(0)),
            ];
            if evt.id == event_id::MOUSE_WHEEL {
                fields.push(("scrollType", Value::Int(0))); // WHEEL_UNIT_SCROLL
                fields.push(("scrollAmount", Value::Int(scroll_amount.abs())));
                fields.push(("wheelRotation", Value::Int(*scroll_amount)));
                fields.push(("preciseWheelRotation", Value::Double(*scroll_amount as f64)));
            }
            Some(EventSynthesisPlan { class_name, fields })
        }
        AwtEventData::Key {
            key_code,
            key_char,
            modifiers,
        } => Some(EventSynthesisPlan {
            class_name: "java/awt/event/KeyEvent",
            fields: vec![
                ("source", source_value),
                ("id", Value::Int(evt.id)),
                ("when", when),
                ("modifiers", Value::Int(*modifiers)),
                ("modifiersEx", Value::Int(*modifiers)),
                ("keyCode", Value::Int(*key_code)),
                ("keyChar", Value::Int(*key_char as i32)),
                ("keyLocation", Value::Int(1)), // KEY_LOCATION_STANDARD
                ("consumed", Value::Int(0)),
            ],
        }),
        AwtEventData::Window => Some(EventSynthesisPlan {
            class_name: "java/awt/event/WindowEvent",
            fields: vec![
                ("source", source_value),
                ("id", Value::Int(evt.id)),
                ("oldState", Value::Int(0)),
                ("newState", Value::Int(0)),
                ("consumed", Value::Int(0)),
            ],
        }),
        AwtEventData::Paint { .. } => Some(EventSynthesisPlan {
            class_name: "java/awt/event/PaintEvent",
            // The `updateRect` field is intentionally left at its default
            // (`null`) — Swing's repaint manager treats `null` as
            // "repaint everything", which matches the conservative
            // coalescing we already do upstream in `coalesce_paint_events`.
            // A future improvement is to allocate a `java/awt/Rectangle`
            // here so listeners that consult `getUpdateRect()` see a
            // populated bounding box; tracked in the TODO below.
            fields: vec![
                ("source", source_value),
                ("id", Value::Int(evt.id)),
                ("consumed", Value::Int(0)),
            ],
        }),
        // Component / Focus / Action: not yet synthesized — `getNextEvent`
        // returns null for these and the next dispatch cycle drops them.
        // See TODO in `register_event_natives`.
        AwtEventData::Component { .. }
        | AwtEventData::Focus { .. }
        | AwtEventData::Action { .. } => None,
        // Invocation events are handled separately by the natives layer:
        // the receiver class is `InvocationEvent` and we have to bind the
        // event hash to the runnable's callback id outside the plan.
        AwtEventData::Invocation { .. } => None,
    }
}

/// Materialise `evt` as its Java event object, or `null` for a kind with no
/// plan ([`plan_event_synthesis`]).
///
/// gc-common w23-a: the `source` component is resolved from its global root
/// AFTER the event object's allocation. The callers used to resolve it first
/// and bake it into the plan, so a collection triggered by `new_object` moved
/// the component and the event's `source` field received its pre-move
/// address (`getNextEvent` / `peekEvent`, every mouse / key / window / paint
/// event whose peer has a registered source).
fn materialise_event(ctx: &mut dyn NativeContext, evt: &AwtEvent) -> MethodCallResult {
    let Some(plan) = plan_event_synthesis(evt, None) else {
        return null_ok();
    };
    let obj_val = ctx.new_object(plan.class_name)?;
    if let Some(Value::Object(Some(obj))) = &obj_val {
        // No GC point between here and the stores below.
        let source = lookup_peer_source(ctx, evt.source_peer_id);
        for (name, value) in plan_fields_with_source(&plan, source) {
            ctx.set_field_by_name(*obj, name, value);
        }
    }
    Ok(obj_val)
}

/// The field stores of `plan` with its `source` store replaced by `source`
/// (the component resolved after the event object's allocation).
fn plan_fields_with_source(
    plan: &EventSynthesisPlan,
    source: Option<ObjectRef>,
) -> impl Iterator<Item = (&'static str, Value)> + '_ {
    plan.fields.iter().map(move |&(name, value)| {
        if name == "source" {
            (name, Value::Object(source))
        } else {
            (name, value)
        }
    })
}

fn event_int_field(ctx: &dyn NativeContext, obj: ObjectRef, field: &str, default: i32) -> i32 {
    match ctx.get_field_by_name(obj, field) {
        Value::Int(v) => v,
        _ => default,
    }
}

fn event_long_field(ctx: &dyn NativeContext, obj: ObjectRef, field: &str, default: i64) -> i64 {
    match ctx.get_field_by_name(obj, field) {
        Value::Long(v) => v,
        Value::Int(v) => v as i64,
        _ => default,
    }
}

fn event_object_field(ctx: &dyn NativeContext, obj: ObjectRef, field: &str) -> Option<ObjectRef> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(v)) => Some(v),
        _ => None,
    }
}

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn peer_for_posted_event_source(ctx: &dyn NativeContext, event_obj: ObjectRef) -> PeerId {
    let Some(source) = event_object_field(ctx, event_obj, "source") else {
        return PeerId(0);
    };
    peer_of(ctx, source).unwrap_or(PeerId(0))
}

fn posted_awt_event_from_java(ctx: &dyn NativeContext, event_obj: ObjectRef) -> Option<AwtEvent> {
    let id = event_int_field(ctx, event_obj, "id", 0);
    let peer = peer_for_posted_event_source(ctx, event_obj);
    let timestamp =
        event_long_field(ctx, event_obj, "when", current_time_millis() as i64).max(0) as u64;

    match id {
        event_id::MOUSE_CLICKED
        | event_id::MOUSE_PRESSED
        | event_id::MOUSE_RELEASED
        | event_id::MOUSE_MOVED
        | event_id::MOUSE_ENTERED
        | event_id::MOUSE_EXITED
        | event_id::MOUSE_DRAGGED => Some(AwtEvent::mouse(
            id,
            peer,
            timestamp,
            event_int_field(ctx, event_obj, "x", 0),
            event_int_field(ctx, event_obj, "y", 0),
            event_int_field(ctx, event_obj, "button", 0),
            event_int_field(ctx, event_obj, "clickCount", 0),
            event_int_field(
                ctx,
                event_obj,
                "modifiersEx",
                event_int_field(ctx, event_obj, "modifiers", 0),
            ),
        )),
        event_id::MOUSE_WHEEL => Some(AwtEvent::mouse_wheel(
            peer,
            timestamp,
            event_int_field(ctx, event_obj, "x", 0),
            event_int_field(ctx, event_obj, "y", 0),
            event_int_field(
                ctx,
                event_obj,
                "modifiersEx",
                event_int_field(ctx, event_obj, "modifiers", 0),
            ),
            event_int_field(
                ctx,
                event_obj,
                "wheelRotation",
                event_int_field(ctx, event_obj, "scrollAmount", 0),
            ),
        )),
        event_id::KEY_TYPED | event_id::KEY_PRESSED | event_id::KEY_RELEASED => {
            let key_char = char::from_u32(event_int_field(ctx, event_obj, "keyChar", 0) as u32)
                .unwrap_or('\0');
            Some(AwtEvent::key(
                id,
                peer,
                timestamp,
                event_int_field(ctx, event_obj, "keyCode", 0),
                key_char,
                event_int_field(
                    ctx,
                    event_obj,
                    "modifiersEx",
                    event_int_field(ctx, event_obj, "modifiers", 0),
                ),
            ))
        }
        event_id::WINDOW_OPENED
        | event_id::WINDOW_CLOSING
        | event_id::WINDOW_CLOSED
        | event_id::WINDOW_ACTIVATED
        | event_id::WINDOW_DEACTIVATED
        | event_id::WINDOW_GAINED_FOCUS
        | event_id::WINDOW_LOST_FOCUS => Some(AwtEvent::window(id, peer, timestamp)),
        event_id::PAINT | event_id::UPDATE => {
            let rect = event_object_field(ctx, event_obj, "updateRect");
            let field_obj = rect.unwrap_or(event_obj);
            Some(AwtEvent::paint(
                id,
                peer,
                timestamp,
                event_int_field(ctx, field_obj, "x", 0),
                event_int_field(ctx, field_obj, "y", 0),
                event_int_field(ctx, field_obj, "width", 0).max(0),
                event_int_field(ctx, field_obj, "height", 0).max(0),
            ))
        }
        _ => None,
    }
}

// JDK-ONLY-CLASSIFY: stub — all 7 target `java.awt.EventQueue` /
// `java.awt.event.*` methods with concrete bytecode in JDK 25. The event pump
// here synthesizes events for a VM with no display; that is compatibility
// behaviour standing in for a windowing system, not a bridge to one. A real
// bridge would appear once `platform/` is wired and would live on the
// `sun.awt.*` peer natives, which are ACC_NATIVE.
fn register_event_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/awt/EventQueue",
        "isDispatchThread",
        "()Z",
        |_ctx, _args| bool_ok(edt::is_edt()),
    );
    registry.register(
        "java/awt/EventQueue",
        "invokeLater",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(runnable) = get_obj(args, 0) {
                // Allocate a fresh callback id, register the Runnable, post
                // an InvocationEvent.  The EDT will dequeue it via
                // `getNextEvent` and dispatch it through
                // `InvocationEvent.dispatch()V` (registered below).
                //
                // `invokeLater` is asynchronous, so the Runnable is held by a
                // (strong, remapped) global root until the dispatch resolves
                // and releases it.
                let root = add_global_root_or_oom(ctx, runnable, "AWT Runnable")?;
                let (_, removed_roots) =
                    edt::edt_for_vm(root.vm).invoke_later_runnable(root, PeerId(0));
                release_vm_roots(ctx, removed_roots);
            }
            void_ok()
        },
    );
    registry.register(
        "java/awt/EventQueue",
        "invokeAndWait",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(runnable) = get_obj(args, 0) {
                // Block until the Runnable's `dispatch()V` native finishes
                // calling `run()`. Round-9 misc fix: when called from the
                // EDT itself this previously panicked across the JNI
                // boundary (UB on most VMs). Convert the structured error
                // into the JDK-spec'd `IllegalStateException` so the Java
                // caller observes the documented behaviour instead.
                //
                let root = add_global_root_or_oom(ctx, runnable, "AWT Runnable")?;
                invoke_and_wait_rooted(ctx, root)?;
            }
            void_ok()
        },
    );
    registry.register(
        "java/awt/EventQueue",
        "postEvent",
        "(Ljava/awt/AWTEvent;)V",
        |ctx, args| {
            let Some(event_obj) = get_obj(args, 1) else {
                return void_ok();
            };
            if let Some(evt) = posted_awt_event_from_java(ctx, event_obj) {
                edt::edt_for_vm(ctx.vm_identity()).post_event(evt);
            } else {
                let id = event_int_field(ctx, event_obj, "id", 0);
                tracing::debug!(
                    id,
                    "EventQueue.postEvent ignored unsupported AWTEvent family"
                );
            }
            void_ok()
        },
    );
    registry.register(
        "java/awt/EventQueue",
        "getNextEvent",
        "()Ljava/awt/AWTEvent;",
        |ctx, _args| {
            // JDK contract: block the calling thread (the EDT) until an event
            // arrives, the queue is closed, or the thread is interrupted.
            //
            // Prior to this revision the implementation polled the queue
            // once, returned null for every non-invocation event, and let the
            // EDT dispatch loop drop mouse / key / window / paint events on
            // the floor — silencing every listener registered through
            // `Component.addMouseListener` / `addKeyListener` /
            // `addWindowListener`. The fix:
            //   1. Block via `wait_event_blocking` so the dispatch loop
            //      doesn't busy-spin on an empty queue.
            //   2. Synthesize the proper Java event subclass
            //      (`MouseEvent` / `KeyEvent` / `WindowEvent` /
            //      `PaintEvent`) via [`plan_event_synthesis`].
            //   3. Preserve the `source` `Component`, the timestamp (`when`),
            //      and any modifier mask the platform backend captured.
            //   4. Keep the existing `InvocationEvent` binding (the
            //      `dispatch()V` native consults the side-table to find the
            //      registered Runnable).
            //
            // If the EDT shuts down or the calling thread is interrupted
            // before an event arrives we return `null` — the JDK dispatch
            // loop treats that the same as "shut down" and exits cleanly.
            // gc-common w22-a: the wait below can last forever (an idle EDT),
            // so it runs inside a blocking region. It used to block with the
            // thread still counted as running Java: every stop-the-world
            // collection of the VM waited for this thread to reach a
            // safepoint it never reaches while the queue stays empty -- the
            // whole VM hung at its next GC once an AWT event loop idled. No
            // raw reference is held across the region.
            //
            // gc-common w23-a: this VM's own queue (it used to be one queue
            // for the process). Its teardown closes it, which ends the wait.
            let queue = edt::edt_for_vm(ctx.vm_identity());
            ctx.begin_blocking_region();
            let next = queue.wait_event_blocking(
                // Block indefinitely — the JDK contract is "wait until an
                // event arrives or the thread is interrupted". The EDT
                // dispatch loop never wants `getNextEvent` to time out.
                u64::MAX,
                // Re-check shutdown / interrupt every 50ms while sleeping
                // so a Toolkit shutdown doesn't stay blocked forever.
                50,
                // We don't have a robust way to read the Java-side
                // interrupt flag from this native — `NativeContext::is_interrupted`
                // requires a `&mut self` reference we don't hold here. The
                // shutdown flag (toggled by `EventDispatchThread::stop`)
                // already covers the only legitimate "wake up empty" path
                // for the production code.
                || false,
            );
            ctx.end_blocking_region();
            drop(queue);
            let Some(evt) = next else {
                return null_ok();
            };
            use crate::event::AwtEventData;
            if let AwtEventData::Invocation { callback_id } = evt.data {
                let inv = ctx.new_object("java/awt/event/InvocationEvent")?;
                if let Some(Value::Object(Some(obj))) = &inv {
                    // gc-common w29-c: the binding names THIS event by a weak
                    // root, so a second pending event with the same identity
                    // hash gets its own row. `obj` is current: nothing since
                    // the allocation can collect.
                    let obj = *obj;
                    let hash = ctx.identity_hash_code(obj);
                    let weak = ctx.add_weak_global_root(obj);
                    let released = bind_invocation_event(
                        ctx.vm_identity(),
                        hash,
                        obj,
                        weak,
                        callback_id,
                        &|handle: usize| ctx.resolve_global_root(handle),
                    );
                    release_vm_roots(ctx, released);
                }
                return Ok(inv);
            }
            // All non-invocation event kinds: materialise the matching Java
            // class with `source` / `id` / `when` / modifiers / payload set
            // (the source is resolved from its global root after the
            // allocation, see `materialise_event`).
            // TODO: Component / Focus / Action events still answer null here.
            // They are far less common than mouse/key/window/paint, but a
            // future pass should extend `plan_event_synthesis` to cover them.
            materialise_event(ctx, &evt)
        },
    );
    registry.register(
        "java/awt/EventQueue",
        "peekEvent",
        "()Ljava/awt/AWTEvent;",
        |ctx, _args| {
            // Observation-only: clone the head of the queue without releasing
            // any `invokeAndWait` waiter (see `EventDispatchThread::peek_event`).
            let Some(evt) = edt::edt_for_vm(ctx.vm_identity()).peek_event() else {
                return null_ok();
            };
            use crate::event::AwtEventData;
            if matches!(evt.data, AwtEventData::Invocation { .. }) {
                // Peeking at an InvocationEvent without consuming it would
                // re-bind the callback id on every call. Return null — the
                // JDK behaviour for `peekEvent` is "may return null", and
                // Swing's dispatch loop only consults `getNextEvent` anyway.
                return null_ok();
            }
            materialise_event(ctx, &evt)
        },
    );

    // -- InvocationEvent.dispatch -------------------------------------------
    //
    // The EDT's dispatch loop calls `AWTEvent.dispatch()` on whatever
    // `EventQueue.getNextEvent` returned.  For InvocationEvents this
    // native looks the Runnable up by the event object's identity hash,
    // calls `Runnable.run()` virtually on the EDT, then signals any
    // `invokeAndWait` waiter and drops both registry entries.
    registry.register(
        "java/awt/event/InvocationEvent",
        "dispatch",
        "()V",
        |ctx, args| {
            let Some(this) = get_obj(args, 0) else {
                return void_ok();
            };
            let hash = ctx.identity_hash_code(this);
            // gc-common w29-c: only THIS event's row (its weak root resolves
            // to `this`), never another pending event's with the same hash.
            let Some(binding) = take_invocation_event_callback(
                ctx.vm_identity(),
                hash,
                this,
                &|handle: usize| ctx.resolve_global_root(handle),
            ) else {
                // No binding — either this event was not synthesized by us
                // (a real-JDK class created it directly), or it was already
                // dispatched.  Nothing to do.
                return void_ok();
            };
            if binding.weak != 0 {
                // The bucket is this VM's, so the weak root is too.
                ctx.remove_global_root(binding.weak);
            }
            let callback_id = binding.callback_id;
            // The Runnable is held by its global root (remapped by every
            // collection) until here: resolve it through the root, never
            // through a cached pointer. A row already taken (dispatched
            // twice) skips `run()` and only signals completion, so a blocked
            // `invokeAndWait` waiter doesn't hang.
            drain_retired_awt_roots(ctx);
            let queue = edt::edt_for_vm(ctx.vm_identity());
            let root = queue.take_runnable_root(callback_id);
            if let Some(root) = root.filter(|root| root.vm != ctx.vm_identity()) {
                // gc-common w22-a: never resolve another VM's handle here (it
                // indexes the other VM's table). Since w23-a every VM has its
                // own queue, so only the shared identity-0 queue (mock
                // contexts) can still hold another VM's row; kept as a guard.
                // Park the root for its VM and release that VM's waiter.
                tracing::warn!(
                    callback_id,
                    owner_vm = root.vm,
                    "InvocationEvent.dispatch: Runnable belongs to another VM; skipped"
                );
                release_vm_roots(ctx, [root]);
                queue.signal_invocation_complete(callback_id);
                return void_ok();
            }
            if let Some(root) = root {
                // Run on whatever thread invoked us — by contract this is
                // the EDT, since the EDT dispatch loop is what calls
                // `dispatch()`.  We propagate failures out of the native so
                // the EDT's exception handling sees them, but signal
                // completion in BOTH the success and failure paths
                // (otherwise an exception in `run()` would hang
                // `invokeAndWait` forever).
                let result = match ctx.resolve_global_root(root.handle) {
                    Some(runnable) => ctx.invoke_virtual(runnable, "run", "()V", &[]),
                    None => {
                        tracing::warn!(
                            callback_id,
                            root_handle = root.handle,
                            "InvocationEvent.dispatch: Runnable global root could not be resolved"
                        );
                        void_ok()
                    }
                };
                release_vm_roots(ctx, [root]);
                queue.signal_invocation_complete(callback_id);
                // Surface any exception thrown by Runnable.run() to the EDT.
                result?;
            } else {
                // The Runnable was already taken (e.g. dispatched twice, or
                // its row was displaced). Still signal so a waiting
                // `invokeAndWait` caller doesn't hang.
                tracing::warn!(
                    callback_id,
                    "InvocationEvent.dispatch: Runnable skipped (already taken or displaced)"
                );
                queue.signal_invocation_complete(callback_id);
            }
            void_ok()
        },
    );
}

// ---------------------------------------------------------------------------
// Font natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — all 12 target `java.awt.Font` /
// `java.awt.FontMetrics` methods with concrete bytecode in JDK 25. Font metrics
// are computed here from the crate's own tables rather than from a platform
// font engine, so these are approximations of class-library behaviour. The
// platform boundary in the real JDK sits below this, in `sun.font.*`
// ACC_NATIVE methods that this crate does not register.
fn register_font_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/awt/Font",
        "getFamily",
        "()Ljava/lang/String;",
        |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let name = ctx
                    .read_string(this)
                    .unwrap_or_else(|| "Dialog".to_string());
                let s = ctx.create_string(&name);
                return obj_ok(s);
            }
            null_ok()
        },
    );
    // Font.getFontName()/getFontName(Locale) — return the font's face name.
    //
    // The stock JDK resolves these through `getFont2D()` -> `sun.font.Font2D`,
    // which on Windows pulls in `sun.awt.Win32FontManager` and its native EUDC
    // font-file lookup. CratonVM has no Win32 font manager, so a headless app
    // that calls `Font.getFontName()` (common during AWT init) trips an
    // `UnsatisfiedLinkError`. Resolve the name from the Font's own `name`
    // field instead — for the logical fonts (Dialog, SansSerif, …) used in
    // headless mode this is the correct face name, mirroring `getFamily`.
    fn font_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Object(Some(n)) = ctx.get_field_by_name(this, "name") {
                if let Some(name) = ctx.read_string(n) {
                    return obj_ok(ctx.create_string(&name));
                }
            }
            let fallback = ctx
                .read_string(this)
                .unwrap_or_else(|| "Dialog".to_string());
            return obj_ok(ctx.create_string(&fallback));
        }
        null_ok()
    }
    registry.register(
        "java/awt/Font",
        "getFontName",
        "()Ljava/lang/String;",
        font_name,
    );
    registry.register(
        "java/awt/Font",
        "getFontName",
        "(Ljava/util/Locale;)Ljava/lang/String;",
        font_name,
    );
    registry.register("java/awt/Font", "getSize", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(s) = ctx.get_field_by_name(this, "size") {
                return int_ok(s);
            }
        }
        int_ok(12)
    });
    registry.register("java/awt/Font", "getStyle", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(s) = ctx.get_field_by_name(this, "style") {
                return int_ok(s);
            }
        }
        int_ok(0)
    });
    // FontMetrics receivers carry their (Font.size) via the `font` field on
    // FontMetrics. `args[0]` is the FontMetrics receiver (an `ObjectRef`),
    // **not** an int — `get_int(args, 0)` would always return 0 and the
    // `.max(12)` floor would mask the bug. Resolve the actual font size by
    // reading `this.font.size`.
    fn font_metrics_size(ctx: &dyn NativeContext, args: &[Value]) -> f32 {
        let this = match get_obj(args, 0) {
            Some(o) => o,
            None => return 12.0,
        };
        // FontMetrics has a `Font font` field; Font has an `int size` field.
        let font = match ctx.get_field_by_name(this, "font") {
            Value::Object(Some(o)) => o,
            // Some callers may stash the size directly on the FontMetrics.
            _ => match ctx.get_field_by_name(this, "size") {
                Value::Int(s) => return (s as f32).max(1.0),
                _ => return 12.0,
            },
        };
        match ctx.get_field_by_name(font, "size") {
            Value::Int(s) if s > 0 => s as f32,
            _ => 12.0,
        }
    }

    // Bug awt-font-image #1: resolve the FontMetrics' full (family, style, size)
    // so the advance natives below can drive FontMetrics.stringWidth/charWidth/
    // getMaxAdvance through the SAME shared advance model in `crate::font` that
    // FontEngine and Graphics2D::draw_string use. Previously these natives used
    // standalone flat heuristics (size*0.55 etc.) decoupled from the family and
    // from what the renderer actually draws.
    fn font_metrics_spec(ctx: &dyn NativeContext, args: &[Value]) -> (String, i32, i32) {
        let size = font_metrics_size(ctx, args).round().max(1.0) as i32;
        let this = match get_obj(args, 0) {
            Some(o) => o,
            None => return ("Dialog".to_string(), 0, size),
        };
        let font = match ctx.get_field_by_name(this, "font") {
            Value::Object(Some(o)) => o,
            _ => return ("Dialog".to_string(), 0, size),
        };
        let family = match ctx.get_field_by_name(font, "name") {
            Value::Object(Some(n)) => ctx.read_string(n).unwrap_or_else(|| "Dialog".to_string()),
            _ => "Dialog".to_string(),
        };
        let style = match ctx.get_field_by_name(font, "style") {
            Value::Int(s) => s,
            _ => 0,
        };
        (family, style, size)
    }

    registry.register("java/awt/FontMetrics", "getAscent", "()I", |ctx, args| {
        int_ok((font_metrics_size(ctx, args) * 0.8).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getDescent", "()I", |ctx, args| {
        int_ok((font_metrics_size(ctx, args) * 0.2).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getLeading", "()I", |ctx, args| {
        int_ok((font_metrics_size(ctx, args) * 0.05).round().max(1.0) as i32)
    });
    registry.register("java/awt/FontMetrics", "getHeight", "()I", |ctx, args| {
        let s = font_metrics_size(ctx, args);
        int_ok(((s * 0.8).round() + (s * 0.2).round() + (s * 0.05).round().max(1.0)) as i32)
    });
    registry.register(
        "java/awt/FontMetrics",
        "stringWidth",
        "(Ljava/lang/String;)I",
        |ctx, args| {
            let text = read_string(ctx, args, 1).unwrap_or_default();
            let (family, style, size) = font_metrics_spec(ctx, args);
            // Bug awt-font-image #1: sum per-glyph advances from the shared model
            // (same source of truth as Graphics2D::draw_string), not
            // char_count * size * 0.55. This is also char-count-correct for CJK /
            // multibyte text since it iterates `chars()`.
            int_ok(crate::font::text_advance(&family, style, size, &text).round() as i32)
        },
    );
    registry.register("java/awt/FontMetrics", "charWidth", "(C)I", |ctx, args| {
        // arg[1] is the `char` to measure (passed as an int code unit).
        let ch = char::from_u32(get_int(args, 1) as u32).unwrap_or(' ');
        let (family, style, size) = font_metrics_spec(ctx, args);
        int_ok(crate::font::glyph_advance(&family, style, size, ch).round() as i32)
    });
    registry.register(
        "java/awt/FontMetrics",
        "getMaxAdvance",
        "()I",
        |ctx, args| {
            // Bug awt-font-image #1: widest per-glyph advance from the shared
            // model, not the bare point size.
            let (family, style, size) = font_metrics_spec(ctx, args);
            int_ok(crate::font::max_glyph_advance(&family, style, size).round() as i32)
        },
    );
}

// ---------------------------------------------------------------------------
// Swing natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: stub — javax.swing is pure Java; not one method in this
// group is ACC_NATIVE in JDK 25. 15 of the 16 registrations shadow concrete
// bytecode, and the `JOptionPane.showInputDialog` entry does not match any
// descriptor on the real class. Swing on a real JDK image should run its own
// bytecode down to the AWT peer layer.
fn register_swing_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "javax/swing/UIManager",
        "getSystemLookAndFeelClassName",
        "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("javax.swing.plaf.metal.MetalLookAndFeel")),
    );
    registry.register(
        "javax/swing/UIManager",
        "getCrossPlatformLookAndFeelClassName",
        "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("javax.swing.plaf.metal.MetalLookAndFeel")),
    );
    // KEEP (deliberate no-op): both look-and-feel getters above answer
    // `MetalLookAndFeel` unconditionally, so there is exactly one L&F to be
    // in. Recording the requested class name would only let a caller read
    // back a name that nothing renders with.
    registry.register(
        "javax/swing/UIManager",
        "setLookAndFeel",
        "(Ljava/lang/String;)V",
        |_ctx, _args| void_ok(),
    );

    registry.register(
        "javax/swing/UIManager",
        "getColor",
        "(Ljava/lang/Object;)Ljava/awt/Color;",
        |ctx, args| {
            let key = read_string(ctx, args, 0).unwrap_or_default();
            let state = swing::swing_state().lock();
            if let Some(argb) = state.defaults.get_color(&key) {
                drop(state); // release lock before calling ctx methods
                let color_obj = ctx.new_object("java/awt/Color")?;
                if let Some(Value::Object(Some(obj))) = &color_obj {
                    ctx.set_field_by_name(*obj, "value", Value::Int(argb as i32));
                }
                return Ok(color_obj);
            }
            null_ok()
        },
    );
    registry.register(
        "javax/swing/UIManager",
        "getFont",
        "(Ljava/lang/Object;)Ljava/awt/Font;",
        |ctx, args| {
            let key = read_string(ctx, args, 0).unwrap_or_default();
            let state = swing::swing_state().lock();
            if let Some((family, style, size)) = state.defaults.get_font(&key) {
                let family = family.clone();
                drop(state);
                let font_obj = ctx.new_object("java/awt/Font")?;
                if let Some(Value::Object(Some(obj))) = font_obj {
                    // gc-common w19-g: the font is rooted across its name's
                    // allocation and re-read for the stores and the return
                    // (it was held raw across `create_string`).
                    let mut scope = NativeHandleScope::new(ctx);
                    let font_h = scope.root(obj);
                    let name = scope.create_string(&family);
                    let font = scope.get(&font_h);
                    scope.set_field_by_name(font, "name", Value::Object(Some(name)));
                    scope.set_field_by_name(font, "style", Value::Int(style));
                    scope.set_field_by_name(font, "size", Value::Int(size));
                    return Ok(Some(Value::Object(Some(font))));
                }
                return Ok(font_obj);
            }
            null_ok()
        },
    );
    registry.register(
        "javax/swing/UIManager",
        "getInsets",
        "(Ljava/lang/Object;)Ljava/awt/Insets;",
        |ctx, args| {
            let key = read_string(ctx, args, 0).unwrap_or_default();
            let state = swing::swing_state().lock();
            if let Some((top, left, bottom, right)) = state.defaults.get_insets(&key) {
                let (top, left, bottom, right) = (top, left, bottom, right);
                drop(state);
                let insets = ctx.new_object("java/awt/Insets")?;
                if let Some(Value::Object(Some(obj))) = &insets {
                    ctx.set_field_by_name(*obj, "top", Value::Int(top));
                    ctx.set_field_by_name(*obj, "left", Value::Int(left));
                    ctx.set_field_by_name(*obj, "bottom", Value::Int(bottom));
                    ctx.set_field_by_name(*obj, "right", Value::Int(right));
                }
                return Ok(insets);
            }
            null_ok()
        },
    );
    registry.register(
        "javax/swing/UIManager",
        "getInt",
        "(Ljava/lang/Object;)I",
        |ctx, args| {
            let key = read_string(ctx, args, 0).unwrap_or_default();
            let state = swing::swing_state().lock();
            let v = state.defaults.get_integer(&key).unwrap_or(0);
            int_ok(v)
        },
    );
    registry.register(
        "javax/swing/UIManager",
        "getBoolean",
        "(Ljava/lang/Object;)Z",
        |ctx, args| {
            let key = read_string(ctx, args, 0).unwrap_or_default();
            let state = swing::swing_state().lock();
            let v = state.defaults.get_boolean(&key).unwrap_or(false);
            bool_ok(v)
        },
    );

    // Headless behaviour: there is no display for the user to pick a file
    // in, so the open/save choosers return `JFileChooser.CANCEL_OPTION`
    // (the int `1`) without blocking — i.e. "the user dismissed the
    // dialog". No file is selected. This is a fixed no-interaction result,
    // not a real prompt.
    registry.register(
        "javax/swing/JFileChooser",
        "showOpenDialog",
        "(Ljava/awt/Component;)I",
        |_ctx, _args| int_ok(1),
    );
    registry.register(
        "javax/swing/JFileChooser",
        "showSaveDialog",
        "(Ljava/awt/Component;)I",
        |_ctx, _args| int_ok(1),
    );

    registry.register(
        "javax/swing/JOptionPane",
        "showMessageDialog",
        "(Ljava/awt/Component;Ljava/lang/Object;Ljava/lang/String;I)V",
        |ctx, args| {
            let msg = read_string(ctx, args, 1).unwrap_or_default();
            let title = read_string(ctx, args, 2).unwrap_or_default();
            tracing::info!("[JOptionPane] {title}: {msg}");
            void_ok()
        },
    );
    // Headless behaviour: there is no display for the user to respond in,
    // so the confirm dialog returns `JOptionPane.YES_OPTION` / `OK_OPTION`
    // (the int `0`) without blocking. This is a fixed no-interaction
    // result, not a real prompt.
    registry.register(
        "javax/swing/JOptionPane",
        "showConfirmDialog",
        "(Ljava/awt/Component;Ljava/lang/Object;Ljava/lang/String;I)I",
        |_ctx, _args| int_ok(0),
    );
    // Headless behaviour: with no display to type into, the input dialog
    // returns an empty string without blocking. This is a fixed
    // no-interaction result, not a real prompt.

    registry.register(
        "javax/swing/SwingUtilities",
        "isEventDispatchThread",
        "()Z",
        |_ctx, _args| bool_ok(edt::is_edt()),
    );
    registry.register(
        "javax/swing/SwingUtilities",
        "invokeLater",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(runnable) = get_obj(args, 0) {
                // Same plumbing as `EventQueue.invokeLater` — register the
                // Runnable so `InvocationEvent.dispatch()V` can find it,
                // then post. Stamp the current GC count so dispatch fails
                // closed if a moving collection runs first (see
                // `take_runnable_checked` / `lookup_peer_source`).
                let root = add_global_root_or_oom(ctx, runnable, "Swing Runnable")?;
                let (_, removed_roots) =
                    edt::edt_for_vm(root.vm).invoke_later_runnable(root, PeerId(0));
                release_vm_roots(ctx, removed_roots);
            }
            void_ok()
        },
    );
    registry.register(
        "javax/swing/SwingUtilities",
        "invokeAndWait",
        "(Ljava/lang/Runnable;)V",
        |ctx, args| {
            if let Some(runnable) = get_obj(args, 0) {
                // Round-9 misc fix: surface the EDT-from-EDT case as an
                // `IllegalStateException` on the Java thread instead of
                // panicking across the JNI boundary. The wording matches
                // the JDK exactly so existing exception filters keep
                // working.
                //
                let root = add_global_root_or_oom(ctx, runnable, "Swing Runnable")?;
                invoke_and_wait_rooted(ctx, root)?;
            }
            void_ok()
        },
    );
}

/// The shared body of `EventQueue.invokeAndWait` / `SwingUtilities.invokeAndWait`
/// once the Runnable is rooted (`root`, this VM's): post it, block until the
/// EDT has run it, release what is left.
///
/// gc-common w22-a: the wait runs inside a blocking region. It used to block
/// on the completion condvar with the thread still counted as running Java,
/// so a stop-the-world collection requested meanwhile -- by the EDT itself,
/// allocating the `InvocationEvent` or inside `run()` -- waited for this
/// thread forever while this thread waited for the EDT: a deadlock. Nothing
/// raw is held across the region (the Runnable is the global root).
fn invoke_and_wait_rooted(
    ctx: &mut dyn NativeContext,
    root: VmRoot,
) -> Result<(), MethodCallFailed> {
    // gc-common w23-a: the calling VM's own queue (taken before the region).
    let queue = edt::edt_for_vm(root.vm);
    ctx.begin_blocking_region();
    let outcome = queue.invoke_and_wait_runnable(root, PeerId(0));
    ctx.end_blocking_region();
    match outcome {
        Ok((callback_id, removed_roots)) => {
            release_vm_roots(ctx, removed_roots);
            if let Some(left) = queue.take_runnable_root(callback_id) {
                release_vm_roots(ctx, [left]);
            }
            Ok(())
        }
        Err(InvokeAndWaitError::OnEdt) => {
            release_vm_roots(ctx, [root]);
            Err(RuntimeError::IllegalStateException {
                message: InvokeAndWaitError::OnEdt.jdk_message().to_string(),
            }
            .into())
        }
    }
}

// ---------------------------------------------------------------------------
// Clipboard natives
// ---------------------------------------------------------------------------

// JDK-ONLY-CLASSIFY: unknown — needs census. All 3 target methods with concrete
// bytecode in JDK 25, which argues stub; but a system clipboard IS an OS
// resource, and the real JDK reaches it through `sun.awt.datatransfer` peers
// that this VM does not implement. Whether these should become bridges on the
// peer classes or be deleted so the real bytecode fails cleanly cannot be
// decided from source. Evidence needed: `invocations` from a run that actually
// touches `Toolkit.getSystemClipboard`.
fn register_clipboard_natives(registry: &mut NativeMethodRegistry) {
    use crate::clipboard::{get_clipboard, ClipboardKind};

    registry.register(
        "java/awt/datatransfer/Clipboard",
        "getName",
        "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("System")),
    );

    // `getContents` is wired to the in-process clipboard backend
    // (`clipboard.rs`). When the system clipboard holds text, it is
    // returned as a real `java.awt.datatransfer.StringSelection`, which
    // implements `Transferable` — so callers get genuine clipboard text
    // rather than an unconditional `null`. Non-text flavors (image / file
    // list / raw) are not yet representable as a `Transferable` here and
    // still yield `null`.
    registry.register(
        "java/awt/datatransfer/Clipboard",
        "getContents",
        "(Ljava/lang/Object;)Ljava/awt/datatransfer/Transferable;",
        |ctx, _args| {
            let text = {
                let mgr = get_clipboard().lock();
                mgr.get_text(ClipboardKind::System).map(|s| s.to_string())
            };
            let Some(text) = text else {
                return null_ok();
            };
            // Build a `StringSelection(String)` — it implements `Transferable`.
            //
            // gc-common w19-g: the string first, then one
            // `new_object_initialized` (which pins its argument across the
            // allocation and the selection across `<init>`). The old order
            // held the selection raw across the string's allocation and the
            // constructor, and returned its pre-constructor address.
            let s = ctx.create_string(&text);
            ctx.new_object_initialized(
                "java/awt/datatransfer/StringSelection",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(s))],
            )
        },
    );

    // `setContents` stores the transferable's text into the `clipboard.rs`
    // backend so a subsequent `getContents` reflects it. Text is extracted
    // by reading the `StringSelection.data` field (the standard text
    // `Transferable`); transferables that carry no readable string field
    // are accepted as a no-op.
    registry.register(
        "java/awt/datatransfer/Clipboard",
        "setContents",
        "(Ljava/awt/datatransfer/Transferable;Ljava/awt/datatransfer/ClipboardOwner;)V",
        |ctx, args| {
            if let Some(transferable) = get_obj(args, 1) {
                // `StringSelection` keeps the payload in a `data` field.
                if let Value::Object(Some(data)) = ctx.get_field_by_name(transferable, "data") {
                    if let Some(text) = ctx.read_string(data) {
                        get_clipboard()
                            .lock()
                            .set_text(ClipboardKind::System, text, None);
                    }
                }
            }
            void_ok()
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{event_id, modifiers, AwtEvent, BUTTON1};

    #[test]
    fn registration_count() {
        let mut registry = NativeMethodRegistry::new();
        register_all(&mut registry);
        let count = registry.len();
        assert!(count >= 50, "Expected >= 50 AWT natives, got {count}");
    }

    /// V1: `read_int_array` must cap its up-front reserve so a bogus reported
    /// array length can't force a giant eager allocation. The reserve is
    /// `len.min(MAX_INT_ARRAY_PREALLOC)`; this guards the cap against being
    /// set to a degenerate value and documents the bound.
    #[test]
    fn int_array_prealloc_cap_is_sane() {
        assert!(MAX_INT_ARRAY_PREALLOC > 0);
        // A hostile length must collapse to the cap, not the raw length.
        let hostile = usize::MAX;
        assert_eq!(hostile.min(MAX_INT_ARRAY_PREALLOC), MAX_INT_ARRAY_PREALLOC);
        // A legitimate small length must be honored exactly.
        assert_eq!(8usize.min(MAX_INT_ARRAY_PREALLOC), 8);
    }

    /// Build a fake `ObjectRef` for tests — guaranteed 8-aligned & non-null.
    fn fake_object_ref(seed: u64) -> ObjectRef {
        let ptr = ((seed + 1) << 3) as *mut u8;
        // SAFETY: the arithmetic above constructs a unique non-null,
        // eight-byte-aligned sentinel used only as an opaque test identity.
        unsafe { ObjectRef::from_raw(ptr) }
    }

    fn find_field<'a>(plan: &'a EventSynthesisPlan, name: &str) -> Option<&'a Value> {
        plan.fields.iter().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// The VM the pre-w22 tests below file their rows under (the mock
    /// default identity); the w22-a tests use private identities.
    const TEST_VM: usize = 0;

    /// A pre-w24 (strong) peer-source row.
    fn strong_source(root_handle: usize, java_hash: i32) -> PeerSourceEntry {
        PeerSourceEntry {
            root_handle,
            java_hash,
            weak: false,
            shown_root: 0,
        }
    }

    /// A gc-common w24-b (weak) peer-source row.
    fn weak_source(root_handle: usize, java_hash: i32) -> PeerSourceEntry {
        PeerSourceEntry {
            weak: true,
            ..strong_source(root_handle, java_hash)
        }
    }

    fn register_peer_source_root_for_test(peer_id: PeerId, root_handle: usize, java_hash: i32) {
        peer_source_table().lock().insert(
            TEST_VM,
            peer_id.0,
            strong_source(root_handle, java_hash),
        );
    }

    fn lookup_peer_source_for_test(
        peer_id: PeerId,
        root_handle: usize,
        source: ObjectRef,
    ) -> Option<ObjectRef> {
        lookup_peer_source_with(TEST_VM, peer_id, |handle| {
            (handle == root_handle).then_some(source)
        })
    }

    /// gc-common w22-a (`common-w21a-more-process-wide-global-root-handle-caches`
    /// item 2): the peer-source table is per VM. Two VMs register the same
    /// peer id (the peer registry mints ids per identity hash, process-wide)
    /// with the same handle number, each in its own global-ref table: each
    /// resolves only its own row, a replacement hands back the replaced row's
    /// OWN VM (gc-common w24-b removed the FIFO cap), and a VM's teardown drops
    /// its rows and nobody else's.
    #[test]
    fn w22a_peer_sources_are_per_vm_and_forgotten_with_the_vm() {
        const VM_A: usize = 0x0A22_1A08;
        const VM_B: usize = 0x0A22_1B08;
        const PEER: u64 = 0x0A22_1000;
        let src_a = fake_object_ref(0x0A22_1A);
        let src_b = fake_object_ref(0x0A22_1B);
        {
            let mut t = peer_source_table().lock();
            for vm in [VM_A, VM_B] {
                let displaced = t.insert(vm, PEER, strong_source(1, 7));
                assert!(displaced.is_empty());
            }
        }
        // Each VM resolves handle 1 in ITS OWN table.
        let in_a = |h: usize| (h == 1).then_some(src_a);
        let in_b = |h: usize| (h == 1).then_some(src_b);
        assert_eq!(lookup_peer_source_with(VM_A, PeerId(PEER), in_a), Some(src_a));
        assert_eq!(lookup_peer_source_with(VM_B, PeerId(PEER), in_b), Some(src_b));

        // A replacement in B hands back B's old root, tagged with B.
        let displaced = peer_source_table()
            .lock()
            .insert(VM_B, PEER, strong_source(2, 7));
        assert_eq!(displaced, vec![VmRoot { vm: VM_B, handle: 1 }]);

        // Teardown of A drops A's row only.
        forget_vm_awt_roots(VM_A);
        assert_eq!(lookup_peer_source_with(VM_A, PeerId(PEER), in_a), None);
        assert!(peer_source_table().lock().get(VM_B, PEER).is_some());
        forget_vm_awt_roots(VM_B);
        assert!(peer_source_table().lock().get(VM_B, PEER).is_none());
    }

    /// gc-common w22-a: a root displaced by VM B's call but created by VM A is
    /// never released through B: it is parked for A, handed to A's drain, and
    /// dropped (unreleased) by A's teardown.
    #[test]
    fn w22a_foreign_roots_are_parked_for_their_own_vm() {
        const VM_A: usize = 0x0A22_2A08;
        const VM_B: usize = 0x0A22_2B08;
        const VM_C: usize = 0x0A22_2C08;
        let roots = vec![
            VmRoot { vm: VM_A, handle: 5 },
            VmRoot { vm: VM_B, handle: 6 },
            VmRoot { vm: VM_B, handle: 0 },
            VmRoot { vm: VM_C, handle: 7 },
        ];
        let (own, foreign) = partition_vm_roots(VM_B, roots);
        assert_eq!(own, vec![6], "B releases only its own non-zero handle");
        assert_eq!(
            foreign,
            vec![VmRoot { vm: VM_A, handle: 5 }, VmRoot { vm: VM_C, handle: 7 }]
        );
        park_retired_awt_roots(foreign);
        assert!(take_retired_awt_roots(VM_B).is_empty());
        assert_eq!(take_retired_awt_roots(VM_A), vec![5]);
        assert!(take_retired_awt_roots(VM_A).is_empty(), "drained once");
        // C never ran again: its teardown drops the parked row unreleased.
        forget_vm_awt_roots(VM_C);
        assert!(take_retired_awt_roots(VM_C).is_empty());
    }

    /// gc-common w22-a: `InvocationEvent` bindings are per VM -- VM B's
    /// dispatch of an event whose identity hash collides with a pending VM A
    /// event must not take A's callback id.
    #[test]
    fn w22a_invocation_bindings_are_per_vm() {
        const VM_A: usize = 0x0A22_3A08;
        const VM_B: usize = 0x0A22_3B08;
        const HASH: i32 = 0x0A22_3000;
        // No weak roots (a context without a collector): one row per bucket.
        let no_roots = |_: usize| -> Option<ObjectRef> { None };
        let event = w29c_obj(0x0A22_3000);
        let callback = |row: Option<InvocationRow>| row.map(|r| r.callback_id);
        assert!(bind_invocation_event(VM_A, HASH, event, 0, 41, &no_roots).is_empty());
        assert_eq!(
            callback(take_invocation_event_callback(VM_B, HASH, event, &no_roots)),
            None
        );
        assert!(bind_invocation_event(VM_B, HASH, event, 0, 42, &no_roots).is_empty());
        forget_vm_awt_roots(VM_B);
        assert_eq!(
            callback(take_invocation_event_callback(VM_B, HASH, event, &no_roots)),
            None
        );
        assert_eq!(
            callback(take_invocation_event_callback(VM_A, HASH, event, &no_roots)),
            Some(41)
        );
    }

    /// A reference for the table tests: never dereferenced (8-aligned).
    fn w29c_obj(addr: usize) -> ObjectRef {
        // SAFETY: a key only; the tables compare it, never dereference it.
        unsafe { ObjectRef::from_raw(addr as *mut u8) }
    }

    /// gc-common w29-c (`common-w28b-remaining-identity-hash-keyed-side-tables`
    /// rank 30): two undispatched `InvocationEvent`s of ONE VM with the SAME
    /// identity hash keep their own callback ids. The second binding used to
    /// replace the first, so the first event's `dispatch()` ran the second's
    /// Runnable. Weak roots are modelled by a resolver table (`native-awt`
    /// has no mock context): handle -> the event's current reference, `None`
    /// once the event was collected.
    #[test]
    fn w29c_same_hash_pending_events_keep_their_own_callbacks() {
        use std::collections::HashMap;
        const VM: usize = 0x0C29_AE08;
        const HASH: i32 = 0x0C29_A000;
        let (first, second) = (w29c_obj(0x1_0C29_A000), w29c_obj(0x2_0C29_A000));
        let first_moved = w29c_obj(0x3_0C29_A000);
        let roots: std::cell::RefCell<HashMap<usize, Option<ObjectRef>>> =
            std::cell::RefCell::new(HashMap::from([(11, Some(first)), (12, Some(second))]));
        let resolve = |h: usize| roots.borrow().get(&h).copied().flatten();
        struct Forget;
        impl Drop for Forget {
            fn drop(&mut self) {
                invocation_event_callbacks().lock().forget_vm(VM);
            }
        }
        let _forget = Forget;

        assert!(bind_invocation_event(VM, HASH, first, 11, 101, &resolve).is_empty());
        assert!(
            bind_invocation_event(VM, HASH, second, 12, 102, &resolve).is_empty(),
            "the first, live, event's row is kept"
        );
        // The first event moves (its weak root follows it); a dispatch of the
        // second takes only the second's row.
        roots.borrow_mut().insert(11, Some(first_moved));
        let row = take_invocation_event_callback(VM, HASH, second, &resolve);
        assert_eq!(
            row,
            Some(InvocationRow {
                weak: 12,
                callback_id: 102
            })
        );
        assert_eq!(
            take_invocation_event_callback(VM, HASH, first, &resolve),
            None,
            "the stale pre-move reference finds nothing"
        );
        assert_eq!(
            take_invocation_event_callback(VM, HASH, first_moved, &resolve).map(|r| r.callback_id),
            Some(101)
        );
        assert_eq!(
            take_invocation_event_callback(VM, HASH, first_moved, &resolve),
            None,
            "dispatched once"
        );

        // A collected event's row is released by the next binding in its
        // bucket (its weak root handed back for release through its VM).
        let third = w29c_obj(0x4_0C29_A000);
        roots.borrow_mut().insert(13, Some(third));
        assert!(bind_invocation_event(VM, HASH, third, 13, 103, &resolve).is_empty());
        roots.borrow_mut().insert(13, None);
        let fourth = w29c_obj(0x5_0C29_A000);
        roots.borrow_mut().insert(14, Some(fourth));
        let released = bind_invocation_event(VM, HASH, fourth, 14, 104, &resolve);
        assert_eq!(released, vec![VmRoot { vm: VM, handle: 13 }]);
        assert_eq!(
            take_invocation_event_callback(VM, HASH, fourth, &resolve).map(|r| r.callback_id),
            Some(104)
        );
    }

    // ── gc-common w23-a: AWT state per VM ────────────────────────────────

    /// One VM's events never reach another VM's queue, and a VM's teardown
    /// drops (and closes) its queue: a dispatch loop still blocked in it
    /// returns instead of waiting forever.
    #[test]
    fn w23a_event_queues_are_per_vm_and_closed_at_teardown() {
        use std::time::Duration;
        const VM_A: usize = 0x0A23_4A08;
        const VM_B: usize = 0x0A23_4B08;
        let a = edt::edt_for_vm(VM_A);
        let b = edt::edt_for_vm(VM_B);
        assert!(!Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &edt::edt_for_vm(VM_A)), "one queue per VM");
        a.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
        let (_, displaced) = a.invoke_later_runnable(VmRoot { vm: VM_A, handle: 9 }, PeerId(0));
        assert!(displaced.is_empty());
        assert!(b.poll_event().is_none(), "B must not see A's events");
        assert_eq!(b.queue_length(), 0);
        assert_eq!(a.queue_length(), 2);
        assert_eq!(a.poll_event().map(|e| e.id), Some(event_id::WINDOW_OPENED));

        // A dispatch loop blocked on A's (now empty after the next poll)
        // queue is released by A's teardown; B's queue is untouched.
        let _ = a.poll_event();
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = {
            let a = Arc::clone(&a);
            std::thread::spawn(move || {
                let got = a.wait_event_blocking(u64::MAX, 5, || false);
                let _ = tx.send(got.is_none());
            })
        };
        std::thread::sleep(Duration::from_millis(20));
        forget_vm_awt_roots(VM_A);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
        waiter.join().unwrap();
        assert!(a.is_closed());
        assert!(!edt::vm_has_edt(VM_A));
        assert!(edt::vm_has_edt(VM_B));
        assert!(!b.is_closed());
        forget_vm_awt_roots(VM_B);
        assert!(!edt::vm_has_edt(VM_B));
    }

    /// Two VMs' components with the same identity hash get distinct peers;
    /// a VM's teardown destroys its peers and their Swing rows only.
    #[test]
    fn w23a_peers_are_per_vm_and_forgotten_with_the_vm() {
        const VM_A: usize = 0x0A23_5A08;
        const VM_B: usize = 0x0A23_5B08;
        const HASH: i32 = 0x0A23_5000;
        let (pa, pb) = {
            let mut reg = peer::peer_registry().lock();
            let pa = reg.create_peer(ComponentType::Frame);
            reg.register_java_mapping(VM_A, HASH, pa);
            let pb = reg.create_peer(ComponentType::Frame);
            reg.register_java_mapping(VM_B, HASH, pb);
            (pa, pb)
        };
        assert_ne!(pa, pb);
        {
            let mut s = swing::swing_state().lock();
            s.mark_dirty(pa, 0, 0, 640, 480);
            s.mark_dirty(pb, 0, 0, 640, 480);
        }
        forget_vm_awt_roots(VM_A);
        {
            let reg = peer::peer_registry().lock();
            assert!(reg.get(pa).is_none());
            assert_eq!(reg.peer_for_java(VM_A, HASH), None);
            assert_eq!(reg.peer_for_java(VM_B, HASH), Some(pb));
        }
        {
            let s = swing::swing_state().lock();
            assert!(!s.dirty_regions.contains_key(&pa));
            assert!(s.dirty_regions.contains_key(&pb));
        }
        forget_vm_awt_roots(VM_B);
        assert!(peer::peer_registry().lock().get(pb).is_none());
        assert!(!swing::swing_state().lock().dirty_regions.contains_key(&pb));
    }

    /// Graphics contexts are per VM: the same identity hash in two VMs names
    /// two contexts (B's draws never land in A's raster), and a VM's teardown
    /// drops its contexts only.
    #[test]
    fn w23a_graphics_contexts_are_per_vm() {
        const VM_A: usize = 0x0A23_6A08;
        const VM_B: usize = 0x0A23_6B08;
        const HASH: i32 = 0x0A23_6000;
        let ga = gfx_for_key_without_weak_root((VM_A, HASH));
        let gb = gfx_for_key_without_weak_root((VM_B, HASH));
        assert!(!Arc::ptr_eq(&ga, &gb), "a hash collision must not share a context");
        assert!(Arc::ptr_eq(&ga, &gfx_for_key_without_weak_root((VM_A, HASH))));
        forget_vm_awt_roots(VM_A);
        assert!(!gfx_registry().lock().contains_key((VM_A, HASH)));
        assert!(gfx_registry().lock().contains_key((VM_B, HASH)));
        forget_vm_awt_roots(VM_B);
        assert!(!gfx_registry().lock().contains_key((VM_B, HASH)));
    }

    /// `BufferedImage` rows are per VM (the same hash in two VMs names two
    /// rasters, so B never reads A's pixels), and a VM's teardown frees its
    /// rasters -- including one whose row a same-VM hash collision displaced.
    #[test]
    fn w23a_buffered_images_are_per_vm_and_freed_with_the_vm() {
        const VM_A: usize = 0x0A23_7A08;
        const VM_B: usize = 0x0A23_7B08;
        const HASH: i32 = 0x0A23_7000;
        let (ia, ia2, ib) = {
            let mut reg = image::image_registry();
            (
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
            )
        };
        // No weak roots (weak root 0): the pre-w24 key-only behaviour.
        let img = fake_object_ref(0x0A23_7A);
        {
            let mut t = buffered_image_ids().lock();
            t.bind((VM_A, HASH), img, 0, ia, &no_weak_roots);
            t.bind((VM_B, HASH), img, 0, ib, &no_weak_roots);
            assert_eq!(t.get((VM_A, HASH), img, &no_weak_roots), Some(ia));
            assert_eq!(t.get((VM_B, HASH), img, &no_weak_roots), Some(ib));
            // A same-VM collision: the newer image takes the row.
            let leftovers = t.bind((VM_A, HASH), img, 0, ia2, &no_weak_roots);
            assert_eq!(leftovers, ImageBindLeftovers::default());
            assert_eq!(t.get((VM_A, HASH), img, &no_weak_roots), Some(ia2));
        }
        forget_vm_awt_roots(VM_A);
        {
            let reg = image::image_registry();
            assert!(reg.get(ia).is_none(), "displaced raster freed with its VM");
            assert!(reg.get(ia2).is_none());
            assert!(reg.get(ib).is_some(), "B's raster survives A's teardown");
        }
        let get = |key: ObjKey| buffered_image_ids().lock().get(key, img, &no_weak_roots);
        assert_eq!(get((VM_A, HASH)), None);
        assert_eq!(get((VM_B, HASH)), Some(ib));
        forget_vm_awt_roots(VM_B);
        assert!(image::image_registry().get(ib).is_none());
        assert_eq!(get((VM_B, HASH)), None);
    }

    // ── gc-common w24-b: weak rows ───────────────────────────────────────

    /// A resolver for rows filed without weak roots (weak root 0 is never
    /// resolved; any other handle reads as cleared).
    fn no_weak_roots(_handle: usize) -> Option<ObjectRef> {
        None
    }

    /// `gfx_handle_for_key` with no weak root (a context without a
    /// collector): the pre-w24 key-only row.
    fn gfx_for_key_without_weak_root(key: ObjKey) -> GfxHandle {
        let obj = fake_object_ref(0x0B24_0060);
        let (handle, displaced, unused) = gfx_handle_for_key(key, obj, 0, &no_weak_roots);
        assert!(displaced.is_empty());
        assert_eq!(unused, 0);
        handle
    }

    /// A weak-root table for the tests: `handle -> object` while alive.
    struct WeakRoots(std::cell::RefCell<Vec<(usize, ObjectRef)>>);

    impl WeakRoots {
        fn new(rows: &[(usize, ObjectRef)]) -> Self {
            Self(std::cell::RefCell::new(rows.to_vec()))
        }
        fn resolve(&self, handle: usize) -> Option<ObjectRef> {
            self.0
                .borrow()
                .iter()
                .find(|&&(h, _)| h == handle)
                .map(|&(_, obj)| obj)
        }
        /// The collector found `handle`'s object dead: it reads as cleared.
        fn clear(&self, handle: usize) {
            self.0.borrow_mut().retain(|&(h, _)| h != handle);
        }
    }

    /// A row belongs to the object its weak root names: a second LIVE object
    /// with the same `(vm, hash)` gets a row of its own; removing the primary
    /// promotes it; a row whose object died is replaced (and reported dead);
    /// the sweep takes out exactly the dead rows of its VM.
    #[test]
    fn w24b_keyed_rows_tell_live_collisions_apart() {
        const VM: usize = 0x0B24_1A08;
        const KEY: ObjKey = (VM, 0x0B24_1000);
        let (a, b, c) = (
            fake_object_ref(0x0B24_11),
            fake_object_ref(0x0B24_12),
            fake_object_ref(0x0B24_13),
        );
        let roots = WeakRoots::new(&[(1, a), (2, b), (3, c)]);
        let resolve = |h: usize| roots.resolve(h);
        let mut rows: KeyedRows<u32> = KeyedRows::new();
        assert!(rows.insert(KEY, a, 1, 10, &resolve).is_empty());
        assert_eq!(rows.get(KEY, b, &resolve), None, "a's row is not b's");
        assert!(rows.insert(KEY, b, 2, 20, &resolve).is_empty(), "a collision displaces nothing");
        assert_eq!(rows.get(KEY, a, &resolve), Some(10));
        assert_eq!(rows.get(KEY, b, &resolve), Some(20));

        // a goes (dispose): b is promoted.
        let removed = rows.remove(KEY, a, &resolve).expect("a's row");
        assert_eq!((removed.weak, removed.value), (1, 10));
        assert_eq!(rows.get(KEY, b, &resolve), Some(20));
        assert!(rows.collided.is_empty());

        // b dies; c (same key) replaces its row, which is reported dead.
        roots.clear(2);
        assert_eq!(rows.get(KEY, c, &resolve), None);
        let displaced = rows.insert(KEY, c, 3, 30, &resolve);
        assert_eq!(displaced.len(), 1);
        assert!(displaced[0].dead);
        assert_eq!((displaced[0].row.weak, displaced[0].row.value), (2, 20));
        assert_eq!(rows.get(KEY, c, &resolve), Some(30));

        // The sweep takes out c's row once c dies, and only rows of its VM.
        let other_vm_key: ObjKey = (VM + 8, 0x0B24_1000);
        assert!(rows.insert(other_vm_key, c, 3, 31, &resolve).is_empty());
        assert!(rows.take_dead(VM, &resolve).is_empty(), "c is alive");
        roots.clear(3);
        let dead = rows.take_dead(VM, &resolve);
        assert_eq!(dead.len(), 1);
        assert_eq!((dead[0].weak, dead[0].value), (3, 30));
        assert!(!rows.contains_key(KEY));
        assert!(rows.contains_key(other_vm_key), "another VM's row is not swept");
    }

    /// Without weak roots (0: a context with no collector) a row is judged by
    /// its key alone, exactly as before w24-b: a second object shares it,
    /// and the sweep never takes it.
    #[test]
    fn w24b_rows_without_weak_roots_keep_the_key_only_behaviour() {
        const KEY: ObjKey = (0x0B24_2A08, 0x0B24_2000);
        let (a, b) = (fake_object_ref(0x0B24_21), fake_object_ref(0x0B24_22));
        let mut rows: KeyedRows<u32> = KeyedRows::new();
        assert!(rows.insert(KEY, a, 0, 10, &no_weak_roots).is_empty());
        assert_eq!(rows.get(KEY, b, &no_weak_roots), Some(10), "shared by key");
        let displaced = rows.insert(KEY, b, 0, 20, &no_weak_roots);
        assert_eq!(displaced.len(), 1);
        assert!(!displaced[0].dead, "superseded, not dead");
        assert!(rows.take_dead(KEY.0, &no_weak_roots).is_empty());
        assert_eq!(rows.get(KEY, a, &no_weak_roots), Some(20));
    }

    /// The sweep of one VM: rows whose weak root cleared go -- the dead
    /// image's raster is destroyed, the dead Graphics context dropped, the
    /// dead component's peer destroyed with its Swing rows -- and their weak
    /// roots come back for release; live rows, a shown component's row and
    /// another VM's rows stay.
    #[test]
    fn w24b_sweep_drops_dead_rows_frees_rasters_and_keeps_live_ones() {
        const VM: usize = 0x0B24_3A08;
        const OTHER_VM: usize = 0x0B24_3B08;
        let objs: Vec<ObjectRef> = (0..7).map(|i| fake_object_ref(0x0B24_30 + i)).collect();
        let (dead_img, live_img, dead_gfx, live_gfx) = (objs[0], objs[1], objs[2], objs[3]);
        let (dead_comp, live_comp, shown_comp) = (objs[4], objs[5], objs[6]);
        let roots = WeakRoots::new(&[
            (0xB2401, dead_img),
            (0xB2402, live_img),
            (0xB2403, dead_gfx),
            (0xB2404, live_gfx),
            (0xB2405, dead_comp),
            (0xB2406, live_comp),
            (0xB2407, shown_comp),
            (0xB2408, dead_img),
        ]);
        let resolve = |h: usize| roots.resolve(h);

        let (raster_dead, raster_live, raster_other) = {
            let mut reg = image::image_registry();
            (
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
            )
        };
        {
            let mut t = buffered_image_ids().lock();
            t.bind((VM, 1), dead_img, 0xB2401, raster_dead, &resolve);
            t.bind((VM, 2), live_img, 0xB2402, raster_live, &resolve);
            t.bind((OTHER_VM, 1), dead_img, 0xB2408, raster_other, &resolve);
        }
        let gfx = |target: GfxTarget| -> GfxHandle {
            Arc::new(Mutex::new(GfxEntry {
                state: Graphics2DState::create(4, 4),
                target,
                disposed: false,
            }))
        };
        {
            let mut g = gfx_registry().lock();
            g.insert((VM, 3), dead_gfx, 0xB2403, gfx(GfxTarget::Image(raster_dead)), &resolve);
            g.insert((VM, 4), live_gfx, 0xB2404, gfx(GfxTarget::Detached), &resolve);
        }
        let (p_dead, p_live, p_shown) = {
            let mut reg = peer::peer_registry().lock();
            let ids = (
                reg.create_peer(ComponentType::Frame),
                reg.create_peer(ComponentType::Frame),
                reg.create_peer(ComponentType::Frame),
            );
            reg.register_java_mapping(VM, 5, ids.0);
            reg.register_java_mapping(VM, 6, ids.1);
            reg.register_java_mapping(VM, 7, ids.2);
            ids
        };
        {
            let mut t = peer_source_table().lock();
            t.insert(VM, p_dead.0, weak_source(0xB2405, 5));
            t.insert(VM, p_live.0, weak_source(0xB2406, 6));
            t.insert(VM, p_shown.0, weak_source(0xB2407, 7));
            assert!(t.set_shown_root(VM, p_shown.0, 0xB2407, 0xB24FF));
        }
        swing::swing_state().lock().mark_dirty(p_dead, 0, 0, 640, 480);

        // Nothing died yet: nothing goes.
        let none = free_dead_awt_rows(take_dead_awt_rows(VM, &resolve));
        assert!(none.is_empty());

        // The collector found the dead image, Graphics and component (and,
        // for the test, the shown one: its row holds a strong root, so the
        // sweep must not judge it).
        for h in [0xB2401, 0xB2403, 0xB2405, 0xB2407, 0xB2408] {
            roots.clear(h);
        }
        let mut released = free_dead_awt_rows(take_dead_awt_rows(VM, &resolve));
        released.sort_unstable();
        assert_eq!(released, vec![0xB2401, 0xB2403, 0xB2405]);
        {
            let reg = image::image_registry();
            assert!(reg.get(raster_dead).is_none(), "a dead image's raster is freed");
            assert!(reg.get(raster_live).is_some());
            assert!(reg.get(raster_other).is_some(), "another VM's sweep frees it");
        }
        assert!(!gfx_registry().lock().contains_key((VM, 3)));
        assert!(gfx_registry().lock().contains_key((VM, 4)));
        {
            let reg = peer::peer_registry().lock();
            assert!(reg.get(p_dead).is_none(), "a dead component's peer is destroyed");
            assert_eq!(reg.peer_for_java(VM, 5), None);
            assert!(reg.get(p_live).is_some());
            assert!(reg.get(p_shown).is_some(), "a shown component's peer stays");
        }
        assert!(peer_source_table().lock().get(VM, p_dead.0).is_none());
        assert!(peer_source_table().lock().get(VM, p_shown.0).is_some());
        assert!(!swing::swing_state()
            .lock()
            .dirty_regions
            .contains_key(&p_dead));
        assert!(
            free_dead_awt_rows(take_dead_awt_rows(VM, &resolve)).is_empty(),
            "one sweep takes a dead row once"
        );

        let other = free_dead_awt_rows(take_dead_awt_rows(OTHER_VM, &resolve));
        assert_eq!(other, vec![0xB2408]);
        assert!(image::image_registry().get(raster_other).is_none());
        forget_vm_awt_roots(VM);
        forget_vm_awt_roots(OTHER_VM);
        assert!(image::image_registry().get(raster_live).is_none());
    }

    /// Binding an image over a row whose image died frees that raster and
    /// hands its weak root back; a LIVE same-key image keeps its own row.
    #[test]
    fn w24b_image_bind_frees_a_dead_rows_raster_and_keeps_collisions_apart() {
        const VM: usize = 0x0B24_4A08;
        const KEY: ObjKey = (VM, 0x0B24_4000);
        let (a, b, c) = (
            fake_object_ref(0x0B24_41),
            fake_object_ref(0x0B24_42),
            fake_object_ref(0x0B24_43),
        );
        let roots = WeakRoots::new(&[(1, a), (2, b), (3, c)]);
        let resolve = |h: usize| roots.resolve(h);
        let (ra, rb, rc) = {
            let mut reg = image::image_registry();
            (
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
                reg.create(2, 2, ImageType::IntArgb).expect("create"),
            )
        };
        let mut t = buffered_image_ids().lock();
        assert_eq!(t.bind(KEY, a, 1, ra, &resolve), ImageBindLeftovers::default());
        assert_eq!(t.bind(KEY, b, 2, rb, &resolve), ImageBindLeftovers::default());
        assert_eq!(t.get(KEY, a, &resolve), Some(ra), "a reads its own pixels");
        assert_eq!(t.get(KEY, b, &resolve), Some(rb), "b reads its own pixels");
        roots.clear(1);
        let leftovers = t.bind(KEY, c, 3, rc, &resolve);
        assert_eq!(
            leftovers,
            ImageBindLeftovers {
                weak_roots: vec![1],
                rasters: vec![ra],
            }
        );
        assert_eq!(t.get(KEY, c, &resolve), Some(rc));
        assert_eq!(t.get(KEY, b, &resolve), Some(rb));
        let freed = t.forget_vm(VM);
        drop(t);
        let mut reg = image::image_registry();
        assert!(reg.destroy(ra), "the caller destroys a dead row's raster");
        for id in freed {
            reg.destroy(id);
        }
    }

    /// A peer belongs to the component its source row's weak root names; a
    /// strong row (no weak roots) or a missing row judges nothing. The shown
    /// root is installed once, only on the row it was minted for, and a
    /// replaced row hands both its roots back.
    #[test]
    fn w24b_peer_source_rows_judge_identity_and_hold_a_shown_root() {
        const VM: usize = 0x0B24_5A08;
        let (a, b) = (fake_object_ref(0x0B24_51), fake_object_ref(0x0B24_52));
        let roots = WeakRoots::new(&[(1, a)]);
        let resolve = |h: usize| roots.resolve(h);
        const WEAK_PEER: u64 = 0x0B24_5001;
        const STRONG_PEER: u64 = 0x0B24_5002;
        const NO_ROW_PEER: u64 = 0x0B24_5003;
        {
            let mut t = peer_source_table().lock();
            t.insert(VM, WEAK_PEER, weak_source(1, 5));
            t.insert(VM, STRONG_PEER, strong_source(9, 5));
        }
        let verdict = |peer: u64, obj| peer_row_verdict(VM, PeerId(peer), obj, &resolve);
        assert_eq!(verdict(WEAK_PEER, a), RowVerdict::Same);
        assert_eq!(verdict(WEAK_PEER, b), RowVerdict::Other);
        assert_eq!(verdict(STRONG_PEER, b), RowVerdict::Same);
        assert_eq!(verdict(NO_ROW_PEER, b), RowVerdict::Same);

        {
            let mut t = peer_source_table().lock();
            assert!(!t.set_shown_root(VM, WEAK_PEER, 2, 77), "minted for another row");
            assert!(t.set_shown_root(VM, WEAK_PEER, 1, 77));
            assert!(!t.set_shown_root(VM, WEAK_PEER, 1, 78), "already shown");
            assert_eq!(t.take_shown_root(VM, WEAK_PEER), Some(77));
            assert_eq!(t.take_shown_root(VM, WEAK_PEER), None);
            assert!(t.set_shown_root(VM, WEAK_PEER, 1, 79));
            let mut replaced = t.insert(VM, WEAK_PEER, weak_source(3, 5));
            replaced.sort_unstable_by_key(|r| r.handle);
            assert_eq!(
                replaced,
                vec![VmRoot { vm: VM, handle: 1 }, VmRoot { vm: VM, handle: 79 }]
            );
        }
        roots.clear(1);
        forget_vm_awt_roots(VM);
        assert!(peer_source_table().lock().get(VM, WEAK_PEER).is_none());
    }

    /// The sweep runs once per collection of its VM, and a VM's teardown
    /// forgets its generation.
    #[test]
    fn w24b_sweep_gate_runs_once_per_collection() {
        const VM: usize = 0x0B24_6A08;
        assert!(!awt_sweep_due(VM, 5), "first sight records the count");
        assert!(!awt_sweep_due(VM, 5));
        assert!(awt_sweep_due(VM, 6));
        assert!(!awt_sweep_due(VM, 6));
        forget_vm_awt_roots(VM);
        assert!(!awt_sweep_due(VM, 7), "teardown forgot the VM");
        forget_vm_awt_roots(VM);
        assert!(!awt_sweep_generations().lock().contains_key(&VM));
    }

    /// The event's `source` store takes the component resolved after the
    /// event object's allocation, not a value baked into the plan before it.
    #[test]
    fn w23a_event_source_is_supplied_after_the_allocation() {
        let evt = AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(3), 999);
        let plan = plan_event_synthesis(&evt, None).expect("WindowEvent plan");
        let moved = fake_object_ref(0x0A23_80);
        let fields: Vec<_> = plan_fields_with_source(&plan, Some(moved)).collect();
        assert!(fields.contains(&("source", Value::Object(Some(moved)))));
        assert!(fields.contains(&("id", Value::Int(event_id::WINDOW_CLOSING))));
        assert_eq!(fields.len(), plan.fields.len());
    }

    /// TASK #28: drive a synthetic MouseEvent end-to-end through a local
    /// EDT queue and the `getNextEvent` synthesis pipeline. Before this
    /// task `getNextEvent` returned null for every non-invocation event
    /// and the EDT silently dropped mouse / key / window listeners. The
    /// plan produced here is exactly what the native callback writes
    /// into the Java MouseEvent object — so asserting on the plan
    /// exercises the same code path the dispatch loop hits at runtime,
    /// minus the `new_object` / `set_field_by_name` round-trips (which
    /// require a full `NativeContext` we don't have in unit-test scope).
    ///
    /// Uses a *local* `EventDispatchThread` instead of the shared
    /// per-VM `edt::edt_for_vm` queues: tests run in parallel under
    /// `cargo test`, so a shared singleton would race with the
    /// `invoke_later` / `getNextEvent` tests in other modules.
    #[test]
    fn mouse_event_round_trips_through_event_queue_with_correct_coords() {
        // Register a fake Java component as the source for peer 7. The
        // peer-source table is process-wide but keyed by peer id, so a
        // unique peer id per test avoids cross-test interference.
        let source = fake_object_ref(7);
        let root = 7_007;
        // gc_gen 0 on both register and lookup: no collection runs in-test,
        // so the same-generation gate (finding H8) permits the round-trip.
        register_peer_source_root_for_test(PeerId(7), root, 7);

        // Inject a synthetic MOUSE_PRESSED via a local EDT (the same
        // entry point the platform backends use after translating a
        // WM_LBUTTONDOWN / X11 ButtonPress / Cocoa mouseDown into an
        // `AwtEvent`).
        let edt = crate::edt::EventDispatchThread::new();
        edt.post_event(AwtEvent::mouse(
            event_id::MOUSE_PRESSED,
            PeerId(7),
            42, // timestamp
            120,
            240,
            BUTTON1,
            1,
            modifiers::BUTTON1_DOWN_MASK,
        ));

        // This is what the native callback would dequeue:
        let evt = edt.poll_event().expect("event must be present");
        assert_eq!(evt.id, event_id::MOUSE_PRESSED);

        // And this is what it would write into the Java MouseEvent object:
        let plan = plan_event_synthesis(
            &evt,
            lookup_peer_source_for_test(evt.source_peer_id, root, source),
        )
        .expect("MouseEvent must produce a plan");
        assert_eq!(plan.class_name, "java/awt/event/MouseEvent");
        assert_eq!(
            find_field(&plan, "id"),
            Some(&Value::Int(event_id::MOUSE_PRESSED))
        );
        assert_eq!(find_field(&plan, "when"), Some(&Value::Long(42)));
        assert_eq!(find_field(&plan, "x"), Some(&Value::Int(120)));
        assert_eq!(find_field(&plan, "y"), Some(&Value::Int(240)));
        assert_eq!(find_field(&plan, "button"), Some(&Value::Int(BUTTON1)));
        assert_eq!(find_field(&plan, "clickCount"), Some(&Value::Int(1)));
        assert_eq!(
            find_field(&plan, "modifiers"),
            Some(&Value::Int(modifiers::BUTTON1_DOWN_MASK)),
        );
        // The source must round-trip from the peer-id → Java component map.
        assert_eq!(
            find_field(&plan, "source"),
            Some(&Value::Object(Some(source)))
        );
        // Consumed flag must start at 0 — listeners may flip it later.
        assert_eq!(find_field(&plan, "consumed"), Some(&Value::Int(0)));
    }

    /// TASK #28: confirm a `WindowEvent::WINDOW_CLOSING` survives the
    /// EDT queue, is synthesized as `java/awt/event/WindowEvent`, and
    /// carries the right `id` + `source`. This is the path a real
    /// platform-backend "close-button clicked" notification follows.
    #[test]
    fn window_closing_event_returns_correct_window_event_class() {
        let source = fake_object_ref(3);
        let root = 3_003;
        register_peer_source_root_for_test(PeerId(3), root, 3);

        let edt = crate::edt::EventDispatchThread::new();
        edt.post_event(AwtEvent::window(event_id::WINDOW_CLOSING, PeerId(3), 999));

        let evt = edt.poll_event().expect("window event must be present");
        assert_eq!(evt.id, event_id::WINDOW_CLOSING);

        let plan = plan_event_synthesis(
            &evt,
            lookup_peer_source_for_test(evt.source_peer_id, root, source),
        )
        .expect("WindowEvent must produce a plan");
        assert_eq!(plan.class_name, "java/awt/event/WindowEvent");
        assert_eq!(
            find_field(&plan, "id"),
            Some(&Value::Int(event_id::WINDOW_CLOSING)),
        );
        assert_eq!(
            find_field(&plan, "source"),
            Some(&Value::Object(Some(source)))
        );
        assert_eq!(find_field(&plan, "oldState"), Some(&Value::Int(0)));
        assert_eq!(find_field(&plan, "newState"), Some(&Value::Int(0)));
    }

    /// Sanity: a `KeyEvent::KEY_PRESSED` carries `keyCode`, `keyChar`,
    /// and modifiers through the synthesis plan. Regression guard for
    /// the listener-pipeline rewrite — without this the JDK
    /// `KeyEvent.getKeyCode()` accessor returned 0 for everything.
    #[test]
    fn key_event_carries_keycode_keychar_and_modifiers() {
        let source = fake_object_ref(5);
        let root = 5_005;
        register_peer_source_root_for_test(PeerId(5), root, 5);

        let evt = AwtEvent::key(
            event_id::KEY_PRESSED,
            PeerId(5),
            123,
            crate::event::vk::VK_A,
            'a',
            modifiers::SHIFT_DOWN_MASK,
        );
        let plan = plan_event_synthesis(
            &evt,
            lookup_peer_source_for_test(evt.source_peer_id, root, source),
        )
        .expect("KeyEvent must produce a plan");
        assert_eq!(plan.class_name, "java/awt/event/KeyEvent");
        assert_eq!(
            find_field(&plan, "keyCode"),
            Some(&Value::Int(crate::event::vk::VK_A))
        );
        assert_eq!(find_field(&plan, "keyChar"), Some(&Value::Int('a' as i32)));
        assert_eq!(
            find_field(&plan, "modifiers"),
            Some(&Value::Int(modifiers::SHIFT_DOWN_MASK)),
        );
        assert_eq!(
            find_field(&plan, "source"),
            Some(&Value::Object(Some(source)))
        );
    }

    /// Sanity: a paint event materialises as `PaintEvent` (the EDT
    /// dispatch loop relies on this class name so RepaintManager's
    /// dispatch-on-EDT path actually fires).
    #[test]
    fn paint_event_uses_paint_event_class() {
        let evt = AwtEvent::paint(event_id::PAINT, PeerId(0), 0, 0, 0, 100, 100);
        let plan = plan_event_synthesis(&evt, None).expect("PaintEvent must produce a plan");
        assert_eq!(plan.class_name, "java/awt/event/PaintEvent");
        assert_eq!(find_field(&plan, "id"), Some(&Value::Int(event_id::PAINT)));
    }

    /// Finding H8: `lookup_peer_source` must fail closed once a GC has run
    /// since the source was registered. The cached raw pointer could have
    /// been relocated/freed by the moving collector, so resurrecting it
    /// would be a use-after-free. The same-generation entry round-trips;
    /// a later generation yields `None`.
    #[test]
    fn peer_source_lookup_uses_global_root_handle() {
        let source = fake_object_ref(11);
        let root = 11_011;
        register_peer_source_root_for_test(PeerId(11), root, 11);
        assert_eq!(
            lookup_peer_source_for_test(PeerId(11), root, source),
            Some(source)
        );
        assert_eq!(
            lookup_peer_source_with(TEST_VM, PeerId(11), |_| None),
            None,
            "unresolvable global-root handles fail closed"
        );
        // Unknown peer -> None.
        assert_eq!(
            lookup_peer_source_for_test(PeerId(9999), root, source),
            None
        );
    }

    /// V3: `lookup_peer_source` must fail closed on a cached pointer that
    /// cannot designate a real heap object — even at the matching GC
    /// generation. A non-8-byte-aligned pointer is provably bogus (heap
    /// objects are 8-byte aligned), so the guard returns `None` rather than
    /// feeding a malformed pointer to `ObjectRef::from_raw`.
    #[test]
    fn peer_source_lookup_rejects_zero_root_handle() {
        // Insert a misaligned entry directly (a real `ObjectRef` can't carry
        // a misaligned pointer, but a corrupt/forged side-table entry could).
        peer_source_table().lock().insert(
            TEST_VM,
            424242,
            strong_source(0, 7),
        );
        // Matching generation, non-null — only the alignment guard stops it.
        let source = fake_object_ref(42);
        assert_eq!(lookup_peer_source_for_test(PeerId(424242), 0, source), None);

        // A null cached pointer also fails closed at the same generation.
        peer_source_table().lock().insert(
            TEST_VM,
            424243,
            strong_source(0, 7),
        );
        assert_eq!(lookup_peer_source_for_test(PeerId(424243), 0, source), None);
    }

    /// Component / Focus / Action events are TODO and intentionally
    /// produce no plan. Regression guard so we notice if the match arm
    /// quietly grows a new entry without a test.
    #[test]
    fn unsupported_event_kinds_return_none_for_now() {
        let comp = AwtEvent::component(event_id::COMPONENT_RESIZED, PeerId(1), 0, 0, 0, 10, 10);
        assert!(plan_event_synthesis(&comp, None).is_none());
        let focus = AwtEvent::focus(event_id::FOCUS_GAINED, PeerId(1), 0, false);
        assert!(plan_event_synthesis(&focus, None).is_none());
        let action = AwtEvent::action(PeerId(1), 0, "ok".to_string());
        assert!(plan_event_synthesis(&action, None).is_none());
    }

    /// `wait_event_blocking` must unblock as soon as an event is posted
    /// to the queue. Without this the EDT dispatch loop would
    /// busy-spin instead of sleeping between events.
    #[test]
    fn wait_event_blocking_returns_when_event_arrives() {
        use std::sync::Arc;
        let edt = Arc::new(crate::edt::EventDispatchThread::new());
        edt.start();
        let edt2 = Arc::clone(&edt);
        let producer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(15));
            edt2.post_event(AwtEvent::window(event_id::WINDOW_OPENED, PeerId(1), 0));
        });
        // 2-second total budget; 10ms re-check interval. Way more than
        // the producer needs (15ms) but bounded so a regression doesn't
        // hang the whole `cargo test` run.
        let got = edt.wait_event_blocking(2_000, 10, || false);
        producer.join().unwrap();
        edt.stop();
        let evt = got.expect("must observe the posted event");
        assert_eq!(evt.id, event_id::WINDOW_OPENED);
    }

    // ── Image flush / dispose-graphics buffer reclamation ─────────────

    /// Insert a `GfxEntry` directly into the process-wide registry under a
    /// unique hash and return that hash. Test-only shortcut that bypasses the
    /// `NativeContext`-dependent `register_gfx_for_image` path.
    fn insert_gfx_entry(hash: i32, target: GfxTarget, disposed: bool) {
        let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
            state: Graphics2DState::create(2, 2),
            target,
            disposed,
        }));
        let obj = fake_object_ref(hash as u32 as u64);
        gfx_registry()
            .lock()
            .insert((TEST_VM, hash), obj, 0, handle, &no_weak_roots);
    }

    fn gfx_contains(hash: i32) -> bool {
        gfx_registry().lock().contains_key((TEST_VM, hash))
    }

    fn remove_gfx_entry(hash: i32) {
        let obj = fake_object_ref(hash as u32 as u64);
        gfx_registry()
            .lock()
            .remove((TEST_VM, hash), obj, &no_weak_roots);
    }

    /// `flush_image` reaps a *disposed* Graphics2D scratch context tied to the
    /// flushed image id, freeing its buffer.
    #[test]
    fn flush_image_reaps_disposed_scratch_for_that_image() {
        let img = ImageId(0x5111_1000); // arbitrary unique id
        let hash = 0x5111_0001u32 as i32;
        insert_gfx_entry(hash, GfxTarget::Image(img), /*disposed=*/ true);
        assert!(gfx_contains(hash));
        flush_image(img);
        assert!(
            !gfx_contains(hash),
            "disposed scratch for the image must be reclaimed"
        );
    }

    /// `flush_image` must NOT touch a *live* (undisposed) context, even one
    /// targeting the same image — dropping it would discard an in-progress
    /// render's buffer (use-after-free of meaning, lost pixels).
    #[test]
    fn flush_image_keeps_live_context_for_that_image() {
        let img = ImageId(0x5111_2000);
        let hash = 0x5111_0002u32 as i32;
        insert_gfx_entry(hash, GfxTarget::Image(img), /*disposed=*/ false);
        flush_image(img);
        assert!(gfx_contains(hash), "live context must survive flush");
        // cleanup
        remove_gfx_entry(hash);
    }

    /// `flush_image` is scoped to a single image id — a disposed context for a
    /// *different* image is left alone.
    #[test]
    fn flush_image_ignores_other_images() {
        let flushed = ImageId(0x5111_3000);
        let other = ImageId(0x5111_3001);
        let hash = 0x5111_0003u32 as i32;
        insert_gfx_entry(hash, GfxTarget::Image(other), /*disposed=*/ true);
        flush_image(flushed);
        assert!(
            gfx_contains(hash),
            "other image's context must be untouched"
        );
        // cleanup
        remove_gfx_entry(hash);
    }

    // ── gc-common w25-d: a Graphics draws straight into its image ─────────

    fn image_graphics(id: ImageId) -> GfxEntry {
        GfxEntry {
            state: Graphics2DState::create(1, 1),
            target: GfxTarget::Image(id),
            disposed: false,
        }
    }

    fn new_image(w: u32, h: u32) -> ImageId {
        image::image_registry()
            .create(w, h, ImageType::IntArgb)
            .expect("create image")
    }

    fn pixel(id: ImageId, x: u32, y: u32) -> u32 {
        let reg = image::image_registry();
        reg.get(id).expect("image").get_rgb(x, y)
    }

    fn raster_len(id: ImageId) -> usize {
        let reg = image::image_registry();
        reg.get(id).expect("image").get_data_buffer().len()
    }

    /// The w24-b observation: `setRGB`, then `createGraphics()`, a draw and
    /// `dispose()` erased the `setRGB` pixel (the Graphics' blank scratch was
    /// copied over the whole image at `dispose`). HotSpot keeps it. And the
    /// drawn pixels are in the image before `dispose()`, as on HotSpot.
    #[test]
    fn w25d_graphics_keeps_the_images_pixels_and_draws_at_once() {
        let id = new_image(8, 8);
        image::image_registry()
            .get_mut(id)
            .unwrap()
            .set_rgb(1, 1, 0xFF12_3456);
        let mut g = image_graphics(id);
        with_gfx_entry(&mut g, |s| {
            s.set_color(0, 0, 255, 255);
            s.fill_rect(4, 4, 2, 2);
        });
        // Visible before dispose.
        assert_eq!(pixel(id, 4, 4), 0xFF00_00FF);
        assert_eq!(pixel(id, 5, 5), 0xFF00_00FF);
        // The pixel set before `createGraphics()` survives, and untouched
        // pixels stay transparent.
        assert_eq!(pixel(id, 1, 1), 0xFF12_3456);
        assert_eq!(pixel(id, 0, 0), 0);
        // `dispose` retires the context: later calls no longer draw, and
        // nothing is copied over the image.
        g.state.dispose();
        g.disposed = true;
        with_gfx_entry(&mut g, |s| s.fill_rect(0, 0, 8, 8));
        assert_eq!(pixel(id, 1, 1), 0xFF12_3456);
        assert_eq!(pixel(id, 0, 0), 0);
        // The raster is whole and the context kept its own 1x1 buffer.
        assert_eq!(raster_len(id), 64);
        assert_eq!((g.state.width(), g.state.height()), (1, 1));
        image::image_registry().destroy(id);
    }

    /// Two Graphics on one image (e.g. `createGraphics()` twice, or
    /// `Graphics.create()`) no longer erase each other's work.
    #[test]
    fn w25d_two_graphics_on_one_image_keep_each_others_pixels() {
        let id = new_image(4, 4);
        let mut a = image_graphics(id);
        let mut b = image_graphics(id);
        with_gfx_entry(&mut a, |s| {
            s.set_color(255, 0, 0, 255);
            s.fill_rect(0, 0, 1, 1);
        });
        with_gfx_entry(&mut b, |s| {
            s.set_color(0, 255, 0, 255);
            s.fill_rect(3, 3, 1, 1);
        });
        with_gfx_entry(&mut a, |s| s.fill_rect(1, 0, 1, 1));
        assert_eq!(pixel(id, 0, 0), 0xFFFF_0000);
        assert_eq!(pixel(id, 1, 0), 0xFFFF_0000);
        assert_eq!(pixel(id, 3, 3), 0xFF00_FF00);
        image::image_registry().destroy(id);
    }

    /// The `AwtChecksumProbe` round on the image path: `fillRect` over the
    /// whole 64x48 image, then the red 3-point `fillPolygon`. The image then
    /// holds exactly HotSpot's 570 red pixels over the fill colour.
    #[test]
    fn w25d_probe_round_draws_hotspots_pixels_into_the_image() {
        let id = new_image(64, 48);
        let mut g = image_graphics(id);
        with_gfx_entry(&mut g, |s| {
            s.set_color(10, 20, 30, 255);
            s.fill_rect(0, 0, 64, 48);
            s.set_color(255, 0, 0, 255);
            s.fill_polygon(&[5, 40, 20], &[5, 10, 40]);
        });
        let px = image::image_registry()
            .get(id)
            .unwrap()
            .get_rgb_region(0, 0, 64, 48);
        let red = px.iter().filter(|&&p| p == 0xFFFF_0000).count();
        let fill = px.iter().filter(|&&p| p == 0xFF0A_141E).count();
        assert_eq!((red, fill), (570, 64 * 48 - 570));
        // Row 10 (HotSpot `8..=39`; the old rasteriser filled `8..=40`).
        assert_eq!(px[10 * 64 + 7], 0xFF0A_141E);
        assert_eq!(px[10 * 64 + 8], 0xFFFF_0000);
        assert_eq!(px[10 * 64 + 39], 0xFFFF_0000);
        assert_eq!(px[10 * 64 + 40], 0xFF0A_141E);
        image::image_registry().destroy(id);
    }

    /// `drawImage` on an image's Graphics writes the destination image, from
    /// another image or from itself (a snapshot: the overlapping blit reads
    /// the pixels as they were).
    #[test]
    fn w25d_draw_image_writes_the_target_image() {
        let dst = new_image(4, 4);
        let src = new_image(2, 2);
        image::image_registry()
            .get_mut(src)
            .unwrap()
            .set_rgb(0, 0, 0xFF00_00FF);
        image::image_registry()
            .get_mut(dst)
            .unwrap()
            .set_rgb(3, 3, 0xFF12_3456);
        let mut g = image_graphics(dst);
        draw_buffered_image(&mut g, src, 1, 1);
        assert_eq!(pixel(dst, 1, 1), 0xFF00_00FF);
        assert_eq!(pixel(dst, 3, 3), 0xFF12_3456, "the rest of dst is kept");
        // Self-blit, shifted one pixel left: (1,1) lands on (0,0) and the
        // old (1,1) is read before it is overwritten.
        draw_buffered_image(&mut g, dst, -1, -1);
        assert_eq!(pixel(dst, 0, 0), 0xFF00_00FF);
        assert_eq!(pixel(dst, 2, 2), 0xFF12_3456);
        assert_eq!(raster_len(dst), 16);
        image::image_registry().destroy(dst);
        image::image_registry().destroy(src);
    }

    /// Both `getRGB` natives read through `java_rgb`: an opaque image type
    /// answers alpha 0xFF (HotSpot 25: `setRGB(0, 0, 0x00123456)` on a
    /// `TYPE_INT_RGB` image reads back `ff123456` from `getRGB(0, 0)` AND
    /// from the bulk `getRGB`), an alpha type the stored word.
    #[test]
    fn w25d_get_rgb_forces_alpha_on_opaque_types_only() {
        let rgb = image::image_registry()
            .create(2, 1, ImageType::IntRgb)
            .expect("create");
        let argb = new_image(2, 1);
        {
            let mut reg = image::image_registry();
            reg.get_mut(rgb).unwrap().set_rgb(0, 0, 0x0012_3456);
            reg.get_mut(argb).unwrap().set_rgb(0, 0, 0x0012_3456);
        }
        {
            let reg = image::image_registry();
            assert_eq!(java_rgb(reg.get(rgb).unwrap(), 0, 0), 0xFF12_3456);
            assert_eq!(java_rgb(reg.get(rgb).unwrap(), 1, 0), 0xFF00_0000);
            assert_eq!(java_rgb(reg.get(argb).unwrap(), 0, 0), 0x0012_3456);
            assert_eq!(java_rgb(reg.get(argb).unwrap(), 1, 0), 0);
        }
        image::image_registry().destroy(rgb);
        image::image_registry().destroy(argb);
    }

    /// `Graphics.create()` of an image's Graphics: the child draws into the
    /// same image, starts from the parent's colour and translation, and has
    /// no full-size buffer of its own. Without a parent row: detached.
    #[test]
    fn w25d_graphics_create_inherits_target_and_state() {
        let id = new_image(4, 4);
        let parent: GfxHandle = Arc::new(Mutex::new(image_graphics(id)));
        {
            let mut p = parent.lock();
            p.state.set_color(0, 0, 255, 255);
            p.state.translate(1.0, 1.0);
        }
        let mut child = child_gfx_entry(Some(&parent));
        assert!(matches!(child.target, GfxTarget::Image(t) if t == id));
        assert_eq!((child.state.width(), child.state.height()), (1, 1));
        with_gfx_entry(&mut child, |s| s.fill_rect(0, 0, 1, 1));
        assert_eq!(pixel(id, 1, 1), 0xFF00_00FF);
        assert_eq!(pixel(id, 0, 0), 0);
        assert!(matches!(child_gfx_entry(None).target, GfxTarget::Detached));
        image::image_registry().destroy(id);
    }

    /// A panic inside a drawing call still swaps the image's raster back.
    #[test]
    fn w25d_attached_raster_is_restored_on_unwind() {
        let id = new_image(4, 4);
        image::image_registry()
            .get_mut(id)
            .unwrap()
            .set_rgb(2, 2, 0xFF12_3456);
        let mut g = image_graphics(id);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            with_gfx_entry::<_, ()>(&mut g, |_| panic!("draw call panicked"))
        }));
        assert!(unwound.is_err());
        let reg = image::image_registry();
        let img = reg.get(id).unwrap();
        assert_eq!(img.get_data_buffer().len(), 16);
        assert_eq!(img.get_rgb(2, 2), 0xFF12_3456);
        drop(reg);
        assert_eq!((g.state.width(), g.state.height()), (1, 1));
        image::image_registry().destroy(id);
    }

    /// A Graphics whose image raster is gone (the image was collected) draws
    /// into its own buffer instead of failing.
    #[test]
    fn w25d_graphics_of_a_destroyed_image_draws_into_its_own_buffer() {
        let id = new_image(4, 4);
        image::image_registry().destroy(id);
        let mut g = image_graphics(id);
        with_gfx_entry(&mut g, |s| {
            s.set_color(255, 0, 0, 255);
            s.fill_rect(0, 0, 1, 1);
        });
        assert_eq!(g.state.pixels(), &[0xFFFF_0000]);
    }

    /// Flushing must NOT destroy the image's pixel raster: a memory-backed
    /// `BufferedImage` is re-usable after `flush()` (legal `getRGB` afterwards).
    #[test]
    fn flush_image_preserves_pixel_raster() {
        let id = image::image_registry()
            .create(4, 4, ImageType::IntArgb)
            .expect("create image");
        image::image_registry()
            .get_mut(id)
            .unwrap()
            .set_rgb(1, 1, 0xDEAD_BEEF);
        flush_image(id);
        // Raster still present and pixel intact.
        let reg = image::image_registry();
        let img = reg.get(id).expect("raster must survive flush");
        assert_eq!(img.get_rgb(1, 1), 0xDEAD_BEEF);
    }
}
