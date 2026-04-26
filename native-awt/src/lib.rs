//! AWT/Swing/Java2D native peer implementation for RustJVM.
//!
//! This crate provides the native method implementations that back Java's
//! desktop GUI stack: AWT (Abstract Window Toolkit), Swing, and Java2D.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────┐
//! │  Java: javax.swing / java.awt   │
//! ├─────────────────────────────────┤
//! │  sun.awt / sun.java2d (peers)   │
//! ├─────────────────────────────────┤
//! │  native-awt crate (this)        │
//! │  ┌───────────┬────────────────┐ │
//! │  │ platform  │ software       │ │
//! │  │ backend   │ renderer       │ │
//! │  │ (Win32/   │ (pixel buffer  │ │
//! │  │  X11/     │  rasterizer)   │ │
//! │  │  Cocoa)   │                │ │
//! │  └───────────┴────────────────┘ │
//! └─────────────────────────────────┘
//! ```
//!
//! ## Platform backends
//!
//! - **Windows**: Win32 API via the `windows` crate (CreateWindowExW,
//!   GDI for blitting, DirectWrite for text).
//! - **Linux**: X11 via `x11rb`, fontdue for text rasterization.
//! - **macOS**: AppKit via `objc2-app-kit`.
//!
//! ## Software renderer
//!
//! Graphics2D operations are rendered into ARGB pixel buffers. The
//! platform backend is only responsible for blitting the final buffer
//! to the screen. This keeps the rendering logic platform-independent
//! and testable without a display.
//!
//! ## Thread safety
//!
//! All AWT operations must run on the Event Dispatch Thread (EDT).
//! The EDT is a dedicated OS thread that owns the native event loop.
//! Cross-thread access is mediated through `EventQueue.invokeLater`.

#![allow(clippy::collapsible_if)]

pub mod color;
pub mod event;
pub mod font;
pub mod graphics2d;
pub mod image;
pub mod clipboard;
pub mod peer;
pub mod platform;
pub mod renderer;
pub mod swing;
pub mod edt;
pub mod natives;

use rustjvm_native_api::NativeMethodRegistry;

/// Register all AWT/Swing/Java2D native methods with the VM.
pub fn register_awt_natives(registry: &mut NativeMethodRegistry) {
    natives::register_all(registry);
}
