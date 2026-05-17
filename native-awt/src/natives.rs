//! AWT/Swing/Java2D native method registration.
//!
//! Registers native callbacks for `java/awt/*`, `javax/swing/*`, and
//! `sun/java2d/*` classes. Each callback maps Java-side API calls to
//! the Rust peer/renderer/EDT infrastructure in this crate.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;
use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ObjectRef, Value};

use crate::edt;
use crate::event::PeerId;
use crate::graphics2d::Graphics2DState;
use crate::image::{self, ImageId, ImageType};
use crate::peer::{self, ComponentType};
use crate::swing;

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

struct GfxRegistry {
    map: HashMap<i32, GfxEntry>,
}

impl GfxRegistry {
    fn new() -> Self {
        Self { map: HashMap::new() }
    }
}

fn gfx_registry() -> &'static Mutex<GfxRegistry> {
    static INSTANCE: OnceLock<Mutex<GfxRegistry>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(GfxRegistry::new()))
}

/// Ensure a `Graphics2DState` exists for the given Java Graphics2D receiver,
/// then run `f` against it. Lazily creates a detached context if no
/// associated target has been registered, so misbehaving callers still get
/// drawing semantics (writing into a throwaway buffer) instead of panics.
fn with_gfx<F, R>(ctx: &dyn NativeContext, receiver: ObjectRef, f: F) -> R
where
    F: FnOnce(&mut Graphics2DState) -> R,
    R: Default,
{
    let hash = ctx.identity_hash_code(receiver);
    let mut reg = gfx_registry().lock();
    if !reg.map.contains_key(&hash) {
        // Lazy fallback: caller invoked a draw method on a Graphics2D that
        // we never saw `create`/`getGraphics` for.  Allocate a default-sized
        // detached buffer so the call doesn't panic.
        reg.map.insert(
            hash,
            GfxEntry {
                state: Graphics2DState::create(1, 1),
                target: GfxTarget::Detached,
            },
        );
    }
    let entry = reg.map.get_mut(&hash).expect("just inserted");
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
    let mut reg = gfx_registry().lock();
    reg.map.insert(
        hash,
        GfxEntry {
            state: Graphics2DState::create(w.max(1), h.max(1)),
            target: GfxTarget::Image(image_id),
        },
    );
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
    let mut reg = gfx_registry().lock();
    reg.map.insert(
        hash,
        GfxEntry {
            state: Graphics2DState::create(w, h),
            target: GfxTarget::Peer(peer_id),
        },
    );
}

