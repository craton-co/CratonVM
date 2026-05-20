//! AWT/Swing/Java2D native method registration.
//!
//! Registers native callbacks for `java/awt/*`, `javax/swing/*`, and
//! `sun/java2d/*` classes. Each callback maps Java-side API calls to
//! the Rust peer/renderer/EDT infrastructure in this crate.

use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::{MethodCallResult, RuntimeError};
use rustjvm_types::{ObjectRef, Value};

use crate::edt;
use crate::edt::InvokeAndWaitError;
use crate::event::PeerId;
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
// `edt::get_edt().take_runnable(callback_id)`.
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
    invocation_event_callbacks().lock().insert(event_hash, callback_id);
}

fn take_invocation_event_callback(event_hash: i32) -> Option<u64> {
    invocation_event_callbacks().lock().remove(event_hash)
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
        Self { map: FxHashMap::default() }
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
    }));
    gfx_registry().lock().map.insert(hash, handle);
}

/// Flush Graphics2D pixel data back to its target (BufferedImage or peer)
/// and remove the registry entry. Called from `dispose()`.
fn dispose_gfx(ctx: &dyn NativeContext, receiver: ObjectRef) {
    let hash = ctx.identity_hash_code(receiver);
    let handle = { gfx_registry().lock().map.remove(&hash) };
    let Some(handle) = handle else { return; };
    let mut entry = handle.lock();
    entry.state.dispose();
    match entry.target {
        GfxTarget::Image(image_id) => {
            let w = entry.state.width();
            let h = entry.state.height();
            let pixels = entry.state.pixels().to_vec();
            let mut reg = image::image_registry();
            if let Some(img) = reg.get_mut(image_id) {
                if img.width() == w && img.height() == h {
                    img.get_data_buffer_mut().copy_from_slice(&pixels);
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

/// Read an int[] array into a Vec<i32>.
fn read_int_array(ctx: &dyn NativeContext, obj: ObjectRef) -> Vec<i32> {
    let len = ctx.array_length(obj);
    let mut out = Vec::with_capacity(len);
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

pub fn register_all(registry: &mut NativeMethodRegistry) {
    register_toolkit_natives(registry);
    register_component_natives(registry);
    register_frame_natives(registry);
    register_graphics_natives(registry);
    register_image_natives(registry);
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
    let mut reg = peer::peer_registry().lock();
    if let Some(id) = reg.peer_for_java(hash) {
        return id;
    }
    let id = reg.create_peer(ctype);
    reg.register_java_mapping(hash, id);
    id
}

// ---------------------------------------------------------------------------
// Toolkit natives
// ---------------------------------------------------------------------------

fn register_toolkit_natives(registry: &mut NativeMethodRegistry) {
    registry.register(
        "java/awt/Toolkit", "getDefaultToolkit", "()Ljava/awt/Toolkit;",
        |ctx, _args| ctx.new_object("java/awt/Toolkit"),
    );
    registry.register(
        "java/awt/Toolkit", "getScreenSize", "()Ljava/awt/Dimension;",
        |ctx, _args| {
            let dim = ctx.new_object("java/awt/Dimension")?;
            if let Some(Value::Object(Some(obj))) = &dim {
                ctx.set_field_by_name(*obj, "width", Value::Int(1920));
                ctx.set_field_by_name(*obj, "height", Value::Int(1080));
            }
            Ok(dim)
        },
    );
    registry.register("java/awt/Toolkit", "getScreenResolution", "()I", |_ctx, _args| int_ok(96));
    registry.register("java/awt/Toolkit", "sync", "()V", |_ctx, _args| void_ok());
    registry.register("java/awt/Toolkit", "beep", "()V", |_ctx, _args| void_ok());
    registry.register("sun/awt/SunToolkit", "getDefaultToolkit", "()Ljava/awt/Toolkit;",
        |ctx, _args| ctx.new_object("java/awt/Toolkit"));
}

// ---------------------------------------------------------------------------
// Component natives
// ---------------------------------------------------------------------------

fn register_component_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/Component", "setBounds", "(IIII)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let (x, y) = (get_int(args, 1), get_int(args, 2));
            let (w, h) = (get_int(args, 3).max(0) as u32, get_int(args, 4).max(0) as u32);
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get_mut(pid) {
                    peer.x = x; peer.y = y; peer.width = w; peer.height = h;
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
                if let Some(peer) = reg.get_mut(pid) { peer.visible = visible; }
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
                if let Some(peer) = reg.get_mut(pid) { peer.enabled = enabled; }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "setBackground", "(Ljava/awt/Color;)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let color_val = get_obj(args, 1).map(|c| {
                match ctx.get_field_by_name(c, "value") { Value::Int(v) => v as u32, _ => 0xFFFFFFFF }
            }).unwrap_or(0xFFFFFFFF);
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get_mut(pid) { peer.background = color_val; }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "setForeground", "(Ljava/awt/Color;)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let color_val = get_obj(args, 1).map(|c| {
                match ctx.get_field_by_name(c, "value") { Value::Int(v) => v as u32, _ => 0xFF000000 }
            }).unwrap_or(0xFF000000);
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get_mut(pid) { peer.foreground = color_val; }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "repaint", "()V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get(pid) {
                    swing::swing_state().lock().mark_dirty(pid, 0, 0, peer.width, peer.height);
                }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "getGraphics", "()Ljava/awt/Graphics;", |ctx, args| {
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
    });

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

    registry.register("java/awt/Component", "setFont", "(Ljava/awt/Font;)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Some(font_obj) = get_obj(args, 1) {
                let family = ctx.read_string(font_obj).unwrap_or_else(|| "Dialog".to_string());
                let style = match ctx.get_field_by_name(font_obj, "style") { Value::Int(v) => v, _ => 0 };
                let size = match ctx.get_field_by_name(font_obj, "size") { Value::Int(v) => v, _ => 12 };
                let hash = ctx.identity_hash_code(this);
                let mut reg = peer::peer_registry().lock();
                if let Some(pid) = reg.peer_for_java(hash) {
                    if let Some(peer) = reg.get_mut(pid) {
                        peer.font_family = family; peer.font_style = style; peer.font_size = size;
                    }
                }
            }
        }
        void_ok()
    });

    registry.register("java/awt/Component", "getWidth", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get(pid) { return int_ok(peer.width as i32); }
            }
        }
        int_ok(0)
    });

    registry.register("java/awt/Component", "getHeight", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let hash = ctx.identity_hash_code(this);
            let reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get(pid) { return int_ok(peer.height as i32); }
            }
        }
        int_ok(0)
    });
}

