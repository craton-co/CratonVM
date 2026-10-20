// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The HIP backend on a machine with no ROCm, through the PUBLIC
//! surface.
//!
//! # Why this is not `#[ignore]`d
//!
//! Every other driver-backed test in this directory needs hardware and
//! is skipped or ignored without it. This one needs the OPPOSITE, and
//! it is the only HIP test public CI can run — so it runs on every PR,
//! on `ubuntu-latest`, with nothing installed.
//!
//! It guards the single most important invariant in `backend_hip.rs`.
//! `cubecl-hip-sys` resolves symbols through `dlopen` and its loader
//! **panics** when the libraries are missing, because the generated
//! wrappers have no common error type to return. Meanwhile
//! `BackendApi::probe_device` is documented as "must not panic on a
//! machine with no driver", and the VM's entire fall-back-to-CPU path
//! rests on getting an `Err` back: `OffloadCache::new` probes, and on
//! `Err` logs "requested but unavailable; falling back to CPU" and sets
//! `ctx = None`.
//!
//! Without the guard in `backend_hip.rs`, a `cratonvm --gpu` built for
//! HIP and run on a box without ROCm would abort the JVM at startup
//! instead of running the program on the CPU. That is a crash on the
//! most ordinary configuration there is.
//!
//! `backend_hip.rs` has a unit test for the same property. This one
//! exists as well because it goes through `probe`, `probe_device`,
//! `DeviceContext::new` and `toolchain_version` — the functions callers
//! actually hold — rather than the module-private twins.

#![cfg(feature = "hip")]

use cratonvm_cuda_bridge::{BackendKind, DeviceContext, DeviceError};

/// True when this machine has no usable HIP runtime, which is what the
/// whole file is about.
///
/// Determined by asking the bridge, not by inspecting the environment:
/// the question is "does the backend think it can talk to a device",
/// and that is exactly what a probe answers.
fn no_runtime_here() -> bool {
    matches!(
        cratonvm_cuda_bridge::probe(),
        Err(DeviceError::NoDriver) | Err(DeviceError::Driver(_))
    ) && cratonvm_cuda_bridge::toolchain_version().is_err()
}

#[test]
fn the_compiled_in_backend_is_hip() {
    assert_eq!(cratonvm_cuda_bridge::backend_kind(), BackendKind::Hip);
}

/// Each of these is a separate entry point into the binding crate, and
/// each would panic independently if its guard were dropped.
#[test]
fn every_entry_point_reports_no_driver_rather_than_panicking() {
    if !no_runtime_here() {
        eprintln!("SKIP: this machine has a HIP runtime; the no-runtime path is untestable here");
        return;
    }

    assert!(
        matches!(cratonvm_cuda_bridge::probe(), Err(DeviceError::NoDriver)),
        "probe() must report NoDriver, not panic and not succeed"
    );
    assert!(
        matches!(
            cratonvm_cuda_bridge::probe_device(0),
            Err(DeviceError::NoDriver)
        ),
        "probe_device(0) must report NoDriver"
    );
    // A far-out ordinal exercises the same path with a different
    // argument, because a guard that only covered ordinal 0 would look
    // identical in the test above.
    assert!(
        matches!(
            cratonvm_cuda_bridge::probe_device(7),
            Err(DeviceError::NoDriver)
        ),
        "probe_device(7) must report NoDriver"
    );
    assert!(
        matches!(DeviceContext::new(0), Err(DeviceError::NoDriver)),
        "DeviceContext::new must report NoDriver, which is what makes \
         OffloadCache::new fall back to the CPU instead of aborting"
    );
    assert!(
        cratonvm_cuda_bridge::toolchain_version().is_err(),
        "toolchain_version must report an error, not panic"
    );
    // The CUDA-shaped shim must also refuse rather than invent a PTX
    // ISA ceiling the VM's clamp would then act on.
    assert!(cratonvm_cuda_bridge::driver_cuda_version().is_err());
}

/// The message a user sees when they ask for a GPU and there is none.
///
/// It used to name CUDA and the `cuda` feature, which on a HIP build is
/// advice that cannot help: the missing thing is ROCm. Asserted rather
/// than left to review because it is the one line that reaches a user
/// who has no other diagnostic.
#[test]
fn the_no_driver_message_names_no_vendor() {
    let msg = DeviceError::NoDriver.to_string();
    let lower = msg.to_ascii_lowercase();
    assert!(
        !lower.contains("cuda"),
        "NoDriver still names CUDA, which is wrong advice on a HIP build: {msg}"
    );
    assert!(
        lower.contains("gpu") && lower.contains("driver backend"),
        "NoDriver should say what is missing in neutral terms: {msg}"
    );
}
