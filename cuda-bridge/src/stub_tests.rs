//! Verification of Part A's no-driver contract.
//!
//! When built without the `cuda` feature (the default), every
//! fallible entry point must return `DeviceError::NoDriver` so the
//! workspace can build and test on machines without a CUDA toolkit.
use super::*;

#[test]
fn probe_returns_no_driver_in_stub_mode() {
    match probe() {
        Err(DeviceError::NoDriver) => {}
        other => panic!("expected NoDriver, got {other:?}"),
    }
}

#[test]
fn device_context_new_returns_no_driver_in_stub_mode() {
    match DeviceContext::new(0) {
        Err(DeviceError::NoDriver) => {}
        Err(other) => panic!("expected NoDriver, got {other:?}"),
        Ok(_) => panic!("expected NoDriver, got Ok(DeviceContext)"),
    }
}

#[test]
fn launch_config_elementwise_sizing() {
    // The pure-Rust helper has no driver dependency; make sure it
    // computes sensible grid/block geometry.
    let cfg = LaunchConfig::elementwise(1_000_000);
    assert_eq!(cfg.block, (256, 1, 1));
    assert_eq!(cfg.grid, ((1_000_000u32).div_ceil(256), 1, 1));
    assert_eq!(cfg.shared_bytes, 0);

    // For n=0 we still want a non-degenerate (grid >= 1) launch
    // shape so callers don't accidentally pass zero-dim grids.
    let cfg0 = LaunchConfig::elementwise(0);
    assert_eq!(cfg0.grid, (1, 1, 1));
}

/// AUDIT 2026-05-29 (H10b): the per-buffer `last_write` slot must be
/// *shared* (same `Arc`) between a `DeviceBuffer` and the
/// `KernelArg::DevicePtr` built from it. This is the mechanism that
/// lets `launch_on_stream` stamp the kernel-completion event into
/// the originating buffer's slot so a subsequent `to_host_async`
/// waits on THIS buffer's producing kernel rather than a clobbered
/// context-wide event. Driver-free: exercises the slot wiring only.
#[test]
fn push_device_ptr_shares_last_write_slot() {
    let buf: DeviceBuffer<f32> = DeviceBuffer::for_test();
    let buf_slot = buf.last_write.clone();
    let args = KernelArgs::new().push_device_ptr(&buf);
    let arg_slot = match &args.raw[0] {
        KernelArg::DevicePtr { last_write, .. } => last_write.clone(),
        _ => panic!("expected DevicePtr arg"),
    };
    // The two handles must point at the SAME allocation.
    assert!(
        std::sync::Arc::ptr_eq(&buf_slot, &arg_slot),
        "buffer and KernelArg must share the same last_write slot"
    );
    // A write through the arg's handle must be observable through
    // the buffer's handle (this is how launch_on_stream propagates
    // kernel_done back to the buffer).
    let ctx = DeviceContext::for_test();
    let ev = std::sync::Arc::new(Event::new(&ctx).expect("stub event"));
    let ev_id = ev.id();
    *arg_slot.lock().unwrap() = Some(ev);
    let seen = buf_slot.lock().unwrap().as_ref().map(|e| e.id());
    assert_eq!(seen, Some(ev_id));
}

#[test]
fn unchecked_upload_records_ordering_before_stub_error() {
    let ctx = DeviceContext::for_test();
    let stream = Stream::for_test();
    let host = [1_u32, 2, 3, 4];

    // SAFETY: this module is `cfg(all(test, not(feature = "cuda")))`, so the
    // stub backend is the one compiled in and no DMA is ever submitted to a
    // driver. The borrowed-host contract — `host` must stay valid until the
    // upload's `last_write` event fires — is therefore vacuous; `host`
    // outlives the call regardless. Same argument as the `SAFETY` on
    // `from_host_async`'s own delegation above.
    match unsafe { DeviceBuffer::<u32>::from_host_async_unchecked(&ctx, &host, &stream) } {
        Err(DeviceError::NoDriver) => {}
        Err(other) => panic!("expected NoDriver, got {other:?}"),
        Ok(_) => panic!("expected NoDriver, got Ok(DeviceBuffer)"),
    }

    let ops = stream.ops();
    assert_eq!(
        ops.len(),
        2,
        "unchecked upload should record event + upload before stub allocation error"
    );
    assert!(
        matches!(ops[0], StreamOp::EventRecord { .. }),
        "upload must publish its last_write event before the UploadAsync op: {ops:?}"
    );
    assert_eq!(
        ops[1],
        StreamOp::UploadAsync {
            bytes: std::mem::size_of_val(&host)
        }
    );
}

#[test]
fn unchecked_download_records_borrowed_dma_without_synchronizing() {
    let ctx = DeviceContext::for_test();
    let stream = Stream::for_test();
    let buf: DeviceBuffer<f32> = DeviceBuffer::for_test();
    let last_write = std::sync::Arc::new(Event::new(&ctx).expect("stub event"));
    let last_write_id = last_write.id();
    *buf.last_write.lock().unwrap_or_else(|p| p.into_inner()) = Some(last_write);

    let mut dst = [0.0_f32; 2];
    // SAFETY: stub-only module (see the sibling upload test), so no DMA is
    // queued and the borrowed-destination contract — `dst` unread and at a
    // fixed address until the download completes — is vacuous. `dst` is a
    // stack local that outlives the call and is only read after it returns.
    match unsafe { buf.to_host_async_unchecked(&mut dst, &stream) } {
        Err(DeviceError::NoDriver) => {}
        Err(other) => panic!("expected NoDriver, got {other:?}"),
        Ok(()) => panic!("expected NoDriver, got Ok(())"),
    }

    let ops = stream.ops();
    assert_eq!(
        ops,
        vec![
            StreamOp::EventWait {
                event_id: last_write_id
            },
            StreamOp::DownloadAsync {
                bytes: std::mem::size_of_val(&dst)
            },
        ],
        "unchecked download must not insert a Synchronize op"
    );
}

/// AUDIT 2026-06-21: every PTX load must get a process-unique module
/// name so cudarc's per-context module map does not have one
/// `DeviceModule` clobber another. Verify the minted names are
/// well-formed and never repeat across successive calls.
#[test]
fn module_names_are_unique() {
    const N: usize = 1024;
    let names: Vec<String> = (0..N).map(|_| next_module_name()).collect();
    for name in &names {
        assert!(
            name.starts_with("cratonvm_mod_"),
            "unexpected module name {name:?}"
        );
    }
    let distinct: std::collections::HashSet<&String> = names.iter().collect();
    assert_eq!(
        distinct.len(),
        N,
        "module names must be unique across loads"
    );
}
