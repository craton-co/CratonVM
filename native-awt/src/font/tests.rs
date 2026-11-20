use super::*;

// ── FontSpec ──────────────────────────────────────────────────────

#[test]
fn font_spec_style_flags() {
    let plain = FontSpec::new("Dialog", PLAIN, 12);
    assert!(!plain.is_bold());
    assert!(!plain.is_italic());

    let bold = FontSpec::new("Dialog", BOLD, 12);
    assert!(bold.is_bold());
    assert!(!bold.is_italic());

    let italic = FontSpec::new("Dialog", ITALIC, 12);
    assert!(!italic.is_bold());
    assert!(italic.is_italic());

    let bi = FontSpec::new("Dialog", BOLD_ITALIC, 12);
    assert!(bi.is_bold());
    assert!(bi.is_italic());
}

// ── Metrics ──────────────────────────────────────────────────────

#[test]
fn metrics_size_12() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("SansSerif", PLAIN, 12);
    let m = engine.get_metrics(&spec);

    assert_eq!(m.ascent, 10);
    assert_eq!(m.descent, 2);
    assert_eq!(m.leading, 1);
    assert_eq!(m.height, 13);
    // Bug awt-font-image #1: max_advance is the widest per-glyph advance
    // (0.80 * 12 = 9.6 -> 10), matching the shared advance model, not the
    // old decoupled `spec.size` upper bound (12).
    assert_eq!(m.max_advance, 10);
}

#[test]
fn metrics_monospaced() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("Monospaced", PLAIN, 20);
    let m = engine.get_metrics(&spec);

    assert_eq!(m.ascent, 16);
    assert_eq!(m.descent, 4);
    assert_eq!(m.leading, 1);
    assert_eq!(m.height, 21);
    assert_eq!(m.max_advance, 12);
}

// PERF (awt-perf #1): the CLOCK eviction must keep the metrics cache
// bounded at `METRICS_CACHE_CAP`, must keep the `clock` hand in 1:1
// correspondence with the map, and must give a recently-used (referenced)
// entry a second chance over a cold one.
#[test]
fn metrics_cache_clock_eviction_bounds_and_protects_hot_entry() {
    let mut engine = FontEngine::new();

    // A "hot" entry we will keep touching so its referenced bit stays set.
    let hot = FontSpec::new("SansSerif", PLAIN, 7);
    let _ = engine.get_metrics(&hot);

    // Overflow the cache well past capacity with distinct (size) keys,
    // re-touching the hot entry along the way so it earns a second chance.
    for size in 100..(100 + METRICS_CACHE_CAP as i32 + 50) {
        let _ = engine.get_metrics(&FontSpec::new("SansSerif", PLAIN, size));
        let _ = engine.get_metrics(&hot); // refresh referenced bit
    }

    // Never exceeds the hard cap, and the clock mirrors the map exactly.
    assert!(engine.metrics_cache.len() <= METRICS_CACHE_CAP);
    assert_eq!(engine.metrics_cache.len(), engine.clock.len());

    // The continually-touched hot entry survived the churn.
    let hot_key = (intern_arc(&hot.family), hot.style, hot.size);
    assert!(
        engine.metrics_cache.contains_key(&hot_key),
        "CLOCK must not evict a repeatedly-referenced entry"
    );

    // Metrics are a pure function of the key, so any value the cache
    // returns equals a fresh computation — eviction can never change it.
    assert_eq!(engine.get_metrics(&hot), FontEngine::compute_metrics(&hot));
}

#[test]
fn metrics_caching() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("Dialog", PLAIN, 14);
    let m1 = engine.get_metrics(&spec);
    let m2 = engine.get_metrics(&spec);
    assert_eq!(m1, m2);
}

#[test]
fn metrics_height_is_sum() {
    let mut engine = FontEngine::new();
    for size in [8, 10, 12, 14, 16, 18, 24, 36, 48, 72] {
        let spec = FontSpec::new("Dialog", PLAIN, size);
        let m = engine.get_metrics(&spec);
        assert_eq!(m.height, m.ascent + m.descent + m.leading, "size={}", size);
    }
}

// ── String width ─────────────────────────────────────────────────

#[test]
fn string_width_empty() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("Dialog", PLAIN, 12);
    assert_eq!(engine.string_width(&spec, ""), 0);
}

#[test]
fn string_width_proportional() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("SansSerif", PLAIN, 20);
    let w = engine.string_width(&spec, "Hello");
    // Bug awt-font-image #1: per-glyph sum, not char_count * average.
    // H,e,o = 0.55 each; l,l = 0.35 each -> (3*0.55 + 2*0.35) * 20 = 47.
    assert_eq!(w, 47);
}

