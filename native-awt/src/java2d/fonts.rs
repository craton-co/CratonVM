// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The native half of the JDK's font stack (`libfontmanager`): fontconfig
//! lookups (`FontConfigManager`), the FreeType scaler (`FreetypeFontScaler`)
//! and the glyph cache memory (`StrikeCache`), so the genuine `sun.font.*`
//! classes (`SunFontManager`, `TrueTypeFont`, `FileFontStrike`, `GlyphList`,
//! `FontDesignMetrics`, ...) run on CratonVM.
//!
//! TrueType (`glyf`) fonts at an untransformed size run through the `tt`
//! module, a port of the FreeType 2.14 the JDK bundles (bytecode hinting, the
//! smooth and monochrome rasterisers), so hinted advances and glyph bitmaps
//! match HotSpot bit for bit. Everything else (CFF, rotated or sheared
//! contexts, synthetic bold and italic, LCD) falls back to `ttf-parser`
//! outlines and an exact-area coverage scan converter.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};

use super::tt::font::TtFont;
use super::tt::{GlyphOutline, TtSize};

fn internal(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::InternalError {
        message: msg.into(),
    }
    .into()
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

fn arg_f32(args: &[Value], i: usize) -> f32 {
    match args.get(i) {
        Some(Value::Float(v)) => *v,
        _ => 0.0,
    }
}

fn n_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

// ---------------------------------------------------------------------------
// Installed fonts (what fontconfig would report)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct FontFace {
    file: String,
    index: u32,
    family: String,
    style: String,
    full: String,
    bold: bool,
    italic: bool,
}

fn name_of(face: &ttf_parser::Face<'_>, id: u16) -> Option<String> {
    let mut best: Option<String> = None;
    for n in face.names() {
        if n.name_id != id {
            continue;
        }
        if let Some(s) = n.to_string() {
            // Prefer English (0x409) / Unicode platform names.
            if n.language_id == 0x409 || best.is_none() {
                best = Some(s);
            }
        }
    }
    best
}

fn scan_dir(dir: &std::path::Path, out: &mut Vec<FontFace>, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            scan_dir(&p, out, depth + 1);
            continue;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            continue;
        }
        let Ok(data) = std::fs::read(&p) else {
            continue;
        };
        let n = ttf_parser::fonts_in_collection(&data).unwrap_or(1);
        for idx in 0..n {
            let Ok(face) = ttf_parser::Face::parse(&data, idx) else {
                continue;
            };
            let family = name_of(&face, 16)
                .or_else(|| name_of(&face, 1))
                .unwrap_or_default();
            if family.is_empty() {
                continue;
            }
            let style = name_of(&face, 17)
                .or_else(|| name_of(&face, 2))
                .unwrap_or_else(|| "Regular".into());
            let full = name_of(&face, 4).unwrap_or_else(|| format!("{family} {style}"));
            out.push(FontFace {
                file: p.to_string_lossy().into_owned(),
                index: idx,
                family,
                style,
                full,
                bold: face.is_bold(),
                italic: face.is_italic() || face.is_oblique(),
            });
        }
    }
}

fn font_roots() -> Vec<std::path::PathBuf> {
    let mut v = Vec::new();
    if cfg!(target_os = "windows") {
        v.push("C:\\Windows\\Fonts".into());
    } else if cfg!(target_os = "macos") {
        v.push("/System/Library/Fonts".into());
        v.push("/Library/Fonts".into());
        if let Some(h) = std::env::var_os("HOME") {
            v.push(std::path::Path::new(&h).join("Library/Fonts"));
        }
    } else {
        v.push("/usr/share/fonts".into());
        v.push("/usr/local/share/fonts".into());
        if let Some(h) = std::env::var_os("HOME") {
            v.push(std::path::Path::new(&h).join(".fonts"));
            v.push(std::path::Path::new(&h).join(".local/share/fonts"));
        }
    }
    v
}

fn installed_fonts() -> &'static Vec<FontFace> {
    static FONTS: OnceLock<Vec<FontFace>> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut out = Vec::new();
        for r in font_roots() {
            scan_dir(&r, &mut out, 0);
        }
        out
    })
}

fn preference(generic: &str) -> &'static [&'static str] {
    match generic {
        "serif" => &[
            "DejaVu Serif",
            "Bitstream Vera Serif",
            "Times New Roman",
            "Thorndale AMT",
            "Luxi Serif",
            "Nimbus Roman No9 L",
            "Nimbus Roman",
            "Times",
            "Liberation Serif",
            "Noto Serif",
        ],
        "monospace" => &[
            "DejaVu Sans Mono",
            "Bitstream Vera Sans Mono",
            "Andale Mono",
            "Courier New",
            "Cumberland AMT",
            "Luxi Mono",
            "Nimbus Mono L",
            "Nimbus Mono",
            "Courier",
            "Liberation Mono",
            "Noto Sans Mono",
        ],
        _ => &[
            "DejaVu Sans",
            "Bitstream Vera Sans",
            "Verdana",
            "Arial",
            "Albany AMT",
            "Luxi Sans",
            "Nimbus Sans L",
            "Nimbus Sans",
            "Helvetica",
            "Lucida Sans Unicode",
            "Tahoma",
            "Liberation Sans",
            "Noto Sans",
        ],
    }
}

/// The face fontconfig would pick first for `generic` and the style bits.
fn best_match(generic: &str, bold: bool, italic: bool) -> Option<FontFace> {
    let fonts = installed_fonts();
    let pick = |family: &str| -> Option<FontFace> {
        let cands: Vec<&FontFace> = fonts
            .iter()
            .filter(|f| f.family.eq_ignore_ascii_case(family))
            .collect();
        cands
            .iter()
            .min_by_key(|f| (f.bold != bold) as u8 * 2 + (f.italic != italic) as u8)
            .map(|f| (*f).clone())
    };
    for fam in preference(generic) {
        if let Some(f) = pick(fam) {
            return Some(f);
        }
    }
    fonts.first().cloned()
}

fn str_obj(ctx: &mut dyn NativeContext, s: &str) -> Value {
    Value::Object(Some(ctx.create_string(s)))
}

fn make_fc_font(
    scope: &mut NativeHandleScope<'_>,
    f: &FontFace,
) -> Result<cratonvm_native_api::NativeHandle, MethodCallFailed> {
    let obj = scope
        .new_object_initialized("sun/font/FontConfigManager$FontConfigFont", "()V", &[])?
        .and_then(|v| {
            if let Value::Object(Some(o)) = v {
                Some(o)
            } else {
                None
            }
        })
        .ok_or_else(|| internal("cannot construct FontConfigFont"))?;
    let h = scope.root(obj);
    for (name, val) in [
        ("familyName", f.family.as_str()),
        ("styleStr", f.style.as_str()),
        ("fullName", f.full.as_str()),
        ("fontFile", f.file.as_str()),
    ] {
        let s = scope.create_string(val);
        let o = scope.get(&h);
        scope.set_field_by_name(o, name, Value::Object(Some(s)));
    }
    Ok(h)
}

