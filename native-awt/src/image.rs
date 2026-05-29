// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! BufferedImage backing store and global image registry.
//!
//! This module provides the pixel-buffer implementation behind
//! `java.awt.image.BufferedImage`. All pixel data is stored internally
//! as ARGB u32 arrays regardless of the declared `ImageType`.

use std::sync::OnceLock;

use parking_lot::Mutex;
use rustc_hash::FxHashMap;

// ── Graphics2D reference ─────────────────────────────────────────────────

/// Opaque handle to a Graphics2D rendering context.
/// The actual context lives in the graphics2d module; this is just an ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Graphics2DRef(pub u64);

/// Opaque handle to a registered image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(pub u64);

// ── ImageType ────────────────────────────────────────────────────────────

/// Image type constants matching `java.awt.image.BufferedImage.TYPE_*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ImageType {
    Custom = 0,
    IntRgb = 1,
    IntArgb = 2,
    IntArgbPre = 3,
    IntBgr = 4,
    Byte3Bgr = 5,
    Byte4Abgr = 6,
    ByteGray = 10,
    ByteBinary = 12,
    ByteIndexed = 13,
}

impl ImageType {
    /// Whether this image type has an alpha channel.
    pub fn has_alpha(self) -> bool {
        matches!(
            self,
            ImageType::IntArgb | ImageType::IntArgbPre | ImageType::Byte4Abgr | ImageType::Custom
        )
    }
}

// ── BufferedImageData ────────────────────────────────────────────────────

/// Pixel buffer backing a `BufferedImage`.
///
/// All pixels are stored as ARGB regardless of the declared image type.
/// Conversion to/from the declared format happens at the Java boundary.
pub struct BufferedImageData {
    image_type: ImageType,
    width: u32,
    height: u32,
    /// Row-major ARGB pixel data. Length = width * height.
    pixels: Vec<u32>,
    /// Monotonically increasing counter for Graphics2D context IDs.
    next_graphics_id: u64,
}

impl BufferedImageData {
    /// Create a new image filled with transparent black (or opaque black for
    /// non-alpha types).
    ///
    /// # Panics
    /// Panics if `width * height` overflows `usize`. Java-controlled callers
    /// must use [`BufferedImageData::try_new`] instead, which surfaces the
    /// overflow as a recoverable error.
    pub fn new(width: u32, height: u32, image_type: ImageType) -> Self {
        Self::try_new(width, height, image_type)
            .expect("pixel-buffer size overflow in BufferedImageData::new")
    }

    /// Fallible constructor: returns `None` if `width * height` overflows
    /// (the pixel count cannot be represented as a `u32`).
    ///
    /// `BufferedImage.<init>` floors dimensions at 1 but never caps them, so
    /// Java-controlled sizes can reach ~2^31 per axis. The pixel count is the
    /// natural `u32` linear index (a Java array is `int`-indexed); requiring
    /// it to fit in `u32` rejects any image larger than ~65535x65535 before
    /// the backing `Vec` is allocated, instead of overflowing the multiply.
    pub fn try_new(width: u32, height: u32, image_type: ImageType) -> Option<Self> {
        let len = width.checked_mul(height)? as usize;
        let fill = if image_type.has_alpha() {
            0x0000_0000 // transparent
        } else {
            0xFF00_0000 // opaque black
        };
        Some(BufferedImageData {
            image_type,
            width,
            height,
            pixels: vec![fill; len],
            next_graphics_id: 1,
        })
    }

    // ── Accessors ────────────────────────────────────────────────────

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn image_type(&self) -> ImageType {
        self.image_type
    }

    // ── Pixel access ─────────────────────────────────────────────────

    #[inline]
    fn index(&self, x: u32, y: u32) -> usize {
        // `debug_assert!` only — in release, the subsequent `pixels[idx]`
        // indexing already panics on OOB. Keeping a release-mode `assert!`
        // here forces two bounds checks per pixel and blocks LLVM from
        // eliding the implicit `Vec` bounds check.
        debug_assert!(
            x < self.width && y < self.height,
            "pixel ({}, {}) out of bounds ({}x{})",
            x,
            y,
            self.width,
            self.height
        );
        (y as usize) * (self.width as usize) + (x as usize)
    }

    /// Get the ARGB value of a single pixel.
    #[inline]
    pub fn get_rgb(&self, x: u32, y: u32) -> u32 {
        self.pixels[self.index(x, y)]
    }

    /// Set the ARGB value of a single pixel (with bounds checking).
    #[inline]
    pub fn set_rgb(&mut self, x: u32, y: u32, argb: u32) {
        let idx = self.index(x, y);
        self.pixels[idx] = argb;
    }

