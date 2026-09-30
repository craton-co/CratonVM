// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1's promoted-bytes counter on the SERIAL evacuator.
//!
//! `CRATONVM_G1_PARALLEL_EVAC=0` routes the pause through
//! `G1Collector::evacuate_object`, which stores its forwarding word rather than
//! CASing it and therefore has no losing arm to mis-count. This binary is the
//! half of the evidence that says the two evacuators agree on what a promotion
//! is: it runs the workload `g1_promoted_bytes_parallel` runs and asserts the
//! same two properties.
//!
//! An arm that counts a different quantity is the divergence class
//! `CRATONVM_G1_PARALLEL_EVAC` is supposed to be free of -- it changes how FAST
//! a pause runs, not what it reports.

mod g1_promoted_bytes_common;

/// See the twin in `g1_promoted_bytes_parallel` for why this goes through
/// `with_process_overrides` rather than `set_var`.
const ARM: &[(&str, Option<&str>)] = &[("CRATONVM_G1_PARALLEL_EVAC", Some("0"))];

#[test]
fn the_serial_evacuator_counts_the_bytes_it_promotes() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        let measured = g1_promoted_bytes_common::run();
        g1_promoted_bytes_common::assert_promotion_is_counted("serial evac", measured);
    });
}

#[test]
fn the_dispatcher_reports_what_the_serial_evacuator_counted() {
    cratonvm_types::flags::with_process_overrides(ARM, || {
        g1_promoted_bytes_common::assert_the_dispatcher_delegates("serial evac");
    });
}
