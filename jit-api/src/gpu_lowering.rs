// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The contract every GPU lowering backend must satisfy.
//!
//! The trait exists so the interpreter's offload glue (Part E of the
//! GPU-offload plan) can hold an `Arc<dyn GpuLowering>` and not care
//! whether the PTX came from our own bytecode → PTX emitter or — at
//! some future point — from a Rust-authored helper compiled via
//! cuda-oxide.
//!
//! Today the workspace has no in-tree implementor of this trait. The
//! `cratonvm-jit-cuda` crate exposes concrete analyzer/lowering functions
//! directly instead of enabling the `jit-api/gpu-lowering` feature. See
//! `docs/gpu/cuda-oxide-evaluation.md` for the reasoning behind keeping this
//! optional seam without making it part of the active GPU path.
//!
//! # Status: no producer, no consumer, and a decision that is overdue
//!
//! Stated as a fact rather than left to be rediscovered. This module is 112
//! lines of `pub` API with:
//!
//! * no implementor anywhere in the workspace (`grep -rn "impl GpuLowering"`
//!   is empty);
//! * no caller — nothing constructs or holds an `Arc<dyn GpuLowering>`;
//! * no build that even compiles it. The `gpu-lowering` feature is off by
//!   default and no workspace crate, workflow or `--features` line enables it,
//!   so `cargo test --workspace` never type-checks this file. Its own tests
//!   exist to give it coverage *when* built, which is never.
//!
//! "Part E of the GPU-offload plan" landed as `cratonvm-jit-cuda`'s concrete
//! entry points, which is the divergence: the real GPU path does not hold an
//! `Arc<dyn GpuLowering>` and has no reason to. The trait is being kept for a
//! cuda-oxide integration whose evaluation is now two years old.
//!
//! **The decision is one of two**, and the cost of not taking it is a public
//! API that reads as a supported extension point:
//!
//! 1. *Wire it.* Implement `GpuLowering` for `cratonvm-jit-cuda`'s existing
//!    `bytecode -> PTX` entry point and have the offload gate hold the trait
//!    object. This also puts the module in the build, which is the part with
//!    real value: an uncompiled file cannot be kept correct.
//! 2. *Delete it.* Remove this module, the `gpu-lowering` feature from
//!    `jit-api/Cargo.toml`, the `#[cfg(feature = "gpu-lowering")] pub mod`
//!    line in `lib.rs`, the two paragraphs about it in that crate's module
//!    doc, and the `docs/gpu/` text that presents it as the pluggable-backend
//!    story.
//!
//! Re-check `docs/gpu/cuda-oxide-evaluation.md` against the two years of
//! divergence before choosing (1). Tracked in `NOTES-runtime.md`.

use crate::CachedBytecodeMethod;

/// Outcome of lowering a single method.
pub struct LoweredKernel {
    /// PTX text suitable for `cuLinkAddData` / `cuModuleLoadData`.
    pub ptx: String,
    /// The PTX entry-point name. Mangled by the implementor; the
    /// caller uses it to resolve the launch handle.
    pub kernel_name: String,
}

// AUDIT 2026-05-16: marked `#[non_exhaustive]` so adding variants
// in future (e.g. `Truncated`, `IntegerOverflow`) is not a breaking
// change for downstream pattern-matching code.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoweringError {
    Unsupported(&'static str),
    Internal(String),
}

impl std::fmt::Display for LoweringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoweringError::Unsupported(s) => write!(f, "unsupported: {s}"),
            LoweringError::Internal(s) => write!(f, "internal error: {s}"),
        }
    }
}

impl std::error::Error for LoweringError {}

/// Producer of GPU kernels from a Java method's metadata.
pub trait GpuLowering: Send + Sync {
    /// A short human-readable name for diagnostics
    /// (e.g. `"custom PTX backend"`). Logged when `--print-gpu-decisions` is
    /// on.
    fn name(&self) -> &'static str;

    /// Lower one method. Producers must return `Err(Unsupported)` for
    /// methods that pass the eligibility analyzer but trip an
    /// unimplemented opcode — the caller falls back to the
    /// interpreter and may blacklist the method to avoid re-trying.
    fn lower(
        &self,
        class_name: &str,
        method: &CachedBytecodeMethod,
    ) -> Result<LoweredKernel, LoweringError>;
}

#[cfg(test)]
mod tests {
    // Round-10 fix [STUB/jit-api]: this module currently has zero
    // consumers in-workspace (the `gpu-lowering` feature is off by
    // default and no other crate enables it — see the crate-level
    // "`gpu-lowering` feature status" note in `lib.rs`). Rather than
    // delete the public seam blindly, exercise the error type's
    // `Display`/`Error` impls so the code is at least covered when the
    // feature *is* built, and a regression in the human-facing messages
    // is caught. These tests only compile/run under `--features
    // gpu-lowering` because the whole module is gated on it.
    use super::*;

    #[test]
    fn lowering_error_display_unsupported() {
        let e = LoweringError::Unsupported("invokedynamic");
        assert_eq!(e.to_string(), "unsupported: invokedynamic");
    }

    #[test]
    fn lowering_error_display_internal() {
        let e = LoweringError::Internal("ptx emit failed".to_string());
        assert_eq!(e.to_string(), "internal error: ptx emit failed");
    }

    #[test]
    fn lowering_error_is_std_error() {
        // Confirm the `std::error::Error` impl is wired (usable as a
        // boxed trait object, and `source()` is the default `None`).
        fn assert_error<E: std::error::Error>(_: &E) {}
        let e = LoweringError::Unsupported("athrow");
        assert_error(&e);
        let boxed: Box<dyn std::error::Error> = Box::new(LoweringError::Internal("x".into()));
        assert_eq!(boxed.to_string(), "internal error: x");
        assert!(std::error::Error::source(&e).is_none());
    }

    #[test]
    fn lowering_error_debug_is_nonempty() {
        // `#[derive(Debug)]` must keep producing something useful for
        // log lines / panics.
        let e = LoweringError::Unsupported("monitorenter");
        assert!(format!("{e:?}").contains("Unsupported"));
    }
}