/// `FontConfigManager.getFontConfig(String, FontConfigInfo, FcCompFont[], boolean)`.
fn n_get_font_config(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let info = arg_obj(args, 1);
    let fonts_arr = arg_obj(args, 2).ok_or_else(|| internal("getFontConfig: null array"))?;
    let include_fallbacks = arg_int(args, 3) != 0;
    if installed_fonts().is_empty() {
        return Ok(None);
    }
    let mut scope = NativeHandleScope::new(ctx);
    let arr_h = scope.root(fonts_arr);
    if let Some(i) = info {
        scope.set_field_by_name(i, "fcVersion", Value::Int(21400));
    }
    let n = scope.array_length(scope.get(&arr_h));
    for k in 0..n {
        let arr = scope.get(&arr_h);
        let Value::Object(Some(comp)) = scope.get_array_element(arr, k) else {
            continue;
        };
        let comp_h = scope.root(comp);
        let fam = match scope.get_field_by_name(scope.get(&comp_h), "fcFamily") {
            Value::Object(Some(s)) => scope.read_string(s).unwrap_or_default(),
            _ => String::new(),
        };
        let style = match scope.get_field_by_name(scope.get(&comp_h), "style") {
            Value::Int(v) => v,
            _ => 0,
        };
        let (bold, italic) = (style & 1 != 0, style & 2 != 0);
        let Some(first) = best_match(&fam, bold, italic) else {
            continue;
        };
        let fh = make_fc_font(&mut scope, &first)?;
        let c = scope.get(&comp_h);
        let f = scope.get(&fh);
        scope.set_field_by_name(c, "firstFont", Value::Object(Some(f)));
        if include_fallbacks {
            // The matched face first, then every other installed face.
            let mut list: Vec<FontFace> = vec![first.clone()];
            for f in installed_fonts() {
                if f.file != first.file || f.index != first.index {
                    list.push(f.clone());
                }
            }
            let cid = scope.class_id_of_object(scope.get(&fh));
            let all = scope.new_ref_array(cid, list.len());
            let all_h = scope.root(all);
            for (i, f) in list.iter().enumerate() {
                let h = make_fc_font(&mut scope, f)?;
                let a = scope.get(&all_h);
                let o = scope.get(&h);
                scope.set_array_element(a, i, Value::Object(Some(o)));
            }
            let c = scope.get(&comp_h);
            let a = scope.get(&all_h);
            scope.set_field_by_name(c, "allFonts", Value::Object(Some(a)));
        }
    }
    Ok(None)
}

/// `FcFontManager.getFontPathNative(boolean, boolean)`: the directories that hold fonts.
fn n_get_font_path(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let mut dirs: Vec<String> = Vec::new();
    for f in installed_fonts() {
        if let Some(d) = std::path::Path::new(&f.file).parent() {
            let d = d.to_string_lossy().into_owned();
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    let sep = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };
    let s = ctx.create_string(&dirs.join(sep));
    Ok(Some(Value::Object(Some(s))))
}

// ---------------------------------------------------------------------------
// Scaler state
// ---------------------------------------------------------------------------

struct ScalerRow {
    data: Arc<Vec<u8>>,
    index: u32,
    /// The font parsed for the FreeType port (`glyf` fonts only).
    tt: Option<Arc<TtFont>>,
}

struct ScalerCtx {
    scaler: i64,
    /// `{m00, m10, m01, m11}` of the glyph transform (Java, y down).
    m: [f64; 4],
    aa: i32,
    fm: i32,
    bold: f32,
    italic: f32,
}

#[derive(Default)]
struct FontTable {
    next: i64,
    scalers: HashMap<i64, ScalerRow>,
    ctxs: HashMap<i64, ScalerCtx>,
    /// Live glyph blocks: address -> length.
    blocks: HashMap<usize, usize>,
    invisible: usize,
    /// Hinting state per (scaler, 26.6 size).
    sizes: HashMap<(i64, i64), TtSize>,
    /// Hinted advances per (scaler, 26.6 size, glyph).
    adv: HashMap<(i64, i64, u16), (f32, bool)>,
}

static FONTS: VmScoped<FontTable> = VmScoped::new();

/// Drop one VM's font state (glyph blocks are freed).
pub fn forget_vm(vm: usize) {
    FONTS.forget(vm);
}

fn face_of<'a>(row: &'a ScalerRow) -> Option<ttf_parser::Face<'a>> {
    ttf_parser::Face::parse(&row.data, row.index).ok()
}

/// `FreetypeFontScaler.initNativeScaler(Font2D, int, int, boolean, int)`.
fn n_init_scaler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let font = arg_obj(args, 1).ok_or_else(|| internal("initNativeScaler: null font"))?;
    let index = arg_int(args, 3).max(0) as u32;
    let path = match ctx.get_field_by_name(font, "platName") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let Some(path) = path else {
        return Ok(Some(Value::Long(0)));
    };
    let Ok(data) = std::fs::read(&path) else {
        return Ok(Some(Value::Long(0)));
    };
    if ttf_parser::Face::parse(&data, index).is_err() {
        return Ok(Some(Value::Long(0)));
    }
    let vm = ctx.vm_identity();
    let data = Arc::new(data);
    let tt = TtFont::parse(data.clone(), index).map(Arc::new);
    let id = FONTS.with(vm, |t| {
        t.next += 1;
        let id = t.next;
        t.scalers.insert(id, ScalerRow { data, index, tt });
        id
    });
    Ok(Some(Value::Long(id)))
}

fn n_dispose_scaler(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = arg_long(args, 2);
    let vm = ctx.vm_identity();
    FONTS.with(vm, |t| {
        t.scalers.remove(&id);
        t.ctxs.retain(|_, c| c.scaler != id);
        t.sizes.retain(|k, _| k.0 != id);
        t.adv.retain(|k, _| k.0 != id);
    });
    Ok(None)
}

/// `FreetypeFontScaler.createScalerContextNative(long, double[], int, int, float, float)`.
fn n_create_ctx(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let scaler = arg_long(args, 1);
    let mut m = [12.0, 0.0, 0.0, 12.0];
    if let Some(a) = arg_obj(args, 2) {
        for (i, slot) in m.iter_mut().enumerate() {
            if let Value::Double(d) = ctx.get_array_element(a, i) {
                *slot = d;
            }
        }
    }
    let c = ScalerCtx {
        scaler,
        m,
        aa: arg_int(args, 3),
        fm: arg_int(args, 4),
        bold: arg_f32(args, 5),
        italic: arg_f32(args, 6),
    };
    let vm = ctx.vm_identity();
    let id = FONTS.with(vm, |t| {
        t.next += 1;
        let id = t.next;
        t.ctxs.insert(id, c);
        id
    });
    Ok(Some(Value::Long(id)))
}

struct Snap {
    scaler: i64,
    data: Arc<Vec<u8>>,
    index: u32,
    tt: Option<Arc<TtFont>>,
    m: [f64; 4],
    aa: i32,
    fm: i32,
    bold: f32,
    italic: f32,
}

impl Snap {
    /// The FreeType port applies to every `glyf` context; transforms, synthetic bold
    /// and oblique are applied on top of the hinted outline.
    fn tt_ok(&self) -> bool {
        self.tt.is_some() && self.size26() > 0
    }

