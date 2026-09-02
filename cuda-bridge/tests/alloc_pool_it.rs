// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The device allocation pool's admission rule, checked against a device.
//!
//! # What the pool must never do
//!
//! `AllocPool` recycles a freed device block to the next allocation of
//! exactly that size, which turns a ~117 us `cuMemAlloc` into a hash
//! lookup. The whole hazard is one case: handing a block to a new
//! allocation while a kernel is STILL WRITING the old one. The new
//! owner's data would then be overwritten by the previous owner's
//! kernel — the same shape as the `zeros` memset race in
//! `concurrent_dispatch_it.rs`, and just as size-dependent, because the
//! window is the length of the kernel.
//!
//! The admission rule is therefore: a buffer is recycled only if its
//! `last_write` event has ALREADY fired at drop time. A buffer whose
//! event has not fired is freed the ordinary way (`cuMemFree`, which
//! the driver documents as synchronizing with respect to the device)
//! and never enters the pool. There is deliberately no wait — an
//! earlier version called `ev.synchronize()` here and serialised a
//! launch chain on the submitting thread (99 us/launch against 52).
//!
//! These tests assert both halves of that rule through the engagement
//! census, which is the only observable the pool has: `parked` counts
//! admissions, `hit` counts blocks actually reused.
//!
//! # These tests were verified to be able to fail
//!
//! With the `matches!(ev.query(), Ok(true))` admission check replaced by
//! an unconditional `true` — i.e. the pool taking every block regardless
//! of whether its kernel had finished —
//! `a_buffer_whose_kernel_is_still_running_is_not_recycled` FAILED
//! (`parked` moved by 1). With the pool's `put` short-circuited to
//! `free_now`, `an_idle_buffer_is_recycled_by_the_next_allocation`
//! FAILED. Neither test passes against a pool that does nothing, and
//! neither passes against a pool that does too much.
//!
//! Requires the `cuda` feature and a real device.

#![cfg(feature = "cuda")]

use cratonvm_cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, KernelArgs, LaunchConfig, Stream,
};
use cratonvm_types::gpu_event_census::alloc_totals;

/// A deliberately slow elementwise kernel: each thread spins on a
/// dependent chain before its store, so the launch is still running
/// when the host reaches the next statement.
///
/// The chain is dependent (`mad` on its own result) so ptxas cannot
/// eliminate it, and the store happens AFTER it, which is what makes
/// the buffer's `last_write` event outlive the drop below.
const PTX: &str = r#"
.version 7.5
.target sm_75
.address_size 64

.visible .entry slowfill(
    .param .u64 dst,
    .param .s32 n,
    .param .s32 spin
) {
    .reg .u32 %ru<4>;
    .reg .s32 %r<8>;
    .reg .u64 %rd<3>;
    .reg .pred %p<3>;

    ld.param.u64 %rd0, [dst];
    ld.param.s32 %r0, [n];
    ld.param.s32 %r1, [spin];
    mov.u32 %ru0, %ctaid.x;
    mov.u32 %ru1, %ntid.x;
    mov.u32 %ru2, %tid.x;
    mad.lo.u32 %ru3, %ru0, %ru1, %ru2;
    mov.b32 %r2, %ru3;
    setp.ge.u32 %p0, %r2, %r0;
    @%p0 bra L_done;
    mov.s32 %r3, 1;
    mov.s32 %r4, 0;
L_spin:
    setp.ge.s32 %p1, %r4, %r1;
    @%p1 bra L_store;
    mad.lo.s32 %r3, %r3, 3, 1;
    add.s32 %r4, %r4, 1;
    bra L_spin;
L_store:
    and.b32 %r5, %r3, 1;
    add.s32 %r6, %r2, %r5;
    mad.wide.s32 %rd1, %r2, 4, %rd0;
    st.global.s32 [%rd1], %r6;
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

const N: usize = 1024 * 1024;
/// Iterations of the dependent chain per thread. Large enough that the
/// launch is unquestionably still in flight one host statement later,
/// small enough that the test finishes in well under a second.
const SPIN: i32 = 200_000;

/// The hazard case: drop a buffer while its kernel is still writing it.
/// The pool must refuse it.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn a_buffer_whose_kernel_is_still_running_is_not_recycled() {
    let Some(ctx) = ctx_or_skip("a_buffer_whose_kernel_is_still_running_is_not_recycled") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["slowfill"]).expect("module");
    let stream = Stream::new(&ctx).expect("stream");

    let dst = DeviceBuffer::<i32>::zeros(&ctx, N).expect("dst");
    let cfg = LaunchConfig::elementwise(N as u32);
    let args = KernelArgs::new()
        .push_device_ptr(&dst)
        .push_i32(N as i32)
        .push_i32(SPIN);
    module
        .launch_on_stream(&ctx, "slowfill", &cfg, args, &stream)
        .expect("launch");

    // The census is process-global and other tests in this binary move
    // it, so the assertion is on the DELTA across this drop alone.
    let (_, _, parked_before) = alloc_totals();
    drop(dst);
    let (_, _, parked_after) = alloc_totals();

    // The kernel is still running, so nothing may have entered the pool.
    // If this fires, a block still being written is on the free list and
    // the next allocation of that size will be handed a buffer a live
    // kernel is about to overwrite.
    assert_eq!(
        parked_after, parked_before,
        "a buffer whose kernel had not finished was admitted to the pool \
         (parked {parked_before} -> {parked_after})"
    );

    stream.synchronize().expect("drain");
}

/// The ordinary case: a buffer whose work has retired is recycled, and
/// the next allocation of that size is served from the pool rather than
/// from `cuMemAlloc`.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn an_idle_buffer_is_recycled_by_the_next_allocation() {
    let Some(ctx) = ctx_or_skip("an_idle_buffer_is_recycled_by_the_next_allocation") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["slowfill"]).expect("module");
    let stream = Stream::new(&ctx).expect("stream");

    // A size no other test in this binary uses, so the block this drop
    // parks is the block the next allocation takes.
    const M: usize = 977 * 1024;
    let dst = DeviceBuffer::<i32>::zeros(&ctx, M).expect("dst");
    let cfg = LaunchConfig::elementwise(M as u32);
    let args = KernelArgs::new()
        .push_device_ptr(&dst)
        .push_i32(M as i32)
        // A trivial spin: this kernel is meant to FINISH.
        .push_i32(1);
    module
        .launch_on_stream(&ctx, "slowfill", &cfg, args, &stream)
        .expect("launch");
    // The event has fired by the time this returns, which is the
    // condition the admission rule tests for.
    stream.synchronize().expect("drain");

    let (_, _, parked_before) = alloc_totals();
    drop(dst);
    let (_, _, parked_after) = alloc_totals();
    assert_eq!(
        parked_after,
        parked_before + 1,
        "an idle buffer was not admitted to the pool \
         (parked {parked_before} -> {parked_after})"
    );

    let (hits_before, _, _) = alloc_totals();
    let reused = DeviceBuffer::<i32>::zeros(&ctx, M).expect("reuse");
    let (hits_after, _, _) = alloc_totals();
    assert_eq!(
        hits_after,
        hits_before + 1,
        "the next allocation of the parked size did not come from the pool \
         (hits {hits_before} -> {hits_after})"
    );

    // And it is a usable buffer, not just a recycled pointer: `zeros`
    // owes its caller zeroed memory even when the block it took carries
    // the previous owner's data.
    let mut host = vec![-1i32; M];
    reused.to_host(&mut host).expect("download");
    assert!(
        host.iter().all(|&v| v == 0),
        "a recycled block was handed back from `zeros` still holding the \
         previous owner's data"
    );
}
