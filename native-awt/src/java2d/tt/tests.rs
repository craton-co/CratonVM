// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests against the JDK's own `libfreetype.so` (via the Python
//! oracle `ft4.py`/`ft5.py`). They run only where the DejaVu fonts and the
//! oracle dumps are present (the Linux CI host); elsewhere they pass vacuously.

use std::sync::Arc;

use super::font::TtFont;
use super::{GlyphOutline, TtSize};

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for i in 0..256u32 {
        let mut c = i;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        table[i as usize] = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

pub(crate) fn outline_crc(o: &GlyphOutline) -> u32 {
    let mut b = Vec::new();
    b.extend_from_slice(&o.advance.to_le_bytes());
    for p in &o.pts {
        b.extend_from_slice(&p.x.to_le_bytes());
        b.extend_from_slice(&p.y.to_le_bytes());
    }
    for t in &o.tags {
        b.push(*t);
    }
    for e in &o.contours {
        b.extend_from_slice(&(*e as i16).to_le_bytes());
    }
    crc32(&b)
}

fn font_path(tag: &str) -> &'static str {
    match tag {
        "sans" => "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "bold" => "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        "mono" => "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "serif" => "/usr/share/fonts/truetype/dejavu/DejaVuSerif.ttf",
        _ => "/usr/share/fonts/truetype/dejavu/DejaVuSerif-Bold.ttf",
    }
}

fn load_font(tag: &str) -> Option<TtFont> {
    let d = std::fs::read(font_path(tag)).ok()?;
    TtFont::parse(Arc::new(d), 0)
}

/// Compare hashes of loaded outlines with `orc-<font>-<mode>.txt`
/// (`size gid adv npoints crc` lines from `ft4.py hash`).
fn compare(tag: &str, mode: &str, hinted: bool) {
    let Some(font) = load_font(tag) else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(format!("/data/sbr5/awtjfr/orc-{tag}-{mode}.txt"))
    else {
        return;
    };
    let mut size_cache: Option<(f64, TtSize)> = None;
    let (mut total, mut bad) = (0usize, 0usize);
    let mut firsts: Vec<String> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let size: f64 = f[0].parse().unwrap();
        let gid: u16 = f[1].parse().unwrap();
        let want_crc = u32::from_str_radix(f[4], 16).unwrap();
        if size_cache.as_ref().map(|c| c.0) != Some(size) {
            size_cache = Some((size, TtSize::new(&font, (size * 64.0) as i64)));
        }
        let sz = &mut size_cache.as_mut().unwrap().1;
        total += 1;
        let ok = match sz.load(&font, gid, hinted) {
            Some(o) => outline_crc(&o) == want_crc,
            None => false,
        };
        if !ok {
            bad += 1;
            if firsts.len() < 40 {
                firsts.push(format!("size {size} gid {gid}"));
            }
        }
    }
    eprintln!("{tag} {mode}: {bad} of {total} outlines differ; first: {firsts:?}");
    assert_eq!(bad, 0, "{tag} {mode}: {bad} of {total}; first {firsts:?}");
}

#[test]
fn unhinted_outlines_match_freetype() {
    compare("sans", "nohint", false);
}

#[test]
fn hinted_outlines_match_freetype_sans() {
    compare("sans", "hint", true);
}

#[test]
fn hinted_outlines_match_freetype_other_faces() {
    for tag in ["bold", "mono", "serif", "serifb"] {
        compare(tag, "hint", true);
    }
}

/// `TT_FONT=sans TT_SIZE=12 TT_GID=68 TT_HINT=1 cargo test dump_outline -- --nocapture`
#[test]
fn dump_outline() {
    let (Ok(tag), Ok(size), Ok(gid)) = (
        std::env::var("TT_FONT"),
        std::env::var("TT_SIZE"),
        std::env::var("TT_GID"),
    ) else {
        return;
    };
    let hinted = std::env::var("TT_HINT").map(|v| v == "1").unwrap_or(true);
    let font = load_font(&tag).unwrap();
    let size: f64 = size.parse().unwrap();
    let mut sz = TtSize::new(&font, (size * 64.0) as i64);
    let mut o = sz.load(&font, gid.parse().unwrap(), hinted).unwrap();
    if std::env::var("TT_BOLD").is_ok() {
        super::embolden::embolden(&mut o.pts, &o.contours, (size * 64.0) as i64 / 32);
    }
    println!("adv {} n {} ends {:?}", o.advance, o.pts.len(), o.contours);
    for (i, (p, t)) in o.pts.iter().zip(&o.tags).enumerate() {
        println!("{i} {} {} {t}", p.x, p.y);
    }
}

#[test]
fn norm_len_matches_freetype() {
    let Ok(text) = std::fs::read_to_string("/data/sbr5/awtjfr/orc-normlen.txt") else {
        return;
    };
    let (mut bad, mut total) = (0, 0);
    let mut firsts = Vec::new();
    for line in text.lines() {
        let f: Vec<i64> = line
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let (vx, vy) = super::fixed::norm_len(f[0], f[1]);
        total += 1;
        if (vx, vy) != (f[2], f[3]) {
            bad += 1;
            if firsts.len() < 8 {
                firsts.push(format!(
                    "{} {} want {} {} got {vx} {vy}",
                    f[0], f[1], f[2], f[3]
                ));
            }
        }
    }
    assert_eq!(bad, 0, "norm_len {bad}/{total}: {firsts:?}");
}

