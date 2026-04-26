//! JIT compiler integration.
//!
//! Re-exports types from the `rustjvm-jit` crate and provides VM-specific
//! helper functions that are called from JIT-compiled code at runtime.

// Re-export everything from the JIT crate so existing `crate::jit::*` paths work.
pub use rustjvm_jit::*;

pub mod conservative_roots;
pub mod helpers;
pub mod skip_list;
