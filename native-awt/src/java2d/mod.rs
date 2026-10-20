// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The native half of the JDK's Java2D pipeline (`libawt`'s `sun.java2d`
//! loops and `BufImgSurfaceData`), in Rust.
//!
//! With these natives the genuine `java.awt.image.BufferedImage`,
//! `sun.java2d.SunGraphics2D`, `sun.java2d.pipe.*` and `sun.java2d.loops.*`
//! bytecode runs unmodified: Java owns the pixel arrays (`DataBuffer`s), and
//! the loops here read and write those arrays through a per-VM surface table
//! keyed by `SurfaceData.pData`. Arithmetic follows the `libawt` macros
//! (`AlphaMath.c`, `AlphaMacros.h`) so results match HotSpot byte for byte.
//!
//! Registered loops (everything else falls back to the JDK's own Java loops:
//! `GeneralRenderer`, `Blit.AnyBlit`, `ProcessPath`, ...):
//!
//! * `FillRect`, `FillSpans` (`AnyColor`, `SrcNoEa`, every supported layout)
//! * `MaskFill` (`AnyColor`, `AnyAlpha`, ...)
//! * `Blit` (`SrcNoEa` conversions and `AnyAlpha` compositing) and `MaskBlit`
//!   for every pair of supported layouts.
//!
//! Layouts: [`pixel::Fmt`]. Record: `docs/internal/jdk-only/SBR5-AWTJFR-*`.

pub mod fonts;
mod lcdtext;
mod pathbridge;
mod pgram;
pub mod pixel;
pub mod rows;
mod scale;
pub mod shape;
pub mod tt;

use std::collections::HashMap;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use pixel::{Fmt, ALL_FMTS};

const SURFACE_TYPE: &str = "sun/java2d/loops/SurfaceType";
const COMPOSITE_TYPE: &str = "sun/java2d/loops/CompositeType";
const PRIM_CTOR: &str =
    "(JLsun/java2d/loops/SurfaceType;Lsun/java2d/loops/CompositeType;Lsun/java2d/loops/SurfaceType;)V";

// ---------------------------------------------------------------------------
// Surface table
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SurfRow {
    /// Weak global root of the Java array (`int[]`, `byte[]`, `short[]`).
    root: usize,
    /// Byte offset of pixel (0, 0) in the array payload.
    off: usize,
    w: i32,
    h: i32,
    /// Bytes per scanline.
    scan: usize,
    fmt: Option<Fmt>,
    /// The `SurfaceType` description (diagnostics only).
    desc: String,
}

#[derive(Default)]
struct SurfTable {
    next: i64,
    rows: HashMap<i64, SurfRow>,
}

static SURFS: VmScoped<SurfTable> = VmScoped::new();

/// Drop one VM's surface rows (the VM is going away; its roots die with it).
pub fn forget_vm(vm: usize) {
    SURFS.forget(vm);
    SHAPES.forget(vm);
    crate::jpeg::glue::forget_vm(vm);
    fonts::forget_vm(vm);
}

struct Surf {
    arr: ObjectRef,
    row: SurfRow,
    fmt: Fmt,
}

fn internal(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::InternalError {
        message: msg.into(),
    }
    .into()
}

fn fld_int(ctx: &dyn NativeContext, o: ObjectRef, name: &str) -> i32 {
    match ctx.get_field_by_name(o, name) {
        Value::Int(v) => v,
        _ => 0,
    }
}

fn fld_obj(ctx: &dyn NativeContext, o: ObjectRef, name: &str) -> Option<ObjectRef> {
    match ctx.get_field_by_name(o, name) {
        Value::Object(Some(r)) => Some(r),
        _ => None,
    }
}

fn arg_obj(args: &[Value], i: usize) -> Option<ObjectRef> {
    match args.get(i) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_int(args: &[Value], i: usize) -> i32 {
    match args.get(i) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn arg_long(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Long(v)) => *v,
        _ => 0,
    }
}

fn void() -> MethodCallResult {
    Ok(None)
}

fn lookup_surf(ctx: &dyn NativeContext, sd: ObjectRef) -> Result<Surf, MethodCallFailed> {
    let id = match ctx.get_field_by_name(sd, "pData") {
        Value::Long(v) => v,
        _ => 0,
    };
    let vm = ctx.vm_identity();
    let row = SURFS
        .peek(vm, |t| t.rows.get(&id).cloned())
        .flatten()
        .ok_or_else(|| internal("Java2D surface has no native raster"))?;
    let fmt = row.fmt.ok_or_else(|| {
        internal(format!(
            "Java2D surface layout has no native loop: {:?}",
            row.desc
        ))
    })?;
    let arr = ctx
        .resolve_global_root(row.root)
        .ok_or_else(|| internal("Java2D surface raster was collected"))?;
    Ok(Surf { arr, row, fmt })
}

impl Surf {
    fn read(&self, ctx: &dyn NativeContext, x: i32, y: i32, n: usize, buf: &mut Vec<u8>) {
        let bpp = self.fmt.bpp();
        buf.clear();
        buf.resize(n * bpp, 0);
        let off = self.row.off + y as usize * self.row.scan + x as usize * bpp;
        ctx.read_primitive_array_bytes(self.arr, off, buf);
    }

    fn write(&self, ctx: &mut dyn NativeContext, x: i32, y: i32, buf: &[u8]) {
        let bpp = self.fmt.bpp();
        let off = self.row.off + y as usize * self.row.scan + x as usize * bpp;
        ctx.write_primitive_array_bytes(self.arr, off, buf);
    }
}

// ---------------------------------------------------------------------------
// Clip regions
// ---------------------------------------------------------------------------

struct Clip {
    lox: i32,
    loy: i32,
    hix: i32,
    hiy: i32,
    bands: Option<Vec<i32>>,
}

