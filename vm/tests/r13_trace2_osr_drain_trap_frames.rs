// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `try_osr`'s implicit-signal drains attach the compiled frames the trapping
//! helper snapshotted, for all three signals (round 13 wave 10, lane trace2;
//! `r13w10-orch-intermittent-empty-stack-trace-at-startup-under-g1`).
//!
//! A `/ by zero` or an AIOOBE raised in a compiled callee of an OSR'd loop
//! reaches the OSR exit as a bare flag when no dispatch helper built the
//! throwable on the way out. The NPE drain there took the helper's snapshot
//! and spliced it on; the other two built the throwable from a walk the
//! compiled frames had already left, so every such trace lacked them:
//! `R11RtFibTraceLines` read `recFrames 0` once `run` was OSR'd with `rec`
//! bound directly -- a compile-order race, so one run in about thirty.
//!
//! A text pin, because `try_osr` is a 1 800-line door no unit test can enter
//! and the shape that reaches the drain depends on which of two background
//! compiles finishes first. What it pins: in each drain the snapshot is taken
//! before the throwable is built and attached before the throwable is routed,
//! and the attach refuses the shared preallocated `OutOfMemoryError`.

const BRIDGE: &str = concat!(include_str!("../src/runtime/interpreter/jit_bridge/mod.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/jit_verdicts.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/osr_continuation.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/compile_osr_body.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/osr_exception_exit.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/try_osr.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/native_shadow_arms.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/compile_optimizing_artifact.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/never_run_static_exit_pcs.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/try_jit_compile_callee_slow.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/byte_array_field_declarer_admitted.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/folded_handle_member_row.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/callee_never_taken_rows.rs"), "\n", include_str!("../src/runtime/interpreter/jit_bridge/mint_ldc_slots_at_offer.rs"));

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

/// Offset of `needle` in `hay` at or after `from`, or a panic naming `what`.
fn find_after(hay: &str, from: usize, needle: &str, what: &str) -> usize {
    hay[from..]
        .find(needle)
        .map(|i| i + from)
        .unwrap_or_else(|| panic!("{what}: `{needle}` not found after offset {from}"))
}

#[test]
fn every_osr_drain_takes_and_attaches_its_trap_snapshot() {
    let src = BRIDGE.replace("\r\n", "\n");
    let try_osr = body_of(&src, "fn try_osr(");
    for (signal, drain, build, take) in [
        (
            "NullPointerException",
            "if crate::jit::helpers::take_jit_pending_npe() {",
            "throw_implicit_runtime_error(",
            "crate::jit::helpers::take_jit_pending_trap_frames()",
        ),
        (
            "ArrayIndexOutOfBoundsException",
            "if let Some((index, length)) = crate::jit::helpers::take_jit_pending_aioobe() {",
            "create_implicit_exception_object(",
            "take_osr_drain_trap_frames()",
        ),
        (
            "ArithmeticException",
            "if crate::jit::helpers::take_jit_pending_arithmetic() {",
            "throw_implicit_runtime_error(",
            "take_osr_drain_trap_frames()",
        ),
    ] {
        let at = find_after(&try_osr, 0, drain, signal);
        let taken = find_after(&try_osr, at, take, signal);
        let built = find_after(&try_osr, at, build, signal);
        let attached = find_after(&try_osr, at, "attach_osr_drain_trap_frames(", signal);
        let routed = find_after(&try_osr, at, "route_osr_exception_out_of_artifact(", signal);
        assert!(
            taken < built,
            "{signal}: the snapshot must be taken before the throwable is built -- the \
             construction walks a stack the compiled frames have left"
        );
        assert!(
            built < attached && attached < routed,
            "{signal}: the snapshot must be attached to the built throwable before it is routed"
        );
    }
}

#[test]
fn the_attach_skips_the_shared_out_of_memory_error() {
    let src = BRIDGE.replace("\r\n", "\n");
    let attach = body_of(&src, "fn attach_osr_drain_trap_frames(");
    assert!(
        attach.contains("singleton_oom"),
        "the preallocated OutOfMemoryError is shared by every later OOM throw; trap frames \
         spliced into its trace would name this trap in all of them"
    );
    let _take = body_of(&src, "fn take_osr_drain_trap_frames(");
}
