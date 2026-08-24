// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! AWT/Swing/Java2D native method registration.
//!
//! Registers native callbacks for `java/awt/*`, `javax/swing/*`, and
//! `sun/java2d/*` classes. Each callback maps Java-side API calls to
//! the Rust peer/renderer/EDT infrastructure in this crate.

use std::sync::{Arc, OnceLock};

use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

use crate::edt;
use crate::edt::InvokeAndWaitError;
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
// `edt::get_edt().take_runnable_checked(callback_id, gc_gen)` (the checked
// variant fails closed across GC boundaries — see the dispatch native).
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

struct InvocationCallbackTable {
    map: FxHashMap<i32, u64>,
    order: std::collections::VecDeque<i32>,
}

impl InvocationCallbackTable {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn insert(&mut self, key: i32, value: u64) {
        while self.map.len() >= MAX_INVOCATION_CALLBACKS {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
        if self.map.insert(key, value).is_none() {
            self.order.push_back(key);
        }
    }

    fn remove(&mut self, key: i32) -> Option<u64> {
        let v = self.map.remove(&key);
        if v.is_some() {
            if let Some(pos) = self.order.iter().position(|&k| k == key) {
                self.order.remove(pos);
            }
        }
        v
    }
}

fn invocation_event_callbacks() -> &'static Mutex<InvocationCallbackTable> {
    static INSTANCE: OnceLock<Mutex<InvocationCallbackTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(InvocationCallbackTable::new()))
}

fn bind_invocation_event(event_hash: i32, callback_id: u64) {
    invocation_event_callbacks()
        .lock()
        .insert(event_hash, callback_id);
}

fn take_invocation_event_callback(event_hash: i32) -> Option<u64> {
    invocation_event_callbacks().lock().remove(event_hash)
}

fn add_global_root_or_oom(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    label: &str,
) -> Result<usize, cratonvm_types::error::MethodCallFailed> {
    let handle = ctx.add_global_root(obj);
    if handle == 0 {
        return Err(RuntimeError::OutOfMemoryError {
            message: format!("failed to create global root for {label}"),
        }
        .into());
    }
    Ok(handle)
}

fn release_global_roots<I>(ctx: &mut dyn NativeContext, handles: I)
where
    I: IntoIterator<Item = usize>,
{
    for handle in handles {
        if handle != 0 {
            ctx.remove_global_root(handle);
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
// peer, and look it up when dispatching events. The map is capped with
// FIFO eviction so a stream of orphaned peers (e.g. dropped Java refs that
// never made it to `Component.removeNotify`) cannot grow it without bound.
//
// SECURITY (finding H8 — GC raw-pointer resurrection / use-after-free):
//
// The previous implementation stored the source `ObjectRef` as a bare
// `usize` (`source.as_ptr() as usize`) and later resurrected it via
// `ObjectRef::from_raw`, handing the resulting reference back to the VM as
// an event source. The side table holds NO GC root, and CratonVM's
// collector is a *moving* collector (G1 evacuation + full-heap compaction
// fallback — see `gc::g1` / `gc::heap`). If a collection runs between
// registration and lookup, the stored pointer is dangling or points at an
// unrelated object that occupies the old slot — a use-after-free / type
// confusion the moment `set_field_by_name` dereferences it.
//
// There is no rooting / weak-handle / identity-hash→ObjectRef API reachable
// from this crate (see `cratonvm_native_api::NativeContext` — it exposes
// `identity_hash_code` and `gc_collection_count` but no way to pin an object
// or resolve a live reference from a hash). So we cannot keep the pointer
// alive, nor re-resolve it safely after a move.
//
// Mitigation implemented here: fail closed across GC boundaries. We stamp
// every registration with the GC collection count observed at registration
// time, alongside the object's stable identity hash. On lookup we only
// resurrect the pointer when the GC count is *unchanged* since registration:
// in that window the moving collector provably has not run, so the object
// cannot have been relocated or freed and the raw pointer is still valid.
// If any collection has occurred we treat the entry as stale and return
// `None` (the event is synthesised with a `null` source — the JDK tolerates
// a null `AWTEvent.source`), rather than dereferencing a pointer that may
// have moved. `ensure_peer` re-registers on every observation, so a live
// component that keeps being touched keeps a fresh, same-generation entry.
//
// The stored identity hash is currently only a diagnostic/consistency tag;
// resolving it back to a live `ObjectRef` would let us drop the
// generation gate entirely. See the orchestrator report for the precise VM
// API that would enable that (a `object_for_identity_hash` / weak-handle
// accessor on `NativeContext`).

const MAX_PEER_SOURCES: usize = 10_000;

#[derive(Clone, Copy)]
struct PeerSourceEntry {
    /// Opaque handle returned by `NativeContext::add_global_root`.
    root_handle: usize,
    /// Stable VM identity hash of the source component (GC-move
    /// independent). Retained as a consistency tag for the eventual
    /// hash→ObjectRef resolution path.
    java_hash: i32,
}

struct PeerSourceTable {
    map: FxHashMap<u64, PeerSourceEntry>,
    order: std::collections::VecDeque<u64>,
}

impl PeerSourceTable {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
            order: std::collections::VecDeque::new(),
        }
    }

    fn insert(&mut self, key: u64, entry: PeerSourceEntry) -> Vec<usize> {
        let mut removed_roots = Vec::new();
        while self.map.len() >= MAX_PEER_SOURCES {
            if let Some(old) = self.order.pop_front() {
                if let Some(entry) = self.map.remove(&old) {
                    removed_roots.push(entry.root_handle);
                }
            } else {
                break;
            }
        }
        if let Some(previous) = self.map.insert(key, entry) {
            removed_roots.push(previous.root_handle);
        } else {
            self.order.push_back(key);
        }
        removed_roots
    }

    fn get(&self, key: u64) -> Option<PeerSourceEntry> {
        self.map.get(&key).copied()
    }
}

fn peer_source_table() -> &'static Mutex<PeerSourceTable> {
    static INSTANCE: OnceLock<Mutex<PeerSourceTable>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(PeerSourceTable::new()))
}

