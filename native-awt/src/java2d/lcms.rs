// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `sun.java2d.cmm.lcms.LCMS` -- the JDK's `liblcms` JNI bridge (`LCMS.c`) over
//! `moxcms`. The genuine Java classes (`LCMS`, `LCMSTransform`, `LCMSProfile`,
//! `LCMSImageLayout`, `ICC_Profile`) run unmodified; these are the seven
//! natives they call.
//!
//! Profiles are kept as their ICC bytes (a profile id is an opaque handle);
//! tags are read and rewritten straight from the tag table. A transform is a
//! chain of `moxcms` float transforms, one per adjacent profile pair, between
//! two lcms pixel formatters (`PT_*` words built by `LCMSImageLayout`).

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};
use moxcms::{
    ColorProfile, DataColorSpace, Layout, RenderingIntent, TransformExecutor, TransformOptions,
};

const LCMS: &str = "sun/java2d/cmm/lcms/LCMS";

type F32Exec = Arc<dyn TransformExecutor<f32> + Send + Sync>;

enum Exec {
    Mox(F32Exec),
    Icc(super::icc::IccStage),
}

struct Stage {
    exec: Exec,
    in_ch: usize,
    out_ch: usize,
}

struct Xform {
    stages: Vec<Stage>,
    in_fmt: Fmt,
    out_fmt: Fmt,
}

#[derive(Default)]
struct Table {
    next: i64,
    profiles: HashMap<i64, Vec<u8>>,
    xforms: HashMap<i64, Arc<Xform>>,
}

static LCMS_TABLE: VmScoped<Table> = VmScoped::new();

/// Drop one VM's profiles and transforms.
pub fn forget_vm(vm: usize) {
    LCMS_TABLE.forget(vm);
}

/// An lcms pixel formatter word (`cmsFormatter`), the bits `LCMSImageLayout` sets.
#[derive(Clone, Copy)]
struct Fmt {
    chans: usize,
    extra: usize,
    bytes: usize,
    doswap: bool,
    swapfirst: bool,
    premul: bool,
}

impl Fmt {
    fn decode(f: i32) -> Self {
        Self {
            bytes: (f & 7) as usize,
            chans: ((f >> 3) & 15) as usize,
            extra: ((f >> 7) & 7) as usize,
            doswap: (f >> 10) & 1 != 0,
            swapfirst: (f >> 14) & 1 != 0,
            premul: (f >> 23) & 1 != 0,
        }
    }

    fn pixel_bytes(&self) -> usize {
        (self.chans + self.extra) * self.bytes
    }

    /// Whether the extra channels precede the colour ones in memory.
    fn extras_first(&self) -> bool {
        self.extra > 0 && (self.doswap ^ self.swapfirst)
    }

    /// Memory sample index of colour channel `c`.
    fn color_slot(&self, c: usize) -> usize {
        let base = if self.extras_first() { self.extra } else { 0 };
        base + if self.doswap { self.chans - 1 - c } else { c }
    }

    fn extra_slot(&self, e: usize) -> usize {
        if self.extras_first() {
            e
        } else {
            self.chans + e
        }
    }