#[test]
fn string_width_monospaced() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("Monospaced", PLAIN, 20);
    let w = engine.string_width(&spec, "abc");
    assert_eq!(w, 36);
}

#[test]
fn string_width_bold_wider() {
    let mut engine = FontEngine::new();
    let plain = FontSpec::new("SansSerif", PLAIN, 20);
    let bold = FontSpec::new("SansSerif", BOLD, 20);
    let wp = engine.string_width(&plain, "Hello World");
    let wb = engine.string_width(&bold, "Hello World");
    assert!(wb > wp, "bold ({}) should be wider than plain ({})", wb, wp);
}

// ── Shared advance model agreement (bug awt-font-image #1) ───────────

#[test]
fn string_width_equals_sum_of_glyph_advances() {
    // FontMetrics.stringWidth, FontEngine::string_width, and the
    // Graphics2D::draw_string pen advance must all derive from ONE model.
    // Here we prove FontEngine::string_width equals round(sum of
    // per-glyph glyph_advance), i.e. the same arithmetic draw_string runs.
    let mut engine = FontEngine::new();
    for (family, style, size, text) in [
        ("SansSerif", PLAIN, 20, "Hello World"),
        ("Serif", BOLD, 14, "Mixed Width jiM!"),
        ("Monospaced", PLAIN, 18, "code()"),
        ("Dialog", BOLD_ITALIC, 12, "aWl.iM"),
    ] {
        let spec = FontSpec::new(family, style, size);
        let summed: f64 = text
            .chars()
            .map(|ch| glyph_advance(family, style, size, ch))
            .sum();
        assert_eq!(
            engine.string_width(&spec, text),
            summed.round() as i32,
            "string_width must equal the summed per-glyph advance for {family}/{style}/{size} {text:?}",
        );
        // And the convenience aggregate matches the per-glyph sum exactly.
        assert!(
            (text_advance(family, style, size, text) - summed).abs() < 1e-9,
            "text_advance must equal the per-glyph sum",
        );
    }
}

#[test]
fn max_advance_is_widest_glyph() {
    // getMaxAdvance must bound every single-glyph advance.
    let family = "SansSerif";
    let (style, size) = (PLAIN, 24);
    let maxa = max_glyph_advance(family, style, size);
    for ch in "iMWla@. jr".chars() {
        assert!(
            glyph_advance(family, style, size, ch) <= maxa + 1e-9,
            "glyph {ch:?} advance exceeds max_advance {maxa}",
        );
    }
}

#[test]
fn string_width_serif_wider_than_sans() {
    let mut engine = FontEngine::new();
    let sans = FontSpec::new("SansSerif", PLAIN, 20);
    let serif = FontSpec::new("Serif", PLAIN, 20);
    let ws = engine.string_width(&sans, "Hello World");
    let wse = engine.string_width(&serif, "Hello World");
    assert!(
        wse > ws,
        "serif ({}) should be wider than sans ({})",
        wse,
        ws
    );
}

// ── Char width ───────────────────────────────────────────────────

#[test]
fn char_width_narrow_vs_wide() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("SansSerif", PLAIN, 20);
    let narrow = engine.char_width(&spec, 'i');
    let wide = engine.char_width(&spec, 'M');
    assert!(
        wide > narrow,
        "'M' width ({}) should exceed 'i' width ({})",
        wide,
        narrow
    );
}

#[test]
fn char_width_monospaced_uniform() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("Monospaced", PLAIN, 14);
    let w_a = engine.char_width(&spec, 'a');
    let w_m = engine.char_width(&spec, 'M');
    let w_i = engine.char_width(&spec, 'i');
    assert_eq!(w_a, w_m);
    assert_eq!(w_a, w_i);
}

#[test]
fn char_width_space() {
    let mut engine = FontEngine::new();
    let spec = FontSpec::new("SansSerif", PLAIN, 20);
    let w = engine.char_width(&spec, ' ');
    // 0.30 * 20 = 6
    assert_eq!(w, 6);
}

// ── Logical family mapping ───────────────────────────────────────

#[test]
fn logical_family_sansserif() {
    assert_eq!(FontEngine::get_logical_family("Dialog"), "SansSerif");
    assert_eq!(FontEngine::get_logical_family("SansSerif"), "SansSerif");
    assert_eq!(FontEngine::get_logical_family("Arial"), "SansSerif");
    assert_eq!(FontEngine::get_logical_family("Helvetica"), "SansSerif");
}

