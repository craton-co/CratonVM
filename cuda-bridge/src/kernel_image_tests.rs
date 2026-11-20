use super::*;

/// The refusal path itself needs a live context to reach, so what
/// is testable without hardware is that the report a refusal
/// carries names the image it was handed. That is the whole value
/// of `UnsupportedImage` over `NoDriver`: it has to be readable
/// enough that nobody mistakes a target-selection bug for a machine
/// without a GPU.
#[test]
fn every_image_variant_names_itself() {
    assert_eq!(KernelImage::Ptx(".version 7.5").kind(), "PTX");
    assert_eq!(
        KernelImage::HipSource {
            source: "extern \"C\" __global__ void k() {}",
            offload_arch: "gfx1100",
        }
        .kind(),
        "HIP source"
    );
    assert_eq!(
        KernelImage::HipCodeObject(&[0x7f, b'E']).kind(),
        "HIP code object"
    );
    assert_eq!(
        KernelImage::OpenClSource("__kernel void k() {}").kind(),
        "OpenCL source"
    );
}

/// `--gpu-info` and the VM's log lines print this, and a build that
/// reported the wrong backend would be actively misleading on a box
/// with two vendors installed.
#[test]
fn the_compiled_in_backend_reports_itself() {
    let kind = backend_kind();
    #[cfg(not(feature = "gpu-driver"))]
    assert_eq!(kind, BackendKind::Stub);
    #[cfg(feature = "cuda")]
    assert_eq!(kind, BackendKind::Cuda);
    #[cfg(feature = "cuda-oxide")]
    assert_eq!(kind, BackendKind::CudaOxide);
    #[cfg(feature = "hip")]
    assert_eq!(kind, BackendKind::Hip);
    #[cfg(feature = "opencl")]
    assert_eq!(kind, BackendKind::OpenCl);
    assert!(!kind.as_str().is_empty());
}

/// A HIP toolchain has no PTX ISA ceiling, and the CUDA-shaped
/// shim has to say so by returning `Err` rather than inventing a
/// number the clamp would then act on.
#[test]
fn the_cuda_version_shim_refuses_to_invent_a_ceiling() {
    let shimmed = driver_cuda_version();
    match toolchain_version() {
        Ok(ToolchainVersion::CudaDriver(v)) => assert_eq!(shimmed.ok(), Some(v)),
        Ok(ToolchainVersion::HipRuntime { .. })
        | Ok(ToolchainVersion::OpenCl { .. })
        | Err(_) => assert!(shimmed.is_err()),
    }
}

#[cfg(not(feature = "gpu-driver"))]
#[test]
fn stub_pooled_async_transfers_release_lease_on_no_driver() {
    let ctx = DeviceContext::for_test();
    let stream = Stream::new(&ctx).expect("stub stream");
    let pool = crate::pinned_pool::PinnedHostPool::with_cap(64 * 1024);
    let host = [1.0f32, 2.0, 3.0, 4.0];

    let err = match DeviceBuffer::from_host_async_pooled(&ctx, &host, &stream, &pool) {
        Err(e) => e,
        Ok(_) => panic!("stub backend should return NoDriver"),
    };
    assert!(matches!(err, DeviceError::NoDriver));
    // Lease was cleanly returned to `pool` on error recovery.
    assert_eq!(pool.stats().ready_buffers, 1);
    assert_eq!(pool.stats().misses, 1);
}
