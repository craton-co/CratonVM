// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The HIP backend against a real AMD device, end to end through this
//! crate's public types.
//!
//! # What this is for
//!
//! `hip_no_driver_it.rs` covers the only HIP property public CI can
//! check. Everything else about this backend — that hiprtc compiles,
//! that a code object loads, that a kernel launches with the arguments
//! it was given, that the result comes back — needs an AMD GPU, and
//! this project has none. So this file is the thing to run FIRST on the
//! first AMD box, before any benchmark: it is the shortest path from
//! "the code compiles" to "the code works".
//!
//! Run it with:
//!
//! ```text
//! cargo test -p cratonvm-cuda-bridge --features hip,gpu-it -- --ignored
//! ```
//!
//! # Why the kernels are written here rather than lowered
//!
//! This tests the HOST half. A lowering bug and a host bug produce the
//! same symptom — wrong numbers — and a test that exercises both at
//! once cannot tell them apart. The kernels below are therefore
//! hand-written, trivially correct by inspection, and chosen so each
//! one isolates one host-side contract.

#![cfg(all(feature = "hip", feature = "gpu-it"))]

use cratonvm_cuda_bridge::{
    DeviceArch, DeviceBuffer, DeviceContext, DeviceError, KernelArgs, KernelImage, LaunchConfig,
    Stream,
};

/// `out[i] = in[i] * k + i`, over a guarded 1-D grid.
///
/// Deliberately reads an array, a scalar and the thread index, and
/// writes an array: that covers every `KernelArg` variant the offload
/// path produces except `I64`/`F64`, which `SCALE_WIDE` adds.
///
/// `extern "C"` is not optional. A `__global__` function with C++
/// linkage gets a mangled symbol, and `hipModuleGetFunction` resolves
/// by name — so without it the load fails with "kernel not found" on a
/// kernel that is plainly there.
const SCALE_SRC: &str = r#"
extern "C" __global__ void scale(int* out, const int* in, int n, int k) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        out[i] = in[i] * k + i;
    }
}
"#;

/// The 64-bit and double arms of the argument ABI, plus an `atomicAdd`.
///
/// The atomic is here because `docs/gpu/reductions.md`'s epilogue
/// depends on it and because 64-bit global atomics are a capability the
/// probe reports — a device that lacks them should fail this loudly
/// rather than silently returning a partial sum.
const WIDE_SRC: &str = r#"
extern "C" __global__ void wide(long long* out, double* dout, long long m, double d, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        out[i] = m + (long long) i;
        dout[i] = d * (double) i;
    }
}
extern "C" __global__ void sum_into(unsigned long long* acc, const int* in, int n) {
    int i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i < n) {
        atomicAdd(acc, (unsigned long long) in[i]);
    }
}
"#;

/// The `gfx` string to compile for, or a skip.
///
/// Read from the device rather than hardcoded: a code object is built
/// for one architecture and does not run on another, so a literal here
/// would make this test pass on exactly one card.
fn arch_or_skip(what: &str) -> Option<(DeviceContext, String, u32)> {
    let caps = match cratonvm_cuda_bridge::probe() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP {what}: no device ({e})");
            return None;
        }
    };
    let DeviceArch::Hip(hip) = &caps.arch else {
        eprintln!("SKIP {what}: device is not a HIP device ({:?})", caps.arch);
        return None;
    };
    eprintln!(
        "device {}: {} ({}), wave {}, fp64={} int64-atomics={}",
        caps.ordinal,
        caps.name,
        hip.gcn_arch_name,
        hip.warp_size,
        hip.fp64,
        hip.global_int64_atomics
    );
    let ctx = match DeviceContext::new(caps.ordinal) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP {what}: context ({e})");
            return None;
        }
    };
    Some((ctx, hip.gcn_arch_name.clone(), hip.warp_size))
}

