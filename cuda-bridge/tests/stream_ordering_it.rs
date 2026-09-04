// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The cross-stream ordering contract, checked against a device.
//!
//! # Why this file exists
//!
//! `README.md` makes a strong claim about the async pipeline:
//!
//! > The bridge installs an upload-completion event on the buffer when
//! > `from_host_async` returns; subsequent `launch_on_stream` calls that
//! > consume the buffer issue `cuStreamWaitEvent` against that event on
//! > the user stream before launching the kernel, then record a
//! > kernel-completion event and stash it as the buffer's new
//! > last-write. A subsequent `to_host_async` waits on that kernel event
//! > before issuing the D→H copy. The H→D / kernel / D→H pipeline thus
//! > has no cross-stream races even though the cuda backend internally
//! > fans out to three separate cudarc streams.
//!
//! Until 2026-09-02 that was verified only by `tests/stub_op_log.rs`,
//! which is `#![cfg(not(feature = "cuda"))]` — it inspects an operation
//! log the real backend does not keep. So did every test in `stream.rs`,
//! `event.rs` and `launch.rs`: all three modules are
//! `#[cfg(all(test, not(feature = "cuda")))]`. Enabling the `cuda`
//! feature took the crate from 39 unit tests to 16 and from 6
//! integration tests to 1.
//!
//! A model is worth testing against, and a model is not the thing. A
//! missing `cuStreamWaitEvent` is invisible to an op-log assertion that
//! was written from the same understanding as the code, and on hardware
//! it is a race — wrong data, intermittently, under load. These tests
//! are shaped to make that race manifest rather than to describe it:
//! large transfers so the window is wide, values chosen so a stale read
//! is arithmetically distinguishable from a fresh one, and enough
//! repetitions that an occasional win is not mistaken for correctness.
//!
//! Requires the `cuda` feature and a real device.

//! # These tests were verified to be able to fail
//!
//! A passing ordering test proves nothing on its own — the hazard it
//! guards against is a race, and a race that loses is indistinguishable
//! from correctness. So the guard was removed and they were run again,
//! by short-circuiting the `last_write` wait loop in
//! `launch.rs::launch_on_stream`:
//!
//! ```text
//!   an_upload_on_one_stream_orders_a_kernel_on_another    FAILED
//!   a_buffer_written_on_one_stream_orders_a_reader_on_another  FAILED
//!   async_pipeline_on_one_stream_never_reads_a_stale_buffer    FAILED (intermittently)
//! ```
//!
//! The two cross-stream tests fail deterministically. The single-stream
//! one fails intermittently — `got 0, want 269` at round 19 of 24 on the
//! run that caught it — which is what a race looks like and is why it
//! runs 24 rounds rather than one.
//!
//! Two earlier versions of these tests passed WITH the guard removed and
//! were rewritten:
//!
//! * one put the upload and the launch on the same stream, which is FIFO
//!   and therefore ordered whether or not anything asks;
//! * one added an explicit `Event` alongside the buffer choreography, so
//!   either mechanism alone sufficed and neither was under test.
//!
//! Both are the same mistake — an assertion that cannot distinguish the
//! mechanism from its surroundings — and neither was visible without
//! running the control.
//!
//! # And then they found something
//!
//! Run under `cargo test`'s thread pool rather than one at a time, these
//! failed 4 of 6 with `got 0`. That was not a flaw in them: it was
//! `DeviceBuffer::zeros` handing back a buffer whose asynchronous
//! zeroing memset carried no `last_write` marker, so the memset could
//! land after the kernel that wrote the buffer. See
//! `concurrent_dispatch_it.rs`, which isolates the shape and is the
//! regression test.

#![cfg(feature = "gpu-driver")]

use cratonvm_cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, KernelArgs, LaunchConfig, Stream,
};

/// `out[i] = in[i] * 2 + 1` over `n` elements.
///
/// The transform matters: a kernel that read a stale (zeroed) input
/// would write `1` everywhere, which is distinguishable from every
/// legitimate output because the inputs are non-zero. A plain copy
/// kernel would make a stale read look like a plausible answer.
const PTX: &str = r#"
.version 7.5
.target sm_75
.address_size 64