fn compare_bitmaps(tag: &str, mode: &str, hinted: bool, suffix: &str) {
    let Some(font) = load_font(tag) else {
        return;
    };
    let Ok(text) =
        std::fs::read_to_string(format!("/data/sbr5/awtjfr/orb-{tag}-{mode}-{suffix}.txt"))
    else {
        return;
    };
    let mut size_cache: Option<(f64, TtSize)> = None;
    let (mut total, mut bad) = (0usize, 0usize);
    let mut firsts: Vec<String> = Vec::new();
    let mut all_bad: Vec<String> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 7 {
            continue;
        }
        let size: f64 = f[0].parse().unwrap();
        let gid: u16 = f[1].parse().unwrap();
        let want: (i32, i32, usize, usize) = (
            f[2].parse().unwrap(),
            f[3].parse().unwrap(),
            f[4].parse().unwrap(),
            f[5].parse().unwrap(),
        );
        let want_crc = u32::from_str_radix(f[6], 16).unwrap();
        if size_cache.as_ref().map(|c| c.0) != Some(size) {
            size_cache = Some((size, TtSize::new(&font, (size * 64.0) as i64)));
        }
        let sz = &mut size_cache.as_mut().unwrap().1;
        total += 1;
        let ok = match sz.load(&font, gid, hinted) {
            Some(mut o) => {
                if suffix == "b" {
                    super::embolden::embolden(&mut o.pts, &o.contours, (size * 64.0) as i64 / 32);
                }
                let bm = if suffix == "n" || suffix == "b" {
                    super::gray::render(&o.pts, &o.tags, &o.contours)
                } else if suffix == "l" || suffix == "v" {
                    super::gray::render_lcd(&o.pts, &o.tags, &o.contours, suffix == "v")
                } else {
                    super::mono::render(&o.pts, &o.tags, &o.contours, sz.ppem < 24, &o.marks)
                };
                let empty = o.pts.is_empty();
                if empty {
                    want.2 == 0 && want.3 == 0
                } else {
                    (bm.left, bm.top, bm.width, bm.rows) == want
                        && crc32(&bm.packed(suffix == "m")) == want_crc
                }
            }
            None => false,
        };
        if !ok {
            bad += 1;
            all_bad.push(format!("size {size} gid {gid}"));
            if firsts.len() < 40 {
                firsts.push(format!("size {size} gid {gid}"));
            }
        }
    }
    let mut hist: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for f in &all_bad {
        *hist
            .entry(f.split_whitespace().nth(1).unwrap_or("?").to_string())
            .or_default() += 1;
    }
    if let Ok(sz) = std::env::var("TT_FAILSIZE") {
        let list: Vec<&str> = all_bad
            .iter()
            .filter(|f| f.split_whitespace().nth(1) == Some(sz.as_str()))
            .map(|f| f.split_whitespace().nth(3).unwrap_or("?"))
            .collect();
        eprintln!(
            "{tag} {mode} {suffix}: failing gids at size {sz}: {}",
            list.join(",")
        );
    }
    eprintln!("{tag} {mode} {suffix}: per-size failures {hist:?}");
    eprintln!("{tag} {mode} {suffix}: {bad} of {total} bitmaps differ; first: {firsts:?}");
    assert_eq!(
        bad, 0,
        "{tag} {mode} {suffix}: {bad} of {total}; first {firsts:?}"
    );
}

#[test]
fn gray_bitmaps_match_freetype() {
    compare_bitmaps("sans", "nohint", false, "n");
    compare_bitmaps("sans", "hint", true, "n");
    compare_bitmaps("serif", "hint", true, "n");
    compare_bitmaps("mono", "hint", true, "n");
}

/// `TT_FONT=sans TT_SIZE=12 TT_GID=68 TT_HINT=1 TT_MONO=0 cargo test dump_bitmap -- --nocapture`
#[test]
fn dump_bitmap() {
    let (Ok(tag), Ok(size), Ok(gid)) = (
        std::env::var("TT_FONT"),
        std::env::var("TT_SIZE"),
        std::env::var("TT_GID"),
    ) else {
        return;
    };
    let hinted = std::env::var("TT_HINT").map(|v| v == "1").unwrap_or(true);
    let mono = std::env::var("TT_MONO").map(|v| v == "1").unwrap_or(false);
    let font = load_font(&tag).unwrap();
    let size: f64 = size.parse().unwrap();
    let mut sz = TtSize::new(&font, (size * 64.0) as i64);
    let o = sz.load(&font, gid.parse().unwrap(), hinted).unwrap();
    let bm = if mono {
        super::mono::render(&o.pts, &o.tags, &o.contours, sz.ppem < 24, &o.marks)
    } else {
        super::gray::render(&o.pts, &o.tags, &o.contours)
    };
    println!(
        "left {} top {} w {} rows {}",
        bm.left, bm.top, bm.width, bm.rows
    );
    for r in 0..bm.rows {
        let row: Vec<String> = (0..bm.width)
            .map(|c| format!("{:02x}", bm.data[r * bm.width + c]))
            .collect();
        println!("{}", row.join(" "));
    }
}

#[test]
fn emboldened_bitmaps_match_freetype() {
    compare_bitmaps("sans", "hint", true, "b");
    compare_bitmaps("sans", "nohint", false, "b");
    compare_bitmaps("serif", "hint", true, "b");
}

#[test]
fn lcd_bitmaps_match_freetype() {
    compare_bitmaps("sans", "hint", true, "l");
    compare_bitmaps("sans", "nohint", false, "l");
    compare_bitmaps("sans", "hint", true, "v");
    compare_bitmaps("sans", "nohint", false, "v");
}

#[test]
fn mono_bitmaps_match_freetype() {
    compare_bitmaps("sans", "hint", true, "m");
    compare_bitmaps("mono", "hint", true, "m");
}
