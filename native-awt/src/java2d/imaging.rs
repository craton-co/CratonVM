// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun.awt.image.ImagingLib` -- the JDK's medialib (`libmlib_image`) bridge --
//! for the two operations that have no Java fallback: `ConvolveOp` and
//! `AffineTransformOp` (without them both throw `ImagingOpException`). The
//! lookup operations answer 0, which `ImagingLib.filter` reads as "not done"
//! and `LookupOp` then runs its Java code.
//!
//! Like medialib the kernels work on the RAW bands of a raster (no colour
//! model, no premultiplication). The raster is read and written through its
//! own `getPixels` / `setPixels`, so any sample model works.

use cratonvm_native_api::{
    NativeContext, NativeHandle, NativeHandleScope, NativeMethodRegistry,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

const IMAGING_LIB: &str = "sun/awt/image/ImagingLib";

const EDGE_NO_OP: i32 = 1;
const TYPE_NEAREST_NEIGHBOR: i32 = 1;
const TYPE_BICUBIC: i32 = 3;

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

fn int_of(v: Option<Value>) -> i32 {
    match v {
        Some(Value::Int(i)) => i,
        _ => 0,
    }
}

fn obj_of(v: Option<Value>) -> Option<ObjectRef> {
    match v {
        Some(Value::Object(Some(o))) => Some(o),
        _ => None,
    }
}

fn call_int(
    scope: &mut NativeHandleScope<'_>,
    h: &NativeHandle,
    name: &str,
) -> Result<i32, MethodCallFailed> {
    let recv = scope.get(h);
    Ok(int_of(scope.invoke_virtual(recv, name, "()I", &[])?))
}

/// One raster's raw samples, band-interleaved, row-major.
struct Img {
    x: i32,
    y: i32,
    w: usize,
    h: usize,
    bands: usize,
    max: Vec<i32>,
    data: Vec<i32>,
}

impl Img {
    #[inline]
    fn at(&self, x: usize, y: usize, b: usize) -> i32 {
        self.data[(y * self.w + x) * self.bands + b]
    }
}

fn read_raster(
    scope: &mut NativeHandleScope<'_>,
    raster: &NativeHandle,
) -> Result<Img, MethodCallFailed> {
    let x = call_int(scope, raster, "getMinX")?;
    let y = call_int(scope, raster, "getMinY")?;
    let w = call_int(scope, raster, "getWidth")?.max(0);
    let h = call_int(scope, raster, "getHeight")?.max(0);
    let bands = call_int(scope, raster, "getNumBands")?.max(0) as usize;
    let recv = scope.get(raster);
    let sm = obj_of(scope.invoke_virtual(recv, "getSampleModel", "()Ljava/awt/image/SampleModel;", &[])?);
    let mut max = vec![255; bands];
    if let Some(sm) = sm {
        for (b, m) in max.iter_mut().enumerate() {
            let bits = int_of(scope.invoke_virtual(sm, "getSampleSize", "(I)I", &[Value::Int(b as i32)])?);
            if (1..31).contains(&bits) {
                *m = (1i32 << bits) - 1;
            }
        }
    }
    let recv = scope.get(raster);
    let arr = obj_of(scope.invoke_virtual(
        recv,
        "getPixels",
        "(IIII[I)[I",
        &[
            Value::Int(x),
            Value::Int(y),
            Value::Int(w),
            Value::Int(h),
            Value::Object(None),
        ],
    )?);
    let mut data = vec![0i32; w as usize * h as usize * bands];
    if let Some(a) = arr {
        let n = scope.array_length(a).min(data.len());
        scope.read_int_array_into(a, 0, &mut data[..n]);
    }
    Ok(Img {
        x,
        y,
        w: w as usize,
        h: h as usize,
        bands,
        max,
        data,
    })
}

fn write_raster(
    scope: &mut NativeHandleScope<'_>,
    raster: &NativeHandle,
    img: &Img,
    data: &[i32],
) -> Result<(), MethodCallFailed> {
    let arr = scope.new_array(ArrayElementType::Int, data.len());
    scope.write_int_array_from(arr, 0, data);
    let recv = scope.get(raster);
    scope.invoke_virtual(
        recv,
        "setPixels",
        "(IIII[I)V",
        &[
            Value::Int(img.x),
            Value::Int(img.y),
            Value::Int(img.w as i32),
            Value::Int(img.h as i32),
            Value::Object(Some(arr)),
        ],
    )?;
    Ok(())
}

/// The raster behind a `BufferedImage` (`getRaster()`), rooted in `scope`.
fn raster_of_image(
    scope: &mut NativeHandleScope<'_>,
    image: ObjectRef,
) -> Result<Option<NativeHandle>, MethodCallFailed> {
    let r = obj_of(scope.invoke_virtual(image, "getRaster", "()Ljava/awt/image/WritableRaster;", &[])?);
    Ok(r.map(|r| scope.root(r)))
}

/// Medialib's convolution: the kernel is flipped (the JDK does so before
/// handing it over) and applied as a correlation around the origin
/// `((w-1)/2, (h-1)/2)`; edge pixels (the window leaves the image) are copied
/// from the source or zeroed.
fn convolve(img: &Img, kw: usize, kh: usize, k: &[f32], edge: i32) -> Vec<i32> {
    let (dm, dn) = ((kw - 1) / 2, (kh - 1) / 2);
    let mut out = vec![0i32; img.data.len()];
    for y in 0..img.h {
        for x in 0..img.w {
            let inside = x >= dm && y >= dn && x + (kw - 1 - dm) < img.w && y + (kh - 1 - dn) < img.h;
            for b in 0..img.bands {
                let o = (y * img.w + x) * img.bands + b;
                if !inside {
                    out[o] = if edge == EDGE_NO_OP { img.at(x, y, b) } else { 0 };
                    continue;
                }
                let mut sum = 0f32;
                for j in 0..kh {
                    for i in 0..kw {
                        let kf = k[(kh - 1 - j) * kw + (kw - 1 - i)];
                        sum += kf * img.at(x + i - dm, y + j - dn, b) as f32;
                    }
                }
                out[o] = (sum + 0.5).floor().clamp(0.0, img.max[b] as f32) as i32;
            }
        }
    }
    out
}

fn cubic_weights(t: f64) -> [f64; 4] {
    // Catmull-Rom (medialib MLIB_BICUBIC, a = -0.5).
    let a = -0.5;
    let t2 = t * t;
    let t3 = t2 * t;
    [
        a * t3 - 2.0 * a * t2 + a * t,
        (a + 2.0) * t3 - (a + 3.0) * t2 + 1.0,
        -(a + 2.0) * t3 + (2.0 * a + 3.0) * t2 - a * t,
        -a * t3 + a * t2,
    ]
}

/// Medialib's affine transform: each destination pixel centre is mapped back
/// through the inverse of `m` (`[m00 m10 m01 m11 m02 m12]`); a pixel whose
/// centre lands inside the source is sampled with edge extension, the rest of
/// the destination is left as it was.
fn transform(src: &Img, dst: &Img, m: &[f64], interp: i32) -> Option<Vec<i32>> {
    let det = m[0] * m[3] - m[1] * m[2];
    if det.abs() < 1e-12 || src.w == 0 || src.h == 0 {
        return None;
    }
    let (a, b, c, d, e, f) = (m[0], m[1], m[2], m[3], m[4], m[5]);
    let inv = [d / det, -b / det, -c / det, a / det];
    let mut out = vec![0i32; dst.data.len()];
    let (sw, sh) = (src.w as i64, src.h as i64);
    let clampx = |v: i64| v.clamp(0, sw - 1) as usize;
    let clampy = |v: i64| v.clamp(0, sh - 1) as usize;
    for dy in 0..dst.h {
        for dx in 0..dst.w {
            let px = dx as f64 + 0.5 - e;
            let py = dy as f64 + 0.5 - f;
            let sx = inv[0] * px + inv[2] * py;
            let sy = inv[1] * px + inv[3] * py;
            if sx < 0.0 || sy < 0.0 || sx >= sw as f64 || sy >= sh as f64 {
                continue;
            }
            let (fx, fy) = (sx - 0.5, sy - 0.5);
            for band in 0..dst.bands.min(src.bands) {
                let max = dst.max[band] as f64;
                let v = match interp {
                    TYPE_NEAREST_NEIGHBOR => src.at(clampx(sx.floor() as i64), clampy(sy.floor() as i64), band) as f64,
                    TYPE_BICUBIC => {
                        let (ix, iy) = (fx.floor() as i64, fy.floor() as i64);
                        let wx = cubic_weights(fx - ix as f64);
                        let wy = cubic_weights(fy - iy as f64);
                        let mut acc = 0.0;
                        for (j, wyj) in wy.iter().enumerate() {
                            for (i, wxi) in wx.iter().enumerate() {
                                acc += wyj
                                    * wxi
                                    * src.at(clampx(ix + i as i64 - 1), clampy(iy + j as i64 - 1), band) as f64;
                            }
                        }
                        acc
                    }
                    _ => {
                        let (ix, iy) = (fx.floor() as i64, fy.floor() as i64);
                        let (tx, ty) = (fx - ix as f64, fy - iy as f64);
                        let p = |xx: i64, yy: i64| src.at(clampx(xx), clampy(yy), band) as f64;
                        (p(ix, iy) * (1.0 - tx) + p(ix + 1, iy) * tx) * (1.0 - ty)
                            + (p(ix, iy + 1) * (1.0 - tx) + p(ix + 1, iy + 1) * tx) * ty
                    }
                };
                out[(dy * dst.w + dx) * dst.bands + band] = (v + 0.5).floor().clamp(0.0, max) as i32;
            }
        }
    }
    Some(out)
}

fn kernel_of(
    scope: &mut NativeHandleScope<'_>,
    kernel: ObjectRef,
) -> Result<(usize, usize, Vec<f32>), MethodCallFailed> {
    let w = int_of(scope.invoke_virtual(kernel, "getWidth", "()I", &[])?).max(1) as usize;
    let h = int_of(scope.invoke_virtual(kernel, "getHeight", "()I", &[])?).max(1) as usize;
    let arr = obj_of(scope.invoke_virtual(kernel, "getKernelData", "([F)[F", &[Value::Object(None)])?);
    let mut k = vec![0f32; w * h];
    if let Some(a) = arr {
        for (i, slot) in k.iter_mut().enumerate().take(scope.array_length(a)) {
            if let Value::Float(f) = scope.get_array_element(a, i) {
                *slot = f;
            }
        }
    }
    Ok((w, h, k))
}

fn matrix_of(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<f64> {
    (0..ctx.array_length(arr).min(6))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Double(d) => d,
            _ => 0.0,
        })
        .collect()
}