fn read_clip(ctx: &dyn NativeContext, region: Option<ObjectRef>) -> Clip {
    let Some(r) = region else {
        return Clip {
            lox: i32::MIN,
            loy: i32::MIN,
            hix: i32::MAX,
            hiy: i32::MAX,
            bands: None,
        };
    };
    let bands = fld_obj(ctx, r, "bands").map(|arr| {
        let end = fld_int(ctx, r, "endIndex").max(0) as usize;
        let mut v = vec![0i32; end];
        ctx.read_int_array_into(arr, 0, &mut v);
        v
    });
    Clip {
        lox: fld_int(ctx, r, "lox"),
        loy: fld_int(ctx, r, "loy"),
        hix: fld_int(ctx, r, "hix"),
        hiy: fld_int(ctx, r, "hiy"),
        bands,
    }
}

/// The rectangles `[x1,y1,x2,y2)` of `clip` inside the bounds, in Y-X order.
fn clip_rects(c: &Clip, bx1: i64, by1: i64, bx2: i64, by2: i64) -> Vec<(i32, i32, i32, i32)> {
    let mut out = Vec::new();
    let (lx, ly, hx, hy) = (
        bx1.max(c.lox as i64),
        by1.max(c.loy as i64),
        bx2.min(c.hix as i64),
        by2.min(c.hiy as i64),
    );
    if lx >= hx || ly >= hy {
        return out;
    }
    match &c.bands {
        None => out.push((lx as i32, ly as i32, hx as i32, hy as i32)),
        Some(b) => {
            let mut i = 0usize;
            while i + 3 <= b.len() {
                let (y1, y2, n) = (b[i] as i64, b[i + 1] as i64, b[i + 2] as usize);
                i += 3;
                let ry1 = y1.max(ly);
                let ry2 = y2.min(hy);
                for k in 0..n {
                    if i + 2 * k + 1 >= b.len() {
                        break;
                    }
                    let x1 = (b[i + 2 * k] as i64).max(lx);
                    let x2 = (b[i + 2 * k + 1] as i64).min(hx);
                    if ry1 < ry2 && x1 < x2 {
                        out.push((x1 as i32, ry1 as i32, x2 as i32, ry2 as i32));
                    }
                }
                i += 2 * n;
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Composite
// ---------------------------------------------------------------------------

fn alpha_of(ctx: &dyn NativeContext, comp: Option<ObjectRef>) -> (i32, u32) {
    let Some(c) = comp else {
        return (3, 0xff);
    };
    let rule = fld_int(ctx, c, "rule");
    let extra = match ctx.get_field_by_name(c, "extraAlpha") {
        Value::Float(f) => (f * 255.0 + 0.5) as u32,
        _ => 0xff,
    };
    (if rule == 0 { 3 } else { rule }, extra.min(0xff))
}

// ---------------------------------------------------------------------------
// Natives: IDs and surfaces
// ---------------------------------------------------------------------------

fn n_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    void()
}

fn n_false(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `SurfaceData.isOpaqueGray(IndexColorModel)`.
fn n_is_opaque_gray(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let gray = match arg_obj(args, 0) {
        Some(icm) => match ctx.get_field_by_name(icm, "allgrayopaque") {
            Value::Int(v) => v != 0,
            _ => false,
        },
        None => false,
    };
    Ok(Some(Value::Int(gray as i32)))
}

/// `BufImgSurfaceData.initRaster(Object, int, int, int, int, int, int, IndexColorModel)`.
fn n_init_raster(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("initRaster: null receiver"))?;
    let array = arg_obj(args, 1);
    let off = arg_int(args, 2);
    let width = arg_int(args, 4);
    let height = arg_int(args, 5);
    let scan = arg_int(args, 7);

    let desc = fld_obj(ctx, this, "surfaceType")
        .and_then(|st| fld_obj(ctx, st, "desc"))
        .and_then(|d| ctx.read_string(d))
        .unwrap_or_default();
    let fmt = Fmt::from_desc(&desc);
    let root = match array {
        Some(a) => ctx.add_weak_global_root(a),
        None => 0,
    };
    let vm = ctx.vm_identity();
    let (id, dead) = {
        let probe: &dyn NativeContext = &*ctx;
        SURFS.with(vm, |t| {
            let mut dead = Vec::new();
            t.rows.retain(|_, r| {
                let alive = r.root == 0 || probe.resolve_global_root(r.root).is_some();
                if !alive {
                    dead.push(r.root);
                }
                alive
            });
            t.next += 1;
            let id = t.next;
            t.rows.insert(
                id,
                SurfRow {
                    root,
                    off: off.max(0) as usize,
                    w: width,
                    h: height,
                    scan: scan.max(0) as usize,
                    fmt,
                    desc,
                },
            );
            (id, dead)
        })
    };
    for r in dead {
        ctx.remove_global_root(r);
    }
    ctx.set_field_by_name(this, "pData", Value::Long(id));
    void()
}

// ---------------------------------------------------------------------------
// Natives: loops
// ---------------------------------------------------------------------------

fn prim_fmts(ctx: &dyn NativeContext, this: ObjectRef) -> (Fmt, Fmt) {
    let v = match ctx.get_field_by_name(this, "pNativePrim") {
        Value::Long(v) => v - 1,
        _ => 0,
    };
    let s = ((v >> 8) & 0xff) as usize;
    let d = (v & 0xff) as usize;
    (
        ALL_FMTS[s.min(ALL_FMTS.len() - 1)],
        ALL_FMTS[d.min(ALL_FMTS.len() - 1)],
    )
}

/// `FillRect.FillRect(SunGraphics2D, SurfaceData, int, int, int, int)`.
fn n_fill_rect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("FillRect: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("FillRect: null surface"))?;
    let (x, y, w, h) = (
        arg_int(args, 3),
        arg_int(args, 4),
        arg_int(args, 5),
        arg_int(args, 6),
    );
    let pixel = fld_int(ctx, sg2d, "pixel") as u32;
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    let xor = xor_of(ctx, sg2d, pixel);
    fill_rects(ctx, sd, &clip, pixel, xor, &[(x, y, w, h)])
}

/// The value to XOR into the destination when `sg2d.composite` is an
/// `XORComposite`: `(pixel ^ xorPixel) & ~alphaMask`, as HotSpot's XOR loops
/// compute it from `SurfaceData.pixelFor` values (not from the `ColorModel`).
fn xor_of(ctx: &dyn NativeContext, sg2d: ObjectRef, pixel: u32) -> Option<u32> {
    let comp = fld_obj(ctx, sg2d, "composite")?;
    match ctx.get_field_by_name(comp, "xorPixel") {
        Value::Int(xp) => {
            let mask = fld_int(ctx, comp, "alphaMask") as u32;
            Some((pixel ^ xp as u32) & !mask)
        }
        _ => None,
    }
}

fn fill_rects(
    ctx: &mut dyn NativeContext,
    sd: ObjectRef,
    clip: &Clip,
    pixel: u32,
    xor: Option<u32>,
    rects: &[(i32, i32, i32, i32)],
) -> MethodCallResult {
    let s = lookup_surf(ctx, sd)?;
    let mut buf = Vec::new();
    for &(x, y, w, h) in rects {
        if w <= 0 || h <= 0 {
            continue;
        }
        let (bx1, by1) = (x as i64, y as i64);
        let (bx2, by2) = (bx1 + w as i64, by1 + h as i64);
        let sb = (0i64, 0i64, s.row.w as i64, s.row.h as i64);
        for (x1, y1, x2, y2) in clip_rects(
            clip,
            bx1.max(sb.0),
            by1.max(sb.1),
            bx2.min(sb.2),
            by2.min(sb.3),
        ) {
            let n = (x2 - x1) as usize;
            buf.clear();
            buf.resize(n * s.fmt.bpp(), 0);
            rows::fill_row(s.fmt, &mut buf, n, xor.unwrap_or(pixel));
            for yy in y1..y2 {
                if xor.is_some() {
                    let mut dst = Vec::new();
                    s.read(ctx, x1, yy, n, &mut dst);
                    for (d, v) in dst.iter_mut().zip(buf.iter()) {
                        *d ^= *v;
                    }
                    s.write(ctx, x1, yy, &dst);
                } else {
                    s.write(ctx, x1, yy, &buf);
                }
            }
        }
    }
    void()
}

/// `DrawLine.DrawLine(SunGraphics2D, SurfaceData, int, int, int, int)`:
/// the Bresenham walk of `LineUtils` (error starts at `-errminor/2`, the minor
/// axis bumps when the error reaches zero), clipped pixel by pixel.
fn n_draw_line(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("DrawLine: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("DrawLine: null surface"))?;
    let (x1, y1, x2, y2) = (
        arg_int(args, 3),
        arg_int(args, 4),
        arg_int(args, 5),
        arg_int(args, 6),
    );
    let pixel = fld_int(ctx, sg2d, "pixel") as u32;
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    let s = lookup_surf(ctx, sd)?;
    let (lx, ly) = (x1.min(x2) as i64, y1.min(y2) as i64);
    let (hx, hy) = (x1.max(x2) as i64 + 1, y1.max(y2) as i64 + 1);
    let rects = clip_rects(
        &clip,
        lx.max(0),
        ly.max(0),
        hx.min(s.row.w as i64),
        hy.min(s.row.h as i64),
    );
    if rects.is_empty() {
        return void();
    }
    let inside = |x: i32, y: i32| {
        rects
            .iter()
            .any(|&(a, b, c, d)| x >= a && x < c && y >= b && y < d)
    };
    let mut pts: Vec<(i32, i32)> = Vec::new();
    let (dx, dy) = (x2 as i64 - x1 as i64, y2 as i64 - y1 as i64);
    let (ax, ay) = (dx.abs(), dy.abs());
    if ax == 0 && ay == 0 {
        pts.push((x1, y1));
    } else {
        // The Bresenham walk in closed form (the loop would run up to 2^31 steps for
        // lines far outside the surface): after `j` major steps the minor axis has
        // advanced `floor((-(errminor/2) + j*errmajor) / errminor) + 1` times
        // (never below 0), so only the steps that land inside the clip are visited.
        let xmajor = ax >= ay;
        let (errmajor, errminor) = if xmajor {
            (ay * 2, ax * 2)
        } else {
            (ax * 2, ay * 2)
        };
        let (bmaj, bmin) = if xmajor {
            (dx.signum(), dy.signum())
        } else {
            (dy.signum(), dx.signum())
        };
        let (len, start_maj, start_min) = if xmajor {
            (ax, x1 as i64, y1 as i64)
        } else {
            (ay, y1 as i64, x1 as i64)
        };
        let (mut cm0, mut cm1) = (i64::MAX, i64::MIN);
        for &(a, b, c, d) in &rects {
            let (lo, hi) = if xmajor {
                (a as i64, c as i64)
            } else {
                (b as i64, d as i64)
            };
            cm0 = cm0.min(lo);
            cm1 = cm1.max(hi);
        }
        let (jlo, jhi) = if bmaj > 0 {
            ((cm0 - start_maj).max(0), (cm1 - 1 - start_maj).min(len))
        } else {
            ((start_maj - (cm1 - 1)).max(0), (start_maj - cm0).min(len))
        };
        let e0 = -(errminor / 2) as i128;
        let mut j = jlo;
        while j <= jhi {
            let t = e0 + (j as i128) * (errmajor as i128);
            let m = (t.div_euclid(errminor as i128) + 1).max(0) as i64;
            let maj = start_maj + bmaj * j;
            let min = start_min + bmin * m;
            pts.push(if xmajor {
                (maj as i32, min as i32)
            } else {
                (min as i32, maj as i32)
            });
            j += 1;
        }
    }
    let xor = xor_of(ctx, sg2d, pixel);
    let mut buf = Vec::new();
    for (x, y) in pts {
        if inside(x, y) {
            buf.clear();
            buf.resize(s.fmt.bpp(), 0);
            rows::fill_row(s.fmt, &mut buf, 1, xor.unwrap_or(pixel));
            if xor.is_some() {
                let mut dst = Vec::new();
                s.read(ctx, x, y, 1, &mut dst);
                for (d, v) in dst.iter_mut().zip(buf.iter()) {
                    *d ^= *v;
                }
                s.write(ctx, x, y, &dst);
            } else {
                s.write(ctx, x, y, &buf);
            }
        }
    }
    void()
}

/// `FillSpans.FillSpans(SunGraphics2D, SurfaceData, int, long, SpanIterator)`.
fn n_fill_spans(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("FillSpans: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("FillSpans: null surface"))?;
    let pixel = arg_int(args, 3) as u32;
    let p_iter = arg_long(args, 4);
    let si = arg_obj(args, 5);
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    if p_iter > 0 {
        // A native `ShapeSpanIterator`: drain it directly.
        let vm = ctx.vm_identity();
        let spans = SHAPES.with(vm, |t| {
            let mut v = Vec::new();
            if let Some(it) = t.map.get_mut(&p_iter) {
                while let Some(sp) = it.next_span() {
                    v.push((sp[0], sp[1], sp[2] - sp[0], sp[3] - sp[1]));
                }
            }
            v
        });
        let xor = xor_of(ctx, sg2d, pixel);
        return fill_rects(ctx, sd, &clip, pixel, xor, &spans);
    }
    let Some(si) = si else {
        return void();
    };
    // Pull spans through the Java `SpanIterator.nextSpan(int[])` contract.
    let mut spans: Vec<(i32, i32, i32, i32)> = Vec::new();
    {
        let mut scope = NativeHandleScope::new(ctx);
        let si_h = scope.root(si);
        let arr = scope.new_array(cratonvm_types::ArrayElementType::Int, 4);
        let arr_h = scope.root(arr);
        loop {
            let si_now = scope.get(&si_h);
            let arr_now = scope.get(&arr_h);
            let more =
                scope.invoke_virtual(si_now, "nextSpan", "([I)Z", &[Value::Object(Some(arr_now))]);
            match more {
                Ok(Some(Value::Int(0))) | Ok(None) | Err(_) => break,
                Ok(_) => {}
            }
            let arr_now = scope.get(&arr_h);
            let mut b = [0i32; 4];
            scope.read_int_array_into(arr_now, 0, &mut b);
            spans.push((b[0], b[1], b[2] - b[0], b[3] - b[1]));
        }
    }
    let xor = xor_of(ctx, sg2d, pixel);
    fill_rects(ctx, sd, &clip, pixel, xor, &spans)
}

/// `MaskFill.MaskFill(SunGraphics2D, SurfaceData, Composite, int, int, int, int, byte[], int, int)`.
fn n_mask_fill(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let sg2d = arg_obj(args, 1).ok_or_else(|| internal("MaskFill: null sg2d"))?;
    let sd = arg_obj(args, 2).ok_or_else(|| internal("MaskFill: null surface"))?;
    let comp = arg_obj(args, 3);
    let (x, y, w, h) = (
        arg_int(args, 4),
        arg_int(args, 5),
        arg_int(args, 6),
        arg_int(args, 7),
    );
    let mask = arg_obj(args, 8);
    let maskoff = arg_int(args, 9).max(0) as usize;
    let maskscan = arg_int(args, 10).max(0) as usize;
    if w <= 0 || h <= 0 {
        return void();
    }
    let eargb = fld_int(ctx, sg2d, "eargb") as u32;
    let clip = read_clip(ctx, fld_obj(ctx, sg2d, "clipRegion"));
    let (rule, _extra) = alpha_of(ctx, comp);
    let s = lookup_surf(ctx, sd)?;
    let (src_a, fg_pre) = pixel::prepare_fill_color(s.fmt, eargb);
    let (bx1, by1) = (x as i64, y as i64);
    let (bx2, by2) = (bx1 + w as i64, by1 + h as i64);
    let mut dbuf = Vec::new();
    let mut mbuf: Vec<u8> = Vec::new();
    for (x1, y1, x2, y2) in clip_rects(
        &clip,
        bx1.max(0),
        by1.max(0),
        bx2.min(s.row.w as i64),
        by2.min(s.row.h as i64),
    ) {
        let n = (x2 - x1) as usize;
        for yy in y1..y2 {
            s.read(ctx, x1, yy, n, &mut dbuf);
            let m = match mask {
                Some(arr) => {
                    mbuf.clear();
                    mbuf.resize(n, 0);
                    let moff = maskoff + (yy - y) as usize * maskscan + (x1 - x) as usize;
                    ctx.read_byte_array_into(arr, moff, &mut mbuf);
                    Some(&mbuf[..])
                }
                None => None,
            };
            rows::mask_fill_row(s.fmt, rule, src_a, fg_pre, &mut dbuf, m, n);
            s.write(ctx, x1, yy, &dbuf);
        }
    }
    void()
}

/// `Blit.Blit(...)` and `MaskBlit.MaskBlit(...)`.
fn blit_common(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    convert: bool,
    masked: bool,
) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("Blit: null receiver"))?;
    let src_sd = arg_obj(args, 1).ok_or_else(|| internal("Blit: null source"))?;
    let dst_sd = arg_obj(args, 2).ok_or_else(|| internal("Blit: null destination"))?;
    let comp = arg_obj(args, 3);
    let clip_obj = arg_obj(args, 4);
    let (sx, sy, dx, dy, w, h) = (
        arg_int(args, 5),
        arg_int(args, 6),
        arg_int(args, 7),
        arg_int(args, 8),
        arg_int(args, 9),
        arg_int(args, 10),
    );
    if w <= 0 || h <= 0 {
        return void();
    }
    let (mask, maskoff, maskscan) = if masked {
        (
            arg_obj(args, 11),
            arg_int(args, 12).max(0) as usize,
            arg_int(args, 13).max(0) as usize,
        )
    } else {
        (None, 0, 0)
    };
    let _ = prim_fmts(ctx, this);
    let src = lookup_surf(ctx, src_sd)?;
    let dst = lookup_surf(ctx, dst_sd)?;
    let (rule, extra_a) = alpha_of(ctx, comp);
    let clip = read_clip(ctx, clip_obj);
    // Destination rectangle within the destination surface, and the matching
    // source rectangle within the source surface.
    let (ox, oy) = (sx as i64 - dx as i64, sy as i64 - dy as i64);
    let bx1 = (dx as i64).max(0).max(-ox);
    let by1 = (dy as i64).max(0).max(-oy);
    let bx2 = (dx as i64 + w as i64)
        .min(dst.row.w as i64)
        .min(src.row.w as i64 - ox);
    let by2 = (dy as i64 + h as i64)
        .min(dst.row.h as i64)
        .min(src.row.h as i64 - oy);
    let mut sbuf = Vec::new();
    let mut dbuf = Vec::new();
    let mut mbuf: Vec<u8> = Vec::new();
    // `copyArea` blits a surface onto itself: when the destination lies below
    // the source, rows must be copied bottom-up (the C loops do the same).
    let reverse = src_sd == dst_sd && dy > sy;
    for (x1, y1, x2, y2) in clip_rects(&clip, bx1, by1, bx2, by2) {
        let n = (x2 - x1) as usize;
        let ys: Vec<i32> = if reverse {
            (y1..y2).rev().collect()
        } else {
            (y1..y2).collect()
        };
        for yy in ys {
            let src_y = (yy as i64 + oy) as i32;
            let src_x = (x1 as i64 + ox) as i32;
            src.read(ctx, src_x, src_y, n, &mut sbuf);
            if convert {
                dbuf.clear();
                dbuf.resize(n * dst.fmt.bpp(), 0);
                rows::convert_row(src.fmt, dst.fmt, &sbuf, &mut dbuf, n);
            } else {
                dst.read(ctx, x1, yy, n, &mut dbuf);
                let m = match mask {
                    Some(arr) => {
                        mbuf.clear();
                        mbuf.resize(n, 0);
                        let moff = maskoff + (yy - dy) as usize * maskscan + (x1 - dx) as usize;
                        ctx.read_byte_array_into(arr, moff, &mut mbuf);
                        Some(&mbuf[..])
                    }
                    None => None,
                };
                rows::alpha_blit_row(src.fmt, dst.fmt, rule, extra_a, &sbuf, &mut dbuf, m, n);
            }
            dst.write(ctx, x1, yy, &dbuf);
        }
    }
    void()
}

fn n_blit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("Blit: null receiver"))?;
    // pNativePrim carries the primitive's kind in bit 16: 1 = SrcNoEa convert.
    let convert = match ctx.get_field_by_name(this, "pNativePrim") {
        Value::Long(v) => ((v - 1) >> 16) & 1 == 1,
        _ => false,
    };
    blit_common(ctx, args, convert, false)
}

fn n_mask_blit(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    blit_common(ctx, args, false, true)
}

// ---------------------------------------------------------------------------
// ShapeSpanIterator
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ShapeTable {
    next: i64,
    map: HashMap<i64, shape::ShapeIter>,
}

static SHAPES: VmScoped<ShapeTable> = VmScoped::new();

fn arg_f(args: &[Value], i: usize) -> f32 {
    match args.get(i) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    }
}

