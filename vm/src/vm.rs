// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! The VM orchestrator — ties together all VM subsystems.
//!
//! The architecture separates shared (thread-safe) state from per-thread state:
//!
//! - [`SharedVm`]: Shared state protected by interior mutability (`RwLock`/`Mutex`).
//!   Holds the class manager, heap, native methods, static fields, and resolution cache.
//! - [`JvmThread`]: Per-thread state including call stack, throwable stacks, and test output.
//! - [`Vm`]: Convenience wrapper combining `Arc<SharedVm>` + `JvmThread` for the main thread.
//!
//! The interpreter and all helper functions take `(shared: &SharedVm, thread: &mut JvmThread)`
//! instead of `(vm: &mut Vm)`.

pub(crate) mod jdk_continuation;
mod native_funnel_census;
pub mod realms;
mod compact_strings_policy;
mod subtype_define;
pub(crate) mod vm_exec;
pub(crate) mod vm_init;
mod vm_launcher;
mod vm_object;
mod vm_util;

pub use vm_exec::*;
pub use vm_init::*;
pub use vm_launcher::*;
pub use vm_object::*;
pub use vm_util::*;