#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn hiprtc_compiles_and_the_kernel_runs() {
    let Some((ctx, arch, _wave)) = arch_or_skip("hiprtc_compiles_and_the_kernel_runs") else {
        return;
    };

    let module = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::HipSource {
            source: SCALE_SRC,
            offload_arch: &arch,
        },
        &["scale"],
    )
    .expect("hiprtc should compile a kernel this simple; the error carries its build log");

    const N: usize = 4096;
    let input: Vec<i32> = (0..N as i32).collect();
    let src = DeviceBuffer::from_host(&ctx, &input).expect("upload");
    let dst = DeviceBuffer::<i32>::zeros(&ctx, N).expect("alloc");
    let stream = Stream::new(&ctx).expect("stream");

    let args = KernelArgs::new()
        .push_device_ptr(&dst)
        .push_device_ptr(&src)
        .push_i32(N as i32)
        .push_i32(3);

    let cfg = LaunchConfig::elementwise(N as u32);
    module
        .launch_on_stream(&ctx, "scale", &cfg, args, &stream)
        .expect("launch");
    stream.synchronize().expect("drain");

    let mut out = vec![0i32; N];
    dst.to_host(&mut out).expect("download");
    for (i, got) in out.iter().enumerate() {
        let want = input[i] * 3 + i as i32;
        assert_eq!(
            *got, want,
            "scale[{i}]: host marshalling or launch geometry is wrong"
        );
    }
}

/// A kernel that is not `extern "C"` must fail with an error that says
/// so.
///
/// This is the mistake a HIP renderer is most likely to make, it
/// produces a "kernel not found" on a kernel that is visibly present,
/// and the wrong diagnostic would cost an afternoon.
#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn a_mangled_kernel_name_says_why_it_cannot_be_found() {
    let Some((ctx, arch, _)) = arch_or_skip("a_mangled_kernel_name_says_why_it_cannot_be_found")
    else {
        return;
    };
    let mangled = "__global__ void notexterncee(int* p) { *p = 1; }";
    let err = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::HipSource {
            source: mangled,
            offload_arch: &arch,
        },
        &["notexterncee"],
    );
    let err = match err {
        Ok(_) => panic!("a C++-linkage kernel must not resolve by its source name"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("extern") && msg.contains("mangled"),
        "the error should name the cause, not just report absence: {msg}"
    );
}

/// hiprtc's build log has to reach the caller. In-process compilation
/// is the one thing this backend does better than the CUDA path, and it
/// is worth nothing if the diagnostic is dropped.
#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn a_compile_error_carries_the_hiprtc_log() {
    let Some((ctx, arch, _)) = arch_or_skip("a_compile_error_carries_the_hiprtc_log") else {
        return;
    };
    let broken = "extern \"C\" __global__ void k(int* p) { this is not c++ }";
    let err = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::HipSource {
            source: broken,
            offload_arch: &arch,
        },
        &["k"],
    );
    let err = match err {
        Ok(_) => panic!("this source does not compile and must not load"),
        Err(e) => e,
    };
    assert!(matches!(err, DeviceError::Load(_)), "got {err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("error"),
        "the hiprtc build log should be in the message: {msg}"
    );
}

/// PTX handed to HIP must be refused as the wrong image, NOT reported
/// as a missing device.
///
/// The distinction is the whole reason `UnsupportedImage` exists: a
/// `NoDriver` here would send the VM down its correct-but-slow CPU path
/// and hide a target-selection bug behind right answers.
#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn ptx_is_refused_as_the_wrong_image_not_as_a_missing_device() {
    let Some((ctx, _, _)) = arch_or_skip("ptx_is_refused_as_the_wrong_image") else {
        return;
    };
    let err = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::Ptx(".version 7.5\n.target sm_75\n.address_size 64\n"),
        &["anything"],
    );
    let err = match err {
        Ok(_) => panic!("the HIP backend must not load PTX"),
        Err(e) => e,
    };
    assert!(
        matches!(err, DeviceError::UnsupportedImage(_)),
        "must be UnsupportedImage, not {err:?} -- NoDriver here would look \
         like a machine with no GPU"
    );
}

