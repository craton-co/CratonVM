// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What a captured graph is worth, measured against the launch loop it
//! replaces, on real hardware.
//!
//! # Why this test exists at all
//!
//! GPULlama3's forward pass issues 453 kernel launches per token on one
//! stream, and the host cannot keep up: 14 ms of host time against 24 ms of
//! device time on an idle box, 60 ms against 5 ms on a loaded one. The
//! device finishes and waits. A graph collapses the whole sequence into one
//! `cuGraphLaunch`.
//!
//! Before anyone plumbs graph capture through the VM, a Java API and an
//! application, the mechanism should be shown to work and its ceiling
//! should be a number rather than an argument. That is all this test is:
//! the same 453 launches, issued the ordinary way and replayed from a
//! graph, timed side by side on the device this repository is validated on.
//!
//! Gated behind `gpu-it` and therefore not part of an ordinary `cargo
//! test`; it needs a real driver and a real device.

#![cfg(all(feature = "gpu-it", feature = "cuda"))]

use cratonvm_cuda_bridge::graph::CaptureMode;
use cratonvm_cuda_bridge::{DeviceBuffer, DeviceContext, DeviceModule, KernelArgs, LaunchConfig,
                           Stream};

/// A kernel small enough that the launch cost dominates it, which is the
/// regime the whole exercise is about. `sm_70` so the driver JIT accepts it
/// on anything from Volta on.
const PTX: &str = r#"
.version 7.0
.target sm_70
.address_size 64

.visible .entry bump(
    .param .u64 out_ptr,
    .param .s32 n
)
{
    .reg .pred  %p<2>;
    .reg .s32   %r<6>;
    .reg .u64   %rd<5>;

    ld.param.u64 %rd1, [out_ptr];
    ld.param.s32 %r1, [n];
    mov.u32 %r2, %ctaid.x;
    mov.u32 %r3, %ntid.x;
    mov.u32 %r4, %tid.x;
    mad.lo.s32 %r5, %r2, %r3, %r4;
    setp.ge.s32 %p1, %r5, %r1;
    @%p1 bra DONE;
    cvta.to.global.u64 %rd2, %rd1;
    mul.wide.s32 %rd3, %r5, 4;
    add.u64 %rd4, %rd2, %rd3;
    ld.global.u32 %r2, [%rd4];
    add.s32 %r2, %r2, 1;
    st.global.u32 [%rd4], %r2;
DONE:
    ret;
}
"#;

/// The launch count GPULlama3 issues per token. Using the real number
/// rather than a round one keeps the result directly comparable to the
/// workload that motivated it.
const LAUNCHES: usize = 453;

#[test]
fn a_captured_graph_replays_453_launches_for_less_than_issuing_them() {
    let Ok(ctx) = DeviceContext::new(0) else {
        eprintln!("no CUDA device; skipping");
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["bump"]).expect("load PTX");
    let stream = Stream::new(&ctx).expect("stream");
    let n: i32 = 1024;
    let out: DeviceBuffer<i32> = DeviceBuffer::zeros(&ctx, n as usize).expect("alloc");
    let cfg = LaunchConfig::elementwise(n as u32);
    let args = || KernelArgs::new().push_device_ptr(&out).push_i32(n);

    // Warm up: first launch pays module resolution and any lazy driver
    // setup, which would otherwise land entirely on whichever arm ran
    // first.
    for _ in 0..16 {
        module
            .launch_on_stream(&ctx, "bump", &cfg, args(), &stream)
            .expect("warmup launch");
    }
    stream.synchronize().expect("warmup drain");

    // ── Arm A: issue them, the way a dispatch loop does today. ──
    let t0 = std::time::Instant::now();
    for _ in 0..LAUNCHES {
        module
            .launch_on_stream(&ctx, "bump", &cfg, args(), &stream)
            .expect("launch");
    }
    let issue_host = t0.elapsed();
    stream.synchronize().expect("drain");
    let issue_total = t0.elapsed();

    // ── Capture the same sequence. ──
    stream
        .begin_capture(CaptureMode::ThreadLocal)
        .expect("begin capture");
    let mut nodes = 0usize;
    for _ in 0..LAUNCHES {
        module
            .launch_on_stream(&ctx, "bump", &cfg, args(), &stream)
            .expect("captured launch");
        // Every launch must become exactly one node. A capture that
        // silently swallowed one would replay fewer kernels than the loop
        // ran and still succeed, which is the failure this counts against.
        if stream.capturing_node().expect("capture info").is_some() {
            nodes += 1;
        }
    }
    let graph = stream.end_capture(&ctx).expect("end capture");
    assert_eq!(
        nodes, LAUNCHES,
        "every captured launch should have reported a node"
    );
    assert_eq!(
        graph.node_count().expect("node count"),
        LAUNCHES,
        "the graph should hold exactly the launches that were captured"
    );
    let exec = graph.instantiate().expect("instantiate");

    // Warm the replay path the same way the launch path was warmed.
    exec.launch(&stream).expect("warmup replay");
    stream.synchronize().expect("warmup replay drain");

    // ── Arm B: replay them. ──
    let t1 = std::time::Instant::now();
    exec.launch(&stream).expect("replay");
    let replay_host = t1.elapsed();
    stream.synchronize().expect("replay drain");
    let replay_total = t1.elapsed();

    eprintln!(
        "graph {LAUNCHES} launches: issue host {:.3} ms / total {:.3} ms; \
         replay host {:.3} ms / total {:.3} ms; host cost {:.1}x lower",
        issue_host.as_secs_f64() * 1e3,
        issue_total.as_secs_f64() * 1e3,
        replay_host.as_secs_f64() * 1e3,
        replay_total.as_secs_f64() * 1e3,
        issue_host.as_secs_f64() / replay_host.as_secs_f64().max(1e-9),
    );

    // The claim under test is about HOST cost: the device does the same
    // work either way, and on a kernel this small both arms are host-bound.
    // A generous bound rather than a tight one -- this is a regression
    // guard on the mechanism, not a benchmark assertion, and the number it
    // guards is reported above for whoever wants the real figure.
    assert!(
        replay_host < issue_host,
        "replaying {LAUNCHES} launches should cost the host less than issuing them \
         (issue {:?}, replay {:?})",
        issue_host,
        replay_host,
    );
}