/// Flush Graphics2D pixel data back to its target (BufferedImage or peer)
/// and remove the registry entry. Called from `dispose()`.
fn dispose_gfx(ctx: &dyn NativeContext, receiver: ObjectRef) {
    let hash = ctx.identity_hash_code(receiver);
    let entry = { gfx_registry().lock().map.remove(&hash) };
    let Some(mut entry) = entry else { return; };
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
        registry.register(class, "drawImage", "(Ljava/awt/Image;IILjava/awt/image/ImageObserver;)Z",
            |ctx, args| {
                if let Some(this) = get_obj(args, 0) {
                    if let Some(img) = get_obj(args, 1) {
                        let (x, y) = (get_int(args, 2), get_int(args, 3));
                        if let Value::Long(id) = ctx.get_field_by_name(img, "imageId") {
                            let (pixels, w, h) = {
                                let reg = image::image_registry();
                                match reg.get(ImageId(id as u64)) {
                                    Some(bimg) => (bimg.get_data_buffer().to_vec(),
                                                   bimg.width(), bimg.height()),
                                    None => (Vec::new(), 0, 0),
                                }
                            };
                            if !pixels.is_empty() {
                                with_gfx(ctx, this, |g| g.draw_image(&pixels, w, h, x, y));
                            }
                        }
                    }
                }
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
                let reg = gfx_registry().lock();
                reg.map.get(&hash).and_then(|e| e.state.get_clip_bounds())
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
                let reg = gfx_registry().lock();
                reg.map.get(&hash).map(|e| e.state.get_transform())
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
                // Best-effort: identify by key/value identity hash.  We map
                // anything we can read to a sensible default.  Since the JDK
                // RenderingHints constants aren't fully modelled here we just
                // toggle antialiasing on whenever a key is set — preserves
                // existing behaviour for AA-enabled apps.
                let key_hash = get_obj(args, 1).map(|o| ctx.identity_hash_code(o)).unwrap_or(0);
                let val_hash = get_obj(args, 2).map(|o| ctx.identity_hash_code(o)).unwrap_or(0);
                let _ = (key_hash, val_hash);
                // TODO: RenderingHints key/value identity is not modelled —
                // applying Antialiasing=On unconditionally.  Real mapping
                // requires the JDK constant pool.
                with_gfx(ctx, this, |g| g.set_rendering_hint(K::Antialiasing, V::On));
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
                let reg = gfx_registry().lock();
                reg.map.get(&hash).map(|e| e.target).unwrap_or(GfxTarget::Detached)
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
                        gfx_registry().lock().map.insert(hash, GfxEntry {
                            state: Graphics2DState::create(1, 1),
                            target: GfxTarget::Detached,
                        });
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

fn register_image_natives(registry: &mut NativeMethodRegistry) {
    registry.register("java/awt/image/BufferedImage", "<init>", "(III)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let w = get_int(args, 1).max(1) as u32;
            let h = get_int(args, 2).max(1) as u32;
            let it = match get_int(args, 3) { 1 => ImageType::IntRgb, 3 => ImageType::IntArgbPre, _ => ImageType::IntArgb };
            let img_id = image::image_registry().create(w, h, it);
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
            let (x, y) = (get_int(args, 1) as u32, get_int(args, 2) as u32);
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let reg = image::image_registry();
                if let Some(img) = reg.get(image::ImageId(id as u64)) {
                    return int_ok(img.get_rgb(x, y) as i32);
                }
            }
        }
        int_ok(0)
    });
    registry.register("java/awt/image/BufferedImage", "setRGB", "(III)V", |ctx, args| {
        if let Some(this) = get_obj(args, 0) {
            let (x, y, argb) = (get_int(args, 1) as u32, get_int(args, 2) as u32, get_int(args, 3) as u32);
            if let Value::Long(id) = ctx.get_field_by_name(this, "imageId") {
                let mut reg = image::image_registry();
                if let Some(img) = reg.get_mut(image::ImageId(id as u64)) { img.set_rgb(x, y, argb); }
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
    registry.register("java/awt/EventQueue", "invokeLater", "(Ljava/lang/Runnable;)V", |ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            let callback_id = ctx.identity_hash_code(runnable) as u64;
            edt::get_edt().invoke_later(callback_id, PeerId(0));
        }
        void_ok()
    });
    registry.register("java/awt/EventQueue", "postEvent", "(Ljava/awt/AWTEvent;)V", |_ctx, _args| void_ok());
    registry.register("java/awt/EventQueue", "getNextEvent", "()Ljava/awt/AWTEvent;", |_ctx, _args| {
        let _evt = edt::get_edt().poll_event();
        null_ok()
    });
    registry.register("java/awt/EventQueue", "peekEvent", "()Ljava/awt/AWTEvent;", |_ctx, _args| null_ok());
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
    registry.register("java/awt/FontMetrics", "getAscent", "()I", |_ctx, args| {
        int_ok(((get_int(args, 0).max(12) as f32) * 0.8).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getDescent", "()I", |_ctx, args| {
        int_ok(((get_int(args, 0).max(12) as f32) * 0.2).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getLeading", "()I", |_ctx, args| {
        int_ok(((get_int(args, 0).max(12) as f32) * 0.05).round().max(1.0) as i32)
    });
    registry.register("java/awt/FontMetrics", "getHeight", "()I", |_ctx, args| {
        let s = get_int(args, 0).max(12) as f32;
        int_ok(((s * 0.8).round() + (s * 0.2).round() + (s * 0.05).round().max(1.0)) as i32)
    });
    registry.register("java/awt/FontMetrics", "stringWidth", "(Ljava/lang/String;)I", |ctx, args| {
        let text = read_string(ctx, args, 1).unwrap_or_default();
        let size = get_int(args, 0).max(12) as f32;
        int_ok((text.len() as f32 * size * 0.55).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "charWidth", "(C)I", |_ctx, args| {
        int_ok((get_int(args, 0).max(12) as f32 * 0.55).round() as i32)
    });
    registry.register("java/awt/FontMetrics", "getMaxAdvance", "()I", |_ctx, args| {
        int_ok(get_int(args, 0).max(12))
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
    registry.register("javax/swing/SwingUtilities", "invokeLater", "(Ljava/lang/Runnable;)V", |ctx, args| {
        if let Some(runnable) = get_obj(args, 0) {
            let callback_id = ctx.identity_hash_code(runnable) as u64;
            edt::get_edt().invoke_later(callback_id, PeerId(0));
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
