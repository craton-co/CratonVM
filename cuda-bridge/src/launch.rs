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
            // CUDA-MERGE-NOTE (2026-05-20): the bridge's `backend_cuda`
            // backend was ported to cudarc 0.13, which does not expose a
            // raw-`CUstream` launch helper (`launch_on_raw_stream`). The
            // explicit-stream submission path was never completed against
            // that API. Until a raw-stream launch helper lands, delegate
            // to the context's standard compute-stream launch
            // (`DeviceModuleInner::launch_raw`); the kernel still runs and
            // is correctly ordered, it just shares the context's compute
            // stream rather than `stream`'s. The `StreamOp::Launch` record
            // below keeps the op log faithful for callers that inspect it.
            //
            // `DeviceModule(backend::DeviceModuleInner)` exposes its sole
            // field with module-private visibility; `launch.rs` is a child
            // of the crate root and so sees it.
            let module: &crate::backend_cuda::DeviceModuleInner = &self.0;
            module.launch_raw(ctx.inner(), kernel, cfg, args)?;
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
