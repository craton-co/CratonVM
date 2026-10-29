// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Native method support.
//!
//! Provides a registry for native method implementations and built-in natives
//! for core JDK classes (System, Object, Class, Throwable, etc.).

pub mod builtins;
pub mod collections;
pub mod ffi;
pub mod io;
pub mod jni;
pub mod registry;
pub use builtins::register_essential_natives;
// T2: register_builtins and register_collections_natives are needed
// in BOTH synthetic-jdk and real-JDK modes — native methods are real
// Rust implementations, not synthetic stubs. They provide the ACC_NATIVE
// method bodies that even real JDK classes delegate to.
pub use builtins::{register_builtins, register_synthetic_overrides};
pub use collections::register_collections_natives;
pub use io::register_io_natives;
pub use registry::{NativeCallback, NativeContext, NativeMethodRegistry, StackTraceEntry};
