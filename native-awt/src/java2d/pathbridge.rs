// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `FillPath`, `DrawPath`, `DrawRect` and `DrawPolygons` for the destination
//! layouts whose `ColorModel` data elements differ from `SurfaceData.pixelFor`
//! (`ByteGray`, `Ushort565Rgb`, `Ushort555Rgb`).
//!
//! HotSpot's native loops store `sg2d.pixel`; the JDK's Java fallback
//! (`GeneralRenderer`'s `SolidPixelWriter`) stores
//! `ColorModel.getDataElements(eargb)`, which for those layouts is a different
//! value (linear gray, different rounding). The geometry is the same
//! algorithm in both (`ProcessPath` is a port of the C code), so these natives
//! build the Java `SolidPixelWriter` around the `pixel` HotSpot would store and
//! call the same Java geometry code. No pixel logic is duplicated here.

use cratonvm_native_api::{NativeContext, NativeHandleScope};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use super::pixel::Fmt;
use super::{arg_int, arg_obj, fld_int, internal, lookup_surf, void};

const PW: &str = "sun/java2d/loops/PixelWriter";
const SD: &str = "sun/java2d/SurfaceData";
const REGION: &str = "sun/java2d/pipe/Region";
const HANDLER: &str = "sun/java2d/loops/ProcessPath$DrawHandler";

/// One-element data-elements array holding `pixel` the way the raster's
/// transfer type stores it.
fn elements_for(fmt: Fmt, pixel: u32) -> (ArrayElementType, i32) {
    match fmt {
        Fmt::ByteGray => (ArrayElementType::Byte, pixel as u8 as i8 as i32),
        Fmt::UshortGray | Fmt::Ushort565Rgb | Fmt::Ushort555Rgb => {
            (ArrayElementType::Short, pixel as u16 as i16 as i32)
        }
        _ => (ArrayElementType::Int, pixel as i32),
    }
}

/// Shared prologue: roots, a `SolidPixelWriter(pixel)` and the clip.
macro_rules! prologue {
    ($ctx:expr, $args:expr, $sg2d:ident, $sd:ident, $scope:ident, $sg:ident, $sdh:ident, $pw:ident, $clip:ident, $hint:ident) => {
        let $sg2d = arg_obj($args, 1).ok_or_else(|| internal("path loop: null sg2d"))?;
        let $sd = arg_obj($args, 2).ok_or_else(|| internal("path loop: null surface"))?;
        let surf = lookup_surf($ctx, $sd)?;
        let pixel = fld_int($ctx, $sg2d, "pixel") as u32;
        let $hint = fld_int($ctx, $sg2d, "strokeHint");
        let (et, val) = elements_for(surf.fmt, pixel);
        let mut $scope = NativeHandleScope::new($ctx);
        let $sg = $scope.root($sg2d);
        let $sdh = $scope.root($sd);
        let arr = $scope.new_array(et, 1);
        $scope.set_array_element(arr, 0, Value::Int(val));
        let arr_h = $scope.root(arr);
        let arr_o = $scope.get(&arr_h);
        let pwv = $scope.new_object_initialized(
            "sun/java2d/loops/SolidPixelWriter",
            "(Ljava/lang/Object;)V",
            &[Value::Object(Some(arr_o))],
        )?;
        let Some(Value::Object(Some(pwo))) = pwv else {
            return Err(internal("SolidPixelWriter was not created"));
        };
        let $pw = $scope.root(pwo);
        let sgo = $scope.get(&$sg);
        let clipv = $scope.invoke_virtual(sgo, "getCompClip", &format!("()L{};", REGION), &[])?;
        let Some(Value::Object(Some(clipo))) = clipv else {
            return Err(internal("getCompClip returned null"));
        };
        let $clip = $scope.root(clipo);
    };
}

fn handler(
    scope: &mut NativeHandleScope<'_>,
    sdh: &cratonvm_native_api::NativeHandle,
    pw: &cratonvm_native_api::NativeHandle,
    clip: &cratonvm_native_api::NativeHandle,
    hint: i32,
) -> Result<cratonvm_native_api::NativeHandle, cratonvm_types::error::MethodCallFailed> {
    let (s, p, c) = (scope.get(sdh), scope.get(pw), scope.get(clip));
    let h = scope.new_object_initialized(
        "sun/java2d/loops/PixelWriterDrawHandler",
        &format!("(L{};L{};L{};I)V", SD, PW, REGION),
        &[
            Value::Object(Some(s)),
            Value::Object(Some(p)),
            Value::Object(Some(c)),
            Value::Int(hint),
        ],
    )?;
    let Some(Value::Object(Some(o))) = h else {
        return Err(internal("PixelWriterDrawHandler was not created"));
    };
    Ok(scope.root(o))
}

