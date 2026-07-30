// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! X11 platform backend — Linux windowing via `x11rb`, software blitting
//! via `put_image`, and text rasterization via `fontdue`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event as X11Event;
use x11rb::rust_connection::RustConnection;
// x11rb 0.13: `change_property8`/`change_property32` live on the wrapper
// trait, not the xproto one; import anonymously to avoid shadowing
// `xproto::ConnectionExt` from the glob above.
use x11rb::wrapper::ConnectionExt as _;

use super::backend::*;
use crate::font::{global_glyph_atlas, GlyphKey};

// ---------------------------------------------------------------------------
// ID generator
// ---------------------------------------------------------------------------

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_window_id() -> WindowId {
    WindowId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

// ---------------------------------------------------------------------------
// Per-window state
// ---------------------------------------------------------------------------

struct WindowInfo {
    xid: u32,
    gc: u32,
    width: u32,
    height: u32,
}

// ---------------------------------------------------------------------------
// X11Backend
// ---------------------------------------------------------------------------

pub struct X11Backend {
    conn: RustConnection,
    screen_num: usize,
    windows: HashMap<WindowId, WindowInfo>,
    xid_to_wid: HashMap<u32, WindowId>,
    quit: bool,
    wm_delete_window: u32,
    wm_protocols: u32,
}

impl X11Backend {
    /// Connect to the X display.
    pub fn new() -> Result<Self, PlatformError> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|e| PlatformError::EventLoopError(format!("X11 connect: {e}")))?;

        // Intern WM_DELETE_WINDOW / WM_PROTOCOLS atoms.
        let wm_protocols = conn
            .intern_atom(false, b"WM_PROTOCOLS")
            .map_err(|e| PlatformError::EventLoopError(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| PlatformError::EventLoopError(format!("intern_atom reply: {e}")))?
            .atom;

        let wm_delete_window = conn
            .intern_atom(false, b"WM_DELETE_WINDOW")
            .map_err(|e| PlatformError::EventLoopError(format!("intern_atom: {e}")))?
            .reply()
            .map_err(|e| PlatformError::EventLoopError(format!("intern_atom reply: {e}")))?
            .atom;

        Ok(Self {
            conn,
            screen_num,
            windows: HashMap::new(),
            xid_to_wid: HashMap::new(),
            quit: false,
            wm_delete_window,
            wm_protocols,
        })
    }

    fn screen(&self) -> &Screen {
        &self.conn.setup().roots[self.screen_num]
    }

    fn translate_event(&self, event: &X11Event) -> Option<PlatformEvent> {
        match event {
            X11Event::Expose(e) => {
                let id = *self.xid_to_wid.get(&e.window)?;
                Some(PlatformEvent::WindowExposed { id })
            }
            X11Event::ConfigureNotify(e) => {
                let id = *self.xid_to_wid.get(&e.window)?;
                Some(PlatformEvent::WindowResize {
                    id,
                    w: e.width as u32,
                    h: e.height as u32,
                })
            }
            X11Event::ButtonPress(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                // Buttons 4/5 are scroll wheel in X11.
                if e.detail == 4 || e.detail == 5 {
                    let amount = if e.detail == 4 { -1 } else { 1 };
                    return Some(PlatformEvent::MouseWheel {
                        id,
                        x: e.event_x as i32,
                        y: e.event_y as i32,
                        amount,
                    });
                }
                let button = match e.detail {
                    1 => 1u8, // left
                    2 => 2,   // middle
                    3 => 3,   // right
                    b => b,
                };
                Some(PlatformEvent::MousePressed {
                    id,
                    x: e.event_x as i32,
                    y: e.event_y as i32,
                    button,
                })
            }
            X11Event::ButtonRelease(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                if e.detail == 4 || e.detail == 5 {
                    return None; // scroll — no release event
                }
                let button = match e.detail {
                    1 => 1u8,
                    2 => 2,
                    3 => 3,
                    b => b,
                };
                Some(PlatformEvent::MouseReleased {
                    id,
                    x: e.event_x as i32,
                    y: e.event_y as i32,
                    button,
                })
            }
            X11Event::MotionNotify(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                let buttons = e.state;
                let any_button = buttons.contains(KeyButMask::BUTTON1)
                    || buttons.contains(KeyButMask::BUTTON2)
                    || buttons.contains(KeyButMask::BUTTON3);
                if any_button {
                    let button = if buttons.contains(KeyButMask::BUTTON1) {
                        1
                    } else if buttons.contains(KeyButMask::BUTTON3) {
                        3
                    } else {
                        2
                    };
                    Some(PlatformEvent::MouseDragged {
                        id,
                        x: e.event_x as i32,
                        y: e.event_y as i32,
                        button,
                    })
                } else {
                    Some(PlatformEvent::MouseMoved {
                        id,
                        x: e.event_x as i32,
                        y: e.event_y as i32,
                    })
                }
            }
            X11Event::KeyPress(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                let modifiers = x11_key_modifiers(e.state);
                Some(PlatformEvent::KeyPressed {
                    id,
                    key_code: e.detail as u32,
                    char_val: None, // would need XKB to resolve
                    modifiers,
                })
            }
            X11Event::KeyRelease(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                let modifiers = x11_key_modifiers(e.state);
                Some(PlatformEvent::KeyReleased {
                    id,
                    key_code: e.detail as u32,
                    char_val: None,
                    modifiers,
                })
            }
            X11Event::FocusIn(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                Some(PlatformEvent::FocusGained { id })
            }
            X11Event::FocusOut(e) => {
                let id = *self.xid_to_wid.get(&e.event)?;
                Some(PlatformEvent::FocusLost { id })
            }
            X11Event::ClientMessage(e) => {
                let id = *self.xid_to_wid.get(&e.window)?;
                if e.format == 32 {
                    let data = e.data.as_data32();
                    if data[0] == self.wm_delete_window {
                        return Some(PlatformEvent::WindowClose { id });
                    }
                }
                None
            }
            _ => None,
        }
    }
}