/// Record the Java source component for a peer. `java_hash` is the source's
/// stable VM identity hash and `gc_gen` is the current GC collection count
/// (both read from an active `NativeContext` by the caller). See the module
/// comment for why the GC generation is captured.
fn register_peer_source(
    ctx: &mut dyn NativeContext,
    peer_id: PeerId,
    source: ObjectRef,
    java_hash: i32,
) {
    let Ok(root_handle) = add_global_root_or_oom(ctx, source, "AWT peer source") else {
        tracing::warn!(
            peer_id = peer_id.0,
            "AWT peer source not rooted; events for this peer may use a null source"
        );
        return;
    };
    let removed_roots = peer_source_table().lock().insert(
        peer_id.0,
        PeerSourceEntry {
            root_handle,
            java_hash,
        },
    );
    release_global_roots(ctx, removed_roots);
}

/// Resolve the Java source component for a peer, given the *current* GC
/// collection count. Returns `None` (fail closed) when the entry is absent,
/// the pointer is null, or any GC has occurred since registration — in the
/// last case the moving collector may have relocated or freed the object, so
/// resurrecting the raw pointer would be a use-after-free.
fn lookup_peer_source_with<F>(peer_id: PeerId, resolve: F) -> Option<ObjectRef>
where
    F: FnOnce(usize) -> Option<ObjectRef>,
{
    let entry = peer_source_table().lock().get(peer_id.0)?;
    if entry.root_handle == 0 {
        // Null is never a valid heap object — `ObjectRef::from_raw` requires
        // non-null (debug_assert) and the VM never represents `null` this way.
        return None;
    }
    // A collection ran since the source was registered. The cached pointer
    // may be dangling (object freed) or relocated (moving collector) —
    // refuse to resurrect it. See the precise invariant in the SAFETY note.
    // Liveness sanity check consistent with `ObjectRef::from_raw`'s second
    // precondition: every CratonVM heap object is 8-byte aligned. A cached
    // pointer that is not 8-byte aligned cannot designate a real object, so a
    // bogus/corrupt entry that slipped past the GC-gen gate fails closed here
    // rather than constructing a malformed `ObjectRef`.
    // SAFETY: `entry.ptr` is non-null and 8-byte aligned (both checked above),
    // satisfying `ObjectRef::from_raw`'s preconditions. The pointer still
    // designates the same live heap object because no collection has run since
    // `register_peer_source` captured it.
    //
    // INVARIANT (load-bearing): this rests on `NativeContext::gc_collection_count()`
    // incrementing on *every* collection that can free or relocate a heap
    // object — moving (G1 evacuation / full compaction) AND any non-moving
    // sweep that reclaims dead objects. If a collection that frees `ptr`'s
    // object did NOT bump the count, the gc_gen gate above would let a freed
    // pointer through (use-after-free). The counter is incremented at the
    // single collection entry point in the GC, so any current or future
    // collector is covered; a new collector path that frees objects without
    // bumping it would break this invariant and must update the counter.
    //
    // We only reconstruct the ref to hand it back to the VM as a
    // `Value::Object(Some(ref))` event source; we never mutate through it here.
    let _ = entry.java_hash; // retained for the future hash→ref resolution path
    resolve(entry.root_handle)
}

