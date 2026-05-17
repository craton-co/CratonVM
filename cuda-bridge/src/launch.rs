//! `DeviceModule::launch_on_stream` — stream-aware kernel launch.
//!
//! Phase 2, Item P2-4. Mirrors the existing single-stream
//! [`DeviceModule::launch_raw`] but submits onto an explicit
//! [`crate::Stream`] (Item P2-1) instead of the context's default
//! stream.
//!
//! In stub mode the call records a `StreamOp::Launch { kernel, grid,
//! block }` on the stream and returns `Ok(())`.
//!
//! PHASE2-CUDA-TODO: in `cuda` mode this currently returns
//! `DeviceError::NoDriver`. cudarc 0.13's stream-bound launch path is
//! via `LaunchAsync::launch` on `(CudaFunction, &CudaStream)`;
//! porting requires the `backend_cuda.rs` migration first.

use crate::{DeviceContext, DeviceModule, KernelArgs, LaunchConfig, Result, Stream, StreamOp};

impl DeviceModule {
    /// Submit a kernel launch on a specific stream.
    ///
    /// Behaves like [`DeviceModule::launch_raw`] except the launch is
    /// ordered against `stream` instead of the context's default
    /// stream. Under the stub backend the launch is recorded as a
    /// [`StreamOp::Launch`] on `stream` and `Ok(())` is returned —
    /// downstream tests can inspect the op log without a driver.
    #[cfg_attr(not(feature = "cuda"), allow(unused_variables))]
    pub fn launch_on_stream(
        &self,
        ctx: &DeviceContext,
        kernel: &str,
        cfg: &LaunchConfig,
        args: KernelArgs,
        stream: &Stream,
    ) -> Result<()> {
        #[cfg(not(feature = "cuda"))]
        {
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
            Ok(())
        }

        #[cfg(feature = "cuda")]
        {
            // PHASE2-CUDA-TODO: mirror `backend_cuda::launch_raw` but
            // dispatch on `stream.raw()` once a cudarc 0.13 backend
            // exists. cudarc 0.13's stream-launch path is
            // `<CudaFunction as LaunchAsync<_>>::launch(func, args, cfg)`
            // on `&CudaStream`; the `args` packing must match
            // `KernelArg::DevicePtr(u64)` (tuple variant, see
            // `lib.rs`).
            let _ = (ctx, kernel, cfg, args, stream);
            Err(crate::DeviceError::NoDriver)
        }
    }
}

// ── Tests (stub mode only) ──────────────────────────────────────────
//
// PHASE2-CUDA-TODO: these tests need a stub-mode `DeviceModule`
// fixture and a stub-mode `DeviceContext` fixture that don't exist
// yet. `DeviceModule::from_ptx` and `DeviceContext::new` both return
// `NoDriver` in stub mode, so we cannot drive the call without
// additional test plumbing from a sibling item. The bodies are kept
// as ignored stubs documenting the intended assertions.
#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    use super::*;

    fn sample_cfg() -> LaunchConfig {
        LaunchConfig {
            grid: (4, 1, 1),
            block: (256, 1, 1),
            shared_bytes: 0,
        }
    }

    #[test]
    #[ignore = "PHASE2-CUDA-TODO: needs DeviceModule::for_test() / DeviceContext::for_test() fixtures"]
    fn launch_on_stream_records_kernel_name() {
        let _ = sample_cfg();
    }

    #[test]
    #[ignore = "PHASE2-CUDA-TODO: needs DeviceModule::for_test() / DeviceContext::for_test() fixtures"]
    fn launch_on_stream_records_grid_and_block() {
        let _ = sample_cfg();
    }
}
