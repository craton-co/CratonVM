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

// JDK-ONLY-CLASSIFY: unknown for 167 of 188, bridge for 21 — the census has
// been taken (L5b, 2026-08-05) and it supersedes the static `javap -p -s` read
// this marker used to carry. This is still the crate's ONLY category call:
// `natives::register_all` is passed as a function pointer, so every
// registration in `natives.rs` inherits `Bridge` from this one line. What
// changed is that 21 of the 188 registration rows no longer *only* inherit it:
// the image declares their target ACC_NATIVE, so they state `Bridge` at their
// own call sites in `natives.rs` and this line is no longer the whole story
// for them.
//
// The other 167 still inherit, and this line stays exactly as it is: the
// `Bridge` tag is what keeps them registered under `CRATONVM_NO_STUBS`, and t7
// desktop conformance depends on them. Do NOT narrow it — that is a
// reclassification, a different wave, and the per-function verdicts in
// `natives.rs` are where it must start.
//
// Two corrections the census forced on the old count of "10 of the 122":
// the real figure is 21 of 188 rows (the count was of registration *sites*,
// and 27 drawing primitives are registered three times over), and
// `sun/awt/PlatformGraphicsInfo.hasDisplays0` — the crate's ONE
// `JDK-ONLY-CLASSIFY: bridge` verdict — **is not in it**. See
// `register_headless_natives` in `natives.rs`.
// See jdk-only-ambient-category-audit.md and
// l5-native-io-bridge-residuals-RETIRED-20260810.md.
/// Register all AWT/Swing/Java2D native methods with the VM.
pub fn register_awt_natives(registry: &mut NativeMethodRegistry) {
    registry.with_category(NativeKind::SyntheticStub, natives::register_all);
}