#[test]
fn logical_family_monospaced() {
    assert_eq!(FontEngine::get_logical_family("Monospaced"), "Monospaced");
    assert_eq!(FontEngine::get_logical_family("DialogInput"), "Monospaced");
    assert_eq!(FontEngine::get_logical_family("Courier"), "Monospaced");
    assert_eq!(FontEngine::get_logical_family("Consolas"), "Monospaced");
}

#[test]
fn logical_family_serif() {
    assert_eq!(FontEngine::get_logical_family("Serif"), "Serif");
    assert_eq!(FontEngine::get_logical_family("Times"), "Serif");
    assert_eq!(FontEngine::get_logical_family("Times New Roman"), "Serif");
    assert_eq!(FontEngine::get_logical_family("Georgia"), "Serif");
}

#[test]
fn logical_family_case_insensitive() {
    assert_eq!(FontEngine::get_logical_family("dialog"), "SansSerif");
    assert_eq!(FontEngine::get_logical_family("DIALOG"), "SansSerif");
    assert_eq!(FontEngine::get_logical_family("MONOSPACED"), "Monospaced");
}

#[test]
fn logical_family_unknown_defaults_to_sansserif() {
    assert_eq!(
        FontEngine::get_logical_family("SomeUnknownFont"),
        "SansSerif"
    );
}

#[test]
fn available_families_contains_five() {
    let families = FontEngine::available_families();
    assert_eq!(families.len(), 5);
    assert!(families.contains(&"Dialog".to_string()));
    assert!(families.contains(&"Monospaced".to_string()));
    assert!(families.contains(&"Serif".to_string()));
    assert!(families.contains(&"SansSerif".to_string()));
    assert!(families.contains(&"DialogInput".to_string()));
}

// ── Global singleton ─────────────────────────────────────────────

#[test]
fn singleton_accessible() {
    let mut engine = font_engine();
    let spec = FontSpec::new("Dialog", PLAIN, 12);
    let m = engine.get_metrics(&spec);
    assert!(m.ascent > 0);
}

// ── Glyph atlas ──────────────────────────────────────────────────

#[test]
fn glyph_key_family_id_canonical() {
    // Same string → same hash, every time.
    let a = GlyphKey::family_id_from("SansSerif");
    let b = GlyphKey::family_id_from("SansSerif");
    assert_eq!(a, b);

    // Different strings → almost-certainly different hashes (32-bit).
    let c = GlyphKey::family_id_from("Monospaced");
    assert_ne!(a, c, "Distinct family names should hash apart");
}

#[test]
fn glyph_key_equality_and_hash() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let k1 = GlyphKey::new("Dialog", 12, false, false, 'A');
    let k2 = GlyphKey::new("Dialog", 12, false, false, 'A');
    let k3 = GlyphKey::new("Dialog", 12, true, false, 'A');
    assert_eq!(k1, k2);
    assert_ne!(k1, k3);

    let hash = |k: &GlyphKey| {
        let mut h = DefaultHasher::new();
        k.hash(&mut h);
        h.finish()
    };
    assert_eq!(hash(&k1), hash(&k2));
}

#[test]
fn glyph_atlas_construction_and_bounds() {
    let atlas = GlyphAtlas::new(64);
    assert!(atlas.is_empty());
    assert_eq!(atlas.len(), 0);
    assert_eq!(atlas.cap(), 64);
}

#[test]
fn glyph_atlas_clear() {
    let atlas = GlyphAtlas::new(8);
    // Directly poke a bitmap in so we don't need a real fontdue font.
    // Keep the map<->clock invariant (one clock entry per map key) so the
    // poke mirrors what `get_or_rasterize` would produce.
    {
        let mut cache = atlas.cache.lock();
        let key = GlyphKey::new("Dialog", 12, false, false, 'A');
        cache.map.insert(
            key,
            (
                Arc::new(GlyphBitmap {
                    alpha: Vec::<u8>::new().into(),
                    width: 0,
                    height: 0,
                    bearing_x: 0,
                    bearing_y: 0,
                    advance: 0.0,
                }),
                // Referenced bit (CLOCK); value is irrelevant to this test.
                false,
            ),
        );
        cache.clock.push_back(key);
    }
    assert_eq!(atlas.len(), 1);
    atlas.clear();
    assert!(atlas.is_empty());
}

#[test]
fn glyph_atlas_default_uses_global_cap() {
    let atlas = GlyphAtlas::default();
    assert_eq!(atlas.cap(), GLYPH_ATLAS_CAP);
}

#[test]
fn global_glyph_atlas_returns_same_instance() {
    let a = global_glyph_atlas() as *const GlyphAtlas;
    let b = global_glyph_atlas() as *const GlyphAtlas;
    assert_eq!(a, b, "global_glyph_atlas() must be a singleton");
}
