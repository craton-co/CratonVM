// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! macOS Cocoa platform backend — AppKit windowing, Core Graphics blitting,
//! NSFont text metrics, and NSPasteboard clipboard.
//!
//! Uses `objc2-app-kit` for safe(r) Objective-C interop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, ClassType};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSEventType, NSFont, NSFontAttributeName,
    NSPasteboard, NSPasteboardTypeString, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    CGPoint, CGRect, CGSize, MainThreadMarker, NSPoint, NSRect, NSSize, NSString,
};

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
    window: Retained<NSWindow>,
    view: Retained<NSView>,
}

// ---------------------------------------------------------------------------
// CocoaBackend
// ---------------------------------------------------------------------------

pub struct CocoaBackend {
    windows: HashMap<WindowId, WindowInfo>,
    quit: bool,
    pending_events: Vec<PlatformEvent>,
    mtm: MainThreadMarker,
}

impl CocoaBackend {
    /// Create a new Cocoa backend. Must be called from the main thread.
    pub fn new() -> Result<Self, PlatformError> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            PlatformError::EventLoopError("CocoaBackend must be created on the main thread".into())
        })?;

        // Ensure NSApplication is initialized.
        let _app = NSApplication::sharedApplication(mtm);

        Ok(Self {
            windows: HashMap::new(),
            quit: false,
            pending_events: Vec::new(),
            mtm,
        })
    }

    fn lookup_window_id(&self, ns_window: &NSWindow) -> Option<WindowId> {
        for (&wid, info) in &self.windows {
            // Compare window numbers.
            if info.window.windowNumber() == ns_window.windowNumber() {
                return Some(wid);
            }
        }
        None
    }

    fn translate_ns_event(&self, ns_event: &NSEvent) -> Option<PlatformEvent> {
        // SAFETY: `ns_event` is a live AppKit-owned NSEvent received from the
        // application queue; querying its immutable type is valid here.
        let event_type = unsafe { ns_event.r#type() };
        let ns_window = ns_event.window()?;
        let id = self.lookup_window_id(&ns_window)?;

        match event_type {
            NSEventType::LeftMouseDown => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MousePressed {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 1,
                })
            }
            NSEventType::LeftMouseUp => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MouseReleased {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 1,
                })
            }
            NSEventType::RightMouseDown => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MousePressed {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 3,
                })
            }
            NSEventType::RightMouseUp => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MouseReleased {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 3,
                })
            }
            NSEventType::MouseMoved => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MouseMoved {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                })
            }
            NSEventType::LeftMouseDragged => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MouseDragged {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 1,
                })
            }
            NSEventType::RightMouseDragged => {
                let loc = ns_event.locationInWindow();
                Some(PlatformEvent::MouseDragged {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    button: 3,
                })
            }
            NSEventType::ScrollWheel => {
                let loc = ns_event.locationInWindow();
                // SAFETY: the matched event is a live ScrollWheel NSEvent.
                let dy = unsafe { ns_event.deltaY() } as i32;
                Some(PlatformEvent::MouseWheel {
                    id,
                    x: loc.x as i32,
                    y: loc.y as i32,
                    amount: dy,
                })
            }
            NSEventType::KeyDown => {
                let key_code = ns_event.keyCode() as u32;
                // SAFETY: the matched event is a live KeyDown NSEvent and the
                // returned NSString is retained by the event during this call.
                let chars = unsafe { ns_event.characters() };
                let char_val = chars.and_then(|s| s.to_string().chars().next());
                let modifiers = cocoa_modifiers(ns_event);
                Some(PlatformEvent::KeyPressed {
                    id,
                    key_code,
                    char_val,
                    modifiers,
                })
            }
            NSEventType::KeyUp => {
                let key_code = ns_event.keyCode() as u32;
                // SAFETY: the matched event is a live KeyUp NSEvent and the
                // returned NSString is retained by the event during this call.
                let chars = unsafe { ns_event.characters() };
                let char_val = chars.and_then(|s| s.to_string().chars().next());
                let modifiers = cocoa_modifiers(ns_event);
                Some(PlatformEvent::KeyReleased {
                    id,
                    key_code,
                    char_val,
                    modifiers,
                })
            }
            _ => None,
        }
    }
}

