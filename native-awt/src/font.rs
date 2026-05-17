//! Font specification, metrics, and heuristic text measurement.
//!
//! Backs `java.awt.Font` and `java.awt.FontMetrics`. Provides approximate
//! metrics for logical fonts (Dialog, SansSerif, Serif, Monospaced) using
//! heuristic calculations. Platform backends override these with exact
//! values from the OS font engine.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use parking_lot::Mutex;
use rustjvm_types::intern_arc;

// TODO: switch `metrics_cache` to `rustc_hash::FxHashMap` once `rustc-hash`
// is added to `native-awt/Cargo.toml`. FxHashMap has smaller per-entry
// overhead than std HashMap (no SipHash random state) which matters for
// the bounded cache below.

/// Hard cap on entries in the per-engine font metrics cache.
///
/// Used as a crude safety bound to prevent unbounded growth from a
/// pathological app that cycles through many distinct (family, style, size)
/// triples. When the cache reaches this size, the next insertion clears it
/// entirely — coarse, but it bounds memory at a constant ceiling and
/// avoids per-lookup eviction bookkeeping. A proper LRU is a follow-up
/// (would require adding the `lru` crate to workspace deps).
const METRICS_CACHE_CAP: usize = 1024;

// ── Style flags (match java.awt.Font) ────────────────────────────────────

/// Plain (no styling).
pub const PLAIN: i32 = 0;
/// Bold weight.
pub const BOLD: i32 = 1;
/// Italic style.
pub const ITALIC: i32 = 2;
/// Bold + italic.
pub const BOLD_ITALIC: i32 = 3;

// ── FontSpec ─────────────────────────────────────────────────────────────

/// Font specification — the triple of (family, style, size) that identifies
/// a font in the Java API.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FontSpec {
    /// Font family name (e.g. "Dialog", "SansSerif", "Monospaced").
    pub family: String,
    /// Combination of `PLAIN`, `BOLD`, `ITALIC` flags.
    pub style: i32,
    /// Point size (in Java integer points).
    pub size: i32,
}

impl FontSpec {
    pub fn new(family: impl Into<String>, style: i32, size: i32) -> Self {
        FontSpec {
            family: family.into(),
            style,
            size,
        }
    }

    /// Whether this font spec has the bold flag set.
    #[inline]
    pub fn is_bold(&self) -> bool {
        self.style & BOLD != 0
    }

    /// Whether this font spec has the italic flag set.
    #[inline]
    pub fn is_italic(&self) -> bool {
        self.style & ITALIC != 0
    }
}

// ── FontMetrics ──────────────────────────────────────────────────────────

/// Computed font metrics for a particular font specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontMetrics {
    /// Distance from baseline to top of tallest glyph.
    pub ascent: i32,
    /// Distance from baseline to bottom of lowest descender.
    pub descent: i32,
    /// Recommended inter-line spacing.
    pub leading: i32,
    /// Total line height: ascent + descent + leading.
    pub height: i32,
    /// Maximum advance width of any glyph.
    pub max_advance: i32,
}

// ── FontEngine ───────────────────────────────────────────────────────────

/// Heuristic font metrics calculator.
///
/// Provides approximate metrics for logical fonts without requiring a
/// native font rasterizer. For real rendering, the platform backend
/// replaces these values with OS-provided measurements.
pub struct FontEngine {
    /// Cache: (interned family, style, size) -> metrics.
    ///
    /// The family name is interned to an `Arc<str>` so the key carries a
    /// cheap (refcount-bump) clone instead of a fresh `String` allocation
    /// on every lookup. The cache is bounded by `METRICS_CACHE_CAP` to
    /// prevent unbounded growth.
    metrics_cache: HashMap<(Arc<str>, i32, i32), FontMetrics>,
}

/// Logical font family categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FontCategory {
    SansSerif,
    Serif,
    Monospaced,
}

impl FontEngine {
    pub fn new() -> Self {
        FontEngine {
            metrics_cache: HashMap::new(),
        }
    }