/// A capture that is never ended must not leave the stream unusable for the
/// next test in the binary, and ending one that was never begun must fail
/// rather than hand back an empty graph that replays successfully and does
/// nothing.
#[test]
fn ending_a_capture_that_never_began_is_an_error() {
    let Ok(ctx) = DeviceContext::new(0) else {
        eprintln!("no CUDA device; skipping");
        return;
    };
    let stream = Stream::new(&ctx).expect("stream");
    assert!(
        stream.end_capture(&ctx).is_err(),
        "ending a capture that never began must fail loudly; an empty graph would \
         instantiate, replay, and do nothing"
    );
    assert!(
        stream.capturing_node().expect("capture info").is_none(),
        "a stream that is not capturing has no current node"
    );
}

/// A kernel whose behaviour depends on a value it reads from device
/// memory rather than from a parameter. This is the shape that makes a
/// launch sequence replayable: the graph bakes in the POINTER, and the
/// host writes through it between replays.
const PTX_SCALED: &str = r#"
.version 7.0
.target sm_70
.address_size 64

.visible .entry addk(
    .param .u64 out_ptr,
    .param .u64 k_ptr,
    .param .s32 n
)
{
    .reg .pred  %p<2>;
    .reg .s32   %r<8>;
    .reg .u64   %rd<8>;

    ld.param.u64 %rd1, [out_ptr];
    ld.param.u64 %rd5, [k_ptr];
    ld.param.s32 %r1, [n];
    mov.u32 %r2, %ctaid.x;
    mov.u32 %r3, %ntid.x;
    mov.u32 %r4, %tid.x;
    mad.lo.s32 %r5, %r2, %r3, %r4;
    setp.ge.s32 %p1, %r5, %r1;
    @%p1 bra DONE;
    cvta.to.global.u64 %rd6, %rd5;
    ld.global.u32 %r6, [%rd6];
    cvta.to.global.u64 %rd2, %rd1;
    mul.wide.s32 %rd3, %r5, 4;
    add.u64 %rd4, %rd2, %rd3;
    ld.global.u32 %r7, [%rd4];
    add.s32 %r7, %r7, %r6;
    st.global.u32 [%rd4], %r7;
DONE:
    ret;
}
"#;

/// The claim the whole device-resident-scalar design rests on: a graph
/// captured once can be given new input by writing through a pointer it
/// already holds, and the replay observes the new value.
///
/// Without `copy_from_host` the only way to change a kernel argument is
/// to allocate a new buffer, which a captured graph cannot see -- it
/// would keep running against the old pointer and quietly produce the
/// old answer. That failure is invisible to any check that only asks
/// whether the replay succeeded, which is why this asserts on the
/// CONTENTS.
#[test]
fn a_replay_sees_a_scalar_written_through_the_captured_pointer() {
    let Ok(ctx) = DeviceContext::new(0) else {
        eprintln!("no CUDA device; skipping");
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX_SCALED, &["addk"]).expect("load PTX");
    let stream = Stream::new(&ctx).expect("stream");
    let n: i32 = 256;
    let out: DeviceBuffer<i32> = DeviceBuffer::zeros(&ctx, n as usize).expect("alloc out");
    let k: DeviceBuffer<i32> = DeviceBuffer::from_host(&ctx, &[1i32]).expect("alloc k");
    let cfg = LaunchConfig::elementwise(n as u32);
    let args = || {
        KernelArgs::new()
            .push_device_ptr(&out)
            .push_device_ptr(&k)
            .push_i32(n)
    };

    // Capture three launches, so the assertion is about the graph and
    // not about a single kernel that happened to run.
    stream
        .begin_capture(CaptureMode::ThreadLocal)
        .expect("begin capture");
    for _ in 0..3 {
        module
            .launch_on_stream(&ctx, "addk", &cfg, args(), &stream)
            .expect("captured launch");
    }
    let exec = stream
        .end_capture(&ctx)
        .expect("end capture")
        .instantiate()
        .expect("instantiate");

    let mut host = vec![0i32; n as usize];

    // k == 1: three launches add 1 each.
    exec.launch(&stream).expect("replay 1");
    stream.synchronize().expect("drain 1");
    out.to_host(&mut host).expect("read 1");
    assert_eq!(host[0], 3, "three launches of +1 should total 3");

    // Now change k in place and replay the SAME graph.
    k.copy_from_host(&[10i32]).expect("copy_from_host");
    exec.launch(&stream).expect("replay 2");
    stream.synchronize().expect("drain 2");
    out.to_host(&mut host).expect("read 2");
    assert_eq!(
        host[0], 33,
        "the replay must observe the new scalar (3 + 3*10); a graph replaying against          a stale value would read 6 here and look like it worked"
    );
    assert!(
        host.iter().all(|&v| v == 33),
        "every element should have taken the same path"
    );

    // And a wrong-sized write must be refused rather than partially
    // applied: a half-updated device buffer is a wrong answer.
    assert!(
        k.copy_from_host(&[1i32, 2i32]).is_err(),
        "a length mismatch must fail rather than write what fits"
    );
}
