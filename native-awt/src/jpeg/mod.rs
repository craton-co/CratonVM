// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A JPEG codec reproducing IJG libjpeg 6b (the library the JDK's
//! `libjavajpeg` embeds), so `com.sun.imageio.plugins.jpeg.JPEGImageWriter` /
//! `JPEGImageReader` can run as the JDK's own bytecode on CratonVM and produce
//! the bytes and pixels HotSpot does.
//!
//! * [`enc`]: baseline sequential compression (`jccolor`, `jcsample`,
//!   `jcdctmgr` islow, `jchuff`, `jcmarker`).
//! * [`dec`]: baseline and progressive decompression (`jdhuff`, `jdphuff`,
//!   `jidctint`, `jdsample` fancy upsampling, `jdcolor`).
//! * [`glue`]: the `JPEGImageWriter` / `JPEGImageReader` natives.

pub mod dec;
pub mod enc;
pub mod glue;
pub mod std_tables;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

/// `jpeg_natural_order`: the row-major index of the k-th zigzag coefficient.
pub const NATURAL_ORDER: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// A Huffman table in JPEG wire form: `bits[1..=16]` code counts per length
/// and the symbol values in code order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HuffSpec {
    pub bits: [u8; 17],
    pub vals: Vec<u8>,
}

impl HuffSpec {
    /// From the 16 length counts (`JPEGHuffmanTable.getLengths`) and values.
    pub fn from_lengths(lengths: &[u8], vals: &[u8]) -> HuffSpec {
        let mut bits = [0u8; 17];
        for (i, &l) in lengths.iter().take(16).enumerate() {
            bits[i + 1] = l;
        }
        HuffSpec {
            bits,
            vals: vals.to_vec(),
        }
    }
}

/// Scale a standard quantization table the way
/// `JPEGQTable.getScaledInstance(scale, forceBaseline)` does.
pub fn scale_qtable(base: &[u16; 64], scale: f32, force_baseline: bool) -> [u16; 64] {
    let max = if force_baseline { 255.0 } else { 32767.0 };
    let mut out = [0u16; 64];
    for i in 0..64 {
        let mut v = (base[i] as f32 * scale + 0.5).floor();
        if v < 1.0 {
            v = 1.0;
        }
        if v > max {
            v = max;
        }
        out[i] = v as u16;
    }
    out
}

/// `JPEG.convertToLinearQuality`.
pub fn linear_quality(quality: f32) -> f32 {
    let q = if quality <= 0.0 {
        0.01f32
    } else if quality > 1.0 {
        1.0f32
    } else {
        quality
    };
    if q < 0.5 {
        0.5 / q
    } else {
        2.0 - q * 2.0
    }
}
