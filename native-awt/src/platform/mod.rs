// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Platform-specific windowing backends for AWT.
//!
//! Each target OS has its own backend that implements [`backend::PlatformBackend`].
//! The correct one is selected at compile time via `cfg(target_os)`.

pub mod backend;

#[cfg(target_os = "macos")]
pub mod cocoa;
#[cfg(target_os = "windows")]
pub mod win32;
#[cfg(target_os = "linux")]
pub mod x11;

// Re-export the platform-specific backend as `DefaultBackend`.
#[cfg(target_os = "macos")]
pub use cocoa::CocoaBackend as DefaultBackend;
#[cfg(target_os = "windows")]
pub use win32::Win32Backend as DefaultBackend;
#[cfg(target_os = "linux")]
pub use x11::X11Backend as DefaultBackend;
