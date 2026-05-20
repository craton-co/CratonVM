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
//! - **No synthetic stubs.** Tests load real `.class` files from
//!   `test_classes/gpu/`, compiled by the workspace `build.rs`.

pub mod analyzer;
pub mod emitter;
pub mod lowering;
pub mod signature;

#[cfg(test)]
mod test_support;

pub use analyzer::{analyze, OffloadVerdict, ParamKind, Reason};
pub use emitter::{LoweringError, PtxKernel, PtxModule, PtxParam};
pub use signature::KernelSignature;