    /// Get (or compute and cache) metrics for the given font spec.
    ///
    /// The family name is interned to a process-global `Arc<str>` so the
    /// cache key avoids a fresh `String` allocation on every lookup. The
    /// cache is bounded by [`METRICS_CACHE_CAP`]; if the bound is reached
    /// the cache is cleared wholesale before insertion (see the constant's
    /// docs for the trade-off).
    pub fn get_metrics(&mut self, spec: &FontSpec) -> FontMetrics {
        let family: Arc<str> = intern_arc(&spec.family);
        let key = (Arc::clone(&family), spec.style, spec.size);
        if let Some(m) = self.metrics_cache.get(&key).copied() {
            return m;
        }

        let m = Self::compute_metrics(spec);
        if self.metrics_cache.len() >= METRICS_CACHE_CAP {
            // Coarse eviction: drop everything. See METRICS_CACHE_CAP docs.
            self.metrics_cache.clear();
        }
        self.metrics_cache.insert(key, m);
        m
    }

    /// Approximate the total width of a string in pixels.
    pub fn string_width(&mut self, spec: &FontSpec, text: &str) -> i32 {
        if text.is_empty() {
            return 0;
        }
        // Hoist all per-call lookups out of the per-character path. Each
        // of these is cheap in isolation but `string_width` is hot in
        // measure passes during layout, so we compute them exactly once.
        let cat = Self::categorize(&spec.family);
        let size = spec.size as f64;
        let is_bold = spec.is_bold();

        // Heuristic character count. For ASCII text (the common case for
        // Swing label/menu strings) `str::len` equals the char count and
        // skips the full UTF-8 iteration that `chars().count()` requires.
        // Fall back to `chars().count()` for non-ASCII so multi-byte
        // glyphs aren't over-counted by their byte length.
        let char_count = if text.is_ascii() {
            text.len() as i32
        } else {
            text.chars().count() as i32
        };

        match cat {
            FontCategory::Monospaced => {
                // Every character has the same advance width.
                let char_w = (0.6 * size).round() as i32;
                char_w * char_count
            }
            _ => {
                // Proportional: per-character width varies, but we use
                // a heuristic average. Narrow chars (i, l, 1) are ~0.3*S,
                // wide chars (M, W) are ~0.8*S. Average ~ 0.55*S for
                // sans-serif, slightly wider for serif.
                let avg = if matches!(cat, FontCategory::Serif) {
                    0.58 * size
                } else {
                    0.55 * size
                };
                // Bold glyphs are ~5% wider.
                let multiplier = if is_bold { 1.05 } else { 1.0 };
                let total = avg * multiplier * char_count as f64;
                total.round() as i32
            }
        }
    }

    /// Approximate the advance width of a single character.
    pub fn char_width(&mut self, spec: &FontSpec, ch: char) -> i32 {
        let cat = Self::categorize(&spec.family);
        let size = spec.size as f64;

        match cat {
            FontCategory::Monospaced => (0.6 * size).round() as i32,
            _ => {
                // Rough per-character heuristic.
                let ratio = match ch {
                    'i' | 'l' | '!' | '|' | '\'' | ',' | '.' | ':' | ';' | 'j' | 'f'
                    | 't' | 'r' => 0.35,
                    'M' | 'W' | 'm' | 'w' | '@' => 0.80,
                    ' ' => 0.30,
                    _ => 0.55,
                };
                let base = ratio * size;
                let multiplier = if spec.is_bold() { 1.05 } else { 1.0 };
                (base * multiplier).round() as i32
            }
        }
    }

