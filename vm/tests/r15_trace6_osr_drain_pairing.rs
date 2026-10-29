// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `try_osr`'s implicit-signal drains splice the trap snapshot with the OSR
//! body's `(frame, artifact)` pairing republished (round 15 wave 6, lane
//! trace6; `r15w5-throw1-compatible-osr-getfield-npe-reports-the-back-edge-line`).
//!
//! The body's `OsrContinuationGuard` is withdrawn when it returns, so the
//! splice's dedupe found the registry silent and took the pc-heuristic arm,
//! which drops the OSR entry WITHOUT handing its bci to the parked frame: the
//! caught NPE named the loop's back-edge line. A text pin, for the same reason
//! as `r13_trace2_osr_drain_trap_frames.rs`: `try_osr` cannot be entered from a
//! unit test.

const BRIDGE: &str = include_str!("../src/runtime/interpreter/jit_bridge.rs");

/// The body of `name` (a column-0 `fn` whose signature starts with `start`),
/// up to its first column-0 closing brace.
fn body_of(src: &str, start: &str) -> String {
    let at = src
        .find(start)
        .unwrap_or_else(|| panic!("`{start}` not found in jit_bridge.rs"));
    let rest = &src[at..];
    let end = rest.find("\n}\n").map_or(rest.len(), |e| e + 3);
    rest[..end].to_string()
}

#[test]
fn the_osr_drain_splice_republishes_the_body_pairing() {
    let src = BRIDGE.replace("\r\n", "\n");
    let attach = body_of(&src, "\nfn attach_osr_drain_trap_frames(");
    let publish = attach
        .find("OsrContinuationGuard::publish(")
        .expect("the splice republishes the OSR body's pairing");
    let splice = attach
        .find("attach_snapshotted_trap_frames(")
        .expect("the splice itself");
    assert!(
        publish < splice,
        "the pairing must be live while the snapshot is deduped"
    );
    assert!(
        attach.contains("\"CRATONVM_JIT_OSR_DRAIN_PAIRING\""),
        "the republish keeps its kill switch"
    );
}

#[test]
fn every_osr_drain_passes_the_entered_artifact() {
    let src = BRIDGE.replace("\r\n", "\n");
    let try_osr = body_of(&src, "\npub(super) fn try_osr(");
    let calls: Vec<&str> = try_osr
        .split("attach_osr_drain_trap_frames(")
        .skip(1)
        .collect();
    assert_eq!(calls.len(), 3, "the NPE, AIOOBE and `/ by zero` drains");
    for call in calls {
        let args = &call[..call.find(");").unwrap_or(call.len())];
        assert!(
            args.contains("Arc::as_ptr(&compiled)"),
            "each drain names the body it entered, the same pointer the entry published"
        );
    }
}