fn cocoa_modifiers(event: &NSEvent) -> KeyModifiers {
    let flags = event.modifierFlags();
    let mut m = KeyModifiers::empty();
    let raw = flags.bits();
    // NSEventModifierFlagShift = 1 << 17
    if raw & (1 << 17) != 0 {
        m |= KeyModifiers::SHIFT;
    }
    // NSEventModifierFlagControl = 1 << 18
    if raw & (1 << 18) != 0 {
        m |= KeyModifiers::CTRL;
    }
    // NSEventModifierFlagOption = 1 << 19
    if raw & (1 << 19) != 0 {
        m |= KeyModifiers::ALT;
    }
    // NSEventModifierFlagCommand = 1 << 20
    if raw & (1 << 20) != 0 {
        m |= KeyModifiers::META;
    }
    m
}

// ---------------------------------------------------------------------------
// PlatformBackend
// ---------------------------------------------------------------------------

impl PlatformBackend for CocoaBackend {
    fn create_window(
        &mut self,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<WindowId, PlatformError> {
        let id = next_window_id();

        let content_rect = NSRect::new(
            NSPoint::new(x as f64, y as f64),
            NSSize::new(width as f64, height as f64),
        );

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;

        // SAFETY: construction occurs on the main thread (proved by `mtm`);
        // the rectangle/style/backing values satisfy AppKit's initializer.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(),
                content_rect,
                style,
                NSBackingStoreType::NSBackingStoreBuffered,
                false,
            )
        };

        let title_ns = NSString::from_str(title);
        window.setTitle(&title_ns);

        // SAFETY: construction and attachment occur on the main thread; the
        // retained view and window keep each other valid after this call.
        let view = unsafe {
            let view = NSView::initWithFrame(NSView::alloc(), content_rect);
            window.setContentView(Some(&view));
            view
        };

