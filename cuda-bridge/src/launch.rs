//! `DeviceModule::launch_on_stream` — stream-aware kernel launch.
//!
//! Phase 2, Item P2-4. Mirrors the existing single-stream
//! [`DeviceModule::launch_raw`] but submits onto an explicit
//! [`crate::Stream`] (Item P2-1) instead of the context's default
//! stream.
//!
//! In stub mode the call records a `StreamOp::Launch { kernel, grid,
//! block }` on the stream and returns `Ok(())`. In `cuda` mode it
//! replicates the `launch_raw` body verbatim except for the launch
//! builder, which is constructed from `stream.raw()` rather than
//! `ctx.0.stream`.

use crate::{DeviceContext, DeviceError, DeviceModule, KernelArgs, LaunchConfig, Result, Stream, StreamOp};

#[cfg(feature = "cuda")]
use crate::KernelArg;
#[cfg(feature = "cuda")]
use cudarc::driver::{LaunchConfig as CudarcLaunchConfig, PushKernelArg};

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
            // Mirror of `launch_raw` in `backend_cuda.rs`. The only
            // semantic difference is the launch builder: we build it
            // from `stream.raw()` (the cudarc `CudaStream` behind the
            // Phase-2 `Stream` wrapper) rather than `ctx.0.stream`.
            // See the CRIT-1 audit notes in `lib.rs` /
            // `backend_cuda.rs` for why `args` is held by value across
            // the entire body — the `SyncRecord` guards inside
            // `KernelArg::DevicePtr` must outlive `builder.launch(..)`.
            let inner = &self.0;
            let func = inner
                .functions
                .get(kernel)
                .ok_or_else(|| DeviceError::KernelNotFound(kernel.to_string()))?;
            let cudarc_cfg = CudarcLaunchConfig {
                grid_dim: cfg.grid,
                block_dim: cfg.block,
                shared_mem_bytes: cfg.shared_bytes,
            };
            let cuda_stream = stream.raw();
            let mut builder = cuda_stream.launch_builder(func);
            // Hold `args` by value for the whole body — see audit note.
            let args = args;
            let mut ptr_h: Vec<u64> = Vec::new();
            for a in &args.raw {
                if let KernelArg::DevicePtr { addr, .. } = a {
                    ptr_h.push(*addr);
                }
            }
            let mut ip = 0usize;
            for a in &args.raw {
                match a {
                    KernelArg::I32(v) => {
                        builder.arg(v);
                    }
                    KernelArg::I64(v) => {
                        builder.arg(v);
                    }
                    KernelArg::F32(v) => {
                        builder.arg(v);
                    }
                    KernelArg::F64(v) => {
                        builder.arg(v);
                    }
                    KernelArg::DevicePtr { .. } => {
                        builder.arg(&ptr_h[ip]);
                        ip += 1;
                    }
                }
            }
            unsafe {
                builder
                    .launch(cudarc_cfg)
                    .map_err(|e| DeviceError::Launch(format!("kernel launch: {e}")))?;
            }
            // Explicit late-drop site — mirror of `launch_raw`.
            drop(ptr_h);
            drop(args);
            Ok(())
        }
    }
}

// ── Tests (stub mode only) ──────────────────────────────────────────
//
// PHASE2-GUESS: these tests need a `DeviceModule` value to call
// `launch_on_stream` on. In stub mode `DeviceModule` wraps a
// `backend_stub::DeviceModuleInner` (zero-sized), and the only public
// constructor is `DeviceModule::from_ptx`, which returns
// `Err(DeviceError::NoDriver)` without a real driver. There is no
// public test helper to build a stub `DeviceModule` directly, so the
// tests are marked `#[ignore]` until Item P2-1 (or a follow-up)
// exposes a `DeviceModule::for_test()` or equivalent fixture. The body
// shows the intended assertions and works once such a helper exists.
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

    // PHASE2-GUESS: depends on a stub-mode `DeviceModule` fixture and
    // a stub-mode `DeviceContext` fixture that don't exist yet.
    // `DeviceModule::from_ptx` and `DeviceContext::new` both return
    // `NoDriver` in stub mode, so we cannot drive the call without
    // additional test plumbing from a sibling item.
    #[test]
    #[ignore = "PHASE2-GUESS: needs DeviceModule::for_test() / DeviceContext::for_test() fixtures"]
    fn launch_on_stream_records_kernel_name() {
        // Pseudocode for when the fixtures land:
        //
        //   let ctx = DeviceContext::for_test();
        //   let module = DeviceModule::for_test();
        //   let stream = Stream::for_test();
        //   module
        //       .launch_on_stream(&ctx, "saxpy", &sample_cfg(), KernelArgs::new(), &stream)
        //       .unwrap();
        //   let ops = stream.recorded_ops();
        //   assert!(matches!(
        //       ops.as_slice(),
        //       [StreamOp::Launch { kernel, .. }] if kernel == "saxpy"
        //   ));
        let _ = sample_cfg();
    }

    // PHASE2-GUESS: same fixture gap as above.
    #[test]
    #[ignore = "PHASE2-GUESS: needs DeviceModule::for_test() / DeviceContext::for_test() fixtures"]
    fn launch_on_stream_records_grid_and_block() {
        // Pseudocode for when the fixtures land:
        //
        //   let ctx = DeviceContext::for_test();
        //   let module = DeviceModule::for_test();
        //   let stream = Stream::for_test();
        //   let cfg = sample_cfg();
        //   module
        //       .launch_on_stream(&ctx, "k", &cfg, KernelArgs::new(), &stream)
        //       .unwrap();
        //   let ops = stream.recorded_ops();
        //   match ops.as_slice() {
        //       [StreamOp::Launch { grid, block, .. }] => {
        //           assert_eq!(*grid, (4, 1, 1));
        //           assert_eq!(*block, (256, 1, 1));
        //       }
        //       other => panic!("expected one Launch op, got {other:?}"),
        //   }
        let _ = sample_cfg();
    }
}