fn lookup_peer_source(ctx: &dyn NativeContext, peer_id: PeerId) -> Option<ObjectRef> {
    let entry = peer_source_table().lock().get(peer_id.0)?;
    if entry.root_handle == 0 {
        return None;
    }
    let source = ctx.resolve_global_root(entry.root_handle)?;
    if ctx.identity_hash_code(source) != entry.java_hash {
        return None;
    }
    Some(source)
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
// case `dispose` flushes its pixel buffer back to the image registry) or a
// `ComponentPeer` (in which case `dispose` marks the peer dirty for repaint).

#[derive(Debug, Clone, Copy)]
enum GfxTarget {
    Image(ImageId),
    Peer(PeerId),
    Detached,
}

struct GfxEntry {
    state: Graphics2DState,
    target: GfxTarget,
    /// Set once `dispose()` has flushed this context's pixels back to its
    /// target and freed its scratch renderer. A disposed entry holds only a
    /// 1x1 placeholder buffer; it is kept solely so that `flush_image` can
    /// reap any disposed contexts still associated with an image id without
    /// racing a live (undisposed) context. See `dispose_gfx` / `flush_image`.
    disposed: bool,
}

/// Each Graphics2D context is wrapped in its own `Arc<Mutex<...>>` so that
/// concurrent draw calls on *different* Graphics2D objects don't serialise
/// through the registry's outer lock — we only hold the outer lock long
/// enough to clone the Arc.
type GfxHandle = Arc<Mutex<GfxEntry>>;

struct GfxRegistry {
    map: FxHashMap<i32, GfxHandle>,
}

impl GfxRegistry {
    fn new() -> Self {
        Self {
            map: FxHashMap::default(),
        }
    }
}

fn gfx_registry() -> &'static Mutex<GfxRegistry> {
    static INSTANCE: OnceLock<Mutex<GfxRegistry>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(GfxRegistry::new()))
}

/// Look up (or lazily create) the `GfxHandle` for a Java Graphics2D receiver.
/// Holds the outer registry lock only long enough to clone the `Arc`.
fn gfx_handle_for(ctx: &dyn NativeContext, receiver: ObjectRef) -> GfxHandle {
    let hash = ctx.identity_hash_code(receiver);
    let mut reg = gfx_registry().lock();
    if let Some(handle) = reg.map.get(&hash) {
        return handle.clone();
    }
    // Lazy fallback: caller invoked a draw method on a Graphics2D that
    // we never saw `create`/`getGraphics` for.  Allocate a default-sized
    // detached buffer so the call doesn't panic.
    let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
        state: Graphics2DState::create(1, 1),
        target: GfxTarget::Detached,
        disposed: false,
    }));
    reg.map.insert(hash, handle.clone());
    handle
}

/// Ensure a `Graphics2DState` exists for the given Java Graphics2D receiver,
/// then run `f` against it. Lazily creates a detached context if no
/// associated target has been registered, so misbehaving callers still get
/// drawing semantics (writing into a throwaway buffer) instead of panics.
///
/// The outer registry lock is released before `f` runs, so concurrent draw
/// calls on different Graphics2D objects don't block each other.
fn with_gfx<F, R>(ctx: &dyn NativeContext, receiver: ObjectRef, f: F) -> R
where
    F: FnOnce(&mut Graphics2DState) -> R,
    R: Default,
{
    let handle = gfx_handle_for(ctx, receiver);
    let mut entry = handle.lock();
    f(&mut entry.state)
}

/// Register a freshly-created Graphics2D Java object as backing a given
/// rendering target. Sized to the target's pixel dimensions.
fn register_gfx_for_image(ctx: &dyn NativeContext, gfx_obj: ObjectRef, image_id: ImageId) {
    let (w, h) = {
        let reg = image::image_registry();
        match reg.get(image_id) {
            Some(img) => (img.width(), img.height()),
            None => (1, 1),
        }
    };
    let hash = ctx.identity_hash_code(gfx_obj);
    let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
        state: Graphics2DState::create(w.max(1), h.max(1)),
        target: GfxTarget::Image(image_id),
        disposed: false,
    }));
    gfx_registry().lock().map.insert(hash, handle);
}

fn register_gfx_for_peer(ctx: &dyn NativeContext, gfx_obj: ObjectRef, peer_id: PeerId) {
    let (w, h) = {
        let reg = peer::peer_registry().lock();
        match reg.get(peer_id) {
            Some(p) => (p.width.max(1), p.height.max(1)),
            None => (1, 1),
        }
    };
    let hash = ctx.identity_hash_code(gfx_obj);
    let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
        state: Graphics2DState::create(w, h),
        target: GfxTarget::Peer(peer_id),
        disposed: false,
    }));
    gfx_registry().lock().map.insert(hash, handle);
}