    /// The JDK sets the char size to the length of the matrix's second column
    /// (`euclidianDistance(matrix[2], matrix[3])`) and the rest as a transform.
    fn ptsz(&self) -> f64 {
        self.m[2].hypot(self.m[3])
    }

    /// `FT_Set_Char_Size` argument: the pixel size in 26.6.
    fn size26(&self) -> i64 {
        (self.ptsz() * 64.0) as i64
    }

    /// `FT_Set_Transform` matrix `(xx, xy, yx, yy)` in 16.16 (y up), or `None` for identity.
    fn ft_matrix(&self) -> Option<[i64; 4]> {
        let p = self.ptsz();
        if p <= 0.0 {
            return None;
        }
        // `FloatToFTFixed(dmat / ptsz)`: the quotient narrows to float, then scales.
        let fx = |v: f64| ((v as f32) * 65536.0f32) as i64;
        let m = [
            fx(self.m[0] / p),
            fx(-self.m[2] / p),
            fx(-self.m[1] / p),
            fx(self.m[3] / p),
        ];
        if m == [0x10000, 0, 0, 0x10000] {
            None
        } else {
            Some(m)
        }
    }

    /// Synthetic bold (`boldness` 1.33 from `FileFontStrike`; 0 or 1 mean none).
    fn do_bold(&self) -> bool {
        self.bold > 1.0
    }

    /// `FT_Outline_Embolden` strength, measured against HotSpot: size / 32 (26.6).
    fn bold_strength(&self) -> i64 {
        self.size26() / 32
    }
}

fn snap(ctx: &dyn NativeContext, pctx: i64, pscaler: i64) -> Option<Snap> {
    let vm = ctx.vm_identity();
    FONTS.with(vm, |t| {
        let s = t.scalers.get(&pscaler)?;
        let (m, aa, fm, bold, italic) = match t.ctxs.get(&pctx) {
            Some(c) => (c.m, c.aa, c.fm, c.bold, c.italic),
            None => ([12.0, 0.0, 0.0, 12.0], 1, 1, 0.0, 0.0),
        };
        Some(Snap {
            scaler: pscaler,
            data: s.data.clone(),
            index: s.index,
            tt: s.tt.clone(),
            m,
            aa,
            fm,
            bold,
            italic,
        })
    })
}

/// Load a glyph through the FreeType port (hinted = `FT_LOAD_TARGET_MONO`).
/// Returns the outline and the size's ppem.
fn tt_load(
    ctx: &dyn NativeContext,
    s: &Snap,
    glyph: u16,
    hinted: bool,
) -> Option<(GlyphOutline, i64)> {
    if !s.tt_ok() {
        return None;
    }
    let tt = s.tt.as_ref()?;
    let size26 = s.size26();
    let vm = ctx.vm_identity();
    FONTS.with(vm, |t| {
        let sz = t
            .sizes
            .entry((s.scaler, size26))
            .or_insert_with(|| TtSize::new(tt, size26));
        let ppem = sz.ppem;
        sz.load(tt, glyph, hinted).map(|mut o| {
            use super::tt::fixed::mul_fix;
            if let Some([xx, xy, yx, yy]) = s.ft_matrix() {
                for p in o.pts.iter_mut() {
                    let (x, y) = (p.x, p.y);
                    p.x = mul_fix(x, xx) + mul_fix(y, xy);
                    p.y = mul_fix(x, yx) + mul_fix(y, yy);
                }
            }
            if s.italic != 0.0 {
                // Synthetic oblique: `FT_GlyphSlot_Oblique` shears the (hinted)
                // outline by 0x0366A (tan 12 degrees); the advance stays unsheared.
                for p in o.pts.iter_mut() {
                    p.x += mul_fix(p.y, 0x0366A);
                }
            }
            // The JDK emboldens after it slants (measured: bold italic).
            if s.do_bold() {
                super::tt::embolden::embolden(&mut o.pts, &o.contours, s.bold_strength());
            }
            (o, ppem)
        })
    })
}

/// The advance FreeType reports for `glyph`: the hinted (whole pixel) advance, plus the
/// emboldening strength, transformed by the context matrix; `(x, y)` in pixels, Java y down.
fn tt_advance(ctx: &dyn NativeContext, s: &Snap, glyph: u16) -> Option<(f32, f32)> {
    if !s.tt_ok() {
        return None;
    }
    let key = (s.scaler, s.size26(), glyph);
    let vm = ctx.vm_identity();
    let (base, has_outline) = match FONTS.with(vm, |t| t.adv.get(&key).copied()) {
        Some(a) => a,
        None => {
            // Loaded untransformed so the cache is per size, not per matrix.
            let p = s.ptsz();
            let plain = Snap {
                scaler: s.scaler,
                data: s.data.clone(),
                index: s.index,
                tt: s.tt.clone(),
                m: [p, 0.0, 0.0, p],
                aa: s.aa,
                fm: s.fm,
                bold: 0.0,
                italic: 0.0,
            };
            let (o, _) = tt_load(ctx, &plain, glyph, true)?;
            let a = (o.advance as f32, !o.pts.is_empty());
            FONTS.with(vm, |t| {
                t.adv.insert(key, a);
            });
            a
        }
    };
    let mut adv = base as i64;
    // Only a glyph with an outline is emboldened (a space keeps its advance).
    if s.do_bold() && has_outline {
        adv += s.bold_strength();
    }
    match s.ft_matrix() {
        None => Some((adv as f32 / 64.0, 0.0)),
        Some([xx, _, yx, _]) => {
            use super::tt::fixed::mul_fix;
            Some((
                mul_fix(adv, xx) as f32 / 64.0,
                -(mul_fix(adv, yx) as f32) / 64.0,
            ))
        }
    }
}

/// `FT_Set_Char_Size` y scale in 16.16 for `ptsz` at 72 dpi.
fn y_scale(ptsz: f64, upem: u16) -> i64 {
    let size26 = (ptsz * 64.0) as i64;
    ((size26 << 16) + (upem as i64) / 2) / upem as i64
}

fn mul_fix(a: i64, b: i64) -> i64 {
    let s = if (a < 0) != (b < 0) { -1 } else { 1 };
    let (a, b) = (a.abs(), b.abs());
    s * ((a * b + 0x8000) >> 16)
}

fn pix_round(v: i64) -> i64 {
    (v + 32) & !63
}