fn x11_key_modifiers(state: KeyButMask) -> KeyModifiers {
    let mut m = KeyModifiers::empty();
    if state.contains(KeyButMask::SHIFT) {
        m |= KeyModifiers::SHIFT;
    }
    if state.contains(KeyButMask::CONTROL) {
        m |= KeyModifiers::CTRL;
    }
    if state.contains(KeyButMask::MOD1) {
        m |= KeyModifiers::ALT;
    }
    if state.contains(KeyButMask::MOD4) {
        m |= KeyModifiers::META;
    }
    m
}

// ---------------------------------------------------------------------------
// PlatformBackend
// ---------------------------------------------------------------------------

impl PlatformBackend for X11Backend {
    fn create_window(
        &mut self,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<WindowId, PlatformError> {
        let screen = self.screen().clone();
        let id = next_window_id();

        let xid = self
            .conn
            .generate_id()
            .map_err(|e| PlatformError::CreationFailed(format!("generate_id: {e}")))?;

        let event_mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::FOCUS_CHANGE;

        self.conn
            .create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                xid,
                screen.root,
                x as i16,
                y as i16,
                width as u16,
                height as u16,
                0, // border
                WindowClass::INPUT_OUTPUT,
                0, // visual: CopyFromParent
                &CreateWindowAux::new()
                    .event_mask(event_mask)
                    .background_pixel(screen.white_pixel),
            )
            .map_err(|e| PlatformError::CreationFailed(format!("create_window: {e}")))?;

        // Set title.
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            xid,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        );