    /// Bulk-read a rectangular region as a Vec of ARGB values (row-major).
    pub fn get_rgb_region(&self, x: u32, y: u32, w: u32, h: u32) -> Vec<u32> {
        // `checked_add` — a plain `x + w` on `u32` can wrap (e.g. x=u32::MAX,
        // w=2) and spuriously pass the `<= width` comparison.
        let in_bounds = x
            .checked_add(w)
            .zip(y.checked_add(h))
            .is_some_and(|(x1, y1)| x1 <= self.width && y1 <= self.height);
        assert!(
            in_bounds,
            "region ({},{} {}x{}) exceeds image ({}x{})",
            x,
            y,
            w,
            h,
            self.width,
            self.height
        );
        let mut result = Vec::with_capacity((w as usize) * (h as usize));
        for row in y..y + h {
            let start = (row as usize) * (self.width as usize) + (x as usize);
            result.extend_from_slice(&self.pixels[start..start + w as usize]);
        }
        result
    }

    /// Bulk-write a rectangular region from a slice of ARGB values (row-major).
    pub fn set_rgb_region(&mut self, x: u32, y: u32, w: u32, h: u32, pixels: &[u32]) {
        // `checked_add` — see `get_rgb_region`: a plain `u32` add can wrap and
        // spuriously pass the bounds check for a crafted (x, w) pair.
        let in_bounds = x
            .checked_add(w)
            .zip(y.checked_add(h))
            .is_some_and(|(x1, y1)| x1 <= self.width && y1 <= self.height);
        assert!(
            in_bounds,
            "region ({},{} {}x{}) exceeds image ({}x{})",
            x,
            y,
            w,
            h,
            self.width,
            self.height
        );
        assert_eq!(
            pixels.len(),
            (w as usize) * (h as usize),
            "pixel slice length ({}) does not match region ({}x{}={})",
            pixels.len(),
            w,
            h,
            (w as usize) * (h as usize)
        );
        for row in 0..h {
            let dst_start =
                ((y + row) as usize) * (self.width as usize) + (x as usize);
            let src_start = (row as usize) * (w as usize);
            self.pixels[dst_start..dst_start + w as usize]
                .copy_from_slice(&pixels[src_start..src_start + w as usize]);
        }
    }

    /// Raw read-only access to the pixel buffer.
    #[inline]
    pub fn get_data_buffer(&self) -> &[u32] {
        &self.pixels
    }

    /// Raw mutable access to the pixel buffer.
    #[inline]
    pub fn get_data_buffer_mut(&mut self) -> &mut [u32] {
        &mut self.pixels
    }

    /// Allocate a Graphics2D rendering context ID for this image.
    ///
    /// The actual context creation is handled by the graphics2d module;
    /// this simply mints a unique reference.
    pub fn create_graphics(&mut self) -> Graphics2DRef {
        let id = self.next_graphics_id;
        self.next_graphics_id += 1;
        Graphics2DRef(id)
    }

    /// Copy a rectangular sub-region into a new `BufferedImageData`.
    pub fn get_subimage(&self, x: u32, y: u32, w: u32, h: u32) -> BufferedImageData {
        let region = self.get_rgb_region(x, y, w, h);
        BufferedImageData {
            image_type: self.image_type,
            width: w,
            height: h,
            pixels: region,
            next_graphics_id: 1,
        }
    }

    /// Copy all pixel data from `self` into `dst`.
    ///
    /// `dst` must have the same dimensions.
    pub fn copy_to(&self, dst: &mut BufferedImageData) {
        assert_eq!(
            (self.width, self.height),
            (dst.width, dst.height),
            "copy_to: dimension mismatch ({}x{} vs {}x{})",
            self.width,
            self.height,
            dst.width,
            dst.height
        );
        dst.pixels.copy_from_slice(&self.pixels);
    }

    /// No-op — exists for `BufferedImage.flush()` API compatibility.
    #[inline]
    pub fn flush(&self) {
        // Nothing to release in software-only mode.
    }
}

impl Clone for BufferedImageData {
    fn clone(&self) -> Self {
        BufferedImageData {
            image_type: self.image_type,
            width: self.width,
            height: self.height,
            pixels: self.pixels.clone(),
            next_graphics_id: 1,
        }
    }
}

// ── ImageRegistry (global singleton) ─────────────────────────────────────