fn shape_id(ctx: &dyn NativeContext, this: ObjectRef) -> i64 {
    match ctx.get_field_by_name(this, "pData") {
        Value::Long(v) => v,
        _ => 0,
    }
}

fn with_shape<R>(
    ctx: &dyn NativeContext,
    args: &[Value],
    f: impl FnOnce(&mut shape::ShapeIter) -> R,
) -> Result<R, MethodCallFailed> {
    let this = arg_obj(args, 0).ok_or_else(|| internal("ShapeSpanIterator: null receiver"))?;
    let id = shape_id(ctx, this);
    let vm = ctx.vm_identity();
    SHAPES
        .with(vm, |t| t.map.get_mut(&id).map(f))
        .ok_or_else(|| internal("ShapeSpanIterator used before setNormalize or after dispose"))
}

fn n_ssi_set_normalize(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("ShapeSpanIterator: null receiver"))?;
    let adjust = arg_int(args, 1) != 0;
    let vm = ctx.vm_identity();
    let id = SHAPES.with(vm, |t| {
        t.next += 1;
        let id = t.next;
        t.map.insert(id, shape::ShapeIter::new(adjust));
        id
    });
    ctx.set_field_by_name(this, "pData", Value::Long(id));
    void()
}

fn n_ssi_dispose(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(this) = arg_obj(args, 0) {
        let id = shape_id(ctx, this);
        let vm = ctx.vm_identity();
        SHAPES.with(vm, |t| {
            t.map.remove(&id);
        });
        ctx.set_field_by_name(this, "pData", Value::Long(0));
    }
    void()
}

