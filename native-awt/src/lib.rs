// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! AWT/Swing/Java2D native peer implementation for CratonVM.
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
//! │  │ (scaffold)│ (pixel buffer  │ │
//! │  │           │  rasterizer)   │ │
//! │  └───────────┴────────────────┘ │
//! └─────────────────────────────────┘
//! ```
//!
//! ## Runtime mode: headless-only
//!
//! The current native-method surface (see [`natives::register_all`]) operates
//! in **headless-only** mode: `Graphics2D` rasterizes into in-memory buffers,
//! and `EventQueue` can synthesize invocation, mouse, key, window, and paint
//! events for Java listeners. The platform backends are still scaffold-only,
//! so `Frame.setVisible(true)` does not open an on-screen window today.
//!
//! ## Platform backends (scaffolded, not yet wired)
//!
//! The `platform/` module ships scaffold backends for the three target OSes:
//!
//! - **Windows** (`platform::win32::Win32Backend`): Win32 API via the
//!   `windows` crate, GDI for blitting, DirectWrite for text.
//! - **Linux** (`platform::x11::X11Backend`): X11 via `x11rb`, fontdue for
//!   glyph rasterization.
//! - **macOS** (`platform::cocoa::CocoaBackend`): AppKit via `objc2-app-kit`,
//!   fontdue for glyph rasterization.
//!
//! These backends compile and have their own unit-test coverage, but they
//! are **not instantiated from `natives.rs`** — `Frame.setVisible(true)`
//! does not open an on-screen window today. Wiring the backends through
//! the natives layer (and synthesising Java `AWTEvent` objects in
//! `EventQueue.getNextEvent`) is tracked as future work. Until that lands,
//! treat the backend modules as a forward-looking API surface — not as live
//! display infrastructure.
//!
//! ## Software renderer
//!
//! Graphics2D operations are rendered into ARGB pixel buffers. When the
//! platform backends are wired up, they will be responsible only for
//! blitting the final buffer to the screen — the rendering logic stays
//! platform-independent and testable without a display.
//!
//! ## Thread safety
//!
//! All AWT operations must run on the Event Dispatch Thread (EDT).
//! The EDT is a dedicated OS thread that owns the native event loop.
//! Cross-thread access is mediated through `EventQueue.invokeLater`.

#![allow(clippy::collapsible_if)]
#![deny(
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::undocumented_unsafe_blocks
)]

pub mod clipboard;
pub mod color;
pub mod edt;
pub mod event;
pub mod font;
pub mod graphics2d;
pub mod image;
pub mod natives;
pub mod peer;
pub mod platform;
pub mod renderer;
pub mod swing;

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};

// JDK-ONLY-CLASSIFY: unknown — needs census. This is the crate's ONLY category
// call: `natives::register_all` is passed as a function pointer, so all 122
// registrations in `natives.rs` inherit `Bridge` from this one line without any
// per-site judgement. Measured against JDK 25 (`javap -p -s`), only 10 of the
// 122 target an ACC_NATIVE method (`Toolkit.initIDs`, `Disposer.initIDs`,
// `PlatformGraphicsInfo.hasDisplays0`, the `JPEGImageReader`/`JPEGImageWriter`
// family); 74 target methods with concrete bytecode, 4 are abstract, 4 do not
// exist in the image. Per-function verdicts are annotated in `natives.rs`. Do
// NOT narrow this call before the runtime census: the `Bridge` tag is what
// keeps these registered under `CRATONVM_NO_STUBS` today, and t7 desktop
// conformance depends on them. See docs/jdk-only-ambient-category-audit.md.
/// Register all AWT/Swing/Java2D native methods with the VM.
pub fn register_awt_natives(registry: &mut NativeMethodRegistry) {
    registry.with_category(NativeKind::Bridge, natives::register_all);
}
