// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Complex-text layout: `sun.font.SunLayoutEngine.layout` shaped with `rustybuzz`
//! (a pure-Rust port of HarfBuzz; the JDK shapes with the HarfBuzz it links into
//! `libfontmanager`).
//!
//! `SunLayoutEngine.layout` is a reviewed `Intrinsic`: the JDK's own body reaches
//! HarfBuzz through the Foreign Function & Memory API (`sun.font.HBShaper` binding
//! `HBCreateFace`/`jdk_hb_shape` and upcall stubs), which needs the JDK's
//! `MethodHandle`/`VarHandle` machinery to run over this VM's handle model. This
//! computes what `HBShaper.shape` + `store_layout_results` compute: the glyph ids,
//! clusters, advances (the JDK strike's, with GPOS adjustments added at
//! HarfBuzz's own 16.16 rounding) and offsets, written into the `GVData`.

use std::collections::HashMap;
use std::sync::Arc;

use cratonvm_native_api::vm_scoped::VmScoped;
use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ObjectRef, Value};
use rustybuzz::ttf_parser::{GlyphId, Tag};
use rustybuzz::{BufferClusterLevel, Direction, Face, Feature, Script, UnicodeBuffer};

/// Tables a shaper may ask for, in the order they are probed.
const TABLES: &[&[u8; 4]] = &[
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"OS/2", b"post", b"name", b"glyf", b"loca",
    b"GDEF", b"GSUB", b"GPOS", b"kern", b"morx", b"kerx", b"ankr", b"trak", b"feat", b"mort",
    b"fvar", b"avar", b"gvar", b"HVAR", b"MVAR", b"STAT", b"BASE", b"JSTF", b"MATH", b"CFF ",
    b"CFF2", b"vhea", b"vmtx", b"VORG", b"VVAR", b"sbix", b"CBDT", b"CBLC", b"COLR", b"CPAL",
    b"SVG ", b"fpgm", b"prep", b"cvt ", b"gasp", b"hdmx",
];

struct FaceData {
    // Declared first: the face borrows `sfnt` and must drop before it.
    face: Face<'static>,
    #[allow(dead_code)]
    sfnt: Vec<u8>,
    upem: i64,
}

// SAFETY: a `Face` is immutable parsed data over the owned `sfnt` buffer.
unsafe impl Send for FaceData {}
unsafe impl Sync for FaceData {}

/// Font2D identity hash -> its shaping face (per VM).
static FACES: VmScoped<HashMap<i32, Arc<FaceData>>> = VmScoped::new();

/// Unicode `Default_Ignorable_Code_Point`.
fn default_ignorable(cp: u32) -> bool {
    matches!(
        cp,
        0x00AD | 0x034F | 0x061C | 0x115F..=0x1160 | 0x17B4..=0x17B5 | 0x180B..=0x180F | 0x200B..=0x200F
            | 0x202A..=0x202E | 0x2060..=0x206F | 0x3164 | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0
            | 0xFFF0..=0xFFF8 | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A | 0xE0000..=0xE0FFF
    )
}

fn is_variation_selector(cp: u32) -> bool {
    matches!(cp, 0xFE00..=0xFE0F | 0xE0100..=0xE01EF | 0x180B..=0x180D | 0x180F)
}