/// `FreetypeFontScaler.getFontMetricsNative(Font2D, long, long)`.
fn n_font_metrics(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler) = (arg_long(args, 2), arg_long(args, 3));
    let Some(s) = snap(ctx, pctx, pscaler) else {
        return Ok(Some(Value::Object(None)));
    };
    let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
    let upem = face.units_per_em();
    let ys = y_scale(s.ptsz(), upem);
    let mut asc = face.ascender() as i64;
    let mut desc = face.descender() as i64;
    let mut gap = face.line_gap() as i64;
    if asc == 0 && desc == 0 {
        if let Some(os2) = face.tables().os2 {
            if os2.typographic_ascender() != 0 || os2.typographic_descender() != 0 {
                asc = os2.typographic_ascender() as i64;
                desc = os2.typographic_descender() as i64;
                gap = os2.typographic_line_gap() as i64;
            } else {
                asc = os2.windows_ascender() as i64;
                desc = -(os2.windows_descender() as i64).abs();
                gap = 0;
            }
        }
    }
    let height = asc - desc + gap;
    let f = |a: i64| -> f32 { (a as f32) * (ys as f32) / 65536.0 / 64.0 };
    let ay = -f(asc);
    let dy = -f(desc);
    let ly = f(height) + ay - dy;
    // hhea.advanceWidthMax at offset 10.
    let max_adv = face
        .raw_face()
        .table(ttf_parser::Tag::from_bytes(b"hhea"))
        .filter(|b| b.len() >= 12)
        .map(|b| u16::from_be_bytes([b[10], b[11]]) as i64)
        .unwrap_or(upem as i64);
    let xs = ys;
    let mx = (pix_round(mul_fix(max_adv, xs)) as f32) / 64.0;
    // Synthetic oblique: HotSpot widens the maximum advance by a quarter em
    // (`OBLIQUE_MODIFIER`), measured 17 -> 20 at 9 px up to 75 -> 85 at 40 px.
    let mx = if s.italic != 0.0 {
        mx + (s.ptsz() as f32) * 0.25
    } else {
        mx
    };
    // Synthetic bold widens every advance by the emboldening strength.
    let mx = if s.do_bold() {
        mx + s.bold_strength() as f32 / 64.0
    } else {
        mx
    };
    // The context matrix (relative to the char size) carries the vectors into device space.
    let p = s.ptsz();
    let tv = |vx: f32, vy: f32| -> (f32, f32) {
        if p <= 0.0 {
            return (vx, vy);
        }
        let (vx, vy) = (vx as f64, vy as f64);
        (
            ((s.m[0] * vx + s.m[2] * vy) / p) as f32,
            ((s.m[1] * vx + s.m[3] * vy) / p) as f32,
        )
    };
    let (ax, ay) = tv(0.0, ay);
    let (dx, dy) = tv(0.0, dy);
    let (lx, ly) = tv(0.0, ly);
    let (mx, my) = tv(mx, 0.0);
    let vals = [ax, ay, dx, dy, 0.0, 0.0, lx, ly, mx, my];
    let a: Vec<Value> = vals.iter().map(|&v| Value::Float(v)).collect();
    ctx.new_object_initialized("sun/font/StrikeMetrics", "(FFFFFFFFFF)V", &a)
}

/// The advance of `glyph` as FreeType would report it for `s`.
fn advance_of(
    ctx: &dyn NativeContext,
    s: &Snap,
    face: &ttf_parser::Face<'_>,
    glyph: u16,
) -> (f32, f32) {
    if s.fm != 2 {
        if let Some(a) = tt_advance(ctx, s, glyph) {
            return a;
        }
    }
    if s.tt_ok() {
        // Fractional metrics: `linearHoriAdvance` (unhinted, 16.16) times the transform's
        // first column, as `freetypeScaler.c` computes the glyph advance.
        use super::tt::fixed::{div_fix, mul_div};
        let upem = face.units_per_em() as i64;
        let units = face
            .glyph_hor_advance(ttf_parser::GlyphId(glyph))
            .unwrap_or(0) as i64;
        let linear16 = mul_div(units, div_fix(s.size26(), upem), 64);
        let mut advh = linear16 as f64 / 65536.0;
        if s.do_bold()
            && face
                .glyph_bounding_box(ttf_parser::GlyphId(glyph))
                .is_some()
        {
            advh += s.bold_strength() as f64 / 64.0;
        }
        let (xx, yx) = match s.ft_matrix() {
            Some([xx, _, yx, _]) => (xx as f64 / 65536.0, -(yx as f64) / 65536.0),
            None => (1.0, 0.0),
        };
        return ((advh * xx) as f32, (advh * yx) as f32);
    }
    let upem = face.units_per_em() as f64;
    let adv = face
        .glyph_hor_advance(ttf_parser::GlyphId(glyph))
        .unwrap_or(0) as f64
        / upem;
    // Java matrix {a, b, c, d}: the advance vector is the first column.
    let (mut fx, mut fy) = (s.m[0] * adv, s.m[1] * adv);
    if s.do_bold()
        && s.ptsz() > 0.0
        && face
            .glyph_bounding_box(ttf_parser::GlyphId(glyph))
            .is_some()
    {
        let k = s.bold_strength() as f64 / 64.0 / s.ptsz();
        fx += s.m[0] * k;
        fy += s.m[1] * k;
    }
    if s.fm == 2 {
        (fx as f32, fy as f32)
    } else if fy == 0.0 {
        ((fx + 0.5).floor() as f32, 0.0)
    } else {
        (fx as f32, fy as f32)
    }
}

fn n_glyph_advance(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler, glyph) = (arg_long(args, 2), arg_long(args, 3), arg_int(args, 4));
    let Some(s) = snap(ctx, pctx, pscaler) else {
        return Ok(Some(Value::Float(0.0)));
    };
    let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
    Ok(Some(Value::Float(
        advance_of(ctx, &s, &face, glyph as u16).0,
    )))
}

fn n_glyph_metrics(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler, glyph) = (arg_long(args, 2), arg_long(args, 3), arg_int(args, 4));
    let Some(out) = arg_obj(args, 5) else {
        return Ok(None);
    };
    if let Some(s) = snap(ctx, pctx, pscaler) {
        let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
        let (x, y) = advance_of(ctx, &s, &face, glyph as u16);
        ctx.set_field_by_name(out, "x", Value::Float(x));
        ctx.set_field_by_name(out, "y", Value::Float(y));
    }
    Ok(None)
}

fn n_glyph_code(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pscaler, ch) = (arg_long(args, 2), arg_int(args, 3));
    let vm = ctx.vm_identity();
    let code = FONTS.with(vm, |t| {
        let row = t.scalers.get(&pscaler)?;
        let face = face_of(row)?;
        char::from_u32(ch as u32 & 0xffff)
            .and_then(|c| face.glyph_index(c))
            .map(|g| g.0 as i32)
    });
    Ok(Some(Value::Int(code.unwrap_or(0))))
}

fn n_num_glyphs(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pscaler = arg_long(args, 1);
    let vm = ctx.vm_identity();
    let n = FONTS.with(vm, |t| {
        t.scalers
            .get(&pscaler)
            .and_then(|r| face_of(r))
            .map(|f| f.number_of_glyphs() as i32)
    });
    Ok(Some(Value::Int(n.unwrap_or(0))))
}

