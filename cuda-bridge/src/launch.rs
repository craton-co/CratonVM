// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

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
// Both tests drive `DeviceModule::launch_on_stream` against an inert
// stub module produced by `DeviceModule::from_ptx` (which returns
// `Ok` in stub mode) on a `DeviceContext` built via
// `DeviceContext::stub_for_testing`, and assert on the recorded
// `StreamOp::Launch` payload directly.
#[cfg(all(test, not(feature = "cuda")))]
mod tests {
    use super::*;
    use crate::{DeviceContext, DeviceModule, KernelArgs, Stream};

    fn sample_cfg() -> LaunchConfig {
        LaunchConfig {
            grid: (4, 1, 1),
            block: (256, 1, 1),
            shared_bytes: 0,
        }
    }

    #[test]
    fn launch_on_stream_records_kernel_name() {
        let ctx = DeviceContext::stub_for_testing();
        let stream = Stream::new(&ctx).expect("Stream::new stub");
        let module = DeviceModule::from_ptx(&ctx, "", &[]).expect("from_ptx stub");
        let cfg = sample_cfg();
        module
            .launch_on_stream(&ctx, "vector_add", &cfg, KernelArgs::new(), &stream)
            .expect("launch_on_stream stub");
        let ops = stream.ops();
        let kernel = ops
            .iter()
            .find_map(|op| match op {
                StreamOp::Launch { kernel, .. } => Some(kernel.clone()),
                _ => None,
            })
            .expect("expected a Launch op in the stream log");
        assert_eq!(kernel, "vector_add");
    }

    #[test]
    fn launch_on_stream_records_grid_and_block() {
        let ctx = DeviceContext::stub_for_testing();
        let stream = Stream::new(&ctx).expect("Stream::new stub");
        let module = DeviceModule::from_ptx(&ctx, "", &[]).expect("from_ptx stub");
        let cfg = sample_cfg();
        module
            .launch_on_stream(&ctx, "k", &cfg, KernelArgs::new(), &stream)
            .expect("launch_on_stream stub");
        let (grid, block) = stream
            .ops()
            .iter()
            .find_map(|op| match op {
                StreamOp::Launch { grid, block, .. } => Some((*grid, *block)),
                _ => None,
            })
            .expect("expected a Launch op in the stream log");
        assert_eq!(grid, cfg.grid);
        assert_eq!(block, cfg.block);
    }
}