/// `FillPath.FillPath(SunGraphics2D, SurfaceData, int, int, Path2D.Float)`.
pub(super) fn n_fill_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    path_common(ctx, args, "fillPath")
}

/// `DrawPath.DrawPath(SunGraphics2D, SurfaceData, int, int, Path2D.Float)`.
pub(super) fn n_draw_path(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    path_common(ctx, args, "drawPath")
}

fn path_common(ctx: &mut dyn NativeContext, args: &[Value], method: &str) -> MethodCallResult {
    let (tx, ty) = (arg_int(args, 3), arg_int(args, 4));
    let p2df: ObjectRef = arg_obj(args, 5).ok_or_else(|| internal("path loop: null path"))?;
    prologue!(ctx, args, sg2d, sd, scope, sg, sdh, pw, clip, hint);
    let _ = (sg2d, sd);
    let p_h = scope.root(p2df);
    let hh = handler(&mut scope, &sdh, &pw, &clip, hint)?;
    let (h, p) = (scope.get(&hh), scope.get(&p_h));
    let _ = &sg;
    scope.invoke_static(
        "sun/java2d/loops/ProcessPath",
        method,
        &format!("(L{};Ljava/awt/geom/Path2D$Float;II)Z", HANDLER),
        &[
            Value::Object(Some(h)),
            Value::Object(Some(p)),
            Value::Int(tx),
            Value::Int(ty),
        ],
    )?;
    void()
}

/// `DrawRect.DrawRect(SunGraphics2D, SurfaceData, int, int, int, int)`.
pub(super) fn n_draw_rect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (x, y, w, h) = (
        arg_int(args, 3),
        arg_int(args, 4),
        arg_int(args, 5),
        arg_int(args, 6),
    );
    prologue!(ctx, args, sg2d, sd, scope, sg, sdh, pw, clip, hint);
    let _ = (sg2d, sd, hint, clip);
    let (s, d, p) = (scope.get(&sg), scope.get(&sdh), scope.get(&pw));
    scope.invoke_static(
        "sun/java2d/loops/GeneralRenderer",
        "doDrawRect",
        &format!("(L{};Lsun/java2d/SunGraphics2D;L{};IIII)V", PW, SD),
        &[
            Value::Object(Some(p)),
            Value::Object(Some(s)),
            Value::Object(Some(d)),
            Value::Int(x),
            Value::Int(y),
            Value::Int(w),
            Value::Int(h),
        ],
    )?;
    void()
}

/// `DrawPolygons.DrawPolygons(SunGraphics2D, SurfaceData, int[], int[], int[], int, int, int, boolean)`.
pub(super) fn n_draw_polygons(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let xs = arg_obj(args, 3).ok_or_else(|| internal("DrawPolygons: null xPoints"))?;
    let ys = arg_obj(args, 4).ok_or_else(|| internal("DrawPolygons: null yPoints"))?;
    let np = arg_obj(args, 5).ok_or_else(|| internal("DrawPolygons: null nPoints"))?;
    let num = arg_int(args, 6).max(0) as usize;
    let (tx, ty) = (arg_int(args, 7), arg_int(args, 8));
    let close = arg_int(args, 9);
    prologue!(ctx, args, sg2d, sd, scope, sg, sdh, pw, clip, hint);
    let _ = (sg2d, sd, hint, sg);
    let (xh, yh) = (scope.root(xs), scope.root(ys));
    let mut counts = vec![0i32; num];
    {
        let nh = scope.root(np);
        let n = scope.get(&nh);
        scope.read_int_array_into(n, 0, &mut counts);
    }
    let mut off = 0i32;
    for n in counts {
        let (s, p, c) = (scope.get(&sdh), scope.get(&pw), scope.get(&clip));
        let (x, y) = (scope.get(&xh), scope.get(&yh));
        scope.invoke_static(
            "sun/java2d/loops/GeneralRenderer",
            "doDrawPoly",
            &format!("(L{};L{};[I[IIIL{};IIZ)V", SD, PW, REGION),
            &[
                Value::Object(Some(s)),
                Value::Object(Some(p)),
                Value::Object(Some(x)),
                Value::Object(Some(y)),
                Value::Int(off),
                Value::Int(n),
                Value::Object(Some(c)),
                Value::Int(tx),
                Value::Int(ty),
                Value::Int(close),
            ],
        )?;
        off += n;
    }
    void()
}
