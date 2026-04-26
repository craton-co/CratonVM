//! Font specification, metrics, and heuristic text measurement.
//!
//! Backs `java.awt.Font` and `java.awt.FontMetrics`. Provides approximate
//! metrics for logical fonts (Dialog, SansSerif, Serif, Monospaced) using
//! heuristic calculations. Platform backends override these with exact
//! values from the OS font engine.

use std::collections::HashMap;
use std::sync::OnceLock;

use parking_lot::Mutex;

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// Cache: (family, style, size) -> metrics.
    metrics_cache: HashMap<(String, i32, i32), FontMetrics>,
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
    pub fn get_metrics(&mut self, spec: &FontSpec) -> FontMetrics {
        let key = (spec.family.clone(), spec.style, spec.size);
        if let Some(m) = self.metrics_cache.get(&key) {
            return m.clone();
        }

        let m = Self::compute_metrics(spec);
        self.metrics_cache.insert(key, m.clone());
        m
    }

    /// Approximate the total width of a string in pixels.
    pub fn string_width(&mut self, spec: &FontSpec, text: &str) -> i32 {
        if text.is_empty() {
            return 0;
        }
        let cat = Self::categorize(&spec.family);
        let size = spec.size as f64;

        match cat {
            FontCategory::Monospaced => {
                // Every character has the same advance width.
                let char_w = (0.6 * size).round() as i32;
                char_w * text.chars().count() as i32
            }
            _ => {
                // Proportional: per-character width varies, but we use
                // a heuristic average. Narrow chars (i, l, 1) are ~0.3*S,
                // wide chars (M, W) are ~0.8*S. Average ~ 0.55*S for
                // sans-serif, slightly wider for serif.
                let avg = match cat {
                    FontCategory::Serif => 0.58 * size,
                    _ => 0.55 * size,
                };
                // Bold glyphs are ~5% wider.
                let multiplier = if spec.is_bold() { 1.05 } else { 1.0 };
                let total = avg * multiplier * text.chars().count() as f64;
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
}