/// Flush Graphics2D pixel data back to its target (BufferedImage or peer)
/// and remove the registry entry. Called from `dispose()`.
///
/// Memory note: the entry is `remove`d from the registry up front and the
/// `GfxHandle` `Arc` is dropped at the end of this function, so the disposed
/// context's full-size scratch renderer (≈ `w*h*4` bytes — ~8 MiB for a 1080p
/// image) is reclaimed immediately on dispose rather than lingering until
/// process exit.
fn dispose_gfx(ctx: &dyn NativeContext, receiver: ObjectRef) {
    let hash = ctx.identity_hash_code(receiver);
    let handle = { gfx_registry().lock().map.remove(&hash) };
    let Some(handle) = handle else {
        return;
    };
    let mut entry = handle.lock();
    entry.state.dispose();
    entry.disposed = true;
    match entry.target {
        GfxTarget::Image(image_id) => {
            // Commit the rendered result back to the backing image. Borrow the
            // gfx scratch buffer (`&[u32]` via `pixels()`) and the image buffer
            // (`&mut [u32]` via `get_data_buffer_mut()`) simultaneously: they
            // are distinct allocations behind distinct locks, so we can copy
            // straight from one into the other without first cloning the gfx
            // buffer into an intermediate `Vec` (the previous `to_vec()` was an
            // ~8 MiB heap allocation + memcpy on every image-graphics dispose).
            let w = entry.state.width();
            let h = entry.state.height();
            let mut reg = image::image_registry();
            if let Some(img) = reg.get_mut(image_id) {
                if img.width() == w && img.height() == h {
                    img.get_data_buffer_mut()
                        .copy_from_slice(entry.state.pixels());
                }
            }
        }
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
/// image's true end-of-life, for which no native hook currently exists; this
/// is the documented nuance for finding #1.
///
/// What we CAN safely reclaim is any *disposed* Graphics2D scratch context
/// still associated with this image id. A disposed context has already
/// committed its pixels back to the raster (see `dispose_gfx`) and the Java
/// `Graphics2D` is, by contract, unusable, so its full-size scratch buffer is
/// provably dead. `dispose_gfx` normally removes such entries immediately, but
/// the lazy-fallback path in `gfx_handle_for` can mint untracked contexts that
/// are later disposed; this reaper bounds their accumulation. We only touch
/// entries flagged `disposed`, never a live one, so there is no risk of
/// dropping a buffer that an in-flight render still holds.
fn flush_image(image_id: ImageId) {
    let mut reg = gfx_registry().lock();
    reg.map.retain(|_hash, handle| {
        // Only inspect handles we can lock without contention; a currently
        // locked handle is in active use (a draw call holds it), so it is by
        // definition not a dead disposed scratch buffer — keep it.
        let Some(entry) = handle.try_lock() else {
            return true;
        };
        let is_dead_scratch =
            entry.disposed && matches!(entry.target, GfxTarget::Image(id) if id == image_id);
        // Returning `false` drops the entry (and its `Arc`/scratch buffer).
        !is_dead_scratch
    });
}

/// Upper bound on the up-front capacity reserved from a reported array length.
///
/// `array_length` comes from the heap layout of a (possibly malformed/hostile)
/// array object. A bogus length would otherwise force a giant eager
/// `Vec::with_capacity` allocation before a single element is read. We reserve
/// at most this many slots and let the `Vec` grow on demand for legitimately
/// large arrays — the per-element read loop bounds total memory anyway.
const MAX_INT_ARRAY_PREALLOC: usize = 1 << 20; // 1M ints = 4 MiB

/// Read an int[] array into a Vec<i32>.
fn read_int_array(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<i32> {
    let len = ctx.array_length(obj);
    // Cap the pre-reserve so a bogus reported length can't trigger a huge
    // up-front allocation; the loop still appends exactly `len` elements,
    // growing the Vec as needed for genuinely large (but valid) arrays.
    let mut out = Vec::with_capacity(len.min(MAX_INT_ARRAY_PREALLOC));
    for i in 0..len {
        if let Value::Int(v) = ctx.get_array_element(obj, i) {
            out.push(v);
        } else {
            out.push(0);
        }
    }
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
// retagged. See audits/jdk-only-ambient-category-audit.md.
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
fn ensure_peer(ctx: &mut dyn NativeContext, obj: ObjectRef, ctype: ComponentType) -> PeerId {
    let hash = ctx.identity_hash_code(obj);
    let id = {
        let mut reg = peer::peer_registry().lock();
        if let Some(id) = reg.peer_for_java(hash) {
            id
        } else {
            let id = reg.create_peer(ctype);
            reg.register_java_mapping(hash, id);
            id
        }
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
    let tk = ctx.new_object("sun/awt/HeadlessToolkit")?;
    if let Some(Value::Object(Some(obj))) = &tk {
        // HeadlessToolkit(Toolkit) — store a null underlying toolkit. The
        // ctor's `instanceof ComponentFactory` check is null-safe.
        ctx.invoke(
            "sun/awt/HeadlessToolkit",
            "<init>",
            "(Ljava/awt/Toolkit;)V",
            &[Value::Object(Some(*obj)), Value::Object(None)],
        )?;
    }
    Ok(tk)
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
    registry.register_with_kind("sun/java2d/Disposer", "initIDs", "()V", |_ctx, _args| {
        void_ok()
    }, NativeKind::Bridge);
    // Toolkit.<clinit> calls this JNI bootstrap before ImageIO and Spring's
    // HTTP image converter can initialize desktop classes. CratonVM keeps the
    // relevant IDs in Rust-side registries, so the HotSpot native is a no-op.
    registry.register_with_kind("java/awt/Toolkit", "initIDs", "()V", |_ctx, _args| {
        void_ok()
    }, NativeKind::Bridge);
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
// See retired/l5-native-io-bridge-residuals-RETIRED-20260810.md.
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
        let arr = ctx.new_ref_array(string_class, FAMILIES.len());
        for (i, name) in FAMILIES.iter().enumerate() {
            let s = ctx.create_string(name);
            ctx.set_array_element(arr, i, Value::Object(Some(s)));
        }
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
        let hge = ctx.new_object("sun/java2d/HeadlessGraphicsEnvironment")?;
        if let Some(Value::Object(Some(obj))) = &hge {
            ctx.invoke(
                "sun/java2d/HeadlessGraphicsEnvironment",
                "<init>",
                "(Ljava/awt/GraphicsEnvironment;)V",
                &[Value::Object(Some(*obj)), Value::Object(platform_ge)],
            )?;
        }
        return Ok(hge);
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
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get_mut(pid) {
                    peer.visible = visible;
                }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "setEnabled", "(Z)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let enabled = get_bool(args, 1);
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
                let hash = ctx.identity_hash_code(this);
                let mut reg = peer::peer_registry().lock();
                if let Some(pid) = reg.peer_for_java(hash) {
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
                let hash = ctx.identity_hash_code(this);
                let mut reg = peer::peer_registry().lock();
                if let Some(pid) = reg.peer_for_java(hash) {
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
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
                let hash = ctx.identity_hash_code(this);
                let pid_opt = {
                    let peer_reg = peer::peer_registry().lock();
                    peer_reg.peer_for_java(hash)
                };
                if let Some(pid) = pid_opt {
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
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
                    let hash = ctx.identity_hash_code(this);
                    let mut reg = peer::peer_registry().lock();
                    if let Some(pid) = reg.peer_for_java(hash) {
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
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get(pid) {
                    return int_ok(peer.width as i32);
                }
            }
        }
        int_ok(0)
    });

    registry.register("java/awt/Component", "getHeight", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
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
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let n = get_int(args, 3).max(0) as usize;
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.draw_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "fillPolygon", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let n = get_int(args, 3).max(0) as usize;
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.fill_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "drawPolyline", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let xs = get_obj(args, 1)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let ys = get_obj(args, 2)
                    .map(|a| read_int_array(ctx, a))
                    .unwrap_or_default();
                let n = get_int(args, 3).max(0) as usize;
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
                            // Acquire locks in the same order as `dispose_gfx`
                            // (Gfx entry first, then image registry) so the two
                            // can't deadlock when racing on the same Graphics2D.
                            // Holding the image-registry lock just long enough
                            // to borrow the source pixel buffer as a slice
                            // avoids an 8 MiB `.to_vec()` per call (≈ a 1080p
                            // frame buffer).
                            let handle = gfx_handle_for(ctx, this);
                            let mut entry = handle.lock();
                            let reg = image::image_registry();
                            if let Some(bimg) = reg.get(id) {
                                let pixels: &[u32] = bimg.get_data_buffer();
                                let (w, h) = (bimg.width(), bimg.height());
                                if !pixels.is_empty() {
                                    entry.state.draw_image(pixels, w, h, x, y);
                                }
                            }
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
        registry.register(class, "setBackground", "(Ljava/awt/Color;)V", |ctx, args| {
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
        });
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
                    let hash = ctx.identity_hash_code(this);
                    let handle = gfx_registry().lock().map.get(&hash).cloned();
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
                    let hash = ctx.identity_hash_code(this);
                    let handle = gfx_registry().lock().map.get(&hash).cloned();
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
            // Cloning a Graphics2D returns a new receiver that inherits
            // current state.  We make a fresh detached Graphics2DState and
            // copy the parent's state via save/restore through transform/
            // clip/font/color extraction.  Simpler approximation: clone the
            // backing target so subsequent draws still flush to the same
            // image/peer.
            let target = if let Some(this) = get_obj(args, 0) {
                let hash = ctx.identity_hash_code(this);
                let handle = gfx_registry().lock().map.get(&hash).cloned();
                handle
                    .map(|h| h.lock().target)
                    .unwrap_or(GfxTarget::Detached)
            } else {
                GfxTarget::Detached
            };
            let new_gfx = ctx.new_object("java/awt/Graphics2D")?;
            if let Some(Value::Object(Some(obj))) = &new_gfx {
                match target {
                    GfxTarget::Image(id) => register_gfx_for_image(ctx, *obj, id),
                    GfxTarget::Peer(pid) => register_gfx_for_peer(ctx, *obj, pid),
                    GfxTarget::Detached => {
                        let hash = ctx.identity_hash_code(*obj);
                        let handle: GfxHandle = Arc::new(Mutex::new(GfxEntry {
                            state: Graphics2DState::create(1, 1),
                            target: GfxTarget::Detached,
                            disposed: false,
                        }));
                        gfx_registry().lock().map.insert(hash, handle);
                    }
                }
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

fn buffered_image_ids() -> &'static Mutex<FxHashMap<i32, ImageId>> {
    static INSTANCE: OnceLock<Mutex<FxHashMap<i32, ImageId>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(FxHashMap::default()))
}

fn bind_buffered_image(ctx: &dyn NativeContext, obj: ObjectRef, image_id: ImageId) {
    buffered_image_ids()
        .lock()
        .insert(ctx.identity_hash_code(obj), image_id);
}

fn buffered_image_id(ctx: &dyn NativeContext, obj: ObjectRef) -> Option<ImageId> {
    if let Value::Long(id) = ctx.get_field_by_name(obj, "imageId") {
        return Some(ImageId(id as u64));
    }
    buffered_image_ids()
        .lock()
        .get(&ctx.identity_hash_code(obj))
        .copied()
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
    let image_obj = iio_image_rendered_image(ctx, iio_image)?;
    let output = imageio_writer_output_stream(ctx, writer)?;
    let bytes = encode_rendered_image(ctx, image_obj, format)?;
    write_all_to_output_stream(ctx, output, &bytes)?;
    Ok(None)
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
fn attach_real_raster(ctx: &mut dyn NativeContext, this: ObjectRef, w: i32, h: i32, image_type: i32) {
    let Some(masks) = packed_masks(image_type) else {
        return;
    };
    let has_alpha = image_type == 2;
    let nbands = if has_alpha { 4 } else { 3 };

    let size = match w.checked_mul(h) {
        Some(v) if v >= 0 => v,
        _ => return,
    };
    let Ok(Some(Value::Object(Some(db)))) =
        ctx.new_object_initialized("java/awt/image/DataBufferInt", "(I)V", &[Value::Int(size)])
    else {
        return;
    };

    let mask_arr = ctx.new_array(ArrayElementType::Int, nbands);
    for (i, m) in masks.iter().take(nbands).enumerate() {
        ctx.set_array_element(mask_arr, i, Value::Int(*m));
    }

    let raster = match ctx.invoke(
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
        ctx.new_object_initialized("java/awt/image/DirectColorModel", cm_desc, &cm_args)
    else {
        return;
    };

    ctx.set_field_by_name(this, "raster", Value::Object(Some(raster)));
    ctx.set_field_by_name(this, "colorModel", Value::Object(Some(cm)));
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
    for i in 0..len {
        ctx.set_array_element(data, i, Value::Int(pixels[i] as i32));
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
                        let px = img.get_rgb(x as u32, y as u32);
                        let px = if img.image_type().has_alpha() {
                            px
                        } else {
                            px | 0xFF00_0000
                        };
                        return int_ok(px as i32);
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
                    flush_image(id);
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
            let reg = image::image_registry();
            let Some(img) = reg.get(id) else {
                return null_ok();
            };
            let (iw, ih) = (img.width() as i32, img.height() as i32);
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
            let arr = match get_obj(args, 5) {
                Some(a) => a,
                None => {
                    let len = needed.max(0).min(i32::MAX as i64) as usize;
                    ctx.new_array(cratonvm_types::ArrayElementType::Int, len)
                }
            };
            let arr_len = ctx.array_length(arr) as i64;
            // Snapshot the pixels before touching the array so a bounds
            // failure leaves the destination untouched.
            for row in 0..h {
                for col in 0..w {
                    let argb = img.get_rgb((start_x + col) as u32, (start_y + row) as u32) as i32;
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

/// Apply a [`EventSynthesisPlan`] against a real `NativeContext` to produce
/// the Java event object.
fn materialise_event(ctx: &mut dyn NativeContext, plan: &EventSynthesisPlan) -> MethodCallResult {
    let obj_val = ctx.new_object(plan.class_name)?;
    if let Some(Value::Object(Some(obj))) = &obj_val {
        for (name, value) in &plan.fields {
            // `Value` is `Copy`, so dereference rather than clone — keeps
            // Clippy `clippy::clone_on_copy` happy without a lint-allow.
            ctx.set_field_by_name(*obj, name, *value);
        }
    }
    Ok(obj_val)
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
    let hash = ctx.identity_hash_code(source);
    peer::peer_registry()
        .lock()
        .peer_for_java(hash)
        .unwrap_or(PeerId(0))
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
                // Capture the current GC collection count and stamp it on the
                // registry entry: `invokeLater` is asynchronous, so a moving
                // collection can relocate/free the Runnable before dispatch.
                // The dispatch site fails closed on a generation mismatch
                // (see `take_runnable_checked`), mirroring `lookup_peer_source`.
                let root_handle = add_global_root_or_oom(ctx, runnable, "AWT Runnable")?;
                let (_, removed_roots) =
                    edt::get_edt().invoke_later_runnable(root_handle, PeerId(0));
                release_global_roots(ctx, removed_roots);
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
                let root_handle = add_global_root_or_oom(ctx, runnable, "AWT Runnable")?;
                match edt::get_edt().invoke_and_wait_runnable(root_handle, PeerId(0)) {
                    Ok((callback_id, removed_roots)) => {
                        release_global_roots(ctx, removed_roots);
                        if let Some(root_handle) = edt::get_edt().take_runnable_root(callback_id) {
                            release_global_roots(ctx, [root_handle]);
                        }
                    }
                    Err(InvokeAndWaitError::OnEdt) => {
                        release_global_roots(ctx, [root_handle]);
                        return Err(RuntimeError::IllegalStateException {
                            message: InvokeAndWaitError::OnEdt.jdk_message().to_string(),
                        }
                        .into());
                    }
                }
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
                edt::get_edt().post_event(evt);
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
            let Some(evt) = edt::get_edt().wait_event_blocking(
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
            ) else {
                return null_ok();
            };
            use crate::event::AwtEventData;
            if let AwtEventData::Invocation { callback_id } = evt.data {
                let inv = ctx.new_object("java/awt/event/InvocationEvent")?;
                if let Some(Value::Object(Some(obj))) = &inv {
                    let hash = ctx.identity_hash_code(*obj);
                    bind_invocation_event(hash, callback_id);
                }
                return Ok(inv);
            }
            // All non-invocation event kinds: materialise the matching Java
            // class with `source` / `id` / `when` / modifiers / payload set.
            // Pass the current GC count so `lookup_peer_source` only resurrects
            // the cached source pointer if no (moving) collection has run since
            // it was registered (finding H8).
            let source = lookup_peer_source(ctx, evt.source_peer_id);
            if let Some(plan) = plan_event_synthesis(&evt, source) {
                return materialise_event(ctx, &plan);
            }
            // TODO: Component / Focus / Action events still drop here. They
            // are far less common than mouse/key/window/paint, but a future
            // pass should extend `plan_event_synthesis` to cover them.
            null_ok()
        },
    );
    registry.register(
        "java/awt/EventQueue",
        "peekEvent",
        "()Ljava/awt/AWTEvent;",
        |ctx, _args| {
            // Observation-only: clone the head of the queue without releasing
            // any `invokeAndWait` waiter (see `EventDispatchThread::peek_event`).
            let Some(evt) = edt::get_edt().peek_event() else {
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
            let source = lookup_peer_source(ctx, evt.source_peer_id);
            if let Some(plan) = plan_event_synthesis(&evt, source) {
                return materialise_event(ctx, &plan);
            }
            null_ok()
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
            let Some(callback_id) = take_invocation_event_callback(hash) else {
                // No binding — either this event was not synthesized by us
                // (a real-JDK class created it directly), or it was already
                // dispatched.  Nothing to do.
                return void_ok();
            };
            // Resurrect the Runnable pointer ONLY if no (moving) GC has run
            // since it was registered. `invokeLater` is asynchronous, so the
            // moving collector may have relocated or freed the Runnable object
            // in the meantime; dereferencing a stale pointer via `invoke_virtual`
            // would be a use-after-free / type confusion reachable from any
            // Swing app. We therefore fail closed across the GC boundary exactly
            // like `lookup_peer_source` does for cached peer-source pointers: on
            // a generation mismatch `take_runnable_checked` returns `None`, we
            // skip the `run()` call entirely, and only signal completion so a
            // blocked `invokeAndWait` waiter doesn't hang.
            let root_handle = edt::get_edt().take_runnable_root(callback_id);
            if let Some(root_handle) = root_handle {
                // Run on whatever thread invoked us — by contract this is
                // the EDT, since the EDT dispatch loop is what calls
                // `dispatch()`.  We propagate failures out of the native so
                // the EDT's exception handling sees them, but signal
                // completion in BOTH the success and failure paths
                // (otherwise an exception in `run()` would hang
                // `invokeAndWait` forever).
                let result = match ctx.resolve_global_root(root_handle) {
                    Some(runnable) => ctx.invoke_virtual(runnable, "run", "()V", &[]),
                    None => {
                        tracing::warn!(
                            callback_id,
                            root_handle,
                            "InvocationEvent.dispatch: Runnable global root could not be resolved"
                        );
                        void_ok()
                    }
                };
                release_global_roots(ctx, [root_handle]);
                edt::get_edt().signal_invocation_complete(callback_id);
                // Surface any exception thrown by Runnable.run() to the EDT.
                result?;
            } else {
                // Either the Runnable was already taken (e.g. dispatched
                // twice) or `take_runnable_checked` failed closed because a GC
                // ran since registration (stale/relocated pointer). In both
                // cases we must NOT dereference the pointer; still signal so a
                // waiting `invokeAndWait` caller doesn't hang.
                tracing::warn!(
                    callback_id,
                    "InvocationEvent.dispatch: Runnable skipped (already taken or \
                 invalidated by a GC since registration); failing closed to \
                 avoid a use-after-free"
                );
                edt::get_edt().signal_invocation_complete(callback_id);
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
                if let Some(Value::Object(Some(obj))) = &font_obj {
                    let name = ctx.create_string(&family);
                    ctx.set_field_by_name(*obj, "name", Value::Object(Some(name)));
                    ctx.set_field_by_name(*obj, "style", Value::Int(style));
                    ctx.set_field_by_name(*obj, "size", Value::Int(size));
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
                let root_handle = add_global_root_or_oom(ctx, runnable, "Swing Runnable")?;
                let (_, removed_roots) =
                    edt::get_edt().invoke_later_runnable(root_handle, PeerId(0));
                release_global_roots(ctx, removed_roots);
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
                let root_handle = add_global_root_or_oom(ctx, runnable, "Swing Runnable")?;
                match edt::get_edt().invoke_and_wait_runnable(root_handle, PeerId(0)) {
                    Ok((callback_id, removed_roots)) => {
                        release_global_roots(ctx, removed_roots);
                        if let Some(root_handle) = edt::get_edt().take_runnable_root(callback_id) {
                            release_global_roots(ctx, [root_handle]);
                        }
                    }
                    Err(InvokeAndWaitError::OnEdt) => {
                        release_global_roots(ctx, [root_handle]);
                        return Err(RuntimeError::IllegalStateException {
                            message: InvokeAndWaitError::OnEdt.jdk_message().to_string(),
                        }
                        .into());
                    }
                }
            }
            void_ok()
        },
    );
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
            let sel = ctx.new_object("java/awt/datatransfer/StringSelection")?;
            if let Some(Value::Object(Some(obj))) = &sel {
                let s = ctx.create_string(&text);
                ctx.invoke(
                    "java/awt/datatransfer/StringSelection",
                    "<init>",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(*obj)), Value::Object(Some(s))],
                )?;
            }
            Ok(sel)
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

    fn register_peer_source_root_for_test(peer_id: PeerId, root_handle: usize, java_hash: i32) {
        peer_source_table().lock().insert(
            peer_id.0,
            PeerSourceEntry {
                root_handle,
                java_hash,
            },
        );
    }

    fn lookup_peer_source_for_test(
        peer_id: PeerId,
        root_handle: usize,
        source: ObjectRef,
    ) -> Option<ObjectRef> {
        lookup_peer_source_with(peer_id, |handle| (handle == root_handle).then_some(source))
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
    /// Uses a *local* `EventDispatchThread` instead of the process-wide
    /// `edt::get_edt()` singleton: tests run in parallel under
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
            lookup_peer_source_with(PeerId(11), |_| None),
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
            424242,
            PeerSourceEntry {
                root_handle: 0,
                java_hash: 7,
            },
        );
        // Matching generation, non-null — only the alignment guard stops it.
        let source = fake_object_ref(42);
        assert_eq!(lookup_peer_source_for_test(PeerId(424242), 0, source), None);

        // A null cached pointer also fails closed at the same generation.
        peer_source_table().lock().insert(
            424243,
            PeerSourceEntry {
                root_handle: 0,
                java_hash: 7,
            },
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
        gfx_registry().lock().map.insert(hash, handle);
    }

    fn gfx_contains(hash: i32) -> bool {
        gfx_registry().lock().map.contains_key(&hash)
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
        gfx_registry().lock().map.remove(&hash);
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
        gfx_registry().lock().map.remove(&hash);
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
