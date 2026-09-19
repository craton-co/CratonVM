// SPDX-License-Identifier: Apache-2.0
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

/// Compile `code` as a one-`int`-parameter static method with the given
/// direct calls, the safepoint flag at `flag` and `slow` as the poll's slow
/// path, and run it on `n`.
fn run_spin(
    code: &[u8],
    code_len: usize,
    direct_calls: Vec<(usize, JitDirectCall)>,
    flag: &'static u8,
    slow: extern "C" fn(),
    n: i64,
) -> i64 {
    // The self-tail form is OPT-IN (`CRATONVM_JIT_SELF_TAILCALL`, see
    // `x64/licm.rs::self_tailcall_enabled`); without it the site is the raw
    // recursive CALL and this file would be testing nothing. The gate latches
    // in a `OnceLock` on its first read, and in this test binary that first
    // read is one of these two tests, each of which installs the override
    // before compiling -- so the latch sees it whichever test runs first.
    let _tail_on = cratonvm_types::flags::override_thread(
        cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
            "CRATONVM_JIT_SELF_TAILCALL",
            Some("1"),
        )]),
    );
    let mut helpers = dummy_helpers();
    helpers.safepoint_flag_addr = flag as *const u8 as usize;
    helpers.safepoint_slow_path = slow as *const () as usize;
    let compiled = compile(
        code,
        code_len,
        1,
        1,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // invoke_info: none, so the raw invokestatic is the self-call
        direct_calls,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("JIT compilation of the self-tail-recursive spin failed");
    // SAFETY: machine code produced by the JIT from valid bytecode; the only
    // helper it can reach is `slow`, which takes and returns nothing.
    unsafe { compiled.try_call(&[n]).expect("test JIT call") }
}

/// `static int spin(int n) { if (n == 0) return 0; return spin(n - 1); }`
///
/// ```text
///  0: iload_0
///  1: ifne +5        -> 6
///  4: iconst_0
///  5: ireturn
///  6: iload_0
///  7: iconst_1
///  8: isub
///  9: invokestatic spin   (self; tail position)
/// 12: ireturn
/// ```
///
/// GC-inert: no allocation, no bytecode back-edge, no call but the self-call.
/// Such a method has no entry poll, so before the fix `spin(n)` reached the
/// slow path ZERO times however large `n` was. Now every tail iteration polls.
/// (If the self-tail gate did NOT latch on, the site is the raw recursive CALL
/// of a GC-inert method, which also polls zero times -- so a failure here with
/// `0` hits names either a regression or an environment that forced
/// `CRATONVM_JIT_SELF_TAILCALL` off.)
#[test]
fn a_gc_inert_self_tail_loop_polls_on_every_iteration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn slow_poll() {
        HITS.fetch_add(1, Ordering::SeqCst);
    }
    let code: Vec<u8> = vec![
        0x1a, 0x9a, 0x00, 0x05, 0x03, 0xac, 0x1a, 0x04, 0x64, 0xb8, 0x00, 0x01, 0xac, 0, 0,
    ];
    HITS.store(0, Ordering::SeqCst);
    let n = 7;
    let got = run_spin(&code, 13, Vec::new(), &FLAG, slow_poll, n);
    assert_eq!(got, 0, "spin(n) answers 0");
    let hits = HITS.load(Ordering::SeqCst);
    // `n` tail iterations, plus the entry poll if this build does not treat
    // the method as GC-inert (`CRATONVM_JIT_GC_INERT_SELFREC=0`).
    assert!(
        hits == n as usize || hits == n as usize + 1,
        "spin({n}) reached the safepoint slow path {hits} times; every one of its {n} \
         self-tail iterations is a back-edge and must poll"
    );
}

/// The same spin with `Math.abs(n)` in the recursive argument:
/// `static int spin(int n) { if (n == 0) return 0; return spin(Math.abs(n) - 1); }`
///
/// ```text
///  0: iload_0
///  1: ifne +5        -> 6
///  4: iconst_0
///  5: ireturn
///  6: iload_0
///  7: invokestatic Math.abs   (intrinsic direct call)
/// 10: iconst_1
/// 11: isub
/// 12: invokestatic spin       (self; tail position)
/// 15: ireturn
/// ```
///
/// A direct call makes it NOT GC-inert, so it keeps its method-entry poll and
/// the count is exact: one entry poll plus one per tail iteration. Before the
/// fix it was exactly one. (The raw recursive CALL form would also give
/// `n + 1`, one entry poll per activation; the GC-inert test above is the one
/// that tells the two forms apart.)
#[test]
fn a_self_tail_loop_polls_once_per_iteration_plus_the_entry_poll() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static FLAG: u8 = 1;
    static HITS: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn slow_poll() {
        HITS.fetch_add(1, Ordering::SeqCst);
    }
    let code: Vec<u8> = vec![
        0x1a, 0x9a, 0x00, 0x05, 0x03, 0xac, 0x1a, 0xb8, 0x00, 0x02, 0x04, 0x64, 0xb8, 0x00, 0x01,
        0xac, 0, 0,
    ];
    let direct_calls = vec![(
        7,
        JitDirectCall {
            entry: JitIntrinsic::MathAbsInt.as_entry(),
            needs_context: false,
            num_params: 1,
            return_type: b'I',
            guard_class_id: 0,
        },
    )];
    HITS.store(0, Ordering::SeqCst);
    let n = 5;
    let got = run_spin(&code, 16, direct_calls, &FLAG, slow_poll, n);
    assert_eq!(got, 0, "spin(n) answers 0");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        n as usize + 1,
        "one method-entry poll plus one poll per self-tail iteration"
    );
}