    /// Map a logical font name to its canonical family.
    ///
    /// Java defines these logical names: Dialog, DialogInput, SansSerif,
    /// Serif, Monospaced. They map to platform-specific physical fonts,
    /// but here we just normalize the naming.
    pub fn get_logical_family(name: &str) -> &'static str {
        // Case-insensitive matching, like Java does.
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "dialog" | "sansserif" | "sans-serif" | "default" | "arial" | "helvetica" => {
                "SansSerif"
            }
            "dialoginput" | "monospaced" | "monospace" | "courier" | "courier new"
            | "consolas" | "lucida console" => "Monospaced",
            "serif" | "times" | "times new roman" | "georgia" => "Serif",
            _ => "SansSerif", // fallback
        }
    }

    /// Return the list of available logical font family names.
    pub fn available_families() -> Vec<String> {
        vec![
            "Dialog".to_string(),
            "DialogInput".to_string(),
            "SansSerif".to_string(),
            "Serif".to_string(),
            "Monospaced".to_string(),
        ]
    }

    // ── Internal helpers ─────────────────────────────────────────────

    fn categorize(family: &str) -> FontCategory {
        let canonical = Self::get_logical_family(family);
        match canonical {
            "Monospaced" => FontCategory::Monospaced,
            "Serif" => FontCategory::Serif,
            _ => FontCategory::SansSerif,
        }
    }

    /// Compute heuristic metrics for a font specification.
    ///
    /// For a font of size S:
    /// - ascent  = round(0.80 * S)
    /// - descent = round(0.20 * S)
    /// - leading = round(0.05 * S)
    /// - height  = ascent + descent + leading
    /// - max_advance: monospaced = round(0.6*S), proportional = S (upper bound)
    fn compute_metrics(spec: &FontSpec) -> FontMetrics {
        let s = spec.size as f64;
        let ascent = (0.80 * s).round() as i32;
        let descent = (0.20 * s).round() as i32;
        let leading = (0.05 * s).round() as i32;
        let height = ascent + descent + leading;

        let cat = Self::categorize(&spec.family);
        let max_advance = match cat {
            FontCategory::Monospaced => (0.6 * s).round() as i32,
            _ => spec.size, // upper bound for proportional
        };

        FontMetrics {
            ascent,
            descent,
            leading,
            height,
            max_advance,
        }
    }
}

impl Default for FontEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Global font engine singleton.
pub fn font_engine() -> parking_lot::MutexGuard<'static, FontEngine> {
    static INSTANCE: OnceLock<Mutex<FontEngine>> = OnceLock::new();
    INSTANCE
        .get_or_init(|| Mutex::new(FontEngine::new()))
        .lock()
}

// ══════════════════════════════════════════════════════════════════════════
// Glyph atlas — per-glyph alpha-bitmap cache
// ══════════════════════════════════════════════════════════════════════════
//
// Round-2 N2-2 fix: `rasterize_text` in the X11 and Cocoa backends currently
// loads a fontdue Font and re-rasterizes every glyph on every call. For a UI
// typing in a JTextField each keystroke re-rasterizes every visible character,
// which is unnecessary work since glyph alpha masks are pure functions of
// (family, size, weight, style, codepoint).
//
// `GlyphAtlas` caches `Arc<GlyphBitmap>` keyed by those identifying inputs.
// Lookups are O(1) (FxHashMap) under a single `parking_lot::Mutex`. The
// returned `Arc` shares one allocation across all callers, so the platform
// compositor can hold it without copying.
//
// The atlas is colour-agnostic: it stores the raw alpha mask returned by
// fontdue. The platform composites that mask into ARGB with whatever ink
// colour the caller requested. Keying by colour would multiply the working
// set by ~16M with zero rasterization savings.
//
// TODO(platform-migration): the following sites still call
// `font.rasterize(ch, font_size)` directly per glyph per call and should be
// migrated to `global_glyph_atlas().get_or_rasterize(key, &font)`:
//   - `native-awt/src/platform/x11.rs::rasterize_text` (≈line 595)
//   - `native-awt/src/platform/cocoa.rs::rasterize_text` (≈line 554)
// Both sites use the identical fontdue-based code path, so the migration is
// mechanical: build a `GlyphKey` per char, fetch the bitmap from the atlas,
// then composite `bitmap.alpha` into the ARGB output using
// `bitmap.bearing_x` / `bearing_y` / `advance` for positioning (replacing the
// raw `metrics.xmin` / `metrics.ymin` / `metrics.advance_width` uses).

/// Default capacity (number of distinct glyphs) for the global atlas.
///
/// Sized for typical Swing UIs: a few logical fonts × a handful of point
/// sizes × {plain, bold, italic} × the ASCII printable range plus common
/// punctuation easily fits well under this. Pathological apps cycling many
/// glyph variants will trigger the bulk-eviction path below.
const GLYPH_ATLAS_CAP: usize = 2048;

/// Identifying tuple for a rasterized glyph.
///
/// `family_id` is the FxHash of the family name (after logical-family
/// canonicalization). Using a 32-bit hash instead of the `Arc<str>` keeps
/// the key `Copy` and 16 bytes wide — cheap enough that we don't bother
/// interning. Collisions on family names are negligible in practice (the
/// realistic universe is ~10 family strings) and a collision would only
/// produce a visually-wrong glyph for one application run, not a crash.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct GlyphKey {
    /// Hashed family name (Dialog, SansSerif, Monospaced, …).
    pub family_id: u32,
    /// Pixel size (fontdue takes f32; we round to u32 since sub-pixel sizes
    /// are not exposed by `java.awt.Font` integer points).
    pub size: u32,
    /// Bold weight flag.
    pub bold: bool,
    /// Italic style flag.
    pub italic: bool,
    /// Unicode code point.
    pub ch: u32,
}

