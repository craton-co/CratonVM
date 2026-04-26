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
    NSApplication, NSBackingStoreType, NSColor, NSEvent, NSEventType, NSFont,
    NSPasteboard, NSPasteboardTypeString, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    CGPoint, CGRect, CGSize, MainThreadMarker, NSPoint, NSRect, NSSize, NSString,
};

use super::backend::*;

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
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| PlatformError::EventLoopError(
                "CocoaBackend must be created on the main thread".into(),
            ))?;

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

        if (width * height) as usize > pixels.len() {
            return Err(PlatformError::CreationFailed("pixel buffer too small".into()));
        }

        // Use Core Graphics to create a bitmap context and draw it.
        // This requires objc2-core-graphics or raw CG calls. For now
        // we use NSBitmapImageRep via the view's lockFocus pattern.
        //
        // In practice the Java2D software renderer will blit through
        // a CGImage backed by the pixel buffer. The full CG path
        // requires the "CoreGraphics" feature. We store the buffer
        // for the next drawRect.
        //
        // Minimal approach: use CGContextRef from lockFocus.
        unsafe {
            if info.view.lockFocusIfCanDraw() {
                // Get the current graphics context and draw pixel data.
                // objc2-app-kit provides NSGraphicsContext, from which we
                // can obtain a CGContextRef. Full implementation requires
                // objc2-core-graphics; for now we do a
                // CGBitmapContextCreate → CGImageCreate → draw dance via
                // raw objc messages.

                let cg_colorspace: *mut AnyObject =
                    msg_send![objc2::class!(CGColorSpace), deviceRGBColorSpace];

                // Create CGDataProvider from pixel data.
                let data_ptr = pixels.as_ptr() as *const std::ffi::c_void;
                let data_len = (width * height * 4) as usize;

                let provider: *mut AnyObject = msg_send![
                    objc2::class!(CGDataProvider),
                    dataProviderWithData: std::ptr::null::<std::ffi::c_void>(),
                    data: data_ptr,
                    size: data_len,
                    releaseData: std::ptr::null::<std::ffi::c_void>()
                ];

                if !provider.is_null() {
                    let bits_per_component: usize = 8;
                    let bits_per_pixel: usize = 32;
                    let bytes_per_row: usize = (width * 4) as usize;
                    // kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little = 0x2002
                    let bitmap_info: u32 = 0x2002;

                    let cg_image: *mut AnyObject = msg_send![
                        objc2::class!(CGImage),
                        imageWithWidth: width as usize,
                        height: height as usize,
                        bitsPerComponent: bits_per_component,
                        bitsPerPixel: bits_per_pixel,
                        bytesPerRow: bytes_per_row,
                        colorSpace: cg_colorspace,
                        bitmapInfo: bitmap_info,
                        provider: provider,
                        decode: std::ptr::null::<f64>(),
                        shouldInterpolate: false,
                        intent: 0u32 // kCGRenderingIntentDefault
                    ];

                    if !cg_image.is_null() {
                        // Draw into the current NSGraphicsContext.
                        let ns_gc = objc2_app_kit::NSGraphicsContext::currentContext(self.mtm);
                        if let Some(gc) = ns_gc {
                            let cg_ctx: *mut AnyObject = msg_send![&gc, CGContext];
                            if !cg_ctx.is_null() {
                                let rect = CGRect::new(
                                    CGPoint::new(0.0, 0.0),
                                    CGSize::new(width as f64, height as f64),
                                );
                                let _: () =
                                    msg_send![cg_ctx, drawImage: cg_image, inRect: rect];
                            }
                        }
                    }
                }

                info.view.unlockFocus();
            }
        }
        Ok(())
    }

    fn poll_events(&mut self) -> Vec<PlatformEvent> {
        let app = NSApplication::sharedApplication(self.mtm);
        let mut events = std::mem::take(&mut self.pending_events);

        loop {
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
        unsafe {
            app.run();
        }
    }

    fn post_quit(&mut self) {
        self.quit = true;
        let app = NSApplication::sharedApplication(self.mtm);
        unsafe {
            app.stop(None);
        }
    }

    fn screen_size(&self) -> (u32, u32) {
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
        unsafe {
            let family_ns = NSString::from_str(font_family);
            let font = if bold {
                NSFont::boldSystemFontOfSize(font_size as f64)
            } else {
                NSFont::fontWithName_size(&family_ns, font_size as f64)
                    .unwrap_or_else(|| NSFont::systemFontOfSize(font_size as f64))
            };

            let text_ns = NSString::from_str(text);
            // Use NSString sizeWithAttributes for measurement.
            // We need an NSDictionary with NSFontAttributeName → font.
            let dict: Retained<objc2_foundation::NSDictionary<objc2_foundation::NSString, AnyObject>> = {
                let key = NSString::from_str("NSFont");
                let font_obj: &AnyObject = std::mem::transmute(&*font);
                objc2_foundation::NSDictionary::from_id_slice(
                    &[key],
                    &[font_obj.retain()],
                )
            };

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
        // Measure first.
        let (w_f, h_f) = self.measure_text(text, font_family, font_size, bold, italic);
        let w = (w_f.ceil() as u32).max(1);
        let h = (h_f.ceil() as u32).max(1);

        // Create a bitmap context, draw text, read pixels.
        // Full CG implementation requires objc2-core-graphics.
        // For now return a measured-but-empty raster — the software
        // renderer will composite the color.
        let pixels = vec![0u32; (w * h) as usize];
        let _ = color; // would be used with CGContextSetFillColor

        TextRaster {
            pixels,
            width: w,
            height: h,
            baseline: h_f * 0.8, // approximate
        }
    }

    fn clipboard_get_text(&self) -> Option<String> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            let nsstring = pb.stringForType(NSPasteboardTypeString)?;
            Some(nsstring.to_string())
        }
    }

    fn clipboard_set_text(&mut self, text: &str) -> Result<(), PlatformError> {
        unsafe {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ns_text = NSString::from_str(text);
            let types = objc2_foundation::NSArray::from_retained_slice(&[
                NSPasteboardTypeString.copy(),
            ]);
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

    fn show_message_dialog(
        &mut self,
        title: &str,
        message: &str,
        msg_type: MessageDialogType,
    ) {
        unsafe {
            let alert: Retained<AnyObject> = msg_send![objc2::class!(NSAlert), new];
            let title_ns = NSString::from_str(title);
            let msg_ns = NSString::from_str(message);
            let _: () = msg_send![&alert, setMessageText: &*title_ns];
            let _: () = msg_send![&alert, setInformativeText: &*msg_ns];

            let style: isize = match msg_type {
                MessageDialogType::Warning => 0, // NSAlertStyleWarning
                MessageDialogType::Error => 2,   // NSAlertStyleCritical
                _ => 1,                           // NSAlertStyleInformational
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

// Safety: CocoaBackend must only be used from the main thread, but we
// mark it Send so it satisfies the PlatformBackend bound. The
// MainThreadMarker guarantees construction happens on the main thread,
// and the EDT design ensures all subsequent calls are also on that thread.
unsafe impl Send for CocoaBackend {}