/// Global registry mapping `ImageId` -> `BufferedImageData`.
///
/// Used by native methods to look up image objects by handle.
///
/// Storage uses [`FxHashMap`]: IDs are monotonically-increasing sequential
/// `u64`s, so SipHash provides no security benefit here and FxHash is
/// roughly 2-3x faster for integer keys on this lookup hot path.
///
/// Lifetime / reclamation note:
///
/// Derived scratch buffers (the per-`Graphics2D` render surfaces backing an
/// image) ARE reclaimed: `natives.rs::dispose_gfx` drops a Graphics2D
/// context's full-size buffer the moment `Graphics2D.dispose()` runs, and
/// `natives.rs::flush_image` (wired to `BufferedImage.flush()`) reaps any
/// disposed scratch context still associated with an image id.
///
/// The authoritative pixel raster held here is intentionally NOT freed by
/// `flush()`: a CratonVM `BufferedImage` is memory-backed and therefore not
/// reconstructable, so the JDK contract requires its raster to survive
/// `flush()` (callers may legally read or re-`createGraphics()` afterwards).
/// `destroy()` below performs the actual reclamation and is only safe to call
/// at the image's true end-of-life; no native end-of-life hook for
/// `BufferedImage` exists yet (a finalizer/Cleaner or a bounded LRU keyed on
/// idle time would be the next step), so it is currently exercised only by
/// tests. Do NOT wire `destroy()` to anything that can race a live raster.
pub struct ImageRegistry {
    images: FxHashMap<u64, BufferedImageData>,
    next_id: u64,
}

impl ImageRegistry {
    fn new() -> Self {
        ImageRegistry {
            images: FxHashMap::default(),
            next_id: 1,
        }
    }

    /// Create a new image and return its registry ID.
    ///
    /// Returns `None` if `width * height` overflows `usize` — the caller
    /// (a native method) should surface this as a Java `OutOfMemoryError`
    /// rather than panicking.
    pub fn create(
        &mut self,
        width: u32,
        height: u32,
        image_type: ImageType,
    ) -> Option<ImageId> {
        let data = BufferedImageData::try_new(width, height, image_type)?;
        let id = self.next_id;
        self.next_id += 1;
        self.images.insert(id, data);
        Some(ImageId(id))
    }

    /// Look up an image by ID (immutable).
    pub fn get(&self, id: ImageId) -> Option<&BufferedImageData> {
        self.images.get(&id.0)
    }

    /// Look up an image by ID (mutable).
    pub fn get_mut(&mut self, id: ImageId) -> Option<&mut BufferedImageData> {
        self.images.get_mut(&id.0)
    }

    /// Remove an image from the registry, freeing its pixel data.
    pub fn destroy(&mut self, id: ImageId) -> bool {
        self.images.remove(&id.0).is_some()
    }

    /// Number of images currently registered.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

// ── Global singleton ─────────────────────────────────────────────────────

/// Access the global image registry (locked).
pub fn image_registry() -> parking_lot::MutexGuard<'static, ImageRegistry> {
    static IMAGE_REGISTRY: OnceLock<Mutex<ImageRegistry>> = OnceLock::new();
    IMAGE_REGISTRY
        .get_or_init(|| Mutex::new(ImageRegistry::new()))
        .lock()
}