impl GlyphKey {
    /// Hash a family name into the `family_id` field.
    ///
    /// Uses FxHash (same hasher as the atlas map) for cheap, deterministic
    /// hashing without a per-process random seed. Callers should pass the
    /// canonical family name (see [`FontEngine::get_logical_family`]) so two
    /// aliases like "Dialog" and "Arial" share the same key.
    pub fn family_id_from(name: &str) -> u32 {
        use std::hash::{Hash, Hasher};
        let mut hasher = rustc_hash::FxHasher::default();
        name.hash(&mut hasher);
        // Truncate the 64-bit FxHash to 32 bits; the residual collision rate
        // is far below the noise floor for our ~10-family realistic universe.
        hasher.finish() as u32
    }

    /// Convenience constructor that takes the family name as a string and
    /// hashes it. Use this from the platform sites that have the raw family
    /// string in hand.
    pub fn new(family: &str, size: u32, bold: bool, italic: bool, ch: char) -> Self {
        GlyphKey {
            family_id: Self::family_id_from(family),
            size,
            bold,
            italic,
            ch: ch as u32,
        }
    }
}

/// A cached rasterized glyph: alpha mask plus placement metrics.
///
/// `alpha` is `Arc<[u8]>` so the cache and all callers share one allocation.
/// `width * height` bytes; row-major, no padding. Each byte is the fontdue
/// coverage value in 0..=255 (treated as straight alpha by the compositor).
///
/// Placement fields mirror the subset of `fontdue::Metrics` that the
/// platform compositors actually consume. We do not store the full
/// `fontdue::Metrics` because (a) it changes shape across fontdue versions
/// and (b) we don't need its extra fields here.
#[derive(Clone)]
pub struct GlyphBitmap {
    /// Alpha mask (0..=255). `width * height` bytes, row-major.
    pub alpha: Arc<[u8]>,
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// Horizontal offset from the pen position to the bitmap's left edge.
    /// Maps to `fontdue::Metrics::xmin`.
    pub bearing_x: i32,
    /// Vertical offset from the baseline to the bitmap's bottom edge
    /// (positive = above baseline). Maps to `fontdue::Metrics::ymin`.
    pub bearing_y: i32,
    /// Horizontal pen advance after this glyph, in pixels.
    pub advance: f32,
}

/// Process-wide cache of rasterized glyphs.
///
/// Eviction strategy: when the map reaches `cap`, drop the first
/// `cap / 2` entries observed during hashmap iteration. FxHashMap's
/// iteration order is deterministic for a given insertion history but not
/// LRU-ordered, so this is effectively pseudo-random eviction. That's the
/// right trade-off here:
///
///   - True LRU would require per-lookup bookkeeping (linked-list pointer
///     updates under the same mutex), making the hot get path measurably
///     slower for the steady-state cache-hit case we're optimizing.
///   - The working set of an interactive UI is small (a few hundred glyphs)
///     and refills cheaply: a missed glyph just re-rasterizes once, then
///     stays hot.
///   - The cap is a soft pressure-release valve, not a precision tool.
///
/// If profiling later shows excessive thrashing on real workloads, swap the
/// internals for a real LRU without changing the public API.
pub struct GlyphAtlas {
    cache: Mutex<rustc_hash::FxHashMap<GlyphKey, Arc<GlyphBitmap>>>,
    cap: usize,
}

impl GlyphAtlas {
    /// Construct an empty atlas with the given soft cap.
    pub fn new(cap: usize) -> Self {
        GlyphAtlas {
            cache: Mutex::new(rustc_hash::FxHashMap::default()),
            cap,
        }
    }

    /// Soft cap on cache entries before eviction kicks in.
    #[inline]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Current number of cached glyphs. Mainly useful for tests / debug.
    pub fn len(&self) -> usize {
        self.cache.lock().len()
    }

    /// Whether the cache currently holds no entries.
    pub fn is_empty(&self) -> bool {
        self.cache.lock().is_empty()
    }