fn n_missing_glyph(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

fn n_units_per_em(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pscaler = arg_long(args, 1);
    let vm = ctx.vm_identity();
    let n = FONTS.with(vm, |t| {
        t.scalers
            .get(&pscaler)
            .and_then(|r| face_of(r))
            .map(|f| f.units_per_em() as i64)
    });
    Ok(Some(Value::Long(n.unwrap_or(2048))))
}

// ---------------------------------------------------------------------------
// Outlines
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Quad(f32, f32, f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

struct Collect<'a> {
    segs: Vec<Seg>,
    t: &'a dyn Fn(f32, f32) -> (f32, f32),
}

impl ttf_parser::OutlineBuilder for Collect<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = (self.t)(x, y);
        self.segs.push(Seg::Move(x, y));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = (self.t)(x, y);
        self.segs.push(Seg::Line(x, y));
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (x1, y1) = (self.t)(x1, y1);
        let (x, y) = (self.t)(x, y);
        self.segs.push(Seg::Quad(x1, y1, x, y));
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (x1, y1) = (self.t)(x1, y1);
        let (x2, y2) = (self.t)(x2, y2);
        let (x, y) = (self.t)(x, y);
        self.segs.push(Seg::Cubic(x1, y1, x2, y2, x, y));
    }
    fn close(&mut self) {
        self.segs.push(Seg::Close);
    }
}

/// Outline of `glyph` in device pixels (y down), origin at `(ox, oy)`.
fn glyph_segments(s: &Snap, face: &ttf_parser::Face<'_>, glyph: u16, ox: f32, oy: f32) -> Vec<Seg> {
    let upem = face.units_per_em() as f64;
    let m = s.m;
    let italic = s.italic as f64;
    let t = move |x: f32, y: f32| -> (f32, f32) {
        let (ux, uy) = (x as f64 / upem, y as f64 / upem);
        let ux = ux + italic * uy;
        // Java y-down matrix applied to a y-up em coordinate (FreeType: x' = a x - c y).
        let dx = m[0] * ux - m[2] * uy;
        let dy = -(-m[1] * ux + m[3] * uy);
        (dx as f32 + ox, dy as f32 + oy)
    };
    let mut c = Collect {
        segs: Vec::new(),
        t: &t,
    };
    let _ = face.outline_glyph(ttf_parser::GlyphId(glyph), &mut c);
    c.segs
}

/// `FT_Outline_Decompose` of an unhinted 26.6 outline into Java2D segments
/// (`y` flipped, offset by the glyph origin).
fn outline_segs(o: &GlyphOutline, ox: f32, oy: f32) -> Vec<Seg> {
    let f = |x: i64, y: i64| -> (f32, f32) { (x as f32 / 64.0 + ox, -(y as f32) / 64.0 + oy) };
    let mut segs = Vec::new();
    let mut first = 0usize;
    for &end in &o.contours {
        let last = end as usize;
        let pts = &o.pts[first..=last];
        let tags = &o.tags[first..=last];
        first = last + 1;
        let n = pts.len();
        if n == 0 {
            continue;
        }
        let on = |i: usize| tags[i] & 1 != 0;
        let mut v_start = (pts[0].x, pts[0].y);
        let v_last = (pts[n - 1].x, pts[n - 1].y);
        let mut limit = n - 1;
        let mut idx: isize = 0;
        if !on(0) {
            if on(n - 1) {
                v_start = v_last;
                limit -= 1;
            } else {
                v_start = ((v_start.0 + v_last.0) / 2, (v_start.1 + v_last.1) / 2);
            }
            idx = -1;
        }
        let (sx, sy) = f(v_start.0, v_start.1);
        segs.push(Seg::Move(sx, sy));
        let mut closed_by_conic = false;
        while (idx as i64) < limit as i64 {
            idx += 1;
            let i = idx as usize;
            if on(i) {
                let (x, y) = f(pts[i].x, pts[i].y);
                segs.push(Seg::Line(x, y));
                continue;
            }
            let mut vc = (pts[i].x, pts[i].y);
            loop {
                if (idx as i64) < limit as i64 {
                    idx += 1;
                    let j = idx as usize;
                    let v = (pts[j].x, pts[j].y);
                    if on(j) {
                        let (cx, cy) = f(vc.0, vc.1);
                        let (x, y) = f(v.0, v.1);
                        segs.push(Seg::Quad(cx, cy, x, y));
                        break;
                    }
                    let mid = ((vc.0 + v.0) / 2, (vc.1 + v.1) / 2);
                    let (cx, cy) = f(vc.0, vc.1);
                    let (x, y) = f(mid.0, mid.1);
                    segs.push(Seg::Quad(cx, cy, x, y));
                    vc = v;
                    continue;
                }
                let (cx, cy) = f(vc.0, vc.1);
                let (x, y) = f(v_start.0, v_start.1);
                segs.push(Seg::Quad(cx, cy, x, y));
                closed_by_conic = true;
                break;
            }
            if closed_by_conic {
                break;
            }
        }
        if !closed_by_conic {
            let (x, y) = f(v_start.0, v_start.1);
            segs.push(Seg::Line(x, y));
        }
        segs.push(Seg::Close);
    }
    segs
}

/// The glyph outline for `s`: the FreeType port when it applies (unhinted, like
/// `FT_LOAD_NO_HINTING`), else the `ttf-parser` outline.
fn segments_any(
    ctx: &dyn NativeContext,
    s: &Snap,
    face: &ttf_parser::Face<'_>,
    glyph: u16,
    ox: f32,
    oy: f32,
) -> Vec<Seg> {
    if let Some((o, _)) = tt_load(ctx, s, glyph, false) {
        return outline_segs(&o, ox, oy);
    }
    glyph_segments(s, face, glyph, ox, oy)
}

fn path_object(scope: &mut NativeHandleScope<'_>, segs: &[Seg]) -> MethodCallResult {
    let obj = scope
        .new_object_initialized("java/awt/geom/GeneralPath", "()V", &[])?
        .and_then(|v| {
            if let Value::Object(Some(o)) = v {
                Some(o)
            } else {
                None
            }
        })
        .ok_or_else(|| internal("cannot construct GeneralPath"))?;
    let h = scope.root(obj);
    for sg in segs {
        let (name, desc, args): (&str, &str, Vec<Value>) = match *sg {
            Seg::Move(x, y) => ("moveTo", "(FF)V", vec![Value::Float(x), Value::Float(y)]),
            Seg::Line(x, y) => ("lineTo", "(FF)V", vec![Value::Float(x), Value::Float(y)]),
            Seg::Quad(a, b, c, d) => (
                "quadTo",
                "(FFFF)V",
                vec![
                    Value::Float(a),
                    Value::Float(b),
                    Value::Float(c),
                    Value::Float(d),
                ],
            ),
            Seg::Cubic(a, b, c, d, e, f) => (
                "curveTo",
                "(FFFFFF)V",
                vec![
                    Value::Float(a),
                    Value::Float(b),
                    Value::Float(c),
                    Value::Float(d),
                    Value::Float(e),
                    Value::Float(f),
                ],
            ),
            Seg::Close => ("closePath", "()V", vec![]),
        };
        let o = scope.get(&h);
        scope.invoke_virtual(o, name, desc, &args)?;
    }
    Ok(Some(Value::Object(Some(scope.get(&h)))))
}

fn n_glyph_outline(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler, glyph) = (arg_long(args, 2), arg_long(args, 3), arg_int(args, 4));
    let (x, y) = (arg_f32(args, 5), arg_f32(args, 6));
    let segs = match snap(ctx, pctx, pscaler) {
        Some(s) => {
            let face =
                ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
            segments_any(ctx, &s, &face, glyph as u16, x, y)
        }
        None => Vec::new(),
    };
    let mut scope = NativeHandleScope::new(ctx);
    path_object(&mut scope, &segs)
}