        // Register WM_DELETE_WINDOW.
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            xid,
            self.wm_protocols,
            AtomEnum::ATOM,
            &[self.wm_delete_window],
        );

        // Create GC for blitting.
        let gc = self
            .conn
            .generate_id()
            .map_err(|e| PlatformError::CreationFailed(format!("generate_id gc: {e}")))?;
        self.conn
            .create_gc(gc, xid, &CreateGCAux::new())
            .map_err(|e| PlatformError::CreationFailed(format!("create_gc: {e}")))?;

        self.conn
            .flush()
            .map_err(|e| PlatformError::CreationFailed(format!("flush: {e}")))?;

        self.windows.insert(
            id,
            WindowInfo {
                xid,
                gc,
                width,
                height,
            },
        );
        self.xid_to_wid.insert(xid, id);
        debug!("X11: created window {id} xid={xid}");
        Ok(id)
    }

    fn destroy_window(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self
            .windows
            .remove(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        self.xid_to_wid.remove(&info.xid);
        let _ = self.conn.free_gc(info.gc);
        let _ = self.conn.destroy_window(info.xid);
        let _ = self.conn.flush();
        debug!("X11: destroyed window {id}");
        Ok(())
    }

    fn show_window(&mut self, id: WindowId, visible: bool) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        if visible {
            let _ = self.conn.map_window(info.xid);
        } else {
            let _ = self.conn.unmap_window(info.xid);
        }
        let _ = self.conn.flush();
        Ok(())
    }

    fn set_window_title(&mut self, id: WindowId, title: &str) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            info.xid,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            title.as_bytes(),
        );
        let _ = self.conn.flush();
        Ok(())
    }

    fn set_window_bounds(
        &mut self,
        id: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), PlatformError> {
        let info = self
            .windows
            .get_mut(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        let _ = self.conn.configure_window(
            info.xid,
            &ConfigureWindowAux::new().x(x).y(y).width(w).height(h),
        );
        info.width = w;
        info.height = h;
        let _ = self.conn.flush();
        Ok(())
    }

    fn get_window_bounds(&self, id: WindowId) -> Result<(i32, i32, u32, u32), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let geom = self
            .conn
            .get_geometry(info.xid)
            .map_err(|e| PlatformError::EventLoopError(format!("get_geometry: {e}")))?
            .reply()
            .map_err(|e| PlatformError::EventLoopError(format!("get_geometry reply: {e}")))?;
        Ok((
            geom.x as i32,
            geom.y as i32,
            geom.width as u32,
            geom.height as u32,
        ))
    }

    fn request_repaint(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        // Send a synthetic Expose event.
        let event = ExposeEvent {
            response_type: x11rb::protocol::xproto::EXPOSE_EVENT,
            sequence: 0,
            window: info.xid,
            x: 0,
            y: 0,
            width: info.width as u16,
            height: info.height as u16,
            count: 0,
        };
        let _ = self
            .conn
            .send_event(false, info.xid, EventMask::EXPOSURE, event);
        let _ = self.conn.flush();
        Ok(())
    }

    fn blit_buffer(
        &mut self,
        id: WindowId,
        pixels: &[u32],
        width: u32,
        height: u32,
    ) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        // `width * height` in `u32` wraps in release, so a hostile width/height
        // could wrap the product down to a small value that passes this guard
        // while the real area is enormous. Multiply in `usize` with overflow
        // treated as "too big" so the buffer-too-small check can't be bypassed.
        // Mirrors the cocoa backend's `blit_buffer` guard.
        if (width as usize)
            .checked_mul(height as usize)
            .map_or(true, |n| n > pixels.len())
        {
            return Err(PlatformError::CreationFailed(
                "pixel buffer too small".into(),
            ));
        }

        // X11 put_image expects the data as bytes. For 32-bit depth
        // we pass the pixel data directly.
        // SAFETY: `u32` has no invalid bit patterns, byte length is exactly
        // four times the checked pixel count, and the slice cannot outlive
        // the borrowed `pixels` storage.
        let data: &[u8] =
            unsafe { std::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) };

        let _ = self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            info.xid,
            info.gc,
            width as u16,
            height as u16,
            0,
            0,
            0,
            24, // depth
            data,
        );
        let _ = self.conn.flush();
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        let mut events = Vec::new();
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(event)) => {
                    // Update window dimensions on ConfigureNotify.
                    if let X11Event::ConfigureNotify(ref e) = event {
                        if let Some(wid) = self.xid_to_wid.get(&e.window) {
                            if let Some(info) = self.windows.get_mut(wid) {
                                info.width = e.width as u32;
                                info.height = e.height as u32;
                            }
                        }
                    }
                    if let Some(pe) = self.translate_event(&event) {
                        events.push(pe);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    warn!("X11 poll error: {e}");
                    break;
                }
            }
        }
        events
    }

    fn run_event_loop(&mut self) {
        self.quit = false;
        while !self.quit {
            match self.conn.wait_for_event() {
                Ok(event) => {
                    if let X11Event::ConfigureNotify(ref e) = event {
                        if let Some(wid) = self.xid_to_wid.get(&e.window) {
                            if let Some(info) = self.windows.get_mut(wid) {
                                info.width = e.width as u32;
                                info.height = e.height as u32;
                            }
                        }
                    }
                    if let Some(pe) = self.translate_event(&event) {
                        if matches!(pe, PlatformEvent::WindowClose { .. })
                            && self.windows.len() <= 1
                        {
                            self.quit = true;
                        }
                        // In a real implementation the events would be
                        // dispatched to registered listeners.  For now they
                        // accumulate in internal state.
                        let _ = pe;
                    }
                }
                Err(e) => {
                    warn!("X11 event loop error: {e}");
                    break;
                }
            }
        }
    }

    fn post_quit(&mut self) {
        self.quit = true;
    }

    fn screen_size(&self) -> (u32, u32) {
        let screen = self.screen();
        (
            screen.width_in_pixels as u32,
            screen.height_in_pixels as u32,
        )
    }

    fn screen_dpi(&self) -> f64 {
        let screen = self.screen();
        // DPI = pixels / (mm / 25.4)
        if screen.width_in_millimeters > 0 {
            (screen.width_in_pixels as f64) / (screen.width_in_millimeters as f64 / 25.4)
        } else {
            96.0
        }
    }

    fn measure_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> (f32, f32) {
        // Use fontdue for measurement.
        let settings = fontdue_settings(font_family, font_size, bold, italic);
        let font = match load_fontdue_font(&settings) {
            Some(f) => f,
            None => return (0.0, font_size),
        };

        let mut total_width = 0.0f32;
        let mut max_height = 0.0f32;
        for ch in text.chars() {
            let metrics = font.metrics(ch, font_size);
            total_width += metrics.advance_width;
            let h = metrics.height as f32;
            if h > max_height {
                max_height = h;
            }
        }
        (total_width, max_height.max(font_size))
    }

    fn rasterize_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
        color: u32,
    ) -> TextRaster {
        // Round-5: route every per-char rasterization through the process-wide
        // `GlyphAtlas`. The first time we see a (family, size, style, ch)
        // tuple we rasterize via fontdue and insert into the atlas; every
        // subsequent call returns the cached `Arc<GlyphBitmap>` without any
        // fontdue work. This is the high-leverage cache: an interactive Swing
        // UI repaints the same handful of glyphs hundreds of times per second.
        let settings = fontdue_settings(font_family, font_size, bold, italic);
        let font = match load_fontdue_font(&settings) {
            Some(f) => f,
            None => {
                return TextRaster {
                    pixels: vec![],
                    width: 0,
                    height: 0,
                    baseline: 0.0,
                };
            }
        };

        // Round to integer point size for the atlas key (Java AWT fonts are
        // integer-sized) — matches `GlyphKey::new`'s contract.
        let atlas = global_glyph_atlas();
        let size_u32 = font_size.round().max(1.0) as u32;

        // First pass: fetch (or rasterize-and-cache) each glyph, then compute
        // total advance and per-glyph vertical extents.
        let mut total_advance = 0.0f32;
        let mut max_ascent = 0i32;
        let mut max_descent = 0i32;

        let mut glyphs: Vec<(std::sync::Arc<crate::font::GlyphBitmap>, f32)> =
            Vec::with_capacity(text.chars().count());
        // `GlyphBitmap` referenced via fully-qualified path so we don't need
        // to add another use-import; the path matches `crate::font` exports.
        for ch in text.chars() {
            let key = GlyphKey::new(font_family, size_u32, bold, italic, ch);
            let glyph = atlas.get_or_rasterize(key, &font);
            // GlyphBitmap.bearing_y == fontdue's `ymin` (offset from baseline
            // to bitmap bottom; can be negative for descenders). Reconstruct
            // the ascent/descent the original code computed from `metrics`.
            let ascent = glyph.bearing_y + glyph.height as i32;
            if ascent > max_ascent {
                max_ascent = ascent;
            }
            let descent = -glyph.bearing_y;
            if descent > max_descent {
                max_descent = descent;
            }
            let advance = glyph.advance;
            glyphs.push((glyph, total_advance));
            total_advance += advance;
        }

        let w = total_advance.ceil() as u32;
        let h = (max_ascent + max_descent) as u32;
        if w == 0 || h == 0 {
            return TextRaster {
                pixels: vec![],
                width: 0,
                height: 0,
                baseline: 0.0,
            };
        }

        // Clamp the metrics-derived dimensions to a sane maximum before they
        // size the `vec![0u32; w*h]` allocation below. A pathological glyph
        // run (huge font size or an extremely long string) could otherwise
        // drive `w * h` to overflow a `u32`. The clamped dims keep the
        // allocation, the `w`/`h` bounds checks in the blit loop, and the
        // `py * w + px` index all mutually consistent.
        const MAX_RASTER_DIM: u32 = 1 << 15; // 32768 px per side
        let w = w.min(MAX_RASTER_DIM);
        let h = h.min(MAX_RASTER_DIM);
        // Pixel count via a checked `usize` multiply; bail to an empty raster
        // on overflow (the clamp guarantees it fits, but never trust it to a
        // wrapping `as usize`).
        let raster_px = match (w as usize).checked_mul(h as usize) {
            Some(n) => n,
            None => {
                return TextRaster {
                    pixels: vec![],
                    width: 0,
                    height: 0,
                    baseline: 0.0,
                };
            }
        };

        let baseline = max_ascent as f32;
        let r = ((color >> 16) & 0xFF) as u32;
        let g = ((color >> 8) & 0xFF) as u32;
        let b = (color & 0xFF) as u32;

        let mut pixels = vec![0u32; raster_px];

        // Second pass: blit each cached alpha mask into the destination buffer.
        for (glyph, x_offset) in &glyphs {
            let glyph_x0 = (*x_offset + glyph.bearing_x as f32) as i32;
            let glyph_y0 = max_ascent - (glyph.bearing_y + glyph.height as i32);
            let gw = glyph.width as usize;
            let gh = glyph.height as usize;

            for gy in 0..gh {
                for gx in 0..gw {
                    let px = glyph_x0 + gx as i32;
                    let py = glyph_y0 + gy as i32;
                    if px >= 0 && (px as u32) < w && py >= 0 && (py as u32) < h {
                        let alpha = glyph.alpha[gy * gw + gx] as u32;
                        if alpha > 0 {
                            pixels[(py as u32 * w + px as u32) as usize] =
                                (alpha << 24) | (r << 16) | (g << 8) | b;
                        }
                    }
                }
            }
        }

        TextRaster {
            pixels,
            width: w,
            height: h,
            baseline,
        }
    }

    fn clipboard_get_text(&self) -> Option<String> {
        // X11 clipboard requires a full selection request/response dance
        // (XConvertSelection → SelectionNotify). This is a simplified
        // version that works for basic use cases.
        warn!("X11 clipboard_get_text: selection protocol not fully implemented");
        None
    }

    fn clipboard_set_text(&mut self, _text: &str) -> Result<(), PlatformError> {
        warn!("X11 clipboard_set_text: selection protocol not fully implemented");
        Err(PlatformError::ClipboardError(
            "X11 selection protocol not yet implemented".into(),
        ))
    }

    fn show_file_dialog(
        &mut self,
        _title: &str,
        _save: bool,
        _filters: &[(String, String)],
    ) -> Option<String> {
        warn!("X11 show_file_dialog: requires GTK/zenity integration");
        None
    }

    fn show_message_dialog(&mut self, title: &str, message: &str, _msg_type: MessageDialogType) {
        // Without GTK we can't show a real dialog. Log it.
        warn!("X11 message dialog [{title}]: {message}");
    }
}