        self.windows.insert(id, WindowInfo { window, view });
        debug!("Cocoa: created window {id}");
        Ok(id)
    }

    fn destroy_window(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self
            .windows
            .remove(&id)
            .ok_or(PlatformError::WindowNotFound)?;
        info.window.close();
        debug!("Cocoa: destroyed window {id}");
        Ok(())
    }

    fn show_window(&mut self, id: WindowId, visible: bool) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        if visible {
            info.window.makeKeyAndOrderFront(None);
        } else {
            info.window.orderOut(None);
        }
        Ok(())
    }

    fn set_window_title(&mut self, id: WindowId, title: &str) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let title_ns = NSString::from_str(title);
        info.window.setTitle(&title_ns);
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
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let frame = NSRect::new(
            NSPoint::new(x as f64, y as f64),
            NSSize::new(w as f64, h as f64),
        );
        // SAFETY: the retained window is live and all backend calls are
        // confined to the AppKit main thread.
        unsafe {
            info.window.setFrame_display(frame, true);
        }
        Ok(())
    }

    fn get_window_bounds(&self, id: WindowId) -> Result<(i32, i32, u32, u32), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        let frame = info.window.frame();
        Ok((
            frame.origin.x as i32,
            frame.origin.y as i32,
            frame.size.width as u32,
            frame.size.height as u32,
        ))
    }

    fn request_repaint(&mut self, id: WindowId) -> Result<(), PlatformError> {
        let info = self.windows.get(&id).ok_or(PlatformError::WindowNotFound)?;
        info.view.setNeedsDisplay(true);
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

        if (width as usize)
            .checked_mul(height as usize)
            .map(|n| n > pixels.len())
            .unwrap_or(true)
        {
            return Err(PlatformError::CreationFailed(
                "pixel buffer too small".into(),
            ));
        }

        // Core Graphics blit using the **C ABI** entry points (the previous
        // version of this function sent Objective-C selectors to symbols
        // (`CGColorSpace`, `CGDataProvider`, `CGImage`) that are
        // CoreFoundation types, not Objective-C classes — so every call
        // either no-op'd or aborted with "unrecognized selector". The
        // function below links the real C symbols (declared in the
        // `core_graphics` extern block at the bottom of this file) and
        // releases every CF handle it creates via `CFRelease`.
        //
        // Layout: ARGB32 little-endian (the format the software renderer
        // emits). CGBitmapInfo flags select
        //   kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little
        // so that a `u32` of `0xAARRGGBB` is interpreted byte-for-byte as
        // (B, G, R, A) on a little-endian host, matching the renderer's
        // pre-multiplied output. Memory ordering matches the existing
        // Win32 GDI path; see `core_graphics` extern block for the C API
        // contract.
        // SAFETY: pixel length was checked above; every CoreFoundation handle
        // is null-checked and released exactly once, and all Cocoa calls occur
        // on the main thread while their retained objects are alive.
        unsafe {
            if !info.view.lockFocusIfCanDraw() {
                return Ok(());
            }

            // Wrap unlock-on-drop so an early `return` from a fallible CG
            // step still calls `unlockFocus` (otherwise the next `drawRect:`
            // would assert).
            struct LockGuard<'a>(&'a NSView);
            impl Drop for LockGuard<'_> {
                fn drop(&mut self) {
                    self.0.unlockFocus();
                }
            }
            let _guard = LockGuard(&info.view);

            let cs = core_graphics::CGColorSpaceCreateDeviceRGB();
            if cs.is_null() {
                return Ok(());
            }

            let data_ptr = pixels.as_ptr() as *const std::ffi::c_void;
            let data_len = (width as usize) * (height as usize) * 4;
            let provider = core_graphics::CGDataProviderCreateWithData(
                std::ptr::null_mut(),
                data_ptr,
                data_len,
                None,
            );
            if provider.is_null() {
                core_graphics::CFRelease(cs);
                return Ok(());
            }

            const BITS_PER_COMPONENT: usize = 8;
            const BITS_PER_PIXEL: usize = 32;
            let bytes_per_row = (width as usize) * 4;
            // kCGImageAlphaPremultipliedFirst (2) | kCGBitmapByteOrder32Little (2 << 12) = 0x2002
            const BITMAP_INFO: u32 = 0x2002;
            const INTENT_DEFAULT: i32 = 0; // kCGRenderingIntentDefault

            let image = core_graphics::CGImageCreate(
                width as usize,
                height as usize,
                BITS_PER_COMPONENT,
                BITS_PER_PIXEL,
                bytes_per_row,
                cs,
                BITMAP_INFO,
                provider,
                std::ptr::null(),
                false as u8,
                INTENT_DEFAULT,
            );
            // The image (when non-null) retains the colour space and data
            // provider, so we can drop our own references now.
            core_graphics::CFRelease(provider);
            core_graphics::CFRelease(cs);
            if image.is_null() {
                return Ok(());
            }

            // Pull the destination CGContext from the current NSGraphicsContext.
            if let Some(gc) = objc2_app_kit::NSGraphicsContext::currentContext(self.mtm) {
                let cg_ctx_ptr: *mut std::ffi::c_void = msg_send![&gc, CGContext];
                if !cg_ctx_ptr.is_null() {
                    let rect = CGRect::new(
                        CGPoint::new(0.0, 0.0),
                        CGSize::new(width as f64, height as f64),
                    );
                    core_graphics::CGContextDrawImage(cg_ctx_ptr, rect, image);
                }
            }
            core_graphics::CFRelease(image);
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        let app = NSApplication::sharedApplication(self.mtm);
        let mut events = std::mem::take(&mut self.pending_events);

        loop {
            // SAFETY: `app` is the main-thread NSApplication and all selector
            // arguments are framework-owned constants or copied scalars.
            let ns_event = unsafe {
                app.nextEventMatchingMask_untilDate_inMode_dequeue(
                    objc2_app_kit::NSEventMask::Any,
                    None, // no timeout — return immediately if no events
                    objc2_foundation::NSDefaultRunLoopMode,
                    true,
                )
            };
            match ns_event {
                Some(event) => {
                    if let Some(pe) = self.translate_ns_event(&event) {
                        events.push(pe);
                    }
                    // SAFETY: `event` came from this application's queue and
                    // remains retained for the synchronous dispatch.
                    unsafe {
                        app.sendEvent(&event);
                    }
                }
                None => break,
            }
        }
        events
    }

    fn run_event_loop(&mut self) {
        let app = NSApplication::sharedApplication(self.mtm);
        // SAFETY: `mtm` proves main-thread execution for AppKit's run loop.
        unsafe {
            app.run();
        }
    }

    fn post_quit(&mut self) {
        self.quit = true;
        let app = NSApplication::sharedApplication(self.mtm);
        // SAFETY: `mtm` proves main-thread execution and nil sender is allowed.
        unsafe {
            app.stop(None);
        }
    }

    fn screen_size(&self) -> (u32, u32) {
        // SAFETY: `mtm` proves this AppKit query runs on the main thread.
        let frame = unsafe { objc2_app_kit::NSScreen::mainScreen(self.mtm) };
        match frame {
            Some(screen) => {
                let f = screen.frame();
                (f.size.width as u32, f.size.height as u32)
            }
            None => (1920, 1080),
        }
    }

    fn screen_dpi(&self) -> f64 {
        // macOS reports backing scale factor rather than DPI.
        // A Retina display has factor 2.0, which means 144 DPI
        // (72 * 2). Non-retina is 72 DPI (1 point = 1 pixel).
        // SAFETY: `mtm` proves this AppKit query runs on the main thread.
        let screen = unsafe { objc2_app_kit::NSScreen::mainScreen(self.mtm) };
        match screen {
            Some(s) => {
                let factor = s.backingScaleFactor();
                72.0 * factor
            }
            None => 72.0,
        }
    }

    fn measure_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        _italic: bool,
    ) -> (f32, f32) {
        // SAFETY: all Objective-C objects are retained for the synchronous
        // selectors; `NSFont` is an AnyObject subclass, making the reference
        // cast layout-preserving, and execution is confined to the main thread.
        unsafe {
            let family_ns = NSString::from_str(font_family);
            let font = if bold {
                NSFont::boldSystemFontOfSize(font_size as f64)
            } else {
                NSFont::fontWithName_size(&family_ns, font_size as f64)
                    .unwrap_or_else(|| NSFont::systemFontOfSize(font_size as f64))
            };

            let text_ns = NSString::from_str(text);
            // `-[NSString sizeWithAttributes:]` requires the framework's own
            // `NSFontAttributeName` constant as the dictionary key. The
            // previous version of this code constructed a fresh
            // `NSString::from_str("NSFont")` as the key — AppKit then failed
            // the attribute lookup and silently fell back to the 12pt system
            // font, producing wrong widths for every non-12pt-system layout
            // (a 24pt bold label would under-measure by ~2x). We now use
            // the real `NSFontAttributeName` static (a typed
            // `&NSAttributedStringKey`) from `objc2_app_kit`, matching the
            // JDK's `Font.getStringBounds` behaviour.
            //
            // The dictionary is typed `NSString → AnyObject` because the
            // `NSAttributedStringKey` type alias is just `NSString` and
            // `NSDictionary::from_id_slice` takes `Retained<K>` for the key.
            let key: Retained<NSString> = NSFontAttributeName.copy();
            let font_obj: &AnyObject = std::mem::transmute(&*font);
            let dict: Retained<objc2_foundation::NSDictionary<NSString, AnyObject>> =
                objc2_foundation::NSDictionary::from_id_slice(&[key], &[font_obj.retain()]);

            let size: NSSize = msg_send![&text_ns, sizeWithAttributes: &*dict];
            (size.width as f32, size.height as f32)
        }
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
        // Port of the X11 fontdue implementation. CoreText / CGBitmapContext
        // are intentionally avoided here — fontdue (pure Rust) produces
        // grayscale glyph bitmaps which we pack into the ARGB buffer
        // expected by `TextRaster` (see backend.rs::TextRaster docs).
        // Output format and layout match x11.rs::rasterize_text verbatim so
        // both platforms feed the renderer identically.
        //
        // Round-5: per-char rasterization is routed through the process-wide
        // `GlyphAtlas` so a (family, size, style, ch) tuple is only handed to
        // fontdue once. Interactive Swing repaints reuse the cached
        // `Arc<GlyphBitmap>` for every subsequent character draw.
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
        // total advance and per-glyph vertical extents from the cached metrics.
        let mut total_advance = 0.0f32;
        let mut max_ascent = 0i32;
        let mut max_descent = 0i32;

        let mut glyphs: Vec<(std::sync::Arc<crate::font::GlyphBitmap>, f32)> =
            Vec::with_capacity(text.chars().count());
        for ch in text.chars() {
            let key = GlyphKey::new(font_family, size_u32, bold, italic, ch);
            let glyph = atlas.get_or_rasterize(key, &font);
            // `bearing_y` mirrors fontdue's `ymin` (offset from baseline to
            // bitmap bottom — negative for descenders).
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

        // Clamp the metrics-derived dimensions to a sane maximum and compute
        // the pixel count with a checked multiply before they size the
        // `vec![0u32; raster_px]` allocation below. A pathological glyph run
        // (huge font size or an extremely long string) could otherwise drive
        // `w * h` to overflow a `u32` — wrapping to a small allocation that
        // the blit loop then writes past. The clamped dims keep the
        // allocation, the `w`/`h` bounds checks in the blit loop, and the
        // `py * w + px` index all mutually consistent. Mirrors the x11.rs
        // hardening (its un-hardened twin lived here).
        let (w, h, raster_px) = match clamp_raster_dims(w, h) {
            Some(dims) => dims,
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
        // Straight (non-premultiplied) ARGB to match the X11 path and the
        // `TextRaster` docstring at backend.rs ("ARGB pixels"). The
        // consumer (graphics2d renderer) composites this as a source-over
        // blend, treating the high byte as straight alpha.
        let r = ((color >> 16) & 0xFF) as u32;
        let g = ((color >> 8) & 0xFF) as u32;
        let b = (color & 0xFF) as u32;
        let rgb = (r << 16) | (g << 8) | b;

        let mut pixels = vec![0u32; raster_px];

        // Second pass: blit each cached alpha mask into the destination buffer.
        for (glyph, x_offset) in &glyphs {
            let gw = glyph.width as usize;
            let gh = glyph.height as usize;
            if gw == 0 || gh == 0 {
                continue;
            }
            let glyph_x0 = (*x_offset + glyph.bearing_x as f32) as i32;
            let glyph_y0 = max_ascent - (glyph.bearing_y + glyph.height as i32);

            for gy in 0..gh {
                for gx in 0..gw {
                    let px = glyph_x0 + gx as i32;
                    let py = glyph_y0 + gy as i32;
                    if px >= 0 && (px as u32) < w && py >= 0 && (py as u32) < h {
                        let alpha = glyph.alpha[gy * gw + gx] as u32;
                        if alpha > 0 {
                            pixels[(py as u32 * w + px as u32) as usize] = (alpha << 24) | rgb;
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
        // SAFETY: the backend is main-thread confined; pasteboard objects and
        // returned strings remain retained for each synchronous selector.
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let nsstring = pb.stringForType(NSPasteboardTypeString)?;
            Some(nsstring.to_string())
        }
    }

    fn clipboard_set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        // SAFETY: the backend is main-thread confined and every retained
        // NSString/NSArray stays alive through its synchronous selector.
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns_text = NSString::from_str(text);
            let types =
                objc2_foundation::NSArray::from_retained_slice(&[NSPasteboardTypeString.copy()]);
            pb.declareTypes_owner(&types, None);
            let ok: bool = msg_send![&pb, setString: &*ns_text, forType: NSPasteboardTypeString];
            if ok {
                Ok(())
            } else {
                Err(PlatformError::ClipboardError(
                    "NSPasteboard setString failed".into(),
                ))
            }
        }
    }

    fn show_file_dialog(
        &mut self,
        _title: &str,
        _save: bool,
        _filters: &[(String, String)],
    ) -> Option<String> {
        // Requires NSOpenPanel / NSSavePanel — available via objc2-app-kit
        // but needs additional feature flags (NSOpenPanel, NSSavePanel).
        warn!("Cocoa show_file_dialog: NSOpenPanel/NSSavePanel not yet wired");
        None
    }

    fn show_message_dialog(&mut self, title: &str, message: &str, msg_type: MessageDialogType) {
        // SAFETY: the backend is main-thread confined and all Objective-C
        // objects are retained through the modal alert selectors.
        unsafe {
            let alert: Retained<AnyObject> = msg_send![objc2::class!(NSAlert), new];
            let title_ns = NSString::from_str(title);
            let msg_ns = NSString::from_str(message);
            let _: () = msg_send![&alert, setMessageText: &*title_ns];
            let _: () = msg_send![&alert, setInformativeText: &*msg_ns];

            let style: isize = match msg_type {
                MessageDialogType::Warning => 0, // NSAlertStyleWarning
                MessageDialogType::Error => 2,   // NSAlertStyleCritical
                _ => 1,                          // NSAlertStyleInformational
            };
            let _: () = msg_send![&alert, setAlertStyle: style];
            let _: isize = msg_send![&alert, runModal];
        }
    }
}

impl Drop for CocoaBackend {
    fn drop(&mut self) {
        let ids: Vec<WindowId> = self.windows.keys().copied().collect();
        for id in ids {
            let _ = self.destroy_window(id);
        }
    }
}

// SAFETY: CocoaBackend must only be used from the main thread, but we
// mark it Send so it satisfies the PlatformBackend bound. The
// MainThreadMarker guarantees construction happens on the main thread,
// and the EDT design ensures all subsequent calls are also on that thread.
unsafe impl Send for CocoaBackend {}

/// Maximum side length (in pixels) of a text raster. Bounds the
/// `rasterize_text` pixel buffer so a hostile/extreme glyph run cannot drive
/// `width * height` past `u32`/`usize` limits.
const MAX_RASTER_DIM: u32 = 1 << 15; // 32768 px per side

/// Clamp text-raster dimensions to `MAX_RASTER_DIM` per side and return the
/// clamped `(width, height, pixel_count)` triple, computing the pixel count
/// with a checked multiply. Returns `None` if the product still overflows a
/// `usize` (impossible given the clamp, but the checked multiply is the
/// safety net the security review asked for — never trust the product to a
/// wrapping `as usize`).
fn clamp_raster_dims(w: u32, h: u32) -> Option<(u32, u32, usize)> {
    let w = w.min(MAX_RASTER_DIM);
    let h = h.min(MAX_RASTER_DIM);
    let px = (w as usize).checked_mul(h as usize)?;
    Some((w, h, px))
}

// ---------------------------------------------------------------------------
// fontdue helpers (mirrors x11.rs — see that file for the canonical impl)
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
/// Walks well-known macOS font directories. The result is cached in a
/// process-global `OnceLock` because reading and parsing a font file is
/// expensive and the bytes never change for the lifetime of the process.
///
/// TODO: respect `FontdueSettings.family/bold/italic` and pick a matching
/// face. The X11 path has the same TODO — for now we return a single
/// fallback font for every (family, style) combination, which is enough to
/// make text appear instead of blank rectangles.
fn load_fontdue_font(_settings: &FontdueSettings) -> Option<fontdue::Font> {
    use std::sync::OnceLock;
    static CACHED: OnceLock<Option<fontdue::Font>> = OnceLock::new();

    CACHED
        .get_or_init(|| {
            // macOS ships with a handful of guaranteed-present system fonts.
            // We try the most common ones in order; the first that parses
            // successfully wins. Helvetica.ttc / SFNS variants live in
            // /System/Library/Fonts, user-installed fonts live in
            // /Library/Fonts or ~/Library/Fonts.
            let mut search_paths: Vec<String> = vec![
                "/System/Library/Fonts/Helvetica.ttc".to_string(),
                "/System/Library/Fonts/Geneva.ttf".to_string(),
                "/System/Library/Fonts/Supplemental/Arial.ttf".to_string(),
                "/System/Library/Fonts/SFNSMono.ttf".to_string(),
                "/System/Library/Fonts/SFNS.ttf".to_string(),
                "/System/Library/Fonts/SFNSDisplay.ttf".to_string(),
                "/System/Library/Fonts/SFNSText.ttf".to_string(),
                "/Library/Fonts/Arial.ttf".to_string(),
            ];
            if let Ok(home) = std::env::var("HOME") {
                search_paths.push(format!("{home}/Library/Fonts/Arial.ttf"));
                search_paths.push(format!("{home}/Library/Fonts/Helvetica.ttf"));
            }

            for path in &search_paths {
                if let Ok(data) = std::fs::read(path) {
                    if let Ok(font) =
                        fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                    {
                        debug!("cocoa: loaded font for fontdue from {path}");
                        return Some(font);
                    }
                }
            }

            warn!("cocoa fontdue: no system font found — text will be blank");
            None
        })
        .clone()
}

// ---------------------------------------------------------------------------
// Core Graphics C ABI bindings
// ---------------------------------------------------------------------------
//
// These extern declarations link against the C symbols exported by the
// `CoreGraphics.framework` (CoreFoundation types — `CGColorSpace`,
// `CGDataProvider`, `CGImage`, `CGContext`). They are **not** Objective-C
// classes, so `objc2::msg_send!` cannot reach them; the previous version of
// `blit_buffer` made that mistake and silently no-op'd on macOS. The
// framework is auto-linked by AppKit (which is pulled in via
// `objc2-app-kit`), so no `#[link]` attribute is required here.
//
// Memory ownership: every `*Create*` function returns a +1-retained CF
// handle that the caller must release with `CFRelease`. `CGImageCreate`
// retains its colour-space and data-provider arguments internally, so the
// caller is free to release its own references after `CGImageCreate` returns.
#[allow(non_snake_case)]
mod core_graphics {
    use objc2_foundation::CGRect;

    pub type CGColorSpaceRef = *mut std::ffi::c_void;
    pub type CGDataProviderRef = *mut std::ffi::c_void;
    pub type CGImageRef = *mut std::ffi::c_void;
    pub type CGContextRef = *mut std::ffi::c_void;
    pub type CFTypeRef = *const std::ffi::c_void;

    /// `CGDataProviderReleaseDataCallback` — fired when the provider is
    /// destroyed. Passing `None` (the C `NULL`) tells CG not to touch the
    /// buffer, which is correct here because the pixel data is borrowed
    /// from a Rust slice that outlives the `CGImage` (we `CFRelease` the
    /// image before returning from `blit_buffer`).
    pub type CGDataProviderReleaseDataCallback = Option<
        unsafe extern "C" fn(
            info: *mut std::ffi::c_void,
            data: *const std::ffi::c_void,
            size: usize,
        ),
    >;

    extern "C" {
        pub fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;

        pub fn CGDataProviderCreateWithData(
            info: *mut std::ffi::c_void,
            data: *const std::ffi::c_void,
            size: usize,
            release_data: CGDataProviderReleaseDataCallback,
        ) -> CGDataProviderRef;

        #[allow(clippy::too_many_arguments)]
        pub fn CGImageCreate(
            width: usize,
            height: usize,
            bits_per_component: usize,
            bits_per_pixel: usize,
            bytes_per_row: usize,
            space: CGColorSpaceRef,
            bitmap_info: u32,
            provider: CGDataProviderRef,
            decode: *const f64,
            should_interpolate: u8,
            intent: i32,
        ) -> CGImageRef;

        pub fn CGContextDrawImage(c: CGContextRef, rect: CGRect, image: CGImageRef);

        pub fn CFRelease(cf: CFTypeRef);
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_raster_dims, MAX_RASTER_DIM};

    #[test]
    fn clamp_raster_dims_passes_small_dims_through() {
        assert_eq!(clamp_raster_dims(10, 20), Some((10, 20, 200)));
        assert_eq!(clamp_raster_dims(1, 1), Some((1, 1, 1)));
    }

    #[test]
    fn clamp_raster_dims_clamps_oversized_sides() {
        // Each side is clamped to MAX_RASTER_DIM independently.
        let (w, h, px) = clamp_raster_dims(u32::MAX, 4).unwrap();
        assert_eq!(w, MAX_RASTER_DIM);
        assert_eq!(h, 4);
        assert_eq!(px, MAX_RASTER_DIM as usize * 4);

        let (w, h, px) = clamp_raster_dims(u32::MAX, u32::MAX).unwrap();
        assert_eq!((w, h), (MAX_RASTER_DIM, MAX_RASTER_DIM));
        assert_eq!(px, MAX_RASTER_DIM as usize * MAX_RASTER_DIM as usize);
    }

    #[test]
    fn clamp_raster_dims_no_u32_wrap_on_hostile_product() {
        // 0x10000 * 0x10000 == 0x1_0000_0000 — wraps to 0 in a u32 multiply
        // but must NOT here: the clamp caps both sides to 0x8000 so the
        // pixel count is the (large but exact) clamped product, never 0.
        let (w, h, px) = clamp_raster_dims(0x1_0000, 0x1_0000).unwrap();
        assert_eq!((w, h), (MAX_RASTER_DIM, MAX_RASTER_DIM));
        assert_ne!(px, 0);
        assert_eq!(px, MAX_RASTER_DIM as usize * MAX_RASTER_DIM as usize);
        // Sanity: the clamped product fits a u32 (1<<15 squared == 1<<30).
        assert!(px <= u32::MAX as usize);
    }
}