fn n_glyph_vector_outline(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler) = (arg_long(args, 2), arg_long(args, 3));
    let glyphs = arg_obj(args, 4);
    let n = arg_int(args, 5).max(0) as usize;
    let (x, y) = (arg_f32(args, 6), arg_f32(args, 7));
    let mut ids = vec![0i32; n];
    if let Some(g) = glyphs {
        ctx.read_int_array_into(g, 0, &mut ids);
    }
    let mut segs = Vec::new();
    if let Some(s) = snap(ctx, pctx, pscaler) {
        let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
        let (mut px, mut py) = (x, y);
        for g in ids {
            if g >= 0xfffe {
                continue;
            }
            segs.extend(segments_any(ctx, &s, &face, g as u16, px, py));
            let (ax, ay) = advance_of(ctx, &s, &face, g as u16);
            px += ax;
            py += ay;
        }
    }
    let mut scope = NativeHandleScope::new(ctx);
    path_object(&mut scope, &segs)
}

fn seg_bounds(segs: &[Seg]) -> Option<(f32, f32, f32, f32)> {
    let mut b: Option<(f32, f32, f32, f32)> = None;
    let mut add = |x: f32, y: f32| {
        b = Some(match b {
            None => (x, y, x, y),
            Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
        });
    };
    for s in segs {
        match *s {
            Seg::Move(x, y) | Seg::Line(x, y) => add(x, y),
            Seg::Quad(a, bb, c, d) => {
                add(a, bb);
                add(c, d);
            }
            Seg::Cubic(a, bb, c, d, e, f) => {
                add(a, bb);
                add(c, d);
                add(e, f);
            }
            Seg::Close => {}
        }
    }
    b
}

fn n_glyph_outline_bounds(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler, glyph) = (arg_long(args, 2), arg_long(args, 3), arg_int(args, 4));
    let mut bounds = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    if let Some(s) = snap(ctx, pctx, pscaler) {
        let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
        let segs = segments_any(ctx, &s, &face, glyph as u16, 0.0, 0.0);
        if let Some((x0, y0, x1, y1)) = seg_bounds(&segs) {
            bounds = (x0, y0, x1 - x0, y1 - y0);
        }
    }
    ctx.new_object_initialized(
        "java/awt/geom/Rectangle2D$Float",
        "(FFFF)V",
        &[
            Value::Float(bounds.0),
            Value::Float(bounds.1),
            Value::Float(bounds.2),
            Value::Float(bounds.3),
        ],
    )
}

fn n_glyph_point(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ctx.new_object_initialized(
        "java/awt/geom/Point2D$Float",
        "(FF)V",
        &[Value::Float(0.0), Value::Float(0.0)],
    )
}

// ---------------------------------------------------------------------------
// Rasterisation
// ---------------------------------------------------------------------------

/// Exact-area coverage accumulation buffer (the `font-rs` scheme).
struct Accum {
    w: usize,
    h: usize,
    a: Vec<f32>,
}

impl Accum {
    fn new(w: usize, h: usize) -> Self {
        Accum {
            w,
            h,
            a: vec![0.0; w * h + 4],
        }
    }

    fn line(&mut self, p0: (f32, f32), p1: (f32, f32)) {
        if p0.1 == p1.1 {
            return;
        }
        let (dir, p0, p1) = if p0.1 < p1.1 {
            (1.0f32, p0, p1)
        } else {
            (-1.0f32, p1, p0)
        };
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let y0 = p0.1.max(0.0) as usize;
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let ymax = self.h.min(p1.1.ceil().max(0.0) as usize);
        for y in y0..ymax {
            let linestart = y * self.w;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (x0, x1) = if x < xnext { (x, xnext) } else { (xnext, x) };
            let x0floor = x0.floor();
            let x0i = x0floor as i32;
            let x1ceil = x1.ceil();
            let x1i = x1ceil as i32;
            let mut add = |xi: i32, v: f32| {
                let xi = xi.clamp(0, self.w as i32 - 1) as usize;
                self.a[linestart + xi] += v;
            };
            if x1i <= x0i + 1 {
                let xmf = 0.5 * (x + xnext) - x0floor;
                add(x0i, d - d * xmf);
                add(x0i + 1, d * xmf);
            } else {
                let s = 1.0 / (x1 - x0);
                let x0f = x0 - x0floor;
                let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
                let x1f = x1 - x1ceil + 1.0;
                let am = 0.5 * s * x1f * x1f;
                add(x0i, d * a0);
                if x1i == x0i + 2 {
                    add(x0i + 1, d * (1.0 - a0 - am));
                } else {
                    let a1 = s * (1.5 - x0f);
                    add(x0i + 1, d * (a1 - a0));
                    for xi in x0i + 2..x1i - 1 {
                        add(xi, d * s);
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    add(x1i - 1, d * (1.0 - a2 - am));
                }
                add(x1i, d * am);
            }
            x = xnext;
        }
    }

    fn coverage(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.w * self.h];
        for y in 0..self.h {
            let mut acc = 0.0f32;
            for x in 0..self.w {
                acc += self.a[y * self.w + x];
                let v = acc.abs().min(1.0);
                out[y * self.w + x] = (v * 255.0 + 0.5) as u8;
            }
        }
        out
    }
}

fn flatten(segs: &[Seg], mut f: impl FnMut((f32, f32), (f32, f32))) {
    let mut cur = (0.0f32, 0.0f32);
    let mut start = (0.0f32, 0.0f32);
    for s in segs {
        match *s {
            Seg::Move(x, y) => {
                if cur != start {
                    f(cur, start);
                }
                cur = (x, y);
                start = cur;
            }
            Seg::Line(x, y) => {
                f(cur, (x, y));
                cur = (x, y);
            }
            Seg::Quad(cx, cy, x, y) => {
                let d = ((cur.0 - 2.0 * cx + x).abs() + (cur.1 - 2.0 * cy + y).abs()).max(0.0);
                let n = ((d * 8.0).sqrt().ceil() as usize).clamp(1, 32);
                let mut prev = cur;
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let mt = 1.0 - t;
                    let p = (
                        mt * mt * cur.0 + 2.0 * mt * t * cx + t * t * x,
                        mt * mt * cur.1 + 2.0 * mt * t * cy + t * t * y,
                    );
                    f(prev, p);
                    prev = p;
                }
                cur = (x, y);
            }
            Seg::Cubic(c1x, c1y, c2x, c2y, x, y) => {
                let d = ((cur.0 - 2.0 * c1x + c2x).abs() + (cur.1 - 2.0 * c1y + c2y).abs())
                    .max((c1x - 2.0 * c2x + x).abs() + (c1y - 2.0 * c2y + y).abs());
                let n = ((d * 6.0).sqrt().ceil() as usize).clamp(1, 48);
                let mut prev = cur;
                for i in 1..=n {
                    let t = i as f32 / n as f32;
                    let mt = 1.0 - t;
                    let p = (
                        mt * mt * mt * cur.0
                            + 3.0 * mt * mt * t * c1x
                            + 3.0 * mt * t * t * c2x
                            + t * t * t * x,
                        mt * mt * mt * cur.1
                            + 3.0 * mt * mt * t * c1y
                            + 3.0 * mt * t * t * c2y
                            + t * t * t * y,
                    );
                    f(prev, p);
                    prev = p;
                }
                cur = (x, y);
            }
            Seg::Close => {
                if cur != start {
                    f(cur, start);
                }
                cur = start;
            }
        }
    }
    if cur != start {
        f(cur, start);
    }
}