fn n_ssi_append_poly(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let n = arg_int(args, 3).max(0) as usize;
    let (xoff, yoff) = (arg_int(args, 4), arg_int(args, 5));
    let mut xs = vec![0i32; n];
    let mut ys = vec![0i32; n];
    if let Some(a) = arg_obj(args, 1) {
        ctx.read_int_array_into(a, 0, &mut xs);
    }
    if let Some(a) = arg_obj(args, 2) {
        ctx.read_int_array_into(a, 0, &mut ys);
    }
    with_shape(ctx, args, |it| it.append_poly(&xs, &ys, xoff, yoff))?;
    void()
}

fn n_ssi_set_output_area(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (a, b, c, d) = (
        arg_int(args, 1),
        arg_int(args, 2),
        arg_int(args, 3),
        arg_int(args, 4),
    );
    with_shape(ctx, args, |it| it.set_output_area(a, b, c, d))?;
    void()
}

fn n_ssi_intersect_clip_box(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (a, b, c, d) = (
        arg_int(args, 1),
        arg_int(args, 2),
        arg_int(args, 3),
        arg_int(args, 4),
    );
    with_shape(ctx, args, |it| it.intersect_clip_box(a, b, c, d))?;
    void()
}

fn n_ssi_set_rule(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let r = arg_int(args, 1);
    with_shape(ctx, args, |it| it.set_rule(r))?;
    void()
}