/// Shared body of `convolveBI` / `convolveRaster`.
fn do_convolve(ctx: &mut dyn NativeContext, args: &[Value], images: bool) -> MethodCallResult {
    let (Some(src), Some(dst), Some(kernel)) = (arg_obj(args, 0), arg_obj(args, 1), arg_obj(args, 2)) else {
        return Ok(Some(Value::Int(0)));
    };
    let edge = arg_int(args, 3);
    let mut scope = NativeHandleScope::new(ctx);
    let src_r = if images { raster_of_image(&mut scope, src)? } else { Some(scope.root(src)) };
    let dst_r = if images { raster_of_image(&mut scope, dst)? } else { Some(scope.root(dst)) };
    let (Some(src_r), Some(dst_r)) = (src_r, dst_r) else {
        return Ok(Some(Value::Int(0)));
    };
    let kernel = scope.root(kernel);
    let kobj = scope.get(&kernel);
    let (kw, kh, k) = kernel_of(&mut scope, kobj)?;
    let img = read_raster(&mut scope, &src_r)?;
    let dimg = read_raster(&mut scope, &dst_r)?;
    if img.w != dimg.w || img.h != dimg.h || img.bands != dimg.bands {
        return Ok(Some(Value::Int(0)));
    }
    let out = convolve(&img, kw, kh, &k, edge);
    write_raster(&mut scope, &dst_r, &dimg, &out)?;
    Ok(Some(Value::Int(1)))
}

