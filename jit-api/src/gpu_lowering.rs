//! The contract every GPU lowering backend must satisfy.
//!
//! The trait exists so the interpreter's offload glue (Part E of the
//! GPU-offload plan) can hold an `Arc<dyn GpuLowering>` and not care
//! whether the PTX came from our own bytecode → PTX emitter or — at
//! some future point — from a Rust-authored helper compiled via
//! cuda-oxide.
//!
//! Today there is exactly one implementor: `jit_cuda::PtxEmitter`.
//! See `docs/gpu/cuda-oxide-evaluation.md` for the reasoning behind
//! keeping the trait but not shipping a second implementor.

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
    /// (e.g. `"jit-cuda PtxEmitter"`). Logged when `--print-gpu-decisions`
    /// is on.
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