fn n_ssi_add_segment(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let t = arg_int(args, 1);
    let mut c = [0f32; 6];
    if let Some(a) = arg_obj(args, 2) {
        for (i, slot) in c.iter_mut().enumerate() {
            if let Value::Float(f) = ctx.get_array_element(a, i) {
                *slot = f;
            }
        }
    }
    with_shape(ctx, args, |it| it.add_segment(t, &c))?;
    void()
}

fn n_ssi_get_path_box(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let b = with_shape(ctx, args, |it| it.get_path_box())?;
    if let Some(a) = arg_obj(args, 1) {
        ctx.write_int_array_from(a, 0, &b);
    }
    void()
}

fn n_ssi_next_span(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let span = with_shape(ctx, args, |it| it.next_span())?;
    match span {
        Some(s) => {
            if let Some(a) = arg_obj(args, 1) {
                ctx.write_int_array_from(a, 0, &s);
            }
            Ok(Some(Value::Int(1)))
        }
        None => Ok(Some(Value::Int(0))),
    }
}

fn n_ssi_skip_down_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let y = arg_int(args, 1);
    with_shape(ctx, args, |it| it.skip_down_to(y))?;
    void()
}

fn n_ssi_native_ptr(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = arg_obj(args, 0).ok_or_else(|| internal("ShapeSpanIterator: null receiver"))?;
    Ok(Some(Value::Long(shape_id(ctx, this))))
}