.visible .entry twiceplus(
    .param .u64 src,
    .param .u64 dst,
    .param .s32 n
) {
    .reg .u32 %ru<4>;
    .reg .s32 %r<6>;
    .reg .u64 %rd<5>;
    .reg .pred %p<2>;

    ld.param.u64 %rd0, [src];
    ld.param.u64 %rd1, [dst];
    ld.param.s32 %r0, [n];
    mov.u32 %ru0, %ctaid.x;
    mov.u32 %ru1, %ntid.x;
    mov.u32 %ru2, %tid.x;
    mad.lo.u32 %ru3, %ru0, %ru1, %ru2;
    mov.b32 %r1, %ru3;
    setp.ge.u32 %p0, %r1, %r0;
    @%p0 bra L_done;
    mad.wide.s32 %rd2, %r1, 4, %rd0;
    ld.global.s32 %r2, [%rd2];
    add.s32 %r3, %r2, %r2;
    add.s32 %r4, %r3, 1;
    mad.wide.s32 %rd3, %r1, 4, %rd1;
    st.global.s32 [%rd3], %r4;
L_done:
    ret;
}
"#;

fn ctx_or_skip(what: &str) -> Option<DeviceContext> {
    match DeviceContext::new(0) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("SKIP {what}: {e}");
            None
        }
    }
}

/// Big enough that the H→D copy is still in flight when the launch is
/// queued if nothing orders them. At 4 MiB the upload takes on the order
/// of a millisecond, which is an enormous window for a kernel launch.
const N: usize = 1024 * 1024;

/// upload → launch → download, all through the async API on one user
/// stream, with the backend fanning out to its own copy/compute streams
/// underneath. Every element checked, every round.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn async_pipeline_on_one_stream_never_reads_a_stale_buffer() {
    let Some(ctx) = ctx_or_skip("async_pipeline_on_one_stream_never_reads_a_stale_buffer") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["twiceplus"]).expect("module");
    let stream = Stream::new(&ctx).expect("stream");

    // Rounds, not one shot: a race that loses nine times in ten is
    // still a race, and a single green run would have hidden it.
    const ROUNDS: usize = 24;
    for round in 0..ROUNDS {
        // Distinct values per round, so a buffer left over from the
        // previous round is not mistaken for this round's answer.
        let base = (round as i32) * 7 + 1;
        let host: Vec<i32> = (0..N).map(|i| base + (i as i32 % 4096)).collect();

        let src = DeviceBuffer::from_host_async(&ctx, &host, &stream).expect("upload");
        let dst = DeviceBuffer::<i32>::zeros(&ctx, N).expect("dst");

        let cfg = LaunchConfig::elementwise(N as u32);
        let args = KernelArgs::new()
            .push_device_ptr(&src)
            .push_device_ptr(&dst)
            .push_i32(N as i32);
        module
            .launch_on_stream(&ctx, "twiceplus", &cfg, args, &stream)
            .expect("launch");

        let mut back = vec![0i32; N];
        dst.to_host_async(&mut back, &stream).expect("download");
        stream.synchronize().expect("sync");

        for (i, (&got, &src_v)) in back.iter().zip(host.iter()).enumerate() {
            let want = src_v * 2 + 1;
            assert_eq!(
                got, want,
                "round {round} element {i}: got {got}, want {want}. \
                 A `1` here means the kernel read a zeroed buffer — the \
                 launch did not wait for the upload. An unchanged 0 means \
                 the download did not wait for the kernel."
            );
        }
    }
}

