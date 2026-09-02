// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent use of the bridge, in the two shapes it can take.
//!
//! # These found a real defect, and are the regression test for it
//!
//! `stream_ordering_it.rs` passed 4/4 run serially and failed 4/6 run
//! under `cargo test`'s default thread pool, always with `got 0` — an
//! output buffer never written at all. Each of those tests builds its
//! OWN `DeviceContext`, so that result did not say which of two things
//! was wrong: several contexts on one device, or several threads driving
//! one. The distinction decides whether the VM is affected, because
//! `OffloadCacheRegistry` keeps exactly one `DeviceContext` per device
//! ordinal and hands it to every dispatching Java thread.
//!
//! It was the second — the production shape. `one_context_many_threads`
//! failed 4 of 4 runs at 4 MiB buffers and passed 5 of 5 at 1 MiB, which
//! is the tell: the window is the length of a memset.
//!
//! The cause was `DeviceBuffer::zeros`. cudarc's `alloc_zeros` issues an
//! ASYNC memset and returns; the buffer came back with an EMPTY
//! `last_write` slot, so a following `launch_on_stream` on a user stream
//! had nothing to wait on and the zeroing was free to land after the
//! kernel's stores and wipe them. `zeros` now records that memset as the
//! buffer's last-write, and the existing wait loop does the rest. Both
//! shapes pass 6 of 6.
//!
//! Kept at 4 MiB deliberately. At 1 MiB these tests pass against the
//! unfixed code, which makes them a green light for a corrupting bug.
//!
//! Requires the `cuda` feature and a real device.

#![cfg(feature = "cuda")]

use cratonvm_cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, KernelArgs, LaunchConfig, Stream,
};
use std::sync::Arc;

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

const N: usize = 1024 * 1024;
const THREADS: usize = 4;
const ROUNDS: usize = 24;

/// One thread's work: upload, launch, download, check. `ctx` is whatever
/// the caller decided to share (or not).
fn pipeline(ctx: &DeviceContext, module: &DeviceModule, tag: i32) -> Result<(), String> {
    let stream = Stream::new(ctx).map_err(|e| format!("stream: {e}"))?;
    for round in 0..ROUNDS {
        let base = tag * 1000 + round as i32 + 1;
        let host: Vec<i32> = (0..N).map(|i| base + (i as i32 % 512)).collect();
        let src = DeviceBuffer::from_host_async(ctx, &host, &stream)
            .map_err(|e| format!("upload: {e}"))?;
        let dst = DeviceBuffer::<i32>::zeros(ctx, N).map_err(|e| format!("dst: {e}"))?;
        module
            .launch_on_stream(
                ctx,
                "twiceplus",
                &LaunchConfig::elementwise(N as u32),
                KernelArgs::new()
                    .push_device_ptr(&src)
                    .push_device_ptr(&dst)
                    .push_i32(N as i32),
                &stream,
            )
            .map_err(|e| format!("launch: {e}"))?;
        let mut back = vec![0i32; N];
        dst.to_host_async(&mut back, &stream)
            .map_err(|e| format!("download: {e}"))?;
        stream.synchronize().map_err(|e| format!("sync: {e}"))?;

        for &i in &[0usize, N / 2, N - 1] {
            let want = host[i] * 2 + 1;
            if back[i] != want {
                return Err(format!(
                    "tag {tag} round {round} element {i}: got {}, want {want}",
                    back[i]
                ));
            }
        }
    }
    Ok(())
}

/// THE SHAPE THE VM HAS: one context, many threads.
///
/// `OffloadCacheRegistry` builds one `DeviceContext` per device ordinal
/// and every dispatching Java thread uses it. If this fails, production
/// is affected.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn one_context_many_threads() {
    let ctx = match DeviceContext::new(0) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("SKIP one_context_many_threads: {e}");
            return;
        }
    };
    let module = Arc::new(DeviceModule::from_ptx(&ctx, PTX, &["twiceplus"]).expect("module"));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let ctx = Arc::clone(&ctx);
        let module = Arc::clone(&module);
        handles.push(std::thread::spawn(move || pipeline(&ctx, &module, t as i32)));
    }
    let mut failures = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(()) => {}
            Err(e) => failures.push(e),
        }
    }
    assert!(
        failures.is_empty(),
        "one shared context driven by {THREADS} threads produced wrong \
         results — this is the shape `OffloadCacheRegistry` gives every \
         dispatching Java thread:\n  {}",
        failures.join("\n  ")
    );
}

/// The other shape: a context per thread. Not what the VM does, and the
/// reason `stream_ordering_it.rs`'s tests each build their own is only
/// that a test is a self-contained thing.
///
/// Reported rather than asserted while it is unclear whether several
/// primary-context retentions driven concurrently are something this
/// crate intends to support — the answer changes what to fix, and
/// failing here would say the bridge is broken when it may simply be
/// used wrongly.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn many_contexts_many_threads_reported() {
    if DeviceContext::new(0).is_err() {
        eprintln!("SKIP many_contexts_many_threads_reported: no device");
        return;
    }
    let mut handles = Vec::new();
    for t in 0..THREADS {
        handles.push(std::thread::spawn(move || {
            let ctx = DeviceContext::new(0).map_err(|e| format!("ctx: {e}"))?;
            let module =
                DeviceModule::from_ptx(&ctx, PTX, &["twiceplus"]).map_err(|e| format!("mod: {e}"))?;
            pipeline(&ctx, &module, t as i32)
        }));
    }
    let mut failures = Vec::new();
    for h in handles {
        match h.join().expect("thread panicked") {
            Ok(()) => {}
            Err(e) => failures.push(e),
        }
    }
    if failures.is_empty() {
        println!("many_contexts_many_threads: {THREADS} threads, no divergence");
    } else {
        println!(
            "many_contexts_many_threads: {} of {THREADS} thread(s) diverged:\n  {}",
            failures.len(),
            failures.join("\n  ")
        );
    }
}
