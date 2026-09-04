// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Driver-backed check of [`cratonvm_cuda_bridge::driver_cuda_version`].
//!
//! The VM turns this number into the ceiling on the PTX ISA version it
//! may declare (`jit_cuda::target::max_isa_for_cuda_version`), and then
//! into a possibly-lowered `.target`. Every part of that chain is unit
//! tested against a table; this is the one link that can only be checked
//! against a real driver, because only a real driver knows what it
//! returns.
//!
//! Requires the `cuda` feature and an installed driver. Without them the
//! whole file compiles to nothing — see the `cfg` below — rather than
//! failing on a machine that was never going to have one.

#![cfg(feature = "gpu-driver")]

/// The driver reports a version, and it is one this workspace's table
/// understands.
///
/// The failure this guards is narrow and would be invisible: if
/// `cuDriverGetVersion` returned something unexpected — 0, a negative,
/// or a value in a different unit — `max_isa_for_cuda_version` would
/// saturate to its lowest row and the VM would silently lower every
/// kernel for `sm_70`, on a card that could have run its own
/// architecture. Correct answers, no error, no way to notice.
#[test]
fn the_driver_reports_a_version_this_workspace_can_read() {
    let raw = match cratonvm_cuda_bridge::driver_cuda_version() {
        Ok(v) => v,
        Err(e) => {
            // No driver on this machine. Report and pass: this test is
            // about what a driver says, not about having one.
            eprintln!("SKIP the_driver_reports_a_version_this_workspace_can_read: {e}");
            return;
        }
    };
    let major = raw / 1000;
    let minor = (raw % 1000) / 10;
    eprintln!("cuDriverGetVersion = {raw} (CUDA {major}.{minor})");

    // The encoding is `1000 * major + 10 * minor`. Anything outside this
    // range means the unit changed under us, which is exactly the case
    // that would saturate the ISA table silently.
    assert!(
        (9..=99).contains(&major),
        "cuDriverGetVersion returned {raw}, which does not decode to a \
         plausible CUDA major version ({major}). The ISA ceiling table in \
         jit_cuda::target reads this number directly."
    );
    assert!(
        minor <= 9,
        "cuDriverGetVersion returned {raw}, decoding to minor {minor}"
    );

    // And a driver this new must not resolve to the table's floor, which
    // is what a mis-parsed version would produce.
    assert!(
        raw >= 9000,
        "a driver older than CUDA 9.0 cannot run anything this crate emits"
    );
}