/// Allocate a glyph block (`GlyphInfo` + image bytes) and return its address.
fn alloc_block(ctx: &dyn NativeContext, info: [u8; 40], image: &[u8]) -> i64 {
    let mut v = vec![0u8; 40 + image.len().max(1)];
    v[..40].copy_from_slice(&info);
    v[40..40 + image.len()].copy_from_slice(image);
    let len = v.len();
    let ptr = Box::into_raw(v.into_boxed_slice()) as *mut u8 as usize;
    // The image pointer points just past the header.
    // SAFETY: `ptr` addresses a live allocation of at least 40 + 1 bytes that
    // this function just created and nothing else aliases yet.
    unsafe {
        let img = (ptr + 40) as u64;
        std::ptr::copy_nonoverlapping(img.to_le_bytes().as_ptr(), (ptr + 32) as *mut u8, 8);
    }
    let vm = ctx.vm_identity();
    FONTS.with(vm, |t| {
        t.blocks.insert(ptr, len);
    });
    ptr as i64
}

fn glyph_info_header(
    adv: (f32, f32),
    w: u16,
    h: u16,
    row_bytes: u16,
    left: f32,
    top: f32,
) -> [u8; 40] {
    let mut b = [0u8; 40];
    b[0..4].copy_from_slice(&adv.0.to_le_bytes());
    b[4..8].copy_from_slice(&adv.1.to_le_bytes());
    b[8..10].copy_from_slice(&w.to_le_bytes());
    b[10..12].copy_from_slice(&h.to_le_bytes());
    b[12..14].copy_from_slice(&row_bytes.to_le_bytes());
    b[14] = 1; // managed
    b[16..20].copy_from_slice(&left.to_le_bytes());
    b[20..24].copy_from_slice(&top.to_le_bytes());
    b
}

/// `FreetypeFontScaler.getGlyphImageNative(Font2D, long, long, int)`.
fn n_glyph_image(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (pctx, pscaler, glyph) = (arg_long(args, 2), arg_long(args, 3), arg_int(args, 4));
    let Some(s) = snap(ctx, pctx, pscaler) else {
        return Ok(Some(Value::Long(0)));
    };
    let face = ttf_parser::Face::parse(&s.data, s.index).map_err(|_| internal("bad font"))?;
    let adv = advance_of(ctx, &s, &face, glyph as u16);
    // FreeType port: the JDK loads with TARGET_MONO hinting, except anti-aliased
    // text with fractional metrics, which is unhinted.
    if s.aa >= 4 {
        // LCD text is hinted whatever the metrics mode (measured against HotSpot).
        let hinted = true;
        if let Some((o, _ppem)) = tt_load(ctx, &s, glyph as u16, hinted) {
            if o.pts.is_empty() {
                let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
                return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
            }
            let vertical = s.aa >= 6;
            let bm = super::tt::gray::render_lcd(&o.pts, &o.tags, &o.contours, vertical);
            if bm.width == 0 || bm.rows == 0 {
                let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
                return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
            }
            // The JDK stores LCD glyphs as `width` pixels of three bytes; a vertical
            // bitmap is transposed into the same layout (`CopyFTSubpixelVToSubpixel`).
            let (w, h, image) = if vertical {
                let (w, h) = (bm.width, bm.rows / 3);
                let mut img = vec![0u8; w * 3 * h];
                for y in 0..h {
                    for x in 0..w {
                        for c in 0..3 {
                            img[y * w * 3 + x * 3 + c] = bm.data[(y * 3 + c) * w + x];
                        }
                    }
                }
                (w, h, img)
            } else {
                (bm.width / 3, bm.rows, bm.data)
            };
            let hdr = glyph_info_header(
                adv,
                w as u16,
                h as u16,
                (w * 3) as u16,
                bm.left as f32,
                -(bm.top as f32),
            );
            return Ok(Some(Value::Long(alloc_block(ctx, hdr, &image))));
        }
    }
    if s.aa < 4 {
        let hinted = s.aa == 1 || s.fm != 2;
        if let Some((o, ppem)) = tt_load(ctx, &s, glyph as u16, hinted) {
            if o.pts.is_empty() {
                let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
                return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
            }
            let bm = if s.aa == 1 {
                super::tt::mono::render(&o.pts, &o.tags, &o.contours, ppem < 24, &o.marks)
            } else {
                super::tt::gray::render(&o.pts, &o.tags, &o.contours)
            };
            if bm.width == 0 || bm.rows == 0 {
                let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
                return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
            }
            let hdr = glyph_info_header(
                adv,
                bm.width as u16,
                bm.rows as u16,
                bm.width as u16,
                bm.left as f32,
                -(bm.top as f32),
            );
            return Ok(Some(Value::Long(alloc_block(ctx, hdr, &bm.data))));
        }
    }
    let segs = glyph_segments(&s, &face, glyph as u16, 0.0, 0.0);
    let Some((x0, y0, x1, y1)) = seg_bounds(&segs) else {
        let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
        return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
    };
    // Integer pixel box (FreeType: floor of the minimum, ceil of the maximum).
    let (bx0, by0) = (x0.floor() as i32, y0.floor() as i32);
    let (bx1, by1) = (x1.ceil() as i32, y1.ceil() as i32);
    let (w, h) = ((bx1 - bx0).max(0) as usize, (by1 - by0).max(0) as usize);
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        let hdr = glyph_info_header(adv, 0, 0, 0, 0.0, 0.0);
        return Ok(Some(Value::Long(alloc_block(ctx, hdr, &[]))));
    }
    let mut acc = Accum::new(w, h);
    let (ox, oy) = (-(bx0 as f32), -(by0 as f32));
    flatten(&segs, |a, b| {
        acc.line((a.0 + ox, a.1 + oy), (b.0 + ox, b.1 + oy))
    });
    let cov = acc.coverage();
    let (image, row_bytes) = if s.aa == 1 {
        // Bi-level glyphs, expanded to one byte per pixel (0 or 255) as the JDK's
        // FreeType scaler does (`CopyBW2Grey8`): `rowBytes == width`.
        let bytes: Vec<u8> = cov
            .iter()
            .map(|&v| if v >= 128 { 0xff } else { 0 })
            .collect();
        (bytes, w)
    } else if s.aa >= 4 {
        // LCD request: three sub-pixel samples per pixel (replicated coverage).
        let mut lcd = vec![0u8; w * 3 * h];
        for y in 0..h {
            for x in 0..w {
                let v = cov[y * w + x];
                lcd[y * w * 3 + x * 3] = v;
                lcd[y * w * 3 + x * 3 + 1] = v;
                lcd[y * w * 3 + x * 3 + 2] = v;
            }
        }
        (lcd, w * 3)
    } else {
        (cov, w)
    };
    let width_field = w;
    let hdr = glyph_info_header(
        adv,
        width_field as u16,
        h as u16,
        row_bytes as u16,
        bx0 as f32,
        by0 as f32,
    );
    Ok(Some(Value::Long(alloc_block(ctx, hdr, &image))))
}

