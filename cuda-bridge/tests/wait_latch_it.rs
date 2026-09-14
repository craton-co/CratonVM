// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Eliding a `cuStreamWaitEvent` on an event that has already fired.
//!
//! # The case this is for
//!
//! `Stream::wait_event` already skipped a wait on an event recorded on
//! the waiting stream — a stream is FIFO, so that ordering is free. It
//! could not skip anything else, and the census said so: a `--gpu`
//! auto-offload run read `stream waits issued=237 elided=0 (0.0%)`
//! while the executor path read `issued=0 elided=7844 (100%)`.
//!
//! Two facts produce that. The dispatch path hands out streams
//! round-robin from a pool of four, so consecutive dispatches are almost
//! never on the same stream; and a RESIDENT INPUT buffer keeps the
//! `last_write` its upload or its zeroing stamped on it for its whole
//! life, because nothing ever writes it again. Every launch that
//! consumed such a buffer therefore issued a fresh `cuStreamWaitEvent`
//! against an event that had fired long ago.
//!
//! A CUDA event that has fired cannot un-fire until something records it
//! again, so that wait is a no-op and skipping it is exactly equivalent.
//! The latch is one `AtomicBool` set whenever completion is OBSERVED
//! (`query`, `synchronize`, or one probe at the wait site) and cleared by
//! `set_recorded_on`, which is the only way an event re-enters flight.
//!
//! # What must NOT be elided
//!
//! An event still in flight, and an event that fired and was then
//! RE-RECORDED. The third test is the one that matters: it latches an
//! event, records it again behind a slow kernel, and asserts the wait is
//! issued. Without the clear in `set_recorded_on` that case silently
//! drops a real ordering edge, which is a wrong answer rather than a
//! slow one.
//!
//! # These tests were verified to be able to fail
//!
//! * with the `is_completed()` branch removed from `wait_event`, BOTH
//!   `a_wait_on_an_already_fired_event_is_elided` and
//!   `an_unobserved_but_finished_event_is_latched_by_one_probe` FAILED
//!   (the probe still elides the FIRST wait, so the second one reaches
//!   the driver and the delta is 1 where 2 is asserted);
//! * with the two `store(false)` calls removed from `set_recorded_on`,
//!   `a_re_recorded_event_is_waited_on_again` FAILED — the stale latch
//!   swallowed the wait — and the other two still passed, which is what
//!   makes it the test that discriminates the clear.
//!
//! Requires the `cuda` feature and a real device.

#![cfg(feature = "gpu-driver")]

use cratonvm_cuda_bridge::{
    DeviceBuffer, DeviceContext, DeviceModule, Event, KernelArgs, LaunchConfig, Stream,
};
use cratonvm_types::gpu_event_census::totals;

/// A dependent spin then a store, so a launch is unquestionably still in
/// flight one host statement later. Same kernel as `alloc_pool_it.rs`.
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
const SPIN: i32 = 200_000;

/// `(waits issued, waits elided)` — the deltas are what the assertions
/// use, because the census is process-global and other tests move it.
fn waits() -> (u64, u64) {
    let (_, _, issued, elided) = totals();
    (issued, elided)
}

/// The whole point: an event recorded on stream A and observed complete
/// costs nothing when stream B waits on it.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn a_wait_on_an_already_fired_event_is_elided() {
    let Some(ctx) = ctx_or_skip("a_wait_on_an_already_fired_event_is_elided") else {
        return;
    };
    let a = Stream::new(&ctx).expect("stream a");
    let b = Stream::new(&ctx).expect("stream b");
    let ev = Event::new(&ctx).expect("event");

    a.record_event(&ev).expect("record");
    a.synchronize().expect("drain");
    // Observation is what latches it. Without this the first wait spends
    // the one probe instead, which is the same outcome by a longer road
    // — asserted by the next test.
    assert!(ev.query().expect("query"), "the event should have fired");

    let (issued0, elided0) = waits();
    b.wait_event(&ev).expect("wait 1");
    b.wait_event(&ev).expect("wait 2");
    b.wait_event(&ev).expect("wait 3");
    let (issued1, elided1) = waits();

    assert_eq!(
        issued1, issued0,
        "a wait on an already-fired event reached the driver"
    );
    assert_eq!(
        elided1,
        elided0 + 3,
        "the three waits were not counted as elided"
    );
}

/// The probe path: nobody observed completion, so the FIRST wait spends
/// one `cuEventQuery` and every later one is free.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn an_unobserved_but_finished_event_is_latched_by_one_probe() {
    let Some(ctx) = ctx_or_skip("an_unobserved_but_finished_event_is_latched_by_one_probe") else {
        return;
    };
    let a = Stream::new(&ctx).expect("stream a");
    let b = Stream::new(&ctx).expect("stream b");
    let ev = Event::new(&ctx).expect("event");

    a.record_event(&ev).expect("record");
    // Drain the RECORDING stream, not the event: the work is done but
    // nothing has queried the event, so the latch is still clear.
    a.synchronize().expect("drain");

    let (issued0, elided0) = waits();
    b.wait_event(&ev).expect("wait 1");
    b.wait_event(&ev).expect("wait 2");
    let (issued1, elided1) = waits();

    assert_eq!(
        issued1, issued0,
        "the probe did not recognise a finished event"
    );
    assert_eq!(elided1, elided0 + 2, "both waits should have been elided");
}

/// The hazard: an event that fired, was latched, and then RE-RECORDED
/// behind live work. The latch must be gone and the wait must be issued.
#[test]
#[ignore = "requires a real CUDA device; run with --ignored"]
fn a_re_recorded_event_is_waited_on_again() {
    let Some(ctx) = ctx_or_skip("a_re_recorded_event_is_waited_on_again") else {
        return;
    };
    let module = DeviceModule::from_ptx(&ctx, PTX, &["slowfill"]).expect("module");
    let a = Stream::new(&ctx).expect("stream a");
    let b = Stream::new(&ctx).expect("stream b");
    let ev = Event::new(&ctx).expect("event");

    // Fire it once and latch it.
    a.record_event(&ev).expect("record 1");
    a.synchronize().expect("drain");
    assert!(ev.query().expect("query"), "the event should have fired");

    // Now put real work on stream A and re-record the event behind it.
    let dst = DeviceBuffer::<i32>::zeros(&ctx, N).expect("dst");
    let cfg = LaunchConfig::elementwise(N as u32);
    let args = KernelArgs::new()
        .push_device_ptr(&dst)
        .push_i32(N as i32)
        .push_i32(SPIN);
    module
        .launch_on_stream(&ctx, "slowfill", &cfg, args, &a)
        .expect("launch");
    a.record_event(&ev).expect("record 2");

    let (issued0, _) = waits();
    b.wait_event(&ev).expect("wait");
    let (issued1, _) = waits();

    assert_eq!(
        issued1,
        issued0 + 1,
        "a re-recorded event's wait was swallowed by a stale latch — \
         stream B would have run ahead of work it must follow"
    );

    a.synchronize().expect("drain");
    b.synchronize().expect("drain b");
}