// ---------------------------------------------------------------------------
// Frame natives
// ---------------------------------------------------------------------------

fn register_frame_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/Frame", "setTitle", "(Ljava/lang/String;)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let title = read_string(ctx, args, 1).unwrap_or_default();
            let pid = ensure_peer(ctx, this, ComponentType::Frame);
            let mut reg = peer::peer_registry().lock();
            if let Some(peer) = reg.get_mut(pid) { peer.title = title; }
        }
        void_ok()
    });
    registry.register("java/awt/Frame", "setResizable", "(Z)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let resizable = get_bool(args, 1);
            let hash = ctx.identity_hash_code(this);
            let mut reg = peer::peer_registry().lock();
            if let Some(pid) = reg.peer_for_java(hash) {
                if let Some(peer) = reg.get_mut(pid) { peer.resizable = resizable; }
            }
        }
        void_ok()
    });
    registry.register("java/awt/Frame", "toFront", "()V", |_ctx, _args| void_ok());
    registry.register("java/awt/Frame", "toBack", "()V", |_ctx, _args| void_ok());
    registry.register("java/awt/Frame", "setIconImage", "(Ljava/awt/Image;)V", |_ctx, _args| void_ok());
    registry.register("java/awt/Frame", "setMenuBar", "(Ljava/awt/MenuBar;)V", |_ctx, _args| void_ok());
}

// ---------------------------------------------------------------------------
// Graphics2D natives
// ---------------------------------------------------------------------------

