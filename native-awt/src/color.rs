// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! AWT Color model — ARGB color representation and color-space conversions.
//!
//! This module backs `java.awt.Color` and provides the color primitives used
//! throughout the AWT/Swing rendering pipeline.

/// ARGB color (bits 24-31=alpha, 16-23=red, 8-15=green, 0-7=blue).
///
/// Matches the layout of `java.awt.Color.getRGB()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Color {
    pub argb: u32,
}

// ── Named color constants (match java.awt.Color) ──────────────────────────

impl Color {
    pub const BLACK: Color = Color { argb: 0xFF00_0000 };
    pub const WHITE: Color = Color { argb: 0xFFFF_FFFF };
    pub const RED: Color = Color { argb: 0xFFFF_0000 };
    pub const GREEN: Color = Color { argb: 0xFF00_FF00 };
    pub const BLUE: Color = Color { argb: 0xFF00_00FF };
    pub const YELLOW: Color = Color { argb: 0xFFFF_FF00 };
    pub const CYAN: Color = Color { argb: 0xFF00_FFFF };
    pub const MAGENTA: Color = Color { argb: 0xFFFF_00FF };
    pub const ORANGE: Color = Color { argb: 0xFFFF_C800 };
    pub const PINK: Color = Color { argb: 0xFFFF_AFAF };
    pub const GRAY: Color = Color { argb: 0xFF80_8080 };
    pub const DARK_GRAY: Color = Color { argb: 0xFF40_4040 };
    pub const LIGHT_GRAY: Color = Color { argb: 0xFFC0_C0C0 };
    pub const TRANSPARENT: Color = Color { argb: 0x0000_0000 };
}

// ── Construction ──────────────────────────────────────────────────────────

impl Color {
    /// Create an opaque color from RGB components.
    #[inline]
    pub fn new(r: u8, g: u8, b: u8) -> Self {
        Self::with_alpha(r, g, b, 255)
    }

    /// Create a color from RGBA components.
    #[inline]
    pub fn with_alpha(r: u8, g: u8, b: u8, a: u8) -> Self {
        Color {
            argb: (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32),
        }
    }

    /// Create from a packed ARGB u32.
    #[inline]
    pub fn from_argb(argb: u32) -> Self {
        Color { argb }
    }

    /// Create from a packed RGB u32 (alpha set to 255).
    #[inline]
    pub fn from_rgb(rgb: u32) -> Self {
        Color {
            argb: 0xFF00_0000 | (rgb & 0x00FF_FFFF),
        }
    }
}

// ── Component accessors ──────────────────────────────────────────────────

impl Color {
    #[inline]
    pub fn alpha(&self) -> u8 {
        (self.argb >> 24) as u8
    }

    #[inline]
    pub fn red(&self) -> u8 {
        (self.argb >> 16) as u8
    }

    #[inline]
    pub fn green(&self) -> u8 {
        (self.argb >> 8) as u8
    }

    #[inline]
    pub fn blue(&self) -> u8 {
        self.argb as u8
    }

    /// Returns the packed ARGB value.
    #[inline]
    pub fn to_argb(&self) -> u32 {
        self.argb
    }

    /// Returns the packed RGB value (alpha stripped).
    #[inline]
    pub fn to_rgb(&self) -> u32 {
        self.argb & 0x00FF_FFFF
    }
}

// ── Color manipulation ───────────────────────────────────────────────────

impl Color {
    /// Java's `Color.brighter()` algorithm.
    ///
    /// Each RGB component is divided by 0.7 (i.e. multiplied by 1/0.7 ≈ 1.4286),
    /// capped at 255. Special case: if a component is 0, it is bumped to the
    /// minimum perceptible value (3) so that pure black can still get brighter.
    pub fn brighter(&self) -> Self {
        let factor: f64 = 1.0 / 0.7;

        let mut r = self.red() as i32;
        let mut g = self.green() as i32;
        let mut b = self.blue() as i32;

        // Java's special case: if the color is very dark (all components < 3),
        // give each zero-component a minimum bump so black can brighten.
        // The exact Java threshold: if a component is 0 but the color should
        // get brighter, set it to a floor of (1/factor) rounded up ≈ 2..3.
        // Java source: i = (int)(1.0/(1.0-factor));  where factor=0.7
        // That gives i = (int)(1.0/0.3) = (int)3.333 = 3
        let java_floor = 3_i32;

        if r == 0 && g == 0 && b == 0 {
            return Color::with_alpha(
                java_floor as u8,
                java_floor as u8,
                java_floor as u8,
                self.alpha(),
            );
        }

        // Java: if component > 0 but < i, clamp to i before scaling
        if r > 0 && r < java_floor {
            r = java_floor;
        }
        if g > 0 && g < java_floor {
            g = java_floor;
        }
        if b > 0 && b < java_floor {
            b = java_floor;
        }

        let r2 = ((r as f64 * factor) as i32).min(255) as u8;
        let g2 = ((g as f64 * factor) as i32).min(255) as u8;
        let b2 = ((b as f64 * factor) as i32).min(255) as u8;

        Color::with_alpha(r2, g2, b2, self.alpha())
    }

    /// Java's `Color.darker()` algorithm.
    ///
    /// Each RGB component is multiplied by 0.7, truncated to integer.
    pub fn darker(&self) -> Self {
        let factor: f64 = 0.7;
        let r = (self.red() as f64 * factor) as u8;
        let g = (self.green() as f64 * factor) as u8;
        let b = (self.blue() as f64 * factor) as u8;
        Color::with_alpha(r, g, b, self.alpha())
    }

    /// Porter-Duff SRC_OVER compositing: `fg` over `bg`.
    ///
    /// result = fg * fg_a + bg * bg_a * (1 - fg_a)
    pub fn blend(fg: Color, bg: Color) -> Color {
        let fa = fg.alpha() as f64 / 255.0;
        let ba = bg.alpha() as f64 / 255.0;

        let out_a = fa + ba * (1.0 - fa);
        if out_a == 0.0 {
            return Color::TRANSPARENT;
        }

        let blend_ch = |fc: u8, bc: u8| -> u8 {
            let result = (fc as f64 * fa + bc as f64 * ba * (1.0 - fa)) / out_a;
            result.round().min(255.0) as u8
        };

        let r = blend_ch(fg.red(), bg.red());
        let g = blend_ch(fg.green(), bg.green());
        let b = blend_ch(fg.blue(), bg.blue());
        let a = (out_a * 255.0).round().min(255.0) as u8;

        Color::with_alpha(r, g, b, a)
    }
}

// ── Display ──────────────────────────────────────────────────────────────

impl std::fmt::Display for Color {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Color[r={},g={},b={},a={}]",
            self.red(),
            self.green(),
            self.blue(),
            self.alpha()
        )
    }
}

// ── ColorSpace ───────────────────────────────────────────────────────────

/// Color space identifiers used in Java2D.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpace {
    /// Standard sRGB (gamma-encoded).
    Srgb,
    /// Linear RGB (linear light).
    LinearRgb,
}

impl ColorSpace {
    /// Convert an sRGB component (0.0..1.0) to linear RGB.
    ///
    /// Uses the exact sRGB transfer function (IEC 61966-2-1).
    #[inline]
    pub fn srgb_to_linear(c: f32) -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Convert a linear RGB component (0.0..1.0) to sRGB.
    #[inline]
    pub fn linear_to_srgb(c: f32) -> f32 {
        if c <= 0.0031308 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests;