fn n_ssi_move_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (x, y) = (arg_f(args, 1), arg_f(args, 2));
    with_shape(ctx, args, |it| it.move_to(x, y))?;
    void()
}

fn n_ssi_line_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (x, y) = (arg_f(args, 1), arg_f(args, 2));
    with_shape(ctx, args, |it| it.line_to(x, y))?;
    void()
}

fn n_ssi_quad_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (a, b, c, d) = (
        arg_f(args, 1),
        arg_f(args, 2),
        arg_f(args, 3),
        arg_f(args, 4),
    );
    with_shape(ctx, args, |it| it.quad_to(a, b, c, d))?;
    void()
}

fn n_ssi_curve_to(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v: Vec<f32> = (1..=6).map(|i| arg_f(args, i)).collect();
    with_shape(ctx, args, |it| {
        it.curve_to(v[0], v[1], v[2], v[3], v[4], v[5])
    })?;
    void()
}

fn n_ssi_close_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    with_shape(ctx, args, |it| it.close_path())?;
    void()
}

fn n_ssi_path_done(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    with_shape(ctx, args, |it| it.path_done())?;
    void()
}

// ---------------------------------------------------------------------------
// SpanClipRenderer (alpha tiles clipped to a non-rectangular Region)
// ---------------------------------------------------------------------------

/// `fillTile` (`erase == false`) and `eraseTile` (`erase == true`):
/// `(RegionIterator, byte[] alpha, int offset, int tsize, int[] band)`.
fn span_clip_tile(ctx: &mut dyn NativeContext, args: &[Value], erase: bool) -> MethodCallResult {
    let ri = arg_obj(args, 1).ok_or_else(|| internal("SpanClipRenderer: null iterator"))?;
    let alpha = arg_obj(args, 2).ok_or_else(|| internal("SpanClipRenderer: null tile"))?;
    let offset = arg_int(args, 3).max(0) as usize;
    let tsize = arg_int(args, 4).max(0) as usize;
    let band_arr = arg_obj(args, 5).ok_or_else(|| internal("SpanClipRenderer: null band"))?;
    let mut band = [0i32; 4];
    ctx.read_int_array_into(band_arr, 0, &mut band);
    let clip = read_clip(ctx, fld_obj(ctx, ri, "region"));
    let rects = clip_rects(
        &clip,
        band[0] as i64,
        band[1] as i64,
        band[2] as i64,
        band[3] as i64,
    );
    let (w, h) = (
        (band[2] - band[0]).max(0) as usize,
        (band[3] - band[1]).max(0) as usize,
    );
    let mut covered = vec![false; w * h];
    let mut bbox: Option<(i32, i32, i32, i32)> = None;
    for &(x1, y1, x2, y2) in &rects {
        for y in y1..y2 {
            for x in x1..x2 {
                covered[(y - band[1]) as usize * w + (x - band[0]) as usize] = true;
            }
        }
        bbox = Some(match bbox {
            None => (x1, y1, x2, y2),
            Some((a, b, c, d)) => (a.min(x1), b.min(y1), c.max(x2), d.max(y2)),
        });
    }
    let mut row = vec![0u8; w];
    for y in 0..h {
        let off = offset + y * tsize;
        if erase {
            ctx.read_byte_array_into(alpha, off, &mut row);
            for x in 0..w {
                if !covered[y * w + x] {
                    row[x] = 0;
                }
            }
        } else {
            for x in 0..w {
                row[x] = if covered[y * w + x] { 0xff } else { 0 };
            }
        }
        ctx.write_byte_array_from(alpha, off, &row);
    }
    let nb = match bbox {
        Some((a, b, c, d)) => [a, b, c, d],
        None => [band[0], band[1], band[0], band[1]],
    };
    ctx.write_int_array_from(band_arr, 0, &nb);
    void()
}

fn n_fill_tile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    span_clip_tile(ctx, args, false)
}

fn n_erase_tile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    span_clip_tile(ctx, args, true)
}

// ---------------------------------------------------------------------------
// registerNativeLoops
// ---------------------------------------------------------------------------

