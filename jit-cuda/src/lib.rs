// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Java bytecode → PTX lowering for CratonVM GPU offload.
//!
//! This crate is consumed by the VM to decide whether a static method
//! is GPU-eligible (see [`analyzer`]) and, if so, to emit a PTX text
//! module ([`emitter`]) that the [`cuda-bridge`] crate can load and
//! launch.
//!
//! Scope discipline:
//!
//! - **No `cudarc` / `libcuda` references.** This crate produces
//!   `String` PTX. The runtime lives elsewhere.
//! - **No JVM heap access.** Marshalling is in `vm/runtime/gpu_marshal.rs`.
//! - **No synthetic stubs.** Tests compile real `.java` fixtures from
//!   `../test_classes/gpu/` into `OUT_DIR/gpu-fixtures` and load those
//!   generated `.class` files.

pub mod analyzer;
pub mod annotations;
pub mod emitter;
/// The kernel instruction list.
///
/// **Internal and unstable.** It is `pub` only because a second
/// rendering target lives in a sibling crate and has to read it; it is
/// not a published IR and carries no compatibility promise. Nothing
/// outside this workspace should depend on it, which is why this crate
/// is `publish = false`.
pub mod kir;
pub mod lowering;
pub mod signature;
pub mod target;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod test_support;

pub use analyzer::{analyze, OffloadVerdict, ParamKind, Reason};
pub use annotations::{
    AdmissionHint, ClassAnnotations, EnableAsyncAttrs, GpuExcludeAttrs, GpuKernelAttrs, GridShape,
    MethodAnnotations,
};
pub use emitter::{LoweringError, PtxKernel, PtxModule, PtxParam};
pub use signature::KernelSignature;
pub use target::{
    clamp_target_to_isa, isa_for_target, max_isa_for_cuda_version, min_isa_for_target, IsaVersion,
    SmTarget,
};