// ══════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_image_dimensions() {
        let img = BufferedImageData::new(320, 240, ImageType::IntArgb);
        assert_eq!(img.width(), 320);
        assert_eq!(img.height(), 240);
        assert_eq!(img.image_type(), ImageType::IntArgb);
        assert_eq!(img.get_data_buffer().len(), 320 * 240);
    }

    #[test]
    fn try_new_rejects_pixel_count_overflow() {
        // 70000 x 70000 ~= 4.9e9 pixels: overflows u32. Must return None,
        // not panic in the multiply.
        assert!(BufferedImageData::try_new(70_000, 70_000, ImageType::IntArgb).is_none());
        assert!(BufferedImageData::try_new(0xFFFF_FFFF, 0xFFFF_FFFF, ImageType::IntArgb).is_none());
        // A normal size still succeeds.
        assert!(BufferedImageData::try_new(16, 16, ImageType::IntArgb).is_some());
    }

    #[test]
    fn registry_create_rejects_overflow() {
        let mut reg = ImageRegistry::new();
        assert!(reg.create(70_000, 70_000, ImageType::IntArgb).is_none());
        assert!(reg.create(8, 8, ImageType::IntArgb).is_some());
    }

    #[test]
    fn new_argb_is_transparent() {
        let img = BufferedImageData::new(2, 2, ImageType::IntArgb);
        assert_eq!(img.get_rgb(0, 0), 0x0000_0000);
    }

    #[test]
    fn new_rgb_is_opaque_black() {
        let img = BufferedImageData::new(2, 2, ImageType::IntRgb);
        assert_eq!(img.get_rgb(0, 0), 0xFF00_0000);
    }

    #[test]
    fn set_get_pixel() {
        let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        img.set_rgb(5, 3, 0xFFFF0000);
        assert_eq!(img.get_rgb(5, 3), 0xFFFF0000);
        // Other pixels unchanged
        assert_eq!(img.get_rgb(0, 0), 0x0000_0000);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn get_rgb_oob() {
        let img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        img.get_rgb(10, 0);
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn set_rgb_oob() {
        let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        img.set_rgb(0, 10, 0);
    }

    #[test]
    fn region_read_write() {
        let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        let region = vec![0xFFAABBCC; 6]; // 3x2
        img.set_rgb_region(2, 3, 3, 2, &region);

        let read = img.get_rgb_region(2, 3, 3, 2);
        assert_eq!(read, region);

        // Spot check boundary pixels are untouched
        assert_eq!(img.get_rgb(1, 3), 0);
        assert_eq!(img.get_rgb(5, 3), 0);
    }

    #[test]
    #[should_panic(expected = "exceeds image")]
    fn region_oob() {
        let img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        img.get_rgb_region(8, 8, 5, 5);
    }

    #[test]
    fn subimage() {
        let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
        img.set_rgb(3, 4, 0xDEADBEEF);
        let sub = img.get_subimage(2, 3, 5, 5);
        assert_eq!(sub.width(), 5);
        assert_eq!(sub.height(), 5);
        // (3,4) in original -> (1,1) in subimage
        assert_eq!(sub.get_rgb(1, 1), 0xDEADBEEF);
    }

    #[test]
    fn copy_to() {
        let mut src = BufferedImageData::new(4, 4, ImageType::IntArgb);
        src.set_rgb(2, 2, 0xCAFEBABE);
        let mut dst = BufferedImageData::new(4, 4, ImageType::IntArgb);
        src.copy_to(&mut dst);
        assert_eq!(dst.get_rgb(2, 2), 0xCAFEBABE);
    }

    #[test]
    #[should_panic(expected = "dimension mismatch")]
    fn copy_to_wrong_size() {
        let src = BufferedImageData::new(4, 4, ImageType::IntArgb);
        let mut dst = BufferedImageData::new(8, 8, ImageType::IntArgb);
        src.copy_to(&mut dst);
    }

    #[test]
    fn create_graphics_unique_ids() {
        let mut img = BufferedImageData::new(1, 1, ImageType::IntArgb);
        let g1 = img.create_graphics();
        let g2 = img.create_graphics();
        assert_ne!(g1, g2);
    }

    #[test]
    fn data_buffer_mut() {
        let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
        let buf = img.get_data_buffer_mut();
        buf[0] = 0xFF112233;
        assert_eq!(img.get_rgb(0, 0), 0xFF112233);
    }

    #[test]
    fn flush_is_noop() {
        let img = BufferedImageData::new(1, 1, ImageType::IntArgb);
        img.flush(); // just ensure it doesn't panic
    }

    #[test]
    fn clone_is_independent() {
        let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
        img.set_rgb(0, 0, 0xAABBCCDD);
        let mut img2 = img.clone();
        img2.set_rgb(0, 0, 0x11223344);
        assert_eq!(img.get_rgb(0, 0), 0xAABBCCDD);
        assert_eq!(img2.get_rgb(0, 0), 0x11223344);
    }

    // ── Registry tests ───────────────────────────────────────────────

    #[test]
    fn registry_create_get_destroy() {
        let mut reg = ImageRegistry::new();
        let id = reg.create(64, 64, ImageType::IntArgb).unwrap();
        assert!(reg.get(id).is_some());
        assert_eq!(reg.get(id).unwrap().width(), 64);

        reg.get_mut(id).unwrap().set_rgb(0, 0, 0xDEAD);
        assert_eq!(reg.get(id).unwrap().get_rgb(0, 0), 0xDEAD);

        assert!(reg.destroy(id));
        assert!(reg.get(id).is_none());
    }

    #[test]
    fn registry_unique_ids() {
        let mut reg = ImageRegistry::new();
        let a = reg.create(1, 1, ImageType::IntArgb);
        let b = reg.create(1, 1, ImageType::IntArgb);
        assert_ne!(a, b);
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn registry_destroy_nonexistent() {
        let mut reg = ImageRegistry::new();
        assert!(!reg.destroy(ImageId(999)));
    }

    #[test]
    fn image_type_has_alpha() {
        assert!(ImageType::IntArgb.has_alpha());
        assert!(ImageType::IntArgbPre.has_alpha());
        assert!(ImageType::Byte4Abgr.has_alpha());
        assert!(!ImageType::IntRgb.has_alpha());
        assert!(!ImageType::IntBgr.has_alpha());
        assert!(!ImageType::ByteGray.has_alpha());
    }
}
