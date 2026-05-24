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
//! In `cuda` mode this calls the same `cuLaunchKernel`-driving helper
//! the default-stream `launch_raw` uses (`backend_cuda::
//! launch_on_raw_stream`), passing `stream.raw()` instead of the
//! device's default stream. The kernel-arg packing rules are
//! identical between the two paths — `KernelArg::DevicePtr(u64)`
//! storage cells fed into a `Vec<*mut c_void>` and submitted via
//! `cudarc::driver::result::launch_kernel`.

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
            // AUDIT 2026-05-24 (C32 stream-port fix): the previous
            // implementation honestly admitted this routed every
            // explicit-stream launch back through the context's
            // *compute* stream, breaking every per-stream contract
            // the README advertised. cudarc 0.13's `LaunchAsync::
            // launch_on_stream` does take a `&CudaStream`, so the
            // user-supplied `Stream`'s inner `Arc<CudaStream>` (via
            // `Stream::cuda_stream_arc()`) can be passed straight
            // through. `backend_cuda::DeviceModuleInner::
            // launch_raw_on_stream` is the new entry point that
            // does the marshalling + launch on a caller-supplied
            // cudarc stream rather than `&ctx.compute`.
            //
            // `DeviceModule(backend::DeviceModuleInner)` exposes its
            // sole field with module-private visibility; `launch.rs`
            // is a child of the crate root and so sees it.
            let module: &crate::backend_cuda::DeviceModuleInner = &self.0;
            let cuda_stream = stream.cuda_stream_arc();
            module.launch_raw_on_stream(ctx.inner(), cuda_stream, kernel, cfg, args)?;
            stream.record_op(StreamOp::Launch {
                kernel: kernel.to_string(),
                grid: cfg.grid,
                block: cfg.block,
            });
            Ok(())
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