fn register_graphics_natives(registry: &mut NativeMethodRegistry) {
    for class in &["java/awt/Graphics2D", "sun/java2d/SunGraphics2D", "java/awt/Graphics"] {
        // ── Line / rect / oval / arc ──────────────────────────────
        registry.register(class, "drawLine", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x1, y1, x2, y2) = (get_int(args, 1), get_int(args, 2),
                                         get_int(args, 3), get_int(args, 4));
                with_gfx(ctx, this, |g| g.draw_line(x1, y1, x2, y2));
            }
            void_ok()
        });
        registry.register(class, "drawRect", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                with_gfx(ctx, this, |g| g.draw_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "fillRect", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                with_gfx(ctx, this, |g| g.fill_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "drawOval", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                with_gfx(ctx, this, |g| g.draw_oval(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "fillOval", "(IIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                with_gfx(ctx, this, |g| g.fill_oval(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "drawArc", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                let (start, extent) = (get_int(args, 5), get_int(args, 6));
                with_gfx(ctx, this, |g| g.draw_arc(x, y, w, h, start, extent));
            }
            void_ok()
        });
        registry.register(class, "fillArc", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y, w, h) = (get_int(args, 1), get_int(args, 2),
                                     get_int(args, 3), get_int(args, 4));
                let (start, extent) = (get_int(args, 5), get_int(args, 6));
                with_gfx(ctx, this, |g| g.fill_arc(x, y, w, h, start, extent));
            }
            void_ok()
        });

        // ── Text ──────────────────────────────────────────────────
        registry.register(class, "drawString", "(Ljava/lang/String;II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let text = read_string(ctx, args, 1).unwrap_or_default();
                let (x, y) = (get_int(args, 2), get_int(args, 3));
                with_gfx(ctx, this, |g| g.draw_string(&text, x, y));
            }
            void_ok()
        });

        // ── Polygons / polylines ──────────────────────────────────
        registry.register(class, "drawPolygon", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let xs = get_obj(args, 1).map(|a| read_int_array(ctx, a)).unwrap_or_default();
                let ys = get_obj(args, 2).map(|a| read_int_array(ctx, a)).unwrap_or_default();
                let n = get_int(args, 3).max(0) as usize;
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.draw_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "fillPolygon", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let xs = get_obj(args, 1).map(|a| read_int_array(ctx, a)).unwrap_or_default();
                let ys = get_obj(args, 2).map(|a| read_int_array(ctx, a)).unwrap_or_default();
                let n = get_int(args, 3).max(0) as usize;
                let n = n.min(xs.len()).min(ys.len());
                with_gfx(ctx, this, |g| g.fill_polygon(&xs[..n], &ys[..n]));
            }
            void_ok()
        });
        registry.register(class, "drawPolyline", "([I[II)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let xs = get_obj(args, 1).map(|a| read_int_array(ctx, a)).unwrap_or_default();
                let ys = get_obj(args, 2).map(|a| read_int_array(ctx, a)).unwrap_or_default();
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
        registry.register(class, "drawImage", "(Ljava/awt/Image;IILjava/awt/image/ImageObserver;)Z",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    if let Some(img) = get_obj(args, 1) {
                        let (x, y) = (get_int(args, 2), get_int(args, 3));
                        if let Value::Long(id) = ctx.get_field_by_name(img, "imageId") {
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
                            if let Some(bimg) = reg.get(ImageId(id as u64)) {
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
            });

        // ── Color / paint ─────────────────────────────────────────
        registry.register(class, "setColor", "(Ljava/awt/Color;)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let argb = get_obj(args, 1).map(|c| {
                    match ctx.get_field_by_name(c, "value") {
                        Value::Int(v) => v as u32,
                        _ => 0xFF_000000,
                    }
                }).unwrap_or(0xFF_000000);
                let a = ((argb >> 24) & 0xFF) as u8;
                let r = ((argb >> 16) & 0xFF) as u8;
                let g = ((argb >> 8) & 0xFF) as u8;
                let b = (argb & 0xFF) as u8;
                with_gfx(ctx, this, |gs| gs.set_color(r, g, b, a));
            }
            void_ok()
        });

        // ── Font ──────────────────────────────────────────────────
        registry.register(class, "setFont", "(Ljava/awt/Font;)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                if let Some(font) = get_obj(args, 1) {
                    let family = ctx.read_string(font).unwrap_or_else(|| "Dialog".to_string());
                    let style = match ctx.get_field_by_name(font, "style") {
                        Value::Int(v) => v, _ => 0,
                    };
                    let size = match ctx.get_field_by_name(font, "size") {
                        Value::Int(v) => v, _ => 12,
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
                let (w, h) = (get_int(args, 3).max(0) as u32, get_int(args, 4).max(0) as u32);
                with_gfx(ctx, this, |g| g.set_clip_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "getClipBounds", "()Ljava/awt/Rectangle;", |ctx, args| {
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
        });

        // ── Transform ─────────────────────────────────────────────
        registry.register(class, "setTransform", "(Ljava/awt/geom/AffineTransform;)V", |ctx, args| {
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
                        m00: read("m00"), m01: read("m01"), m02: read("m02"),
                        m10: read("m10"), m11: read("m11"), m12: read("m12"),
                    };
                    with_gfx(ctx, this, |g| g.set_transform(xform));
                }
            }
            void_ok()
        });
        registry.register(class, "getTransform", "()Ljava/awt/geom/AffineTransform;", |ctx, args| {
            let xform = if let Some(this) = get_obj(args, 0) {
                let hash = ctx.identity_hash_code(this);
                let handle = gfx_registry().lock().map.get(&hash).cloned();
                handle.map(|h| h.lock().state.get_transform())
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
        });
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
        registry.register(class, "setRenderingHint",
            "(Ljava/awt/RenderingHints$Key;Ljava/lang/Object;)V", |ctx, args| {
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
                    1 => Some(K::Antialiasing),       // KEY_ANTIALIASING
                    9 => Some(K::TextAntialiasing),   // KEY_TEXT_ANTIALIASING
                    5 => Some(K::Interpolation),      // KEY_INTERPOLATION
                    _ => None,
                };
                let value = match val_id {
                    1 | 9  => Some(V::On),                          // VALUE_*_ON
                    2 | 10 => Some(V::Off),                         // VALUE_*_OFF
                    195    => Some(V::BilinearInterpolation),       // VALUE_INTERPOLATION_BILINEAR
                    196    => Some(V::NearestNeighborInterpolation),// VALUE_INTERPOLATION_NEAREST_NEIGHBOR
                    197    => Some(V::BicubicInterpolation),         // VALUE_INTERPOLATION_BICUBIC
                    0 | -1 => None,
                    _      => Some(V::Default),
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
        });

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
                handle.map(|h| h.lock().target).unwrap_or(GfxTarget::Detached)
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
                let (w, h) = (get_int(args, 3).max(0) as u32, get_int(args, 4).max(0) as u32);
                with_gfx(ctx, this, |g| g.clear_rect(x, y, w, h));
            }
            void_ok()
        });
        registry.register(class, "copyArea", "(IIIIII)V", |ctx, args| {
            if let Some(this) = get_obj(args, 0) {
                let (x, y) = (get_int(args, 1), get_int(args, 2));
                let (w, h) = (get_int(args, 3).max(0) as u32, get_int(args, 4).max(0) as u32);
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
        }
        void_ok()
    });
    registry.register("java/awt/image/BufferedImage", "getWidth", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(w) = ctx.get_field_by_name(this, "width") { return int_ok(w); }
        }
        int_ok(0)
    });
    registry.register("java/awt/image/BufferedImage", "getHeight", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(h) = ctx.get_field_by_name(this, "height") { return int_ok(h); }
        }
        int_ok(0)
    });
    registry.register("java/awt/image/BufferedImage", "getRGB", "(II)I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            // Java `int` coordinates: validate the *signed* values before any
            // `as u32` cast, so a negative coordinate is rejected rather than
            // wrapping to a huge index. Mirrors `BufferedImage.getRGB`'s
            // documented `ArrayIndexOutOfBoundsException` contract.
            let (x, y) = (get_int(args, 1), get_int(args, 2));
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let reg = image::image_registry();
                if let Some(img) = reg.get(image::ImageId(id as u64)) {
                    let (w, h) = (img.width() as i32, img.height() as i32);
                    if x < 0 || y < 0 || x >= w || y >= h {
                        // Match the JDK: the index reported is the offending
                        // linear pixel index `y * width + x`.
                        let index = (y as i64) * (w as i64) + (x as i64);
                        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
                            index: index as i32,
                        }
                        .into());
                    }
                    return int_ok(img.get_rgb(x as u32, y as u32) as i32);
                }
            }
        }
        int_ok(0)
    });
    registry.register("java/awt/image/BufferedImage", "setRGB", "(III)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            // Validate signed coordinates before casting (see `getRGB`).
            let (x, y, argb) = (get_int(args, 1), get_int(args, 2), get_int(args, 3) as u32);
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let mut reg = image::image_registry();
                if let Some(img) = reg.get_mut(image::ImageId(id as u64)) {
                    let (w, h) = (img.width() as i32, img.height() as i32);
                    if x < 0 || y < 0 || x >= w || y >= h {
                        let index = (y as i64) * (w as i64) + (x as i64);
                        return Err(RuntimeError::ArrayIndexOutOfBoundsException {
                            index: index as i32,
                        }
                        .into());
                    }
                    img.set_rgb(x as u32, y as u32, argb);
                }
            }
        }
        void_ok()
    });
    registry.register("java/awt/image/BufferedImage", "getType", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let reg = image::image_registry();
                if let Some(img) = reg.get(image::ImageId(id as u64)) { return int_ok(img.image_type() as i32); }
            }
        }
        int_ok(0)
    });
    registry.register("java/awt/image/BufferedImage", "createGraphics", "()Ljava/awt/Graphics2D;", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let image_id = ImageId(id as u64);
                let gfx = ctx.new_object("java/awt/Graphics2D")?;
                if let Some(Value::Object(Some(gfx_obj))) = &gfx {
                    register_gfx_for_image(ctx, *gfx_obj, image_id);
                }
                return Ok(gfx);
            }
        }
        null_ok()
    });
    registry.register("java/awt/image/BufferedImage", "flush", "()V", |_ctx, _args| void_ok());
}