fn static_obj(ctx: &dyn NativeContext, class: &str, field: &str) -> Option<ObjectRef> {
    let cid = ctx.class_id_by_name(class)?;
    let idx = ctx.static_field_index_by_name(cid, field)?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

struct Plan {
    class: &'static str,
    src: Option<Fmt>,
    comp: &'static str,
    dst: Fmt,
    prim: i64,
}

fn plan() -> Vec<Plan> {
    let mut v = Vec::new();
    for &d in &ALL_FMTS {
        let di = d as i64;
        v.push(Plan {
            class: "sun/java2d/loops/FillRect",
            src: None,
            comp: "SrcNoEa",
            dst: d,
            prim: di + 1,
        });
        if matches!(d, Fmt::ByteGray | Fmt::Ushort565Rgb | Fmt::Ushort555Rgb) {
            for class in [
                "sun/java2d/loops/FillPath",
                "sun/java2d/loops/DrawPath",
                "sun/java2d/loops/DrawRect",
                "sun/java2d/loops/DrawPolygons",
            ] {
                v.push(Plan {
                    class,
                    src: None,
                    comp: "SrcNoEa",
                    dst: d,
                    prim: di + 1,
                });
            }
        }
        v.push(Plan {
            class: "sun/java2d/loops/DrawLine",
            src: None,
            comp: "SrcNoEa",
            dst: d,
            prim: di + 1,
        });
        if matches!(
            d,
            Fmt::IntRgb | Fmt::IntBgr | Fmt::ThreeByteBgr | Fmt::Ushort565Rgb | Fmt::Ushort555Rgb
        ) {
            // HotSpot registers LCD glyph loops for the opaque RGB layouts only.
            v.push(Plan {
                class: "sun/java2d/loops/DrawGlyphListLCD",
                src: None,
                comp: "SrcNoEa",
                dst: d,
                prim: di + 1,
            });
        }
        if d == Fmt::ByteGray {
            // HotSpot's AnyByte XOR loops (the Java fallback would convert
            // colours through the ColorModel, whose gray differs from `pixelFor`).
            for class in [
                "sun/java2d/loops/FillRect",
                "sun/java2d/loops/DrawLine",
                "sun/java2d/loops/FillSpans",
            ] {
                v.push(Plan {
                    class,
                    src: None,
                    comp: "Xor",
                    dst: d,
                    prim: di + 1,
                });
            }
        }
        v.push(Plan {
            class: "sun/java2d/loops/FillSpans",
            src: None,
            comp: "SrcNoEa",
            dst: d,
            prim: di + 1,
        });
        v.push(Plan {
            class: "sun/java2d/loops/MaskFill",
            src: None,
            comp: "AnyAlpha",
            dst: d,
            // Non-zero: `canDoParallelograms()` holds, so rotated rectangles, wide
            // lines and stroked rectangles reach `FillAAPgram` / `DrawAAPgram`.
            prim: di + 1,
        });
        for &s in &ALL_FMTS {
            let si = s as i64;
            let key = (si << 8) | di;
            v.push(Plan {
                class: "sun/java2d/loops/Blit",
                src: Some(s),
                comp: "SrcNoEa",
                dst: d,
                prim: (key | (1 << 16)) + 1,
            });
            v.push(Plan {
                class: "sun/java2d/loops/Blit",
                src: Some(s),
                comp: "AnyAlpha",
                dst: d,
                prim: key + 1,
            });
            v.push(Plan {
                class: "sun/java2d/loops/MaskBlit",
                src: Some(s),
                comp: "AnyAlpha",
                dst: d,
                prim: key + 1,
            });
            v.push(Plan {
                class: "sun/java2d/loops/ScaledBlit",
                src: Some(s),
                comp: "SrcNoEa",
                dst: d,
                prim: (key | (1 << 16)) + 1,
            });
            v.push(Plan {
                class: "sun/java2d/loops/ScaledBlit",
                src: Some(s),
                comp: "AnyAlpha",
                dst: d,
                prim: key + 1,
            });
            v.push(Plan {
                class: "sun/java2d/loops/BlitBg",
                src: Some(s),
                comp: "SrcNoEa",
                dst: d,
                prim: key + 1,
            });
        }
    }
    // TransformHelper: one per source layout, always into IntArgbPre.
    for &s in &ALL_FMTS {
        let key = ((s as i64) << 8) | (Fmt::IntArgbPre as i64);
        v.push(Plan {
            class: "sun/java2d/loops/TransformHelper",
            src: Some(s),
            comp: "SrcNoEa",
            dst: Fmt::IntArgbPre,
            prim: key + 1,
        });
    }
    v
}

/// `GraphicsPrimitiveMgr.registerNativeLoops()`.
fn n_register_native_loops(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let plan = plan();
    let prim_class = ctx
        .class_id_by_name("sun/java2d/loops/GraphicsPrimitive")
        .ok_or_else(|| internal("GraphicsPrimitive is not loaded"))?;
    let mut scope = NativeHandleScope::new(ctx);
    let arr = scope.new_ref_array(prim_class, plan.len());
    let arr_h = scope.root(arr);
    for (i, p) in plan.iter().enumerate() {
        let any_color_or_src = match p.src {
            Some(s) => static_obj(&*scope, SURFACE_TYPE, s.surface_type_field()),
            None => static_obj(&*scope, SURFACE_TYPE, "AnyColor"),
        };
        let comp = static_obj(&*scope, COMPOSITE_TYPE, p.comp);
        let dst = static_obj(&*scope, SURFACE_TYPE, p.dst.surface_type_field());
        let (Some(st), Some(ct), Some(dt)) = (any_color_or_src, comp, dst) else {
            return Err(internal(
                "Java2D surface/composite type constants are missing",
            ));
        };
        let created = scope.new_object_initialized(
            p.class,
            PRIM_CTOR,
            &[
                Value::Long(p.prim),
                Value::Object(Some(st)),
                Value::Object(Some(ct)),
                Value::Object(Some(dt)),
            ],
        )?;
        if let Some(Value::Object(Some(obj))) = created {
            let a = scope.get(&arr_h);
            scope.set_array_element(a, i, Value::Object(Some(obj)));
        }
    }
    let a = scope.get(&arr_h);
    scope.invoke_static(
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "register",
        "([Lsun/java2d/loops/GraphicsPrimitive;)V",
        &[Value::Object(Some(a))],
    )?;
    void()
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Register the Java2D natives. Called with the `Bridge` category: each of
/// these is the implementation of a genuine `ACC_NATIVE` method of the JDK.
pub fn register(registry: &mut NativeMethodRegistry) {
    fonts::register(registry);
    // `SunLayoutEngine.layout` (shaping), a reviewed intrinsic over rustybuzz.
    crate::hb::register(registry);
    for class in [
        "sun/java2d/SurfaceData",
        "sun/java2d/pipe/Region",
        "sun/awt/image/IntegerComponentRaster",
        "sun/awt/image/ByteComponentRaster",
        "sun/awt/image/ShortComponentRaster",
        "sun/awt/image/BytePackedRaster",
        "sun/java2d/Disposer",
    ] {
        registry.register(class, "initIDs", "()V", n_noop);
    }
    registry.register(
        "sun/awt/image/BufImgSurfaceData",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        n_noop,
    );
    registry.register(
        "sun/java2d/pipe/SpanClipRenderer",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;)V",
        n_noop,
    );
    registry.register(
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "initIDs",
        "(Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;Ljava/lang/Class;)V",
        n_noop,
    );
    registry.register(
        "sun/java2d/loops/GraphicsPrimitiveMgr",
        "registerNativeLoops",
        "()V",
        n_register_native_loops,
    );
    registry.register(
        "sun/java2d/SurfaceData",
        "isOpaqueGray",
        "(Ljava/awt/image/IndexColorModel;)Z",
        n_is_opaque_gray,
    );
    registry.register(
        "sun/awt/image/BufImgSurfaceData",
        "initRaster",
        "(Ljava/lang/Object;IIIIIILjava/awt/image/IndexColorModel;)V",
        n_init_raster,
    );
    registry.register(
        "sun/java2d/DefaultDisposerRecord",
        "invokeNativeDispose",
        "(JJ)V",
        n_noop,
    );
    // No mlib: `ImagingLib.init()` is false and the Java fallbacks run.
    registry.register("sun/awt/image/ImagingLib", "init", "()Z", n_false);

    registry.register(
        "sun/java2d/pipe/SpanClipRenderer",
        "fillTile",
        "(Lsun/java2d/pipe/RegionIterator;[BII[I)V",
        n_fill_tile,
    );
    registry.register(
        "sun/java2d/pipe/SpanClipRenderer",
        "eraseTile",
        "(Lsun/java2d/pipe/RegionIterator;[BII[I)V",
        n_erase_tile,
    );
    const SSI: &str = "sun/java2d/pipe/ShapeSpanIterator";
    registry.register(SSI, "initIDs", "()V", n_noop);
    registry.register(SSI, "setNormalize", "(Z)V", n_ssi_set_normalize);
    registry.register(SSI, "dispose", "()V", n_ssi_dispose);
    registry.register(SSI, "appendPoly", "([I[IIII)V", n_ssi_append_poly);
    registry.register(SSI, "setOutputAreaXYXY", "(IIII)V", n_ssi_set_output_area);
    registry.register(SSI, "intersectClipBox", "(IIII)V", n_ssi_intersect_clip_box);
    registry.register(SSI, "setRule", "(I)V", n_ssi_set_rule);
    registry.register(SSI, "addSegment", "(I[F)V", n_ssi_add_segment);
    registry.register(SSI, "getPathBox", "([I)V", n_ssi_get_path_box);
    registry.register(SSI, "nextSpan", "([I)Z", n_ssi_next_span);
    registry.register(SSI, "skipDownTo", "(I)V", n_ssi_skip_down_to);
    registry.register(SSI, "getNativeIterator", "()J", n_ssi_native_ptr);
    registry.register(SSI, "getNativeConsumer", "()J", n_ssi_native_ptr);
    registry.register(SSI, "moveTo", "(FF)V", n_ssi_move_to);
    registry.register(SSI, "lineTo", "(FF)V", n_ssi_line_to);
    registry.register(SSI, "quadTo", "(FFFF)V", n_ssi_quad_to);
    registry.register(SSI, "curveTo", "(FFFFFF)V", n_ssi_curve_to);
    registry.register(SSI, "closePath", "()V", n_ssi_close_path);
    registry.register(SSI, "pathDone", "()V", n_ssi_path_done);

    registry.register(
        "sun/java2d/loops/DrawGlyphListLCD",
        "DrawGlyphListLCD",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Lsun/font/GlyphList;II)V",
        lcdtext::n_draw_glyph_list_lcd,
    );
    registry.register(
        "sun/java2d/loops/FillRect",
        "FillRect",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        n_fill_rect,
    );
    registry.register(
        "sun/java2d/loops/FillPath",
        "FillPath",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IILjava/awt/geom/Path2D$Float;)V",
        pathbridge::n_fill_path,
    );
    registry.register(
        "sun/java2d/loops/DrawPath",
        "DrawPath",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IILjava/awt/geom/Path2D$Float;)V",
        pathbridge::n_draw_path,
    );
    registry.register(
        "sun/java2d/loops/DrawRect",
        "DrawRect",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        pathbridge::n_draw_rect,
    );
    registry.register(
        "sun/java2d/loops/DrawPolygons",
        "DrawPolygons",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;[I[I[IIIIZ)V",
        pathbridge::n_draw_polygons,
    );
    registry.register(
        "sun/java2d/loops/DrawLine",
        "DrawLine",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IIII)V",
        n_draw_line,
    );
    registry.register(
        "sun/java2d/loops/FillSpans",
        "FillSpans",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;IJLsun/java2d/pipe/SpanIterator;)V",
        n_fill_spans,
    );
    registry.register(
        "sun/java2d/loops/MaskFill",
        "MaskFill",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Ljava/awt/Composite;IIII[BII)V",
        n_mask_fill,
    );
    registry.register(
        "sun/java2d/loops/MaskFill",
        "FillAAPgram",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Ljava/awt/Composite;DDDDDD)V",
        pgram::n_fill_aa_pgram,
    );
    registry.register(
        "sun/java2d/loops/MaskFill",
        "DrawAAPgram",
        "(Lsun/java2d/SunGraphics2D;Lsun/java2d/SurfaceData;Ljava/awt/Composite;DDDDDDDD)V",
        pgram::n_draw_aa_pgram,
    );
    registry.register(
        "sun/java2d/loops/Blit",
        "Blit",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIII)V",
        n_blit,
    );
    registry.register(
        "sun/java2d/loops/ScaledBlit",
        "Scale",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIDDDD)V",
        scale::n_scaled_blit,
    );
    registry.register(
        "sun/java2d/loops/BlitBg",
        "BlitBg",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIIII)V",
        scale::n_blit_bg,
    );
    registry.register(
        "sun/java2d/loops/TransformHelper",
        "Transform",
        "(Lsun/java2d/loops/MaskBlit;Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;Ljava/awt/geom/AffineTransform;IIIIIIIII[III)V",
        scale::n_transform_helper,
    );
    registry.register(
        "sun/java2d/loops/MaskBlit",
        "MaskBlit",
        "(Lsun/java2d/SurfaceData;Lsun/java2d/SurfaceData;Ljava/awt/Composite;Lsun/java2d/pipe/Region;IIIIII[BII)V",
        n_mask_blit,
    );
}
