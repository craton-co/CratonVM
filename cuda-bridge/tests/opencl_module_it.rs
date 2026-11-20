// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! End-to-end integration test for the OpenCL backend.
//!
//! Tests program compilation with `clCreateProgramWithSource`, buffer allocation,
//! upload, kernel execution via `clEnqueueNDRangeKernel`, and readback via `clEnqueueReadBuffer`.

#![cfg(feature = "opencl")]

use cratonvm_cuda_bridge::{
    BackendKind, DeviceArch, DeviceBuffer, DeviceContext, KernelArgs, KernelImage, LaunchConfig,
    Stream,
};

const VECTOR_ADD_CL: &str = r#"
__kernel void vector_add(__global int* out, __global const int* a, __global const int* b, int n) {
    int i = get_global_id(0);
    if (i < n) {
        out[i] = a[i] + b[i];
    }
}
"#;

#[test]
fn the_compiled_in_backend_is_opencl() {
    assert_eq!(cratonvm_cuda_bridge::backend_kind(), BackendKind::OpenCl);
}

#[test]
fn opencl_probe_and_execute_vector_add() {
    let caps = match cratonvm_cuda_bridge::probe() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP opencl_probe_and_execute_vector_add: no device ({e})");
            return;
        }
    };

    let DeviceArch::OpenCl(ocl) = &caps.arch else {
        panic!("expected DeviceArch::OpenCl, got {:?}", caps.arch);
    };
    eprintln!(
        "OpenCL device {}: {} ({}), OpenCL {}.{}, compute units {}, max wg {}",
        caps.ordinal,
        caps.name,
        ocl.platform_name,
        ocl.opencl_version.0,
        ocl.opencl_version.1,
        ocl.max_compute_units,
        ocl.max_work_group_size
    );

    let ctx = DeviceContext::new(caps.ordinal).expect("DeviceContext::new");
    let module = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::OpenClSource(VECTOR_ADD_CL),
        &["vector_add"],
    )
    .expect("DeviceModule::from_kernel_image");

    let n = 1024;
    let a: Vec<i32> = (0..n).collect();
    let b: Vec<i32> = (0..n).map(|x| x * 2).collect();
    let mut out: Vec<i32> = vec![0; n as usize];

    let buf_a = DeviceBuffer::from_host(&ctx, &a).expect("buf_a from_host");
    let buf_b = DeviceBuffer::from_host(&ctx, &b).expect("buf_b from_host");
    let buf_out = DeviceBuffer::<i32>::zeros(&ctx, n as usize).expect("buf_out zeros");

    let stream = Stream::new(&ctx).expect("Stream::new");
    let args = KernelArgs::new()
        .push_device_ptr(&buf_out)
        .push_device_ptr(&buf_a)
        .push_device_ptr(&buf_b)
        .push_i32(n);

    let cfg = LaunchConfig::elementwise(n as u32);
    module
        .launch_on_stream(&ctx, "vector_add", &cfg, args, &stream)
        .expect("launch_on_stream");

    stream.synchronize().expect("stream.synchronize");
    buf_out.to_host(&mut out).expect("to_host");

    for i in 0..n as usize {
        assert_eq!(out[i], a[i] + b[i], "mismatch at index {i}");
    }
}