// ---------------------------------------------------------------------------
// EventQueue natives
// ---------------------------------------------------------------------------

fn register_event_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/EventQueue", "isDispatchThread", "()Z",
        |_ctx, _args| bool_ok(edt::is_edt()));
    registry.register("java/awt/EventQueue", "invokeLater", "(Ljava/lang/Runnable;)V", |_ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            // Allocate a fresh callback id, register the Runnable, post
            // an InvocationEvent.  The EDT will dequeue it via
            // `getNextEvent` and dispatch it through
            // `InvocationEvent.dispatch()V` (registered below).
            edt::get_edt().invoke_later_runnable(runnable, PeerId(0));
        }
        void_ok()
    });
    registry.register("java/awt/EventQueue", "invokeAndWait",
        "(Ljava/lang/Runnable;)V", |_ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            // Block until the Runnable's `dispatch()V` native finishes
            // calling `run()`. Round-9 misc fix: when called from the
            // EDT itself this previously panicked across the JNI
            // boundary (UB on most VMs). Convert the structured error
            // into the JDK-spec'd `IllegalStateException` so the Java
            // caller observes the documented behaviour instead.
            if let Err(InvokeAndWaitError::OnEdt) =
                edt::get_edt().invoke_and_wait_runnable(runnable, PeerId(0))
            {
                return Err(RuntimeError::IllegalStateException {
                    message: InvokeAndWaitError::OnEdt.jdk_message().to_string(),
                }
                .into());
            }
        }
        void_ok()
    });
    registry.register("java/awt/EventQueue", "postEvent", "(Ljava/awt/AWTEvent;)V", |_ctx, _args| void_ok());
    registry.register("java/awt/EventQueue", "getNextEvent", "()Ljava/awt/AWTEvent;", |ctx, _args| {
        // Pull the next event off the EDT queue.  For invocation events
        // we have to synthesize a Java `InvocationEvent` whose
        // `dispatch()V` will be called by the EDT dispatch loop —
        // otherwise the Runnable would be silently discarded.
        let Some(evt) = edt::get_edt().poll_event() else {
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
        // TODO: synthesize Java event objects for the non-invocation
        // event kinds (Mouse / Key / Window / ...).  The legacy code
        // returned null here for every event, so callers that drive
        // dispatch through this method will still observe missing
        // events — but at least invocation events now flow correctly
        // and SwingUtilities.invokeLater is no longer a no-op.
        null_ok()
    });
    registry.register("java/awt/EventQueue", "peekEvent", "()Ljava/awt/AWTEvent;", |_ctx, _args| null_ok());

    // -- InvocationEvent.dispatch -------------------------------------------
    //
    // The EDT's dispatch loop calls `AWTEvent.dispatch()` on whatever
    // `EventQueue.getNextEvent` returned.  For InvocationEvents this
    // native looks the Runnable up by the event object's identity hash,
    // calls `Runnable.run()` virtually on the EDT, then signals any
    // `invokeAndWait` waiter and drops both registry entries.
    registry.register("java/awt/event/InvocationEvent", "dispatch", "()V",
        |ctx, args| {
        let Some(this) = get_obj(args, 0) else { return void_ok() };
        let hash = ctx.identity_hash_code(this);
        let Some(callback_id) = take_invocation_event_callback(hash) else {
            // No binding — either this event was not synthesized by us
            // (a real-JDK class created it directly), or it was already
            // dispatched.  Nothing to do.
            return void_ok();
        };
        let runnable = edt::get_edt().take_runnable(callback_id);
        if let Some(runnable) = runnable {
            // Run on whatever thread invoked us — by contract this is
            // the EDT, since the EDT dispatch loop is what calls
            // `dispatch()`.  We propagate failures out of the native so
            // the EDT's exception handling sees them, but signal
            // completion in BOTH the success and failure paths
            // (otherwise an exception in `run()` would hang
            // `invokeAndWait` forever).
            let result = ctx.invoke_virtual(runnable, "run", "()V", &[]);
            edt::get_edt().signal_invocation_complete(callback_id);
            // Surface any exception thrown by Runnable.run() to the EDT.
            result?;
        } else {
            // Runnable already taken (e.g. dispatched twice).  Still
            // signal so a waiter doesn't hang.
            edt::get_edt().signal_invocation_complete(callback_id);
        }
        void_ok()
    });
}