    /// Drop all cached glyphs. Mainly for tests; production code should
    /// rely on bounded growth via [`GlyphAtlas::new`]'s `cap`.
    pub fn clear(&self) {
        self.cache.lock().clear();
    }

    /// Look up a glyph; rasterize via `fontdue_font` and insert if missing.
    ///
    /// The returned `Arc<GlyphBitmap>` is shared with the cache. The hot
    /// path is a single mutex acquire and a hashmap lookup — no allocation.
    ///
    /// **Identity contract.** Two concurrent calls for the same key return
    /// `Arc`s that satisfy `Arc::ptr_eq`. Downstream caches (platform
    /// glyph-atlas textures, GPU upload trackers) key on the bitmap's
    /// pointer identity; if a second thread received a freshly-rasterized
    /// `Arc` that overwrote the first, those caches would silently miss
    /// and re-upload byte-equal bitmaps on every contended lookup.
    ///
    /// Implementation: the first thread to acquire the lock through
    /// `entry(..).or_insert_with(..)` wins and rasterizes; every other
    /// thread blocks on the same mutex and then sees the cached entry.
    /// This briefly holds the mutex across rasterization in the
    /// contended-miss case, but the steady-state hot path stays
    /// single-lookup-and-clone.
    pub fn get_or_rasterize(
        &self,
        key: GlyphKey,
        fontdue_font: &fontdue::Font,
    ) -> Arc<GlyphBitmap> {
        // Hot path: scoped lock, drops before any work.
        if let Some(g) = self.cache.lock().get(&key).cloned() {
            return g;
        }

        // Miss: enter the insert path under a single lock acquisition so
        // racing misses on the same key all observe the SAME Arc — the
        // first inserter wins, the rest get the cached entry. This
        // preserves `Arc::ptr_eq` for downstream identity caches.
        let mut cache = self.cache.lock();

        // Bulk-evict BEFORE the `entry` lookup. If we are about to insert
        // and we're already at cap, free space first.
        if cache.len() >= self.cap && !cache.contains_key(&key) {
            // Pseudo-random bulk eviction: drop the first half of whatever
            // the iterator yields. Cheaper than tracking LRU and adequate
            // for the soft-cap role this serves. See `GlyphAtlas` docs.
            let drop_count = cache.len() / 2;
            let keys_to_drop: Vec<GlyphKey> =
                cache.keys().take(drop_count).copied().collect();
            for k in keys_to_drop {
                cache.remove(&k);
            }
        }

        Arc::clone(cache.entry(key).or_insert_with(|| {
            // `from_u32` is the safe path — we never want to panic on a
            // surrogate or out-of-range code point sneaked in by upstream
            // string handling.
            let ch = std::char::from_u32(key.ch).unwrap_or(' ');
            let (metrics, alpha_vec) = fontdue_font.rasterize(ch, key.size as f32);
            Arc::new(GlyphBitmap {
                alpha: alpha_vec.into(),
                width: metrics.width as u32,
                height: metrics.height as u32,
                bearing_x: metrics.xmin,
                bearing_y: metrics.ymin,
                advance: metrics.advance_width,
            })
        }))
    }
}

impl Default for GlyphAtlas {
    fn default() -> Self {
        Self::new(GLYPH_ATLAS_CAP)
    }
}

/// Process-global glyph atlas, lazily initialised on first use.
///
/// Shared across all platform backends and all threads. The atlas itself is
/// internally synchronised (`Mutex`), so callers do not need any further
/// locking around `get_or_rasterize`.
pub fn global_glyph_atlas() -> &'static GlyphAtlas {
    static ATLAS: OnceLock<GlyphAtlas> = OnceLock::new();
    ATLAS.get_or_init(|| GlyphAtlas::new(GLYPH_ATLAS_CAP))
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
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
        assert_eq!(m.max_advance, 12);
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
        assert_eq!(w, 55);
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
        {
            let mut cache = atlas.cache.lock();
            cache.insert(
                GlyphKey::new("Dialog", 12, false, false, 'A'),
                Arc::new(GlyphBitmap {
                    alpha: Vec::<u8>::new().into(),
                    width: 0,
                    height: 0,
                    bearing_x: 0,
                    bearing_y: 0,
                    advance: 0.0,
                }),
            );
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
}
