// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler integration.
//!
//! Re-exports types from the `cratonvm-jit` crate and provides VM-specific
//! helper functions that are called from JIT-compiled code at runtime.

// Re-export everything from the JIT crate so existing `crate::jit::*` paths work.
pub use cratonvm_jit::*;

pub mod alloc_class_cache;
pub mod code_cache_lifecycle;
pub mod conservative_roots;
pub mod disasm;
pub mod helper_guard;
pub mod helpers;
pub mod xt_root_scan;
