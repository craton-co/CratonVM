// SPDX-License-Identifier: Apache-2.0
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

pub mod realms;
pub(crate) mod vm_exec;
pub(crate) mod vm_init;
mod vm_object;
mod vm_util;

pub use vm_exec::*;
pub use vm_init::*;
pub use vm_object::*;
pub use vm_util::*;

// Types used by the inline `mod tests` below. NEW-11: the inline test
// module is synthetic-jdk specific (it calls `register_builtins`,
// asserts synthetic counts, and exercises hand-allocated synthetics),
// so it — and its supporting `use` block — are gated behind
// `#[cfg(all(test, feature = "synthetic-jdk"))]`. The external test
// files in `vm/tests/` remain available in both feature modes.
//
// JDK-ONLY-LAYOUT: safe (whole file) — this module contains ~500 raw
// `heap.get_field(obj, <literal>)` / `set_field(obj, <literal>, …)` calls, and
// every one of them is inside the `#[cfg(all(test, feature = "synthetic-jdk"))]`
// block below. They hand-build synthetic objects and then assert on the slots
// they themselves wrote, so they are self-consistent by construction and never
// observe a real JDK layout: `synthetic-jdk` is a build-time feature that
// excludes the real class library, whereas `--jdk-only` is a *runtime* policy
// on a real image (contract §1: "Strictness is a runtime policy, not a build
// feature"). Nothing here is reachable from a `--jdk-only` run.
//
// This is a verdict about reachability, not about quality: several of these
// fixtures encode layouts (`Throwable` message at slot 0, `StringBuilder`
// count at slot 1) that are WRONG for real JDK bytes. They are safe only
// because they never meet them. Do not copy a slot number out of this module
// into production code, and do not treat a green run of these tests as
// evidence that a native's slot arithmetic survives stub removal.
// See `jdk-only-object-layout-audit.md`.

// NEW-11: the inline test module is synthetic-jdk specific (it calls
// `register_builtins`, asserts synthetic counts, and exercises hand-allocated
// synthetics), so it is gated behind `#[cfg(all(test, feature =
// "synthetic-jdk"))]`. The external test files in `vm/tests/` remain available
// in both feature modes. It lives in `vm/tests.rs` rather than inline: it was
// 72,443 of this file's 76,799 lines, i.e. the production orchestrator was 6%
// of its own file.
#[cfg(all(test, feature = "synthetic-jdk"))]
mod tests;
