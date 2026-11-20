// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 5, lane `collect5` — a self-recursive TAIL call is
//! a loop, and a loop polls.
//!
//! Under `CRATONVM_JIT=self-tailcall` (opt-in since 2026-08-20) the
//! single-pass backend lowers `invokestatic self; xreturn` to parameter
//! moves plus a `JMP` to `body_entry_offset`, which is taken AFTER the prologue
//! and so after the method-entry safepoint poll. A method whose only loop is
//! its own tail recursion therefore ran with no poll at all:
//! `spin(Integer.MAX_VALUE)` held off every stop-the-world for its whole
//! length (`NOTES-invoke.md`, proposal 5). The fix re-emits the entry poll on
//! the tail back-edge (`op_invoke.rs`, `emit_self_tail_safepoint_poll`).
//!
//! Both shapes are executed, with the safepoint flag permanently raised and a
//! slow path that counts:
//!
//! * a GC-INERT spin (no allocation, no bytecode back-edge, no other call),
//!   which has no method-entry poll at all and takes the
//!   `emit_pre_safepoint_spill_without_shadow` protocol;
//! * the same spin with one `Math.abs` intrinsic call in it, which is not
//!   GC-inert and keeps its entry poll, so the count is exact.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{JitDirectCall, JitIntrinsic};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};

/// Stub helpers, as `r9_invoke_int_results_and_self_tail_call.rs` builds
/// them. Any helper this test does not wire panics if called.
fn dummy_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w5 collect5 test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    unsafe extern "C" fn record_throw_bci(_bci: i64) {}
    let throw_bci = record_throw_bci as *const () as usize;
    unsafe extern "C" fn deopt_unserviceable_stub(
        _vm: i64,
        _info: i64,
        _args: i64,
        _n: i64,
    ) -> i64 {
        i64::MIN
    }
    let deopt_unserviceable = deopt_unserviceable_stub as *const () as usize;
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_card_table_addr: 0,
        jit_card_old_base: 0,
        jit_card_old_end: 0,
        newarray: s,
        new_object: s,
        anewarray_object: s,
        baload: s,
        bastore: s,
        iaload: s,
        iastore: s,
        aaload: s,
        aastore: s,
        multianewarray_2d: s,
        arraylength: s,
        getfield: s,
        putfield_int: s,
        putfield_long: s,
        putfield_float: s,
        putfield_double: s,
        putfield_object: s,
        getstatic: s,
        putstatic_int: s,
        putstatic_long: s,
        putstatic_float: s,
        putstatic_double: s,
        putstatic_object: s,
        checkcast: s,
        instanceof_check: s,
        throw_aioobe: s,
        throw_arithmetic: s,
        invoke_dispatch: s,
        invoke_virtual_mic: s,
        lambda_int_to_double: s,
        write_barrier: s,
        satb_pre_write_barrier: s,
        uncommon_trap: s,
        math_fma_double: s,
        math_fma_float: s,
        tlab_cursor_offset_in_thread: 0,
        tlab_end_offset_in_thread: 8,
        class_id_offset_in_obj: 0,
        get_current_thread: 0,
        tlab_post_init: 0,
        frame_record: 0,
        shadow_stack_offset_in_thread: 0,
        throw_exception: s,
        jit_npe_with_action: s,
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: 0,
        region_bounds_addr: 0,
        native_stack_floor_fn: 0,
        ldc_string: s,
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