    fn max(&self) -> f32 {
        if self.bytes == 1 {
            255.0
        } else {
            65535.0
        }
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

fn throw_cmm(ctx: &mut dyn NativeContext, msg: &str) -> MethodCallFailed {
    let s = ctx.create_string(msg);
    match ctx.construct_throwable(
        "java/awt/color/CMMException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(s))],
    ) {
        Ok(Some(obj)) => MethodCallFailed::ExceptionThrown(obj),
        Err(e) => e,
        Ok(None) => RuntimeError::InternalError {
            message: msg.to_owned(),
        }
        .into(),
    }
}

fn read_bytes(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let mut v = vec![0u8; ctx.array_length(arr)];
    ctx.read_byte_array_into(arr, 0, &mut v);
    v
}

fn new_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    ctx.write_byte_array_from(arr, 0, data);
    arr
}

// ---------------------------------------------------------------------------
// ICC profile bytes: header and tag table
// ---------------------------------------------------------------------------

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `(signature, offset, size)` of every tag, or `None` for a malformed table.
fn tag_table(p: &[u8]) -> Option<Vec<(u32, usize, usize)>> {
    let n = be32(p, 128)? as usize;
    if n > 4096 {
        return None;
    }
    let mut tags = Vec::with_capacity(n);
    for i in 0..n {
        let at = 132 + i * 12;
        let (sig, off, size) = (be32(p, at)?, be32(p, at + 4)? as usize, be32(p, at + 8)? as usize);
        if off.checked_add(size)? > p.len() {
            return None;
        }
        tags.push((sig, off, size));
    }
    Some(tags)
}

/// lcms's pseudo-tag `head`: the 128-byte profile header.
const SIG_HEAD: u32 = 0x6865_6164;

fn get_tag(p: &[u8], sig: u32) -> Option<Vec<u8>> {
    if sig == SIG_HEAD {
        return p.get(..128).map(<[u8]>::to_vec);
    }
    tag_table(p)?
        .into_iter()
        .find(|t| t.0 == sig)
        .map(|(_, off, size)| p[off..off + size].to_vec())
}

/// `p` with tag `sig` set to `data` (added if missing), laid out afresh.
fn set_tag(p: &[u8], sig: u32, data: &[u8]) -> Option<Vec<u8>> {
    if sig == SIG_HEAD {
        if data.len() < 128 {
            return None;
        }
        let mut out = p.to_vec();
        out[4..128].copy_from_slice(&data[4..128]);
        return Some(out);
    }
    let mut tags: Vec<(u32, Vec<u8>)> = tag_table(p)?
        .into_iter()
        .map(|(s, off, size)| (s, p[off..off + size].to_vec()))
        .collect();
    match tags.iter_mut().find(|t| t.0 == sig) {
        Some(t) => t.1 = data.to_vec(),
        None => tags.push((sig, data.to_vec())),
    }
    let mut out = p.get(..128)?.to_vec();
    out.extend_from_slice(&(tags.len() as u32).to_be_bytes());
    let mut off = 132 + tags.len() * 12;
    let mut table = Vec::new();
    let mut body = Vec::new();
    for (s, d) in &tags {
        table.extend_from_slice(&s.to_be_bytes());
        table.extend_from_slice(&(off as u32).to_be_bytes());
        table.extend_from_slice(&(d.len() as u32).to_be_bytes());
        body.extend_from_slice(d);
        while body.len() % 4 != 0 {
            body.push(0);
        }
        off = 132 + tags.len() * 12 + body.len();
    }
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    let total = out.len() as u32;
    out[0..4].copy_from_slice(&total.to_be_bytes());
    Some(out)
}

/// Why `cmsSaveProfileToMem` refuses `p`, if it does. A version 4 profile writes its
/// device/PCS tables as `lutAtoBType` / `lutBtoAType`, which hold only curves, an optional
/// matrix and CLUT in the `mAB` order; a legacy `mft1` / `mft2` table with three input
/// channels and a non-identity matrix reads as `matrix, curves, CLUT, curves` and has no
/// such encoding ("LCMS error 13: LUT is not suitable to be saved as LutBToA").
fn unsaveable(p: &[u8]) -> Option<&'static str> {
    if p.get(8).copied()? < 4 {
        return None;
    }
    const A2B: [u32; 4] = [0x4132_4230, 0x4132_4231, 0x4132_4232, 0x7072_6530];
    const B2A: [u32; 4] = [0x4232_4130, 0x4232_4131, 0x4232_4132, 0x6761_6d74];
    for (sig, off, size) in tag_table(p)? {
        let is_a2b = A2B.contains(&sig);
        if !is_a2b && !B2A.contains(&sig) {
            continue;
        }
        let t = &p[off..off + size];
        let legacy = matches!(t.get(..4), Some(b"mft1") | Some(b"mft2"));
        if !legacy || t.len() < 48 || t[8] != 3 {
            continue;
        }
        let identity = (0..9).all(|k| {
            let v = i32::from_be_bytes([t[12 + 4 * k], t[13 + 4 * k], t[14 + 4 * k], t[15 + 4 * k]]);
            let want = if k % 4 == 0 { 65536 } else { 0 };
            (v - want).abs() <= 1
        });
        if !identity {
            return Some(if is_a2b {
                "LCMS error 13: LUT is not suitable to be saved as LutAtoB"
            } else {
                "LCMS error 13: LUT is not suitable to be saved as LutBToA"
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The natives
// ---------------------------------------------------------------------------

/// `LCMS.loadProfileNative(byte[] data, Object ref)J`.
fn load_profile(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(arr) = arg_obj(args, 0) else {
        return Err(throw_cmm(ctx, "Cannot open profile"));
    };
    let data = read_bytes(ctx, arr);
    if data.len() < 132 || be32(&data, 36) != Some(0x6163_7370) || tag_table(&data).is_none() {
        return Err(throw_cmm(ctx, "Cannot open profile"));
    }
    let vm = ctx.vm_identity();
    let id = LCMS_TABLE.with(vm, |t| {
        t.next += 1;
        let id = t.next << 4;
        t.profiles.insert(id, data);
        id
    });
    Ok(Some(Value::Long(id)))
}

/// `LCMS.getProfileDataNative(long)[B`.
fn get_profile_data(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let id = arg_long(args, 0);
    let data = LCMS_TABLE.with(vm, |t| t.profiles.get(&id).cloned());
    if let Some(msg) = data.as_deref().and_then(unsaveable) {
        return Err(throw_cmm(ctx, msg));
    }
    match data {
        Some(d) => {
            let arr = new_byte_array(ctx, &d);
            Ok(Some(Value::Object(Some(arr))))
        }
        None => Err(throw_cmm(ctx, "Cannot get profile data")),
    }
}

/// `LCMS.getTagNative(long, int)[B`.
fn get_tag_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let (id, sig) = (arg_long(args, 0), arg_int(args, 1) as u32);
    let tag = LCMS_TABLE.with(vm, |t| t.profiles.get(&id).and_then(|p| get_tag(p, sig)));
    match tag {
        Some(d) => {
            let arr = new_byte_array(ctx, &d);
            Ok(Some(Value::Object(Some(arr))))
        }
        None => Err(throw_cmm(ctx, "ICC profile tag not found")),
    }
}

/// `LCMS.setTagDataNative(long, int, byte[])V`.
fn set_tag_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let (id, sig) = (arg_long(args, 0), arg_int(args, 1) as u32);
    let Some(arr) = arg_obj(args, 2) else {
        return Err(throw_cmm(ctx, "Cannot set tag data"));
    };
    let data = read_bytes(ctx, arr);
    let done = LCMS_TABLE.with(vm, |t| {
        let p = t.profiles.get(&id)?;
        let np = set_tag(p, sig, &data)?;
        t.profiles.insert(id, np);
        Some(())
    });
    match done {
        Some(()) => Ok(None),
        None => Err(throw_cmm(ctx, "Cannot set tag data")),
    }
}

fn layout_of(space: DataColorSpace) -> Option<Layout> {
    match space {
        DataColorSpace::Rgb => Some(Layout::Rgb),
        DataColorSpace::Gray => Some(Layout::Gray),
        DataColorSpace::Cmyk => Some(Layout::Cmyka),
        _ => None,
    }
}

fn channels_of(layout: Layout) -> usize {
    match layout {
        Layout::Rgb => 3,
        Layout::Gray => 1,
        _ => 4,
    }
}

fn build_xform(
    profiles: &[Vec<u8>],
    intent: i32,
    in_fmt: Fmt,
    out_fmt: Fmt,
) -> Result<Xform, String> {
    if profiles.len() < 2 {
        return Err("Cannot create transform".into());
    }
    let rendering_intent = match intent {
        1 => RenderingIntent::RelativeColorimetric,
        2 => RenderingIntent::Saturation,
        3 => RenderingIntent::AbsoluteColorimetric,
        _ => RenderingIntent::Perceptual,
    };
    let options = TransformOptions {
        rendering_intent,
        ..TransformOptions::default()
    };
    let mut stages = Vec::new();
    for pair in profiles.windows(2) {
        // `moxcms` takes the usual matrix/TRC and LUT profiles; whatever it refuses
        // (abstract and `3CLR` profiles, XYZ / Lab device spaces) goes to the table
        // evaluator in `icc`.
        let mox = (|| {
            let (src, dst) = (
                ColorProfile::new_from_slice(&pair[0]).ok()?,
                ColorProfile::new_from_slice(&pair[1]).ok()?,
            );
            let (sl, dl) = (layout_of(src.color_space)?, layout_of(dst.color_space)?);
            let exec = src.create_transform_f32(sl, &dst, dl, options).ok()?;
            Some(Stage {
                exec: Exec::Mox(exec),
                in_ch: channels_of(sl),
                out_ch: channels_of(dl),
            })
        })();
        let stage = match mox {
            Some(s) => s,
            None => {
                let (Some(src), Some(dst)) = (
                    super::icc::Profile::parse(&pair[0]),
                    super::icc::Profile::parse(&pair[1]),
                ) else {
                    return Err("Cannot create transform".into());
                };
                let (in_ch, out_ch) = (src.channels(), dst.channels());
                Stage {
                    exec: Exec::Icc(super::icc::IccStage::new(src, dst, intent)),
                    in_ch,
                    out_ch,
                }
            }
        };
        stages.push(stage);
    }
    let first = stages.first().map(|s| s.in_ch).unwrap_or(0);
    let last = stages.last().map(|s| s.out_ch).unwrap_or(0);
    if first != in_fmt.chans || last != out_fmt.chans || in_fmt.bytes == 0 || out_fmt.bytes == 0 || in_fmt.bytes > 2 || out_fmt.bytes > 2 {
        return Err("Cannot create transform".into());
    }
    Ok(Xform {
        stages,
        in_fmt,
        out_fmt,
    })
}

/// `LCMS.createNativeTransform(long[], int, int, int, Object)J`.
fn create_transform(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(ids_arr) = arg_obj(args, 0) else {
        return Err(throw_cmm(ctx, "Unknown profile ID"));
    };
    let ids: Vec<i64> = (0..ctx.array_length(ids_arr))
        .map(|i| match ctx.get_array_element(ids_arr, i) {
            Value::Long(v) => v,
            _ => 0,
        })
        .collect();
    let intent = arg_int(args, 1);
    let (in_fmt, out_fmt) = (Fmt::decode(arg_int(args, 2)), Fmt::decode(arg_int(args, 3)));
    let vm = ctx.vm_identity();
    let profiles: Option<Vec<Vec<u8>>> = LCMS_TABLE.with(vm, |t| {
        ids.iter().map(|id| t.profiles.get(id).cloned()).collect()
    });
    let Some(profiles) = profiles else {
        return Err(throw_cmm(ctx, "Unknown profile ID"));
    };
    match build_xform(&profiles, intent, in_fmt, out_fmt) {
        Ok(x) => {
            let id = LCMS_TABLE.with(vm, |t| {
                t.next += 1;
                let id = t.next << 4;
                t.xforms.insert(id, Arc::new(x));
                id
            });
            Ok(Some(Value::Long(id)))
        }
        Err(msg) => Err(throw_cmm(ctx, &format!("LCMS error: {msg}"))),
    }
}

/// The Java array behind a layout as little-endian bytes, by `DT_*` code.
fn read_layout_array(ctx: &dyn NativeContext, arr: ObjectRef, dt: i32) -> Vec<u8> {
    let n = ctx.array_length(arr);
    match dt {
        0 => read_bytes(ctx, arr),
        1 => {
            let mut v = vec![0u16; n];
            ctx.read_char_array_into(arr, 0, &mut v);
            v.iter().flat_map(|s| s.to_le_bytes()).collect()
        }
        _ => {
            let mut v = vec![0i32; n];
            ctx.read_int_array_into(arr, 0, &mut v);
            v.iter().flat_map(|s| s.to_le_bytes()).collect()
        }
    }
}

fn write_layout_array(ctx: &mut dyn NativeContext, arr: ObjectRef, dt: i32, bytes: &[u8]) {
    match dt {
        0 => {
            ctx.write_byte_array_from(arr, 0, bytes);
        }
        1 => {
            let v: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            ctx.write_char_array_from(arr, 0, &v);
        }
        _ => {
            let v: Vec<i32> = bytes
                .chunks_exact(4)
                .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            ctx.write_int_array_from(arr, 0, &v);
        }
    }
}

fn sample(buf: &[u8], at: usize, bytes: usize) -> f32 {
    if bytes == 1 {
        buf[at] as f32
    } else {
        u16::from_le_bytes([buf[at], buf[at + 1]]) as f32
    }
}

fn put_sample(buf: &mut [u8], at: usize, bytes: usize, v: f32, max: f32) {
    let q = (v * max + 0.5).floor().clamp(0.0, max) as u32;
    if bytes == 1 {
        buf[at] = q as u8;
    } else {
        buf[at..at + 2].copy_from_slice(&(q as u16).to_le_bytes());
    }
}

/// `LCMS.colorConvert(long, int, int, int, int, int, int, Object, Object, int, int)V`.
fn color_convert(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 0);
    let (w, h) = (arg_int(args, 1).max(0) as usize, arg_int(args, 2).max(0) as usize);
    let (src_off, src_row) = (arg_int(args, 3).max(0) as usize, arg_int(args, 4).max(0) as usize);
    let (dst_off, dst_row) = (arg_int(args, 5).max(0) as usize, arg_int(args, 6).max(0) as usize);
    let (Some(src_arr), Some(dst_arr)) = (arg_obj(args, 7), arg_obj(args, 8)) else {
        return Err(throw_cmm(ctx, "Cannot convert: no data"));
    };
    let (src_dt, dst_dt) = (arg_int(args, 9), arg_int(args, 10));
    let vm = ctx.vm_identity();
    let Some(x) = LCMS_TABLE.with(vm, |t| t.xforms.get(&id).cloned()) else {
        return Err(throw_cmm(ctx, "Cannot get transform"));
    };
    let src = read_layout_array(ctx, src_arr, src_dt);
    let mut dst = if src_arr == dst_arr {
        src.clone()
    } else {
        read_layout_array(ctx, dst_arr, dst_dt)
    };
    let (fi, fo) = (x.in_fmt, x.out_fmt);
    let (spx, dpx) = (fi.pixel_bytes(), fo.pixel_bytes());
    let in_ch = fi.chans;
    let mut cur = vec![0f32; w * in_ch];
    for y in 0..h {
        let mut alphas = vec![1f32; w];
        let row = src_off + y * src_row;
        if row + w * spx > src.len() {
            return Err(throw_cmm(ctx, "Invalid image layout"));
        }
        for px in 0..w {
            let base = row + px * spx;
            let a = if fi.extra > 0 {
                sample(&src, base + fi.extra_slot(0) * fi.bytes, fi.bytes) / fi.max()
            } else {
                1.0
            };
            alphas[px] = a;
            for c in 0..in_ch {
                let mut v = sample(&src, base + fi.color_slot(c) * fi.bytes, fi.bytes) / fi.max();
                if fi.premul && fi.extra > 0 {
                    v = if a > 0.0 { (v / a).min(1.0) } else { 0.0 };
                }
                cur[px * in_ch + c] = v;
            }
        }
        let mut buf = cur.clone();
        for st in &x.stages {
            let mut next = vec![0f32; w * st.out_ch];
            match &st.exec {
                Exec::Mox(e) => {
                    if e.transform(&buf[..w * st.in_ch], &mut next).is_err() {
                        return Err(throw_cmm(ctx, "LCMS error: transform failed"));
                    }
                }
                Exec::Icc(s) => s.transform(&buf[..w * st.in_ch], &mut next),
            }
            buf = next;
        }
        let orow = dst_off + y * dst_row;
        if orow + w * dpx > dst.len() {
            return Err(throw_cmm(ctx, "Invalid image layout"));
        }
        let out_ch = fo.chans;
        for px in 0..w {
            let base = orow + px * dpx;
            let a = alphas[px];
            for c in 0..out_ch {
                let mut v = buf[px * out_ch + c].clamp(0.0, 1.0);
                if fo.premul && fo.extra > 0 && fi.extra > 0 {
                    v *= a;
                }
                put_sample(&mut dst, base + fo.color_slot(c) * fo.bytes, fo.bytes, v, fo.max());
            }
            if fo.extra > 0 && fi.extra > 0 {
                put_sample(&mut dst, base + fo.extra_slot(0) * fo.bytes, fo.bytes, a, fo.max());
            }
        }
    }
    write_layout_array(ctx, dst_arr, dst_dt, &dst);
    Ok(None)
}

/// Register the `LCMS` natives (the caller sets the `Bridge` category).
pub fn register(registry: &mut NativeMethodRegistry) {
    registry.register(LCMS, "loadProfileNative", "([BLjava/lang/Object;)J", load_profile);
    registry.register(LCMS, "getProfileDataNative", "(J)[B", get_profile_data);
    registry.register(LCMS, "getTagNative", "(JI)[B", get_tag_native);
    registry.register(LCMS, "setTagDataNative", "(JI[B)V", set_tag_native);
    registry.register(LCMS, "createNativeTransform", "([JIIILjava/lang/Object;)J", create_transform);
    registry.register(
        LCMS,
        "colorConvert",
        "(JIIIIIILjava/lang/Object;Ljava/lang/Object;II)V",
        color_convert,
    );
}