/// ICU `UScriptCode` -> ISO 15924 tag (the numbering `sun.font.ScriptRun` reports).
const SCRIPT_TAGS: &[&[u8; 4]] = &[
    b"Zyyy", b"Zinh", b"Arab", b"Armn", b"Beng", b"Bopo", b"Cher", b"Copt", b"Cyrl", b"Dsrt",
    b"Deva", b"Ethi", b"Geor", b"Goth", b"Grek", b"Gujr", b"Guru", b"Hani", b"Hang", b"Hebr",
    b"Hira", b"Knda", b"Kana", b"Khmr", b"Laoo", b"Latn", b"Mlym", b"Mong", b"Mymr", b"Ogam",
    b"Ital", b"Orya", b"Runr", b"Sinh", b"Syrc", b"Taml", b"Telu", b"Thaa", b"Thai", b"Tibt",
    b"Cans", b"Yiii", b"Tglg", b"Hano", b"Buhd", b"Tagb", b"Brai", b"Cprt", b"Limb", b"Linb",
    b"Osma", b"Shaw", b"Tale", b"Ugar", b"Hrkt", b"Bugi", b"Glag", b"Khar", b"Sylo", b"Talu",
    b"Tfng", b"Xpeo", b"Bali", b"Batk", b"Blis", b"Brah", b"Cham", b"Cirt", b"Cyrs", b"Egyd",
    b"Egyh", b"Egyp", b"Geok", b"Hans", b"Hant", b"Hmng", b"Hung", b"Inds", b"Java", b"Kali",
    b"Latf", b"Latg", b"Lepc", b"Lina", b"Mand", b"Maya", b"Mero", b"Nkoo", b"Orkh", b"Perm",
    b"Phag", b"Phnx", b"Plrd", b"Roro", b"Sara", b"Syre", b"Syrj", b"Syrn", b"Teng", b"Vaii",
    b"Visp", b"Xsux", b"Zxxx", b"Zzzz", b"Cari", b"Jpan", b"Lana", b"Lyci", b"Lydi", b"Olck",
    b"Rjng", b"Saur", b"Sgnw", b"Sund", b"Moon", b"Mtei", b"Armi", b"Avst", b"Cakm", b"Kore",
    b"Kthi", b"Mani", b"Phli", b"Phlp", b"Phlv", b"Prti", b"Samr", b"Tavt", b"Zmth", b"Zsym",
    b"Bamu", b"Lisu", b"Nkgb", b"Sarb", b"Bass", b"Dupl", b"Elba", b"Gran", b"Kpel", b"Loma",
    b"Mend", b"Merc", b"Narb", b"Nbat", b"Palm", b"Sind", b"Wara", b"Afak", b"Jurc", b"Mroo",
    b"Nshu", b"Shrd", b"Sora", b"Takr", b"Tang", b"Wole", b"Hluw", b"Khoj", b"Tirh",
];

fn script_of(code: i32) -> Option<Script> {
    let tag = SCRIPT_TAGS.get(usize::try_from(code).ok()?)?;
    Script::from_iso15924_tag(Tag::from_bytes(tag))
}