/// Two user streams, chained through a buffer and nothing else.
///
/// Stage 1 writes `b` on `s1`; stage 2 reads `b` on `s2`. There is no
/// explicit `Event` here on purpose — the only thing that can order them
/// is the kernel-completion event `launch_on_stream` stashes as the
/// buffer's last-write, and the wait the next launch issues against it.
///
/// The first version of this test also recorded an `Event` by hand and
/// waited on it, which made it pass with the buffer choreography
/// disabled: two mechanisms, either sufficient, so neither tested. The
/// negative control is what found that.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn a_buffer_written_on_one_stream_orders_a_reader_on_another() {
    let Some(ctx) = ctx_or_skip("a_buffer_written_on_one_stream_orders_a_reader_on_another") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["twiceplus"]).expect("module");
    let s1 = Stream::new(&ctx).expect("s1");
    let s2 = Stream::new(&ctx).expect("s2");

    const ROUNDS: usize = 24;
    for round in 0..ROUNDS {
        let base = (round as i32) * 13 + 2;
        let host: Vec<i32> = (0..N).map(|i| base + (i as i32 % 4096)).collect();

        let a = DeviceBuffer::from_host_async(&ctx, &host, &s1).expect("upload");
        let b = DeviceBuffer::<i32>::zeros(&ctx, N).expect("b");
        let c = DeviceBuffer::<i32>::zeros(&ctx, N).expect("c");
        let cfg = LaunchConfig::elementwise(N as u32);

        // stage 1 on s1: b = a*2 + 1
        module
            .launch_on_stream(
                &ctx,
                "twiceplus",
                &cfg,
                KernelArgs::new()
                    .push_device_ptr(&a)
                    .push_device_ptr(&b)
                    .push_i32(N as i32),
                &s1,
            )
            .expect("stage 1");

        // stage 2 on s2: c = b*2 + 1 = a*4 + 3
        module
            .launch_on_stream(
                &ctx,
                "twiceplus",
                &cfg,
                KernelArgs::new()
                    .push_device_ptr(&b)
                    .push_device_ptr(&c)
                    .push_i32(N as i32),
                &s2,
            )
            .expect("stage 2");

        let mut back = vec![0i32; N];
        c.to_host_async(&mut back, &s2).expect("download");
        s2.synchronize().expect("sync s2");
        s1.synchronize().expect("sync s1");

        for (i, (&got, &src_v)) in back.iter().zip(host.iter()).enumerate() {
            let want = src_v * 4 + 3;
            assert_eq!(
                got, want,
                "round {round} element {i}: got {got}, want {want}. A 1 means \
                 stage 2 read a `b` stage 1 had not written yet — the launch \
                 did not wait on the buffer's last-write event."
            );
        }
    }
}

/// The upload and the launch on DIFFERENT streams.
///
/// A single stream is FIFO, so an upload and a launch queued on the same
/// one are ordered whether or not anything asks for it — which is why
/// the first version of this test, which reused one stream, passed with
/// every wait disabled. Splitting them is what makes the buffer's
/// upload-completion event the only thing standing between the kernel
/// and a half-written input.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn an_upload_on_one_stream_orders_a_kernel_on_another() {
    let Some(ctx) = ctx_or_skip("an_upload_on_one_stream_orders_a_kernel_on_another") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["twiceplus"]).expect("module");
    let upload = Stream::new(&ctx).expect("upload stream");
    let compute = Stream::new(&ctx).expect("compute stream");

    const ROUNDS: usize = 24;
    for round in 0..ROUNDS {
        let base = (round as i32) * 31 + 5;
        let host: Vec<i32> = (0..N).map(|i| base + (i as i32 % 4096)).collect();

        let src = DeviceBuffer::from_host_async(&ctx, &host, &upload).expect("upload");
        let dst = DeviceBuffer::<i32>::zeros(&ctx, N).expect("dst");

        module
            .launch_on_stream(
                &ctx,
                "twiceplus",
                &LaunchConfig::elementwise(N as u32),
                KernelArgs::new()
                    .push_device_ptr(&src)
                    .push_device_ptr(&dst)
                    .push_i32(N as i32),
                &compute,
            )
            .expect("launch");

        let mut back = vec![0i32; N];
        dst.to_host_async(&mut back, &compute).expect("download");
        compute.synchronize().expect("sync compute");
        upload.synchronize().expect("sync upload");

        for (i, (&got, &src_v)) in back.iter().zip(host.iter()).enumerate() {
            let want = src_v * 2 + 1;
            assert_eq!(
                got, want,
                "round {round} element {i}: got {got}, want {want}. A 1 means \
                 the kernel on the compute stream ran against a buffer the \
                 upload stream had not finished writing."
            );
        }
    }
}