/// The 64-bit and double argument arms, and a 64-bit global atomic.
#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn wide_arguments_and_atomics_round_trip() {
    let Some((ctx, arch, _)) = arch_or_skip("wide_arguments_and_atomics_round_trip") else {
        return;
    };
    let module = cratonvm_cuda_bridge::DeviceModule::from_kernel_image(
        &ctx,
        &KernelImage::HipSource {
            source: WIDE_SRC,
            offload_arch: &arch,
        },
        &["wide", "sum_into"],
    )
    .expect("compile");

    const N: usize = 1024;
    let stream = Stream::new(&ctx).expect("stream");
    let lout = DeviceBuffer::<i64>::zeros(&ctx, N).expect("alloc i64");
    let dout = DeviceBuffer::<f64>::zeros(&ctx, N).expect("alloc f64");

    let args = KernelArgs::new()
        .push_device_ptr(&lout)
        .push_device_ptr(&dout)
        .push_i64(1_000_000_000_000)
        .push_f64(0.5)
        .push_i32(N as i32);
    module
        .launch_on_stream(
            &ctx,
            "wide",
            &LaunchConfig::elementwise(N as u32),
            args,
            &stream,
        )
        .expect("launch wide");
    stream.synchronize().expect("drain");

    let mut lhost = vec![0i64; N];
    lout.to_host(&mut lhost).expect("download i64");
    let mut dhost = vec![0f64; N];
    dout.to_host(&mut dhost).expect("download f64");
    for i in 0..N {
        assert_eq!(lhost[i], 1_000_000_000_000 + i as i64, "i64 arg at {i}");
        // Exact: 0.5 and a small integer are both exactly
        // representable, and their product is too. An inexact result
        // here would mean the compile options let something reassociate.
        assert_eq!(dhost[i], 0.5 * i as f64, "f64 arg at {i}");
    }

    // The atomic arm. Sum of 0..N-1, accumulated by N threads racing.
    let input: Vec<i32> = (0..N as i32).collect();
    let src = DeviceBuffer::from_host(&ctx, &input).expect("upload");
    let acc = DeviceBuffer::<i64>::zeros(&ctx, 1).expect("alloc acc");
    let args = KernelArgs::new()
        .push_device_ptr(&acc)
        .push_device_ptr(&src)
        .push_i32(N as i32);
    module
        .launch_on_stream(
            &ctx,
            "sum_into",
            &LaunchConfig::elementwise(N as u32),
            args,
            &stream,
        )
        .expect("launch sum_into");
    stream.synchronize().expect("drain");
    let mut got = vec![0i64; 1];
    acc.to_host(&mut got).expect("download acc");
    let want: i64 = (0..N as i64).sum();
    assert_eq!(got[0], want, "64-bit global atomicAdd did not accumulate");
}

/// The wavefront width the device reports is 32 or 64, and nothing
/// else.
///
/// Not a formality. A reduction folded for the wrong width returns a
/// plausible number that is wrong by half — it does not crash, does not
/// trip the failure flag and does not deopt. Every lowering decision
/// downstream reads this number, so a third value arriving from a
/// future architecture must fail here rather than silently pick a
/// branch.
#[test]
#[ignore = "requires an AMD GPU and a ROCm install; enable feature `gpu-it`"]
fn the_wavefront_width_is_one_this_workspace_knows() {
    let Some((_ctx, arch, wave)) = arch_or_skip("the_wavefront_width_is_one_this_workspace_knows")
    else {
        return;
    };
    assert!(
        wave == 32 || wave == 64,
        "{arch} reports warpSize {wave}; every lowering decision that folds a \
         wavefront assumes 32 or 64, and a third value must be handled \
         explicitly rather than defaulted"
    );
}