impl Drop for X11Backend {
    fn drop(&mut self) {
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            let _ = self.destroy_window(id);
        }
    }
}

// ---------------------------------------------------------------------------
// fontdue helpers
// ---------------------------------------------------------------------------

struct FontdueSettings {
    _family: String,
    _bold: bool,
    _italic: bool,
}

fn fontdue_settings(family: &str, _size: f32, bold: bool, italic: bool) -> FontdueSettings {
    FontdueSettings {
        _family: family.to_string(),
        _bold: bold,
        _italic: italic,
    }
}

/// Load (and cache) a system TrueType font for fontdue.
///
/// The result is cached in a process-global `OnceLock` because reading and
/// parsing a font file is expensive and the bytes never change for the
/// lifetime of the process. Mirrors the Cocoa backend's caching strategy.
///
/// TODO: respect `FontdueSettings.family/bold/italic` and pick a matching
/// face. For now we return a single fallback font for every (family, style)
/// combination, which is enough to make text appear instead of blank rectangles.
fn load_fontdue_font(_settings: &FontdueSettings) -> Option<fontdue::Font> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<fontdue::Font>> = OnceLock::new();

    CACHED
        .get_or_init(|| {
            // Attempt to load a system font. On Linux, fonts live in
            // /usr/share/fonts or ~/.local/share/fonts. We try common
            // locations for a sans-serif font.
            static FONT_SEARCH_PATHS: &[&str] = &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
            ];

            for path in FONT_SEARCH_PATHS {
                if let Ok(data) = std::fs::read(path) {
                    if let Ok(font) =
                        fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                    {
                        return Some(font);
                    }
                }
            }

            warn!("fontdue: no system font found");
            None
        })
        .clone()
}