// ---------------------------------------------------------------------------
// Font natives
// ---------------------------------------------------------------------------

fn register_font_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/Font", "getFamily", "()Ljava/lang/String;", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let name = ctx.read_string(this).unwrap_or_else(|| "Dialog".to_string());
            let s = ctx.create_string(&name);
            return obj_ok(s);
        }
        null_ok()
    });
    registry.register("java/awt/Font", "getSize", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(s) = ctx.get_field_by_name(this, "size") { return int_ok(s); }
        }
        int_ok(12)
    });
    registry.register("java/awt/Font", "getStyle", "()I", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            if let Value::Int(s) = ctx.get_field_by_name(this, "style") { return int_ok(s); }
        }
        int_ok(0)
    });
    // FontMetrics receivers carry their (Font.size) via the `font` field on
    // FontMetrics. `args[0]` is the FontMetrics receiver (an `ObjectRef`),
    // **not** an int — `get_int(args, 0)` would always return 0 and the
    // `.max(12)` floor would mask the bug. Resolve the actual font size by
    // reading `this.font.size`.
    fn font_metrics_size(ctx: &dyn NativeContext, args: &[Value]) -> f32 {
        let this = match get_obj(args, 0) { Some(o) => o, None => return 12.0 };
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
    registry.register("java/awt/FontMetrics", "stringWidth", "(Ljava/lang/String;)I", |ctx, args| {
        let text = read_string(ctx, args, 1).unwrap_or_default();
        let size = font_metrics_size(ctx, args);
        int_ok((text.len() as f32 * size * 0.55).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "charWidth", "(C)I", |ctx, args| {
        int_ok((font_metrics_size(ctx, args) * 0.55).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getMaxAdvance", "()I", |ctx, args| {
        int_ok(font_metrics_size(ctx, args).round() as i32)
    });
}

// ---------------------------------------------------------------------------
// Swing natives
// ---------------------------------------------------------------------------

fn register_swing_natives(registry: &mut NativeMethodRegistry) {
    registry.register("javax/swing/UIManager", "getSystemLookAndFeelClassName", "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("javax.swing.plaf.metal.MetalLookAndFeel")));
    registry.register("javax/swing/UIManager", "getCrossPlatformLookAndFeelClassName", "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("javax.swing.plaf.metal.MetalLookAndFeel")));
    registry.register("javax/swing/UIManager", "setLookAndFeel", "(Ljava/lang/String;)V", |_ctx, _args| void_ok());

    registry.register("javax/swing/UIManager", "getColor", "(Ljava/lang/Object;)Ljava/awt/Color;", |ctx, args| {
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
    });
    registry.register("javax/swing/UIManager", "getFont", "(Ljava/lang/Object;)Ljava/awt/Font;", |ctx, args| {
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
    });
    registry.register("javax/swing/UIManager", "getInsets", "(Ljava/lang/Object;)Ljava/awt/Insets;", |ctx, args| {
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
    });
    registry.register("javax/swing/UIManager", "getInt", "(Ljava/lang/Object;)I", |ctx, args| {
        let key = read_string(ctx, args, 0).unwrap_or_default();
        let state = swing::swing_state().lock();
        let v = state.defaults.get_integer(&key).unwrap_or(0);
        int_ok(v)
    });
    registry.register("javax/swing/UIManager", "getBoolean", "(Ljava/lang/Object;)Z", |ctx, args| {
        let key = read_string(ctx, args, 0).unwrap_or_default();
        let state = swing::swing_state().lock();
        let v = state.defaults.get_boolean(&key).unwrap_or(false);
        bool_ok(v)
    });

    registry.register("javax/swing/JFileChooser", "showOpenDialog", "(Ljava/awt/Component;)I", |_ctx, _args| int_ok(1));
    registry.register("javax/swing/JFileChooser", "showSaveDialog", "(Ljava/awt/Component;)I", |_ctx, _args| int_ok(1));

    registry.register("javax/swing/JOptionPane", "showMessageDialog",
        "(Ljava/awt/Component;Ljava/lang/Object;Ljava/lang/String;I)V", |ctx, args| {
        let msg = read_string(ctx, args, 1).unwrap_or_default();
        let title = read_string(ctx, args, 2).unwrap_or_default();
        tracing::info!("[JOptionPane] {title}: {msg}");
        void_ok()
    });
    registry.register("javax/swing/JOptionPane", "showConfirmDialog",
        "(Ljava/awt/Component;Ljava/lang/Object;Ljava/lang/String;I)I", |_ctx, _args| int_ok(0));
    registry.register("javax/swing/JOptionPane", "showInputDialog",
        "(Ljava/awt/Component;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, _args| obj_ok(ctx.create_string("")));

    registry.register("javax/swing/SwingUtilities", "isEventDispatchThread", "()Z",
        |_ctx, _args| bool_ok(edt::is_edt()));
    registry.register("javax/swing/SwingUtilities", "invokeLater", "(Ljava/lang/Runnable;)V", |_ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            // Same plumbing as `EventQueue.invokeLater` — register the
            // Runnable so `InvocationEvent.dispatch()V` can find it,
            // then post.
            edt::get_edt().invoke_later_runnable(runnable, PeerId(0));
        }
        void_ok()
    });
    registry.register("javax/swing/SwingUtilities", "invokeAndWait",
        "(Ljava/lang/Runnable;)V", |_ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            // Round-9 misc fix: surface the EDT-from-EDT case as an
            // `IllegalStateException` on the Java thread instead of
            // panicking across the JNI boundary. The wording matches
            // the JDK exactly so existing exception filters keep
            // working.
            if let Err(InvokeAndWaitError::OnEdt) =
                edt::get_edt().invoke_and_wait_runnable(runnable, PeerId(0))
            {
                return Err(RuntimeError::IllegalStateException {
                    message: InvokeAndWaitError::OnEdt.jdk_message().to_string(),
                }
                .into());
            }
        }
        void_ok()
    });
}

// ---------------------------------------------------------------------------
// Clipboard natives
// ---------------------------------------------------------------------------

fn register_clipboard_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/datatransfer/Clipboard", "getName", "()Ljava/lang/String;",
        |ctx, _args| obj_ok(ctx.create_string("System")));
    registry.register("java/awt/datatransfer/Clipboard", "getContents",
        "(Ljava/lang/Object;)Ljava/awt/datatransfer/Transferable;", |_ctx, _args| null_ok());
    registry.register("java/awt/datatransfer/Clipboard", "setContents",
        "(Ljava/awt/datatransfer/Transferable;Ljava/awt/datatransfer/ClipboardOwner;)V", |_ctx, _args| void_ok());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_count() {
        let mut registry = NativeMethodRegistry::new();
        register_all(&mut registry);
        let count = registry.len();
        assert!(count >= 50, "Expected >= 50 AWT natives, got {count}");
    }
}