/// Shared body of `transformBI` / `transformRaster`.
fn do_transform(ctx: &mut dyn NativeContext, args: &[Value], images: bool) -> MethodCallResult {
    let (Some(src), Some(dst), Some(mtx)) = (arg_obj(args, 0), arg_obj(args, 1), arg_obj(args, 2)) else {
        return Ok(Some(Value::Int(0)));
    };
    let interp = arg_int(args, 3);
    let m = matrix_of(ctx, mtx);
    if m.len() < 6 {
        return Ok(Some(Value::Int(0)));
    }
    let mut scope = NativeHandleScope::new(ctx);
    let src_r = if images { raster_of_image(&mut scope, src)? } else { Some(scope.root(src)) };
    let dst_r = if images { raster_of_image(&mut scope, dst)? } else { Some(scope.root(dst)) };
    let (Some(src_r), Some(dst_r)) = (src_r, dst_r) else {
        return Ok(Some(Value::Int(0)));
    };
    let img = read_raster(&mut scope, &src_r)?;
    let dimg = read_raster(&mut scope, &dst_r)?;
    let Some(painted) = transform(&img, &dimg, &m, interp) else {
        return Ok(Some(Value::Int(0)));
    };
    // Pixels the mapping does not reach keep what the destination held.
    let mut out = dimg.data.clone();
    let (sw, sh) = (img.w as f64, img.h as f64);
    let det = m[0] * m[3] - m[1] * m[2];
    let inv = [m[3] / det, -m[1] / det, -m[2] / det, m[0] / det];
    for y in 0..dimg.h {
        for x in 0..dimg.w {
            let px = x as f64 + 0.5 - m[4];
            let py = y as f64 + 0.5 - m[5];
            let sx = inv[0] * px + inv[2] * py;
            let sy = inv[1] * px + inv[3] * py;
            if sx >= 0.0 && sy >= 0.0 && sx < sw && sy < sh {
                let o = (y * dimg.w + x) * dimg.bands;
                out[o..o + dimg.bands].copy_from_slice(&painted[o..o + dimg.bands]);
            }
        }
    }
    write_raster(&mut scope, &dst_r, &dimg, &out)?;
    Ok(Some(Value::Int(1)))
}

