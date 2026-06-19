// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Platform backend trait and shared types.
//!
//! Every platform (Win32, X11, Cocoa) implements [`PlatformBackend`] so
//! the upper AWT layer is completely platform-agnostic.

use std::fmt;

// ---------------------------------------------------------------------------
// WindowId
// ---------------------------------------------------------------------------

/// Opaque window identifier handed out by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowId(pub u64);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WindowId({})", self.0)
    }
}

// ---------------------------------------------------------------------------
// PlatformError
// ---------------------------------------------------------------------------

/// Errors returned by platform backend operations.
#[derive(Debug)]
pub enum PlatformError {
    /// The given window ID does not refer to a live window.
    WindowNotFound,
    /// Failed to create a native window.
    CreationFailed(String),
    /// Clipboard operation failed.
    ClipboardError(String),
    /// Event loop error.
    EventLoopError(String),
}

impl fmt::Display for PlatformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowNotFound => write!(f, "window not found"),
            Self::CreationFailed(s) => write!(f, "window creation failed: {s}"),
            Self::ClipboardError(s) => write!(f, "clipboard error: {s}"),
            Self::EventLoopError(s) => write!(f, "event loop error: {s}"),
        }
    }
}

impl std::error::Error for PlatformError {}

// ---------------------------------------------------------------------------
// KeyModifiers
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// Modifier keys held during a key/mouse event.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct KeyModifiers: u8 {
        const SHIFT = 0b0000_0001;
        const CTRL  = 0b0000_0010;
        const ALT   = 0b0000_0100;
        const META  = 0b0000_1000;
    }
}

// ---------------------------------------------------------------------------
// PlatformEvent
// ---------------------------------------------------------------------------

/// Events produced by the native event loop.
#[derive(Debug, Clone)]
pub enum PlatformEvent {
    WindowClose {
        id: WindowId,
    },
    WindowResize {
        id: WindowId,
        w: u32,
        h: u32,
    },
    WindowExposed {
        id: WindowId,
    },
    MousePressed {
        id: WindowId,
        x: i32,
        y: i32,
        button: u8,
    },
    MouseReleased {
        id: WindowId,
        x: i32,
        y: i32,
        button: u8,
    },
    MouseMoved {
        id: WindowId,
        x: i32,
        y: i32,
    },
    MouseDragged {
        id: WindowId,
        x: i32,
        y: i32,
        button: u8,
    },
    MouseWheel {
        id: WindowId,
        x: i32,
        y: i32,
        amount: i32,
    },
    KeyPressed {
        id: WindowId,
        key_code: u32,
        char_val: Option<char>,
        modifiers: KeyModifiers,
    },
    KeyReleased {
        id: WindowId,
        key_code: u32,
        char_val: Option<char>,
        modifiers: KeyModifiers,
    },
    FocusGained {
        id: WindowId,
    },
    FocusLost {
        id: WindowId,
    },
}

// ---------------------------------------------------------------------------
// TextRaster
// ---------------------------------------------------------------------------

/// Rasterized text bitmap returned by [`PlatformBackend::rasterize_text`].
#[derive(Debug, Clone)]
pub struct TextRaster {
    /// ARGB pixels, row-major, top-to-bottom.
    pub pixels: Vec<u32>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Distance from top of bitmap to the text baseline.
    pub baseline: f32,
}

// ---------------------------------------------------------------------------
// MessageDialogType
// ---------------------------------------------------------------------------

/// Kind of native message dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDialogType {
    Info,
    Warning,
    Error,
    Question,
}

// ---------------------------------------------------------------------------
// PlatformBackend trait
// ---------------------------------------------------------------------------

/// Abstraction over OS-level windowing, events, text, and clipboard.
///
/// A single instance lives on the EDT thread. All methods are `&mut self`
/// because native resources are inherently single-threaded.
pub trait PlatformBackend: Send {
    // -- Window management --------------------------------------------------

    /// Create a new top-level window and return its ID.
    fn create_window(
        &mut self,
        title: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<WindowId, PlatformError>;

    /// Destroy a window and release all native resources associated with it.
    fn destroy_window(&mut self, id: WindowId) -> Result<(), PlatformError>;

    /// Show or hide a window.
    fn show_window(&mut self, id: WindowId, visible: bool) -> Result<(), PlatformError>;

    /// Change the window title bar text.
    fn set_window_title(&mut self, id: WindowId, title: &str) -> Result<(), PlatformError>;

    /// Move and resize a window.
    fn set_window_bounds(
        &mut self,
        id: WindowId,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<(), PlatformError>;

    /// Query current window position and size.
    fn get_window_bounds(&self, id: WindowId) -> Result<(i32, i32, u32, u32), PlatformError>;

    /// Ask the platform to schedule a repaint for the given window.
    fn request_repaint(&mut self, id: WindowId) -> Result<(), PlatformError>;

    // -- Pixel buffer blitting ----------------------------------------------

    /// Blit an ARGB pixel buffer to the window's client area.
    /// `pixels` is row-major, top-to-bottom, `width * height` elements.
    fn blit_buffer(
        &mut self,
        id: WindowId,
        pixels: &[u32],
        width: u32,
        height: u32,
    ) -> Result<(), PlatformError>;

    // -- Event loop ---------------------------------------------------------

    /// Non-blocking: drain all pending events and return them.
    fn poll_events(&mut self) -> Vec<PlatformEvent>;

    /// Blocking event loop — returns only when the application quits.
    fn run_event_loop(&mut self);

    /// Post a quit message to break out of [`run_event_loop`].
    fn post_quit(&mut self);

    // -- Screen info --------------------------------------------------------

    /// Primary screen resolution in pixels.
    fn screen_size(&self) -> (u32, u32);

    /// Primary screen DPI (96.0 is "100%" on Windows).
    fn screen_dpi(&self) -> f64;

    // -- Text ---------------------------------------------------------------

    /// Measure the bounding box of `text` without rasterizing.
    fn measure_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
    ) -> (f32, f32);

    /// Rasterize `text` into an ARGB bitmap.
    fn rasterize_text(
        &self,
        text: &str,
        font_family: &str,
        font_size: f32,
        bold: bool,
        italic: bool,
        color: u32,
    ) -> TextRaster;

    // -- Clipboard ----------------------------------------------------------

    /// Get UTF-8 text from the system clipboard, if any.
    fn clipboard_get_text(&self) -> Option<String>;

    /// Put UTF-8 text onto the system clipboard.
    fn clipboard_set_text(&mut self, text: &str) -> Result<(), PlatformError>;

    // -- Native dialogs -----------------------------------------------------

    /// Open a file-open or file-save dialog.  Returns the selected path.
    fn show_file_dialog(
        &mut self,
        title: &str,
        save: bool,
        filters: &[(String, String)],
    ) -> Option<String>;

    /// Show a modal message dialog (info / warning / error / question).
    fn show_message_dialog(&mut self, title: &str, message: &str, msg_type: MessageDialogType);
}