fn assemble_sfnt(tables: &[([u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len();
    let mut max_pow = 1usize;
    let mut entry_selector = 0u16;
    while max_pow * 2 <= n {
        max_pow *= 2;
        entry_selector += 1;
    }
    let search_range = (max_pow * 16) as u16;
    let range_shift = (n * 16) as u16 - search_range;
    let cff = tables.iter().any(|(t, _)| t == b"CFF " || t == b"CFF2");
    let mut out = Vec::new();
    out.extend_from_slice(if cff { b"OTTO" } else { &[0, 1, 0, 0] });
    out.extend_from_slice(&(n as u16).to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());
    let mut offset = 12 + 16 * n;
    let mut sorted: Vec<&([u8; 4], Vec<u8>)> = tables.iter().collect();
    sorted.sort_by_key(|(t, _)| *t);
    for (tag, data) in &sorted {
        out.extend_from_slice(tag);
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(offset as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        offset += (data.len() + 3) & !3;
    }
    for (_, data) in &sorted {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

fn internal(msg: impl Into<String>) -> MethodCallFailed {
    RuntimeError::InternalError {
        message: msg.into(),
    }
    .into()
}

fn int_of(v: Option<Value>) -> i32 {
    match v {
        Some(Value::Int(i)) => i,
        _ => 0,
    }
}

fn obj_of(v: Option<Value>) -> Option<ObjectRef> {
    match v {
        Some(Value::Object(o)) => o,
        _ => None,
    }
}

/// The face of `font` (`Font2D`), built from its tables on first use.
fn face_of(ctx: &mut dyn NativeContext, font: ObjectRef) -> Option<Arc<FaceData>> {
    let vm = ctx.vm_identity();
    let key = ctx.identity_hash_code(font);
    if let Some(f) = FACES.with(vm, |m| m.get(&key).cloned()) {
        return Some(f);
    }
    let mut scope = NativeHandleScope::new(ctx);
    let font_h = scope.root(font);
    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::new();
    for tag in TABLES {
        let font_now = scope.get(&font_h);
        let t = i32::from_be_bytes(**tag);
        let got = scope
            .invoke_virtual(font_now, "getTableBytes", "(I)[B", &[Value::Int(t)])
            .ok()?;
        if let Some(arr) = obj_of(got) {
            let n = scope.array_length(arr);
            if n > 0 {
                let mut data = vec![0u8; n];
                scope.read_byte_array_into(arr, 0, &mut data);
                tables.push((**tag, data));
            }
        }
    }
    drop(scope);
    if tables.is_empty() {
        return None;
    }
    let sfnt = assemble_sfnt(&tables);
    // SAFETY: `face` borrows `sfnt`'s heap buffer, which `FaceData` owns (and never
    // moves: `Vec` heap data is stable) for as long as the face exists.
    let slice: &'static [u8] = unsafe { std::slice::from_raw_parts(sfnt.as_ptr(), sfnt.len()) };
    let face = Face::from_slice(slice, 0)?;
    let upem = face.units_per_em() as i64;
    let fd = Arc::new(FaceData { face, sfnt, upem });
    FACES.with(vm, |m| {
        m.insert(key, fd.clone());
    });
    Some(fd)
}

struct Glyph {
    gid: u32,
    cluster: u32,
    x_adv: i32,
    y_adv: i32,
    x_off: i32,
    y_off: i32,
}

/// `SunLayoutEngine.layout(FontStrikeDesc desc, float[] mat, float ptSize, int gmask,
/// int baseIndex, TextRecord tr, int typo_flags, Point2D.Float pt, GVData data)`.
fn n_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let obj = |i: usize| -> Result<ObjectRef, MethodCallFailed> {
        obj_of(args.get(i).copied())
            .ok_or_else(|| RuntimeError::NullPointerException { message: None }.into())
    };
    let this = obj(0)?;
    let desc = obj(1)?;
    let pt_size = match args.get(3) {
        Some(Value::Float(f)) => *f,
        _ => 12.0,
    };
    let gmask = int_of(args.get(4).copied());
    let base_index = int_of(args.get(5).copied());
    let tr = obj(6)?;
    let flags = int_of(args.get(7).copied());
    let pt = obj(8)?;
    let data = obj(9)?;
    if std::env::var_os("CRATONVM_DBG_HB").is_some() {
        eprintln!("[hb] layout flags={flags:#x} gmask={gmask:#x} base={base_index} pt={pt_size}");
    }

    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let desc_h = scope.root(desc);
    let tr_h = scope.root(tr);
    let pt_h = scope.root(pt);
    let data_h = scope.root(data);

    // font = key.font(), script = key.script(), strike = font.getStrike(desc)
    let this_now = scope.get(&this_h);
    let key = obj_of(Some(scope.get_field_by_name(this_now, "key")))
        .ok_or_else(|| internal("SunLayoutEngine: no key"))?;
    let key_h = scope.root(key);
    let key_now = scope.get(&key_h);
    let font = obj_of(scope.invoke_virtual(key_now, "font", "()Lsun/font/Font2D;", &[])?)
        .ok_or_else(|| internal("SunLayoutEngine: no font"))?;
    let font_h = scope.root(font);
    let key_now = scope.get(&key_h);
    let script = int_of(scope.invoke_virtual(key_now, "script", "()I", &[])?);
    let font_now = scope.get(&font_h);
    let desc_now = scope.get(&desc_h);
    let strike = obj_of(scope.invoke_virtual(
        font_now,
        "getStrike",
        "(Lsun/font/FontStrikeDesc;)Lsun/font/FontStrike;",
        &[Value::Object(Some(desc_now))],
    )?)
    .ok_or_else(|| internal("SunLayoutEngine: no strike"))?;
    let strike_h = scope.root(strike);

    let font_now = scope.get(&font_h);
    let Some(face) = face_of(&mut *scope, font_now) else {
        return Ok(None);
    };

    // The run: tr.text[start..limit].
    let tr_now = scope.get(&tr_h);
    let start = int_of(Some(scope.get_field_by_name(tr_now, "start"))).max(0) as usize;
    let limit = int_of(Some(scope.get_field_by_name(tr_now, "limit"))).max(0) as usize;
    let Some(text_arr) = obj_of(Some(scope.get_field_by_name(tr_now, "text"))) else {
        return Ok(None);
    };
    let len = scope.array_length(text_arr);
    let mut text = vec![0u16; len];
    scope.read_char_array_into(text_arr, 0, &mut text);
    let limit = limit.min(len);

    // Code points by UTF-16 index (the cluster values).
    let mut cps: Vec<(usize, u32)> = Vec::new();
    let mut i = start;
    while i < limit {
        let u = text[i];
        let (cp, n) = if (0xD800..0xDC00).contains(&u)
            && i + 1 < limit
            && (0xDC00..0xE000).contains(&text[i + 1])
        {
            (
                0x10000 + (((u as u32) - 0xD800) << 10) + (text[i + 1] as u32 - 0xDC00),
                2,
            )
        } else {
            (u as u32, 1)
        };
        cps.push((i, cp));
        i += n;
    }

    // A variation selector the font's variation lookup maps with its base is merged
    // into the base glyph (HarfBuzz's `get_variation_glyph` path).
    let mut merged: Vec<(usize, u32)> = Vec::new(); // (base cluster, glyph)
    let mut buf = UnicodeBuffer::new();
    let mut k = 0;
    while k < cps.len() {
        let (idx, cp) = cps[k];
        if k + 1 < cps.len() && is_variation_selector(cps[k + 1].1) {
            let font_now = scope.get(&font_h);
            let g = int_of(scope.invoke_virtual(
                font_now,
                "charToVariationGlyphRaw",
                "(II)I",
                &[Value::Int(cp as i32), Value::Int(cps[k + 1].1 as i32)],
            )?);
            if g != 0 {
                merged.push((idx, g as u32));
                buf.add(char::from_u32(cp).unwrap_or('\u{FFFD}'), idx as u32);
                k += 2;
                continue;
            }
        }
        buf.add(char::from_u32(cp).unwrap_or('\u{FFFD}'), idx as u32);
        k += 1;
    }
    if (flags as u32) & 0x8000_0000 != 0 {
        buf.set_direction(Direction::RightToLeft);
    } else {
        buf.set_direction(Direction::LeftToRight);
    }
    if let Some(s) = script_of(script) {
        buf.set_script(s);
    }
    buf.set_cluster_level(BufferClusterLevel::MonotoneCharacters);
    // Kerning and ligatures are the font's TextAttributes: off unless asked for.
    let on = |bit: i32| u32::from(flags & bit != 0);
    let features = vec![
        Feature::new(Tag::from_bytes(b"kern"), on(1), ..),
        Feature::new(Tag::from_bytes(b"liga"), on(2), ..),
        Feature::new(Tag::from_bytes(b"clig"), on(2), ..),
    ];
    let rtl = (flags as u32) & 0x8000_0000 != 0;
    let out = rustybuzz::shape(&face.face, &features, buf);
    let mut infos: Vec<&rustybuzz::GlyphInfo> = out.glyph_infos().iter().collect();
    let mut poss: Vec<&rustybuzz::GlyphPosition> = out.glyph_positions().iter().collect();
    // rustybuzz 0.20 hands the buffer back reversed when `kern` is switched off for some
    // scripts (Arabic); HarfBuzz returns glyphs by direction: clusters ascending for LTR,
    // descending for RTL.
    if let (Some(first), Some(last)) = (infos.first(), infos.last()) {
        if (!rtl && first.cluster > last.cluster) || (rtl && first.cluster < last.cluster) {
            infos.reverse();
            poss.reverse();
        }
    }
    let n = infos.len();

    // HarfBuzz font scale: 16.16 pixels per em.
    let scale = (pt_size * 65536.0) as i32 as i64;
    let x_mult = (scale << 16) / face.upem.max(1);
    let em = |v: i32| -> i32 { (((v as i64) * x_mult + 32768) >> 16) as i32 };

    let space_gid = face.face.glyph_index(' ').map(|g| g.0 as u32);
    let mut glyphs: Vec<Glyph> = Vec::with_capacity(n);
    let mut adv_cache: HashMap<u32, i32> = HashMap::new();
    for (info, pos) in infos.iter().zip(poss.iter()) {
        let mut gid = info.glyph_id;
        if let Some(&(_, g)) = merged.iter().find(|(c, _)| *c as u32 == info.cluster) {
            gid = g;
        } else {
            // A default-ignorable (ZWJ, ZWNJ, ...) is hidden as the invisible glyph 0xFFFF the
            // JDK hands HarfBuzz, whether or not the font maps it.
            let cp = cps
                .iter()
                .find(|(c, _)| *c as u32 == info.cluster)
                .map(|t| t.1)
                .unwrap_or(0);
            // rustybuzz hides an ignorable as a zero-advance space glyph (the cluster of a
            // joiner can be its neighbour's, so the glyph is the evidence, not the code point).
            let hidden_space =
                Some(info.glyph_id) == space_gid && pos.x_advance == 0 && pos.x_offset == 0;
            if default_ignorable(cp) || hidden_space {
                gid = 0xFFFF;
            }
        }
        let rb = pos.x_advance;
        let x_adv = if rb == 0 {
            0
        } else {
            let jdk = match adv_cache.get(&gid) {
                Some(&a) => a,
                None => {
                    // `HBShaper.get_glyph_h_advance`: the strike's advance as 16.16.
                    let strike_now = scope.get(&strike_h);
                    let p = scope.invoke_virtual(
                        strike_now,
                        "getGlyphMetrics",
                        "(I)Ljava/awt/geom/Point2D$Float;",
                        &[Value::Int(gid as i32)],
                    )?;
                    let a = match obj_of(p) {
                        Some(p) => match scope.get_field_by_name(p, "x") {
                            Value::Float(f) => (f * 65536.0) as i32,
                            _ => 0,
                        },
                        None => 0,
                    };
                    adv_cache.insert(gid, a);
                    a
                }
            };
            let hmtx = face
                .face
                .glyph_hor_advance(GlyphId(gid as u16))
                .unwrap_or(0) as i32;
            jdk.wrapping_add(em(rb - hmtx))
        };
        glyphs.push(Glyph {
            gid,
            cluster: info.cluster,
            x_adv,
            y_adv: em(pos.y_advance),
            x_off: em(pos.x_offset),
            y_off: em(pos.y_offset),
        });
    }
    // `store_layout_results`: write the glyphs after the ones already in `data`.
    let data_now = scope.get(&data_h);
    let count = int_of(Some(scope.get_field_by_name(data_now, "_count"))).max(0) as usize;
    let pt_now = scope.get(&pt_h);
    let start_x = match scope.invoke_virtual(pt_now, "getX", "()D", &[])? {
        Some(Value::Double(d)) => d as f32,
        _ => 0.0,
    };
    let pt_now = scope.get(&pt_h);
    let start_y = match scope.invoke_virtual(pt_now, "getY", "()D", &[])? {
        Some(Value::Double(d)) => d as f32,
        _ => 0.0,
    };
    let char_count = limit.saturating_sub(start);
    let need = count + char_count.max(n);
    let field_arr = |scope: &NativeHandleScope<'_>, name: &str| -> Option<ObjectRef> {
        obj_of(Some(scope.get_field_by_name(scope.get(&data_h), name)))
    };
    let glyphs_len = field_arr(&scope, "_glyphs")
        .map(|a| scope.array_length(a))
        .unwrap_or(0);
    let positions_len = field_arr(&scope, "_positions")
        .map(|a| scope.array_length(a))
        .unwrap_or(0);
    if need > glyphs_len || need * 2 + 2 > positions_len {
        let data_now = scope.get(&data_h);
        scope.invoke_virtual(
            data_now,
            "grow",
            "(I)V",
            &[Value::Int((need - count) as i32)],
        )?;
    }
    // `1.5258789E-5f / devScale` with devScale 1.
    let f = f32::from_bits(0x3780_0000);
    let (mut adv_x, mut adv_y) = (0.0f32, 0.0f32);
    for (gi, g) in glyphs.iter().enumerate() {
        let index = count + gi;
        let cluster = g.cluster as i32 - start as i32;
        if let Some(a) = field_arr(&scope, "_indices") {
            scope.set_array_element(a, index, Value::Int(base_index + cluster));
        }
        if let Some(a) = field_arr(&scope, "_glyphs") {
            scope.set_array_element(a, index, Value::Int(gmask | g.gid as i32));
        }
        if let Some(a) = field_arr(&scope, "_positions") {
            scope.set_array_element(
                a,
                index * 2,
                Value::Float((start_x + adv_x) + (g.x_off as f32 * f)),
            );
            scope.set_array_element(
                a,
                index * 2 + 1,
                Value::Float((start_y + adv_y) - (g.y_off as f32 * f)),
            );
        }
        adv_x += g.x_adv as f32 * f;
        adv_y += g.y_adv as f32 * f;
    }
    let end = count + n;
    let data_now = scope.get(&data_h);
    scope.set_field_by_name(data_now, "_count", Value::Int(end as i32));
    let (ex, ey) = (start_x + adv_x, start_y + adv_y);
    if let Some(a) = field_arr(&scope, "_positions") {
        scope.set_array_element(a, end * 2, Value::Float(ex));
        scope.set_array_element(a, end * 2 + 1, Value::Float(ey));
    }
    let pt_now = scope.get(&pt_h);
    scope.set_field_by_name(pt_now, "x", Value::Float(ex));
    scope.set_field_by_name(pt_now, "y", Value::Float(ey));
    Ok(None)
}

/// Register `SunLayoutEngine.layout` as a reviewed intrinsic.
pub fn register(registry: &mut NativeMethodRegistry) {
    let prev = registry.current_category();
    registry.set_category(NativeKind::Intrinsic);
    registry.register(
        "sun/font/SunLayoutEngine",
        "layout",
        "(Lsun/font/FontStrikeDesc;[FFIILsun/font/TextRecord;ILjava/awt/geom/Point2D$Float;Lsun/font/GlyphLayout$GVData;)V",
        n_layout,
    );
    registry.set_category(prev);
}