fn n_true(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(1)))
}

fn n_not_done(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// Register the `ImagingLib` natives (the caller sets the `Bridge` category).
pub fn register(registry: &mut NativeMethodRegistry) {
    registry.register(IMAGING_LIB, "init", "()Z", n_true);
    registry.register(
        IMAGING_LIB,
        "convolveBI",
        "(Ljava/awt/image/BufferedImage;Ljava/awt/image/BufferedImage;Ljava/awt/image/Kernel;I)I",
        |ctx, args| do_convolve(ctx, args, true),
    );
    registry.register(
        IMAGING_LIB,
        "convolveRaster",
        "(Ljava/awt/image/Raster;Ljava/awt/image/Raster;Ljava/awt/image/Kernel;I)I",
        |ctx, args| do_convolve(ctx, args, false),
    );
    registry.register(
        IMAGING_LIB,
        "transformBI",
        "(Ljava/awt/image/BufferedImage;Ljava/awt/image/BufferedImage;[DI)I",
        |ctx, args| do_transform(ctx, args, true),
    );
    registry.register(
        IMAGING_LIB,
        "transformRaster",
        "(Ljava/awt/image/Raster;Ljava/awt/image/Raster;[DI)I",
        |ctx, args| do_transform(ctx, args, false),
    );
    registry.register(
        IMAGING_LIB,
        "lookupByteBI",
        "(Ljava/awt/image/BufferedImage;Ljava/awt/image/BufferedImage;[[B)I",
        n_not_done,
    );
    registry.register(
        IMAGING_LIB,
        "lookupByteRaster",
        "(Ljava/awt/image/Raster;Ljava/awt/image/Raster;[[B)I",
        n_not_done,
    );
}