// ---------------------------------------------------------------------------
// StrikeCache
// ---------------------------------------------------------------------------

fn free_block(ctx: &dyn NativeContext, ptr: usize) {
    if ptr == 0 {
        return;
    }
    let vm = ctx.vm_identity();
    let len = FONTS.with(vm, |t| t.blocks.remove(&ptr));
    if let Some(len) = len {
        // SAFETY: `ptr`/`len` came from `Box::into_raw` of a boxed slice in
        // `alloc_block` and the table entry we just removed makes this the
        // only free.
        unsafe {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                ptr as *mut u8,
                len,
            )));
        }
    }
}

fn n_invisible_glyph(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let vm = ctx.vm_identity();
    let existing = FONTS.with(vm, |t| t.invisible);
    if existing != 0 {
        return Ok(Some(Value::Long(existing as i64)));
    }
    let mut hdr = glyph_info_header((0.0, 0.0), 0, 0, 0, 0.0, 0.0);
    hdr[14] = 0;
    let p = alloc_block(ctx, hdr, &[]);
    FONTS.with(vm, |t| {
        t.invisible = p as usize;
        // Never freed.
        t.blocks.remove(&(p as usize));
    });
    Ok(Some(Value::Long(p)))
}

fn n_free_pointer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let p = match args.first() {
        Some(Value::Long(l)) => *l as usize,
        Some(Value::Int(i)) => *i as u32 as usize,
        _ => 0,
    };
    free_block(ctx, p);
    Ok(None)
}

fn n_free_long_memory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(arr) = arg_obj(args, 0) {
        for i in 0..ctx.array_length(arr) {
            if let Value::Long(p) = ctx.get_array_element(arr, i) {
                free_block(ctx, p as usize);
            }
        }
    }
    Ok(None)
}

fn n_free_int_memory(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(arr) = arg_obj(args, 0) {
        for i in 0..ctx.array_length(arr) {
            if let Value::Int(p) = ctx.get_array_element(arr, i) {
                free_block(ctx, p as u32 as usize);
            }
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Layout (no HarfBuzz): one glyph per char, advances from the strike
// ---------------------------------------------------------------------------

fn n_create_face(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Long(1)))
}

/// Registration (genuine `ACC_NATIVE` methods: the caller sets `Bridge`).
pub fn register(registry: &mut NativeMethodRegistry) {
    registry.register("sun/font/SunFontManager", "initIDs", "()V", n_noop);
    registry.register(
        "sun/font/FontConfigManager",
        "getFontConfigVersion",
        "()I",
        |_c, _a| Ok(Some(Value::Int(21400))),
    );
    registry.register(
        "sun/font/FontConfigManager",
        "getFontConfigAASettings",
        "(Ljava/lang/String;Ljava/lang/String;)I",
        |_c, _a| Ok(Some(Value::Int(-1))),
    );
    registry.register(
        "sun/font/FontConfigManager",
        "getFontConfig",
        "(Ljava/lang/String;Lsun/font/FontConfigManager$FontConfigInfo;[Lsun/font/FontConfigManager$FcCompFont;Z)V",
        n_get_font_config,
    );
    registry.register(
        "sun/awt/FcFontManager",
        "getFontPathNative",
        "(ZZ)Ljava/lang/String;",
        n_get_font_path,
    );
    registry.register(
        "sun/awt/X11FontManager",
        "getFontPathNative",
        "(ZZ)Ljava/lang/String;",
        n_get_font_path,
    );

    const FFS: &str = "sun/font/FreetypeFontScaler";
    registry.register(FFS, "initIDs", "(Ljava/lang/Class;)V", n_noop);
    registry.register(
        FFS,
        "initNativeScaler",
        "(Lsun/font/Font2D;IIZI)J",
        n_init_scaler,
    );
    registry.register(
        FFS,
        "disposeNativeScaler",
        "(Lsun/font/Font2D;J)V",
        n_dispose_scaler,
    );
    registry.register(FFS, "createScalerContextNative", "(J[DIIFF)J", n_create_ctx);
    registry.register(
        FFS,
        "getFontMetricsNative",
        "(Lsun/font/Font2D;JJ)Lsun/font/StrikeMetrics;",
        n_font_metrics,
    );
    registry.register(
        FFS,
        "getGlyphAdvanceNative",
        "(Lsun/font/Font2D;JJI)F",
        n_glyph_advance,
    );
    registry.register(
        FFS,
        "getGlyphMetricsNative",
        "(Lsun/font/Font2D;JJILjava/awt/geom/Point2D$Float;)V",
        n_glyph_metrics,
    );
    registry.register(
        FFS,
        "getGlyphImageNative",
        "(Lsun/font/Font2D;JJI)J",
        n_glyph_image,
    );
    registry.register(
        FFS,
        "getGlyphOutlineBoundsNative",
        "(Lsun/font/Font2D;JJI)Ljava/awt/geom/Rectangle2D$Float;",
        n_glyph_outline_bounds,
    );
    registry.register(
        FFS,
        "getGlyphOutlineNative",
        "(Lsun/font/Font2D;JJIFF)Ljava/awt/geom/GeneralPath;",
        n_glyph_outline,
    );
    registry.register(
        FFS,
        "getGlyphVectorOutlineNative",
        "(Lsun/font/Font2D;JJ[IIFF)Ljava/awt/geom/GeneralPath;",
        n_glyph_vector_outline,
    );
    registry.register(
        FFS,
        "getGlyphPointNative",
        "(Lsun/font/Font2D;JJII)Ljava/awt/geom/Point2D$Float;",
        n_glyph_point,
    );
    registry.register(
        FFS,
        "getGlyphCodeNative",
        "(Lsun/font/Font2D;JC)I",
        n_glyph_code,
    );
    registry.register(FFS, "getNumGlyphsNative", "(J)I", n_num_glyphs);
    registry.register(FFS, "getMissingGlyphCodeNative", "(J)I", n_missing_glyph);
    registry.register(FFS, "getUnitsPerEMNative", "(J)J", n_units_per_em);

    const SC: &str = "sun/font/StrikeCache";
    registry.register(SC, "getInvisibleGlyphPtr", "()J", n_invisible_glyph);
    registry.register(SC, "freeIntPointer", "(I)V", n_free_pointer);
    registry.register(SC, "freeLongPointer", "(J)V", n_free_pointer);
    registry.register(SC, "freeIntMemory", "([IJ)V", n_free_int_memory);
    registry.register(SC, "freeLongMemory", "([JJ)V", n_free_long_memory);

    const SLE: &str = "sun/font/SunLayoutEngine";
    registry.register(SLE, "createFace", "(Lsun/font/Font2D;J)J", n_create_face);
    registry.register(SLE, "disposeFace", "(J)V", n_noop);
}
