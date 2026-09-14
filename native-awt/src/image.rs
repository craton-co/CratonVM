// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! BufferedImage backing store and global image registry.
//!
//! This module provides the pixel-buffer implementation behind
//! `java.awt.image.BufferedImage`. All pixel data is stored internally
//! as ARGB u32 arrays regardless of the declared `ImageType`.

use std::cell::Cell;
use std::io::Cursor;
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

/// Encoded image formats handled by the native `javax.imageio.ImageIO` bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedImageFormat {
    Png,
    Jpeg,
}

impl EncodedImageFormat {
    pub fn parse(name: &str) -> Option<Self> {
        match name
            .trim()
            .trim_start_matches("image/")
            .to_ascii_lowercase()
            .as_str()
        {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            _ => None,
        }
    }

    fn image_format(self) -> image_crate::ImageFormat {
        match self {
            Self::Png => image_crate::ImageFormat::Png,
            Self::Jpeg => image_crate::ImageFormat::Jpeg,
        }
    }
}

/// Hard cap for a single Java-controlled ARGB raster. This matches the
/// registry's 256 MiB soft budget at four bytes per pixel and prevents one
/// live pinned `BufferedImage` from allocating multiple GiB before eviction can
/// help.
pub const MAX_IMAGE_PIXELS: usize = 64 * 1024 * 1024;

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
            .expect("pixel-buffer allocation rejected in BufferedImageData::new")
    }

    /// Fallible constructor: returns `None` if `width * height` overflows,
    /// exceeds [`MAX_IMAGE_PIXELS`], or cannot be reserved.
    ///
    /// `BufferedImage.<init>` floors dimensions at 1 but never caps them, so
    /// Java-controlled sizes can reach ~2^31 per axis. The pixel count is the
    /// natural `u32` linear index (a Java array is `int`-indexed); requiring
    /// it to fit in `u32` rejects any image larger than ~65535x65535 before
    /// the backing `Vec` is allocated, instead of overflowing the multiply.
    pub fn try_new(width: u32, height: u32, image_type: ImageType) -> Option<Self> {
        let len = width.checked_mul(height)? as usize;
        if len > MAX_IMAGE_PIXELS {
            return None;
        }
        let fill = if image_type.has_alpha() {
            0x0000_0000 // transparent
        } else {
            0xFF00_0000 // opaque black
        };
        let mut pixels = Vec::new();
        pixels.try_reserve_exact(len).ok()?;
        pixels.resize(len, fill);
        Some(BufferedImageData {
            image_type,
            width,
            height,
            pixels,
            next_graphics_id: 1,
        })
    }

    /// Build an image from already-materialised ARGB pixels.
    pub fn from_argb_pixels(
        width: u32,
        height: u32,
        image_type: ImageType,
        pixels: Vec<u32>,
    ) -> Option<Self> {
        let len = width.checked_mul(height)? as usize;
        if len > MAX_IMAGE_PIXELS || pixels.len() != len {
            return None;
        }
        Some(BufferedImageData {
            image_type,
            width,
            height,
            pixels,
            next_graphics_id: 1,
        })
    }

    /// Decode a PNG/JPEG byte stream into CratonVM's canonical ARGB raster.
    pub fn decode_encoded(bytes: &[u8]) -> Result<Self, String> {
        let decoded =
            image_crate::load_from_memory(bytes).map_err(|err| format!("decode failed: {err}"))?;
        let rgba = decoded.to_rgba8();
        let (width, height) = rgba.dimensions();
        let len = width
            .checked_mul(height)
            .ok_or_else(|| format!("decoded image dimensions overflow: {width}x{height}"))?
            as usize;
        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(len)
            .map_err(|_| format!("decoded image too large: {width}x{height}"))?;
        for px in rgba.pixels() {
            let [r, g, b, a] = px.0;
            pixels.push(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32));
        }
        Self::from_argb_pixels(width, height, ImageType::IntArgb, pixels)
            .ok_or_else(|| format!("decoded image too large: {width}x{height}"))
    }

    /// Encode this raster as PNG/JPEG bytes.
    pub fn encode(&self, format: EncodedImageFormat) -> Result<Vec<u8>, String> {
        let mut out = Cursor::new(Vec::new());
        match format {
            EncodedImageFormat::Png => {
                let mut rgba = Vec::with_capacity(self.pixels.len() * 4);
                for &argb in &self.pixels {
                    rgba.push(((argb >> 16) & 0xFF) as u8);
                    rgba.push(((argb >> 8) & 0xFF) as u8);
                    rgba.push((argb & 0xFF) as u8);
                    rgba.push(((argb >> 24) & 0xFF) as u8);
                }
                let image = image_crate::RgbaImage::from_raw(self.width, self.height, rgba)
                    .ok_or_else(|| {
                        format!(
                            "invalid RGBA image dimensions: {}x{}",
                            self.width, self.height
                        )
                    })?;
                image_crate::DynamicImage::ImageRgba8(image)
                    .write_to(&mut out, format.image_format())
                    .map_err(|err| format!("encode failed: {err}"))?;
            }
            EncodedImageFormat::Jpeg => {
                let mut rgb = Vec::with_capacity(self.pixels.len() * 3);
                for &argb in &self.pixels {
                    rgb.push(((argb >> 16) & 0xFF) as u8);
                    rgb.push(((argb >> 8) & 0xFF) as u8);
                    rgb.push((argb & 0xFF) as u8);
                }
                let image = image_crate::RgbImage::from_raw(self.width, self.height, rgb)
                    .ok_or_else(|| {
                        format!(
                            "invalid RGB image dimensions: {}x{}",
                            self.width, self.height
                        )
                    })?;
                image_crate::DynamicImage::ImageRgb8(image)
                    .write_to(&mut out, format.image_format())
                    .map_err(|err| format!("encode failed: {err}"))?;
            }
        }
        Ok(out.into_inner())
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
            x, y, w, h, self.width, self.height
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
            x, y, w, h, self.width, self.height
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
            let dst_start = ((y + row) as usize) * (self.width as usize) + (x as usize);
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
/// `destroy()` below performs the actual targeted reclamation.
///
/// Bug awt-font-image #2 — bounded raster memory: previously the only
/// reclamation was `destroy()` (test-only, since no native end-of-life hook
/// for `BufferedImage` exists — the VM exposes no GC→native callback or
/// identity-hash→ObjectRef resolver to this crate), so every `createImage`
/// leaked `width*height*4` bytes for the VM lifetime. We now bound the total
/// raster memory with an LRU keyed on a last-touch logical clock (mirroring
/// the font-metrics / glyph-atlas LRUs elsewhere in this crate). When a new
/// image would push the registry over [`RASTER_BUDGET_BYTES`], the
/// least-recently-touched rasters are evicted until the registry fits again
/// (the just-created image is never evicted, so a single create always
/// succeeds). Every `get`/`get_mut`/`create` refreshes the touched image's
/// stamp, so any image an app is actively reading or drawing stays hot.
///
/// Liveness / pinning (review fix — never evict a still-referenced image):
///
/// Eviction must NEVER discard the raster of a `BufferedImage` that is still
/// live on the Java side — doing so silently blanks pixels an app may still
/// read or draw (corruption). Because this crate has no GC end-of-life hook,
/// we treat every registered image as PINNED (live) from the moment it is
/// created until something explicitly releases it. The LRU now only ever
/// reclaims entries that have been [`unpin`](ImageRegistry::unpin)ned — i.e.
/// proven dead by an explicit `dispose`/`flush`-style release. If the registry
/// is over budget but every remaining entry is still pinned, eviction SKIPS
/// rather than dropping a live raster: the registry simply grows past the soft
/// budget until live images are released. This trades a soft, bounded budget
/// for never losing live pixels — the correct precedence per Java semantics.
pub struct ImageRegistry {
    images: FxHashMap<u64, ImageEntry>,
    next_id: u64,
    /// Monotonic logical clock; bumped on every touch (create / get / get_mut)
    /// so LRU eviction can order entries by last use. `Cell` so `get(&self)`
    /// can refresh a touch without requiring `&mut self` at every call site.
    tick: Cell<u64>,
    /// Running sum of every entry's `byte_len`, kept in step with inserts and
    /// evictions so the budget check is O(1) instead of re-summing the map.
    total_bytes: usize,
    /// Soft ceiling on `total_bytes` before LRU eviction runs. Defaults to
    /// [`RASTER_BUDGET_BYTES`]; overridable only in tests so the eviction path
    /// can be exercised without allocating the full production budget.
    budget_bytes: usize,
}

/// One registered image plus its LRU bookkeeping.
struct ImageEntry {
    data: BufferedImageData,
    /// Value of [`ImageRegistry::tick`] at this entry's last access. `Cell`
    /// so a `get(&self)` read can refresh it through a shared borrow.
    last_touch: Cell<u64>,
    /// Cached raster size in bytes (`width * height * 4`), so eviction can
    /// maintain the running `total_bytes` without recomputing.
    byte_len: usize,
    /// Liveness pin: `true` while a Java `BufferedImage` still references this
    /// raster. Pinned entries are NEVER evicted (evicting one would silently
    /// blank a still-referenced image). Set on `create`, cleared by an explicit
    /// [`ImageRegistry::unpin`] when the Java object is released. `Cell` so a
    /// shared-borrow path can flip it if ever needed.
    pinned: Cell<bool>,
}

/// Soft ceiling on total raster bytes held by the registry before LRU
/// eviction kicks in. 256 MiB comfortably holds dozens of full-HD ARGB
/// images; chosen large so eviction only ever reclaims genuinely idle rasters
/// under realistic Swing/Java2D workloads.
const RASTER_BUDGET_BYTES: usize = 256 * 1024 * 1024;

impl ImageRegistry {
    fn new() -> Self {
        ImageRegistry {
            images: FxHashMap::default(),
            next_id: 1,
            tick: Cell::new(0),
            total_bytes: 0,
            budget_bytes: RASTER_BUDGET_BYTES,
        }
    }

    /// Test-only constructor with a custom raster budget so the LRU eviction
    /// path can be exercised without allocating the full production budget.
    #[cfg(test)]
    fn with_budget(budget_bytes: usize) -> Self {
        ImageRegistry {
            budget_bytes,
            ..ImageRegistry::new()
        }
    }

    /// Bump and return the logical clock.
    #[inline]
    fn next_tick(&self) -> u64 {
        let t = self.tick.get().wrapping_add(1);
        self.tick.set(t);
        t
    }

    /// Byte footprint of a raster of these dimensions (ARGB = 4 bytes/pixel).
    #[inline]
    fn raster_bytes(data: &BufferedImageData) -> usize {
        (data.width() as usize)
            .saturating_mul(data.height() as usize)
            .saturating_mul(4)
    }

    /// Evict least-recently-touched *unpinned* entries until total raster bytes
    /// fit within [`RASTER_BUDGET_BYTES`], never touching `protect` (the image
    /// we just inserted) and never touching a PINNED entry — a pinned entry is
    /// still referenced by a live Java `BufferedImage`, and dropping its raster
    /// would silently blank pixels the app may still read or draw.
    ///
    /// Only entries that have been explicitly [`unpin`](Self::unpin)ned are
    /// eligible victims. If every remaining entry is still pinned (or is the
    /// protected one), eviction SKIPS: the registry is allowed to grow past the
    /// soft budget rather than discard live pixels. Bug awt-font-image #2 +
    /// review fix (never evict a still-referenced image).
    fn evict_until_within_budget(&mut self, protect: u64) {
        while self.total_bytes > self.budget_bytes {
            // Find the oldest (smallest last_touch) UNPINNED entry that isn't
            // the just-inserted `protect` image.
            let victim = self
                .images
                .iter()
                .filter(|(id, e)| **id != protect && !e.pinned.get())
                .min_by_key(|(_, e)| e.last_touch.get())
                .map(|(id, _)| *id);
            match victim {
                Some(id) => {
                    if let Some(e) = self.images.remove(&id) {
                        self.total_bytes = self.total_bytes.saturating_sub(e.byte_len);
                    }
                }
                // No evictable (unpinned, unprotected) entry remains: every
                // other raster is still live. Keep them all and let the
                // registry exceed the soft budget — losing a live raster is
                // never acceptable. A single image larger than the whole
                // budget is likewise kept so the create still works.
                None => break,
            }
        }
    }

    /// Create a new image and return its registry ID.
    ///
    /// Returns `None` if `width * height` overflows `usize` — the caller
    /// (a native method) should surface this as a Java `OutOfMemoryError`
    /// rather than panicking.
    pub fn create(&mut self, width: u32, height: u32, image_type: ImageType) -> Option<ImageId> {
        let data = BufferedImageData::try_new(width, height, image_type)?;
        let byte_len = Self::raster_bytes(&data);
        let id = self.next_id;
        self.next_id += 1;
        let touch = self.next_tick();
        self.total_bytes = self.total_bytes.saturating_add(byte_len);
        self.images.insert(
            id,
            ImageEntry {
                data,
                last_touch: Cell::new(touch),
                byte_len,
                // A freshly-created image is referenced by the Java
                // `BufferedImage` that triggered this `create`, so it starts
                // pinned (live) and is never an eviction victim until released.
                pinned: Cell::new(true),
            },
        );
        // Bug awt-font-image #2: reclaim idle (unpinned) rasters so total
        // memory stays bounded. Protect the image we just inserted; pinned
        // entries are skipped inside `evict_until_within_budget`.
        self.evict_until_within_budget(id);
        Some(ImageId(id))
    }

    /// Mark an image as released by Java — it no longer has an outstanding
    /// `BufferedImage` reference, so its raster may be reclaimed by LRU
    /// eviction when the registry is over budget. Returns `false` if the id is
    /// unknown. Idempotent.
    ///
    /// Wire this to a `BufferedImage` end-of-life signal (e.g. an explicit
    /// dispose/flush-with-release hook) once one exists; until then images stay
    /// pinned for the VM lifetime, which is the safe (never-corrupt) default.
    pub fn unpin(&mut self, id: ImageId) -> bool {
        match self.images.get(&id.0) {
            Some(e) => {
                e.pinned.set(false);
                true
            }
            None => false,
        }
    }

    /// Re-pin a previously [`unpin`](Self::unpin)ned image as live again (e.g.
    /// it was handed back out to Java). Returns `false` if the id is unknown.
    pub fn pin(&mut self, id: ImageId) -> bool {
        match self.images.get(&id.0) {
            Some(e) => {
                e.pinned.set(true);
                true
            }
            None => false,
        }
    }

    /// Whether the image is currently pinned (live / non-evictable). `None` if
    /// the id is unknown. Mainly for tests / diagnostics.
    pub fn is_pinned(&self, id: ImageId) -> Option<bool> {
        self.images.get(&id.0).map(|e| e.pinned.get())
    }

    /// Look up an image by ID (immutable). Refreshes the entry's LRU stamp so
    /// an actively-read image is never treated as idle.
    pub fn get(&self, id: ImageId) -> Option<&BufferedImageData> {
        // Compute the new stamp before borrowing the entry so the two shared
        // (`&self`) borrows don't visually overlap; both only read `self`.
        let touch = self.next_tick();
        let entry = self.images.get(&id.0)?;
        entry.last_touch.set(touch);
        Some(&entry.data)
    }

    /// Look up an image by ID (mutable). Refreshes the entry's LRU stamp.
    pub fn get_mut(&mut self, id: ImageId) -> Option<&mut BufferedImageData> {
        let touch = self.next_tick();
        let entry = self.images.get_mut(&id.0)?;
        entry.last_touch.set(touch);
        Some(&mut entry.data)
    }

    /// Remove an image from the registry, freeing its pixel data.
    pub fn destroy(&mut self, id: ImageId) -> bool {
        match self.images.remove(&id.0) {
            Some(e) => {
                self.total_bytes = self.total_bytes.saturating_sub(e.byte_len);
                true
            }
            None => false,
        }
    }

    /// Number of images currently registered.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Total raster bytes currently held. Mainly for tests / diagnostics.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
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
    fn try_new_rejects_non_overflowing_raster_above_cap() {
        let side = 8_193u32;
        assert!(
            (side as usize) * (side as usize) > MAX_IMAGE_PIXELS,
            "test dimensions must exceed the cap without overflowing"
        );
        assert!(BufferedImageData::try_new(side, side, ImageType::IntArgb).is_none());
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

    #[test]
    fn png_encode_decode_preserves_argb_pixels() {
        let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
        img.set_rgb(0, 0, 0xFFFF_0000);
        img.set_rgb(1, 0, 0xFF00_FF00);
        img.set_rgb(0, 1, 0xFF00_00FF);
        img.set_rgb(1, 1, 0x8040_3020);

        let encoded = img.encode(EncodedImageFormat::Png).expect("encode png");
        assert!(encoded.starts_with(b"\x89PNG"));

        let decoded = BufferedImageData::decode_encoded(&encoded).expect("decode png");
        assert_eq!(decoded.width(), 2);
        assert_eq!(decoded.height(), 2);
        assert_eq!(decoded.get_rgb(0, 0), 0xFFFF_0000);
        assert_eq!(decoded.get_rgb(1, 0), 0xFF00_FF00);
        assert_eq!(decoded.get_rgb(0, 1), 0xFF00_00FF);
        assert_eq!(decoded.get_rgb(1, 1), 0x8040_3020);
    }

    #[test]
    fn jpeg_encode_decode_round_trips_dimensions() {
        let mut img = BufferedImageData::new(3, 2, ImageType::IntRgb);
        img.set_rgb(0, 0, 0xFFFF_0000);
        img.set_rgb(1, 0, 0xFF00_FF00);
        img.set_rgb(2, 0, 0xFF00_00FF);

        let encoded = img.encode(EncodedImageFormat::Jpeg).expect("encode jpeg");
        assert!(encoded.starts_with(&[0xFF, 0xD8]));

        let decoded = BufferedImageData::decode_encoded(&encoded).expect("decode jpeg");
        assert_eq!(decoded.width(), 3);
        assert_eq!(decoded.height(), 2);
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

    // ── Bounded raster memory (bug awt-font-image #2) ────────────────────

    #[test]
    fn registry_total_bytes_tracks_create_and_destroy() {
        let mut reg = ImageRegistry::new();
        assert_eq!(reg.total_bytes(), 0);
        let id = reg.create(10, 10, ImageType::IntArgb).unwrap();
        assert_eq!(reg.total_bytes(), 10 * 10 * 4);
        reg.destroy(id);
        assert_eq!(reg.total_bytes(), 0);
    }

    #[test]
    fn registry_lru_evicts_idle_when_over_budget() {
        // Budget holds two 10x10 (400 B) rasters but not three.
        let mut reg = ImageRegistry::with_budget(900);
        let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
        let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
        // `b` is released by Java (no live BufferedImage), so it becomes an
        // eligible eviction victim; `a` stays pinned (live).
        assert!(reg.unpin(b));
        // Touch `a` so `b` is also the least-recently-used.
        assert!(reg.get(a).is_some());
        // Third create pushes total to 1200 > 900: the idle, unpinned one
        // (`b`) is evicted; the live ones (`a`, `c`) are not.
        let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
        assert!(reg.get(a).is_some(), "live (pinned) image must survive");
        assert!(reg.get(c).is_some(), "just-created image must survive");
        assert!(reg.get(b).is_none(), "released, idle image is evicted");
        assert!(reg.total_bytes() <= 900);
    }

    #[test]
    fn registry_never_evicts_pinned_live_image() {
        // Review fix: a still-referenced (pinned) image is NEVER evicted, even
        // when that pushes the registry well past the soft budget. Losing live
        // pixels is never acceptable.
        let mut reg = ImageRegistry::with_budget(900);
        let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
        let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
        // Everything stays pinned (the default): a third create overshoots the
        // budget but must not drop either live raster.
        let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
        assert!(reg.get(a).is_some(), "pinned image a must survive");
        assert!(reg.get(b).is_some(), "pinned image b must survive");
        assert!(reg.get(c).is_some(), "just-created image c must survive");
        assert_eq!(reg.len(), 3);
        // The registry is allowed to exceed the soft budget rather than corrupt
        // a live image.
        assert!(reg.total_bytes() > 900);
    }

    #[test]
    fn registry_unpin_then_repin_protects_again() {
        let mut reg = ImageRegistry::with_budget(900);
        let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
        let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
        // Release then re-acquire `b`: it is live again and must not be evicted.
        assert!(reg.unpin(b));
        assert_eq!(reg.is_pinned(b), Some(false));
        assert!(reg.pin(b));
        assert_eq!(reg.is_pinned(b), Some(true));
        // Over-budget create: both `a` and `b` are pinned, so neither is dropped.
        let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
        assert!(reg.get(a).is_some());
        assert!(
            reg.get(b).is_some(),
            "re-pinned image must survive eviction"
        );
        assert!(reg.get(c).is_some());
        assert_eq!(reg.len(), 3);
    }

    #[test]
    fn registry_pin_helpers_on_unknown_id() {
        let mut reg = ImageRegistry::new();
        assert!(!reg.unpin(ImageId(999)));
        assert!(!reg.pin(ImageId(999)));
        assert_eq!(reg.is_pinned(ImageId(999)), None);
    }

    #[test]
    fn registry_create_keeps_oversized_single_image() {
        // A single image larger than the whole budget is kept (the create must
        // still succeed); only the just-created image remains.
        let mut reg = ImageRegistry::with_budget(100);
        let id = reg.create(10, 10, ImageType::IntArgb).unwrap(); // 400 B > 100
        assert!(reg.get(id).is_some());
        assert_eq!(reg.len(), 1);
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
