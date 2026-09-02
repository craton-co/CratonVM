// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! What the marshalling path actually costs, on the device it runs on.
//!
//! Two decisions in this crate are recorded as "deferred pending
//! measurement" and neither had been measured:
//!
//! 1. **Should H2D stage through page-locked memory?** `PinnedHostBuffer`
//!    quotes 12.95 GB/s pinned against 3.98 GB/s pageable — but that
//!    3.98 was measured for an ASYNC copy into pageable memory, and the
//!    upload path is SYNCHRONOUS, where the driver stages through its
//!    own pinned buffer and is much faster. The number the change would
//!    have to beat was never taken.
//! 2. **Is a cubin cache worth building?** `DeviceModule::from_ptx`
//!    hands PTX text to the driver's JIT on every process start. Whether
//!    that is 1 ms or 100 ms decides whether a disk cache is worth its
//!    invalidation problem.
//!
//! Reports rather than asserts, except for the one property that must
//! hold regardless of speed: every path must move the bytes correctly.
//! A bandwidth number is a measurement, not a contract, and a test that
//! fails when someone else's build saturates the bus is noise.
//!
//! Requires the `cuda` feature and a real device.
//!
//! ```sh
//! cargo test -p cratonvm-cuda-bridge --features cuda \
//!     --test transfer_bandwidth_it -- --nocapture --ignored
//! ```

#![cfg(feature = "cuda")]

use cratonvm_cuda_bridge::{DeviceBuffer, DeviceContext, DeviceModule, PinnedHostBuffer};
use std::time::Instant;

/// Sizes that bracket what a kernel argument actually is: a few hundred
/// KB up to the tens of MB an image or a weight tile occupies.
const SIZES_MIB: [usize; 4] = [1, 8, 32, 128];

fn gib_per_s(bytes: usize, secs: f64) -> f64 {
    (bytes as f64) / secs / (1024.0 * 1024.0 * 1024.0)
}

/// Median of an odd number of samples — a mean on a shared box measures
/// whatever else was running.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
    v[v.len() / 2]
}

#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn h2d_pageable_versus_pinned() {
    let ctx = match DeviceContext::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP h2d_pageable_versus_pinned: {e}");
            return;
        }
    };
    const REPS: usize = 9;

    println!("  MiB    pageable-sync    pinned-sync     ratio");
    for mib in SIZES_MIB {
        let n = mib * 1024 * 1024 / std::mem::size_of::<f32>();
        let bytes = n * std::mem::size_of::<f32>();

        // Pageable: an ordinary heap Vec, which is what the JVM heap
        // arena is from the driver's point of view.
        let host: Vec<f32> = (0..n).map(|i| i as f32).collect();

        // Page-locked staging, allocated once and reused — the way the
        // chunked writeback already uses it.
        let pinned = PinnedHostBuffer::<f32>::new(&ctx, n).expect("pinned alloc");

        let mut pageable = Vec::with_capacity(REPS);
        let mut staged = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let t = Instant::now();
            let buf = DeviceBuffer::from_host(&ctx, &host).expect("pageable upload");
            pageable.push(t.elapsed().as_secs_f64());
            drop(buf);

            // The staged path pays a host memcpy AND the DMA. Both are
            // inside the timing, because both are what the caller would
            // wait for.
            let t = Instant::now();
            // SAFETY: no DMA is queued against this buffer — the upload
            // below is synchronous and has not been issued yet.
            let dst = unsafe { pinned.as_mut_slice() };
            dst.copy_from_slice(&host);
            let buf = DeviceBuffer::from_host(&ctx, dst).expect("pinned upload");
            staged.push(t.elapsed().as_secs_f64());

            // Correctness, every rep: a fast path that moves the wrong
            // bytes is not a fast path.
            let mut back = vec![0.0f32; n];
            buf.to_host(&mut back).expect("readback");
            assert_eq!(back[0], host[0]);
            assert_eq!(back[n - 1], host[n - 1]);
            assert_eq!(back[n / 2], host[n / 2]);
        }

        let p = gib_per_s(bytes, median(pageable));
        let s = gib_per_s(bytes, median(staged));
        println!("{mib:5}    {p:8.2} GiB/s   {s:8.2} GiB/s   {:.2}x", s / p);
    }
}

/// What a `DeviceModule::from_ptx` costs, which is what a cubin cache
/// would be saving.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn ptx_module_load_cost() {
    let ctx = match DeviceContext::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("SKIP ptx_module_load_cost: {e}");
            return;
        }
    };

    // A body about the size of a lowered element-wise kernel, so the
    // number is representative rather than a floor for an empty module.
    //
    // `salt` makes every module's TEXT different. Loading the SAME text
    // repeatedly measures the driver's own compilation cache and not
    // compilation: a first attempt at this reported a 0.87 ms median
    // against an 89.73 ms first load, which is the cache, and would have
    // settled the cubin-cache question for the wrong reason. Every
    // kernel a real program loads is a different kernel.
    let make_ptx = |salt: usize| {
        let mut body = String::new();
        for i in 0..64 {
            body.push_str(&format!(
                "    add.s32 %r{}, %r{}, {};\n",
                i % 8,
                (i + 1) % 8,
                salt + i
            ));
        }
        format!(
            ".version 7.5\n.target sm_75\n.address_size 64\n\n\
             .visible .entry k(.param .u64 p, .param .s32 n) {{\n\
             \x20   .reg .s32 %r<16>;\n    .reg .u64 %rd<4>;\n\n\
             {body}    ret;\n}}\n"
        )
    };

    const REPS: usize = 9;

    // Warm the context and the driver's JIT machinery, so the first
    // timed load is not paying for CUDA initialisation.
    let _warm = DeviceModule::from_ptx(&ctx, &make_ptx(900_000), &["k"]).expect("warm");

    let mut distinct = Vec::with_capacity(REPS);
    for r in 0..REPS {
        let ptx = make_ptx(r * 1000 + 1);
        let t = Instant::now();
        let m = DeviceModule::from_ptx(&ctx, &ptx, &["k"]).expect("module load");
        distinct.push(t.elapsed().as_secs_f64());
        drop(m);
    }

    // The same text repeatedly, to show what the driver already caches.
    let same = make_ptx(7);
    let _prime = DeviceModule::from_ptx(&ctx, &same, &["k"]).expect("prime");
    let mut repeat = Vec::with_capacity(REPS);
    for _ in 0..REPS {
        let t = Instant::now();
        let m = DeviceModule::from_ptx(&ctx, &same, &["k"]).expect("module load");
        repeat.push(t.elapsed().as_secs_f64());
        drop(m);
    }

    println!(
        "from_ptx: distinct text {:.2} ms median, identical text {:.2} ms median",
        median(distinct) * 1e3,
        median(repeat) * 1e3
    );
    println!(
        "  distinct is what a program pays per kernel per process start and \
         what a cubin cache would save; identical is the driver's own cache."
    );
}
