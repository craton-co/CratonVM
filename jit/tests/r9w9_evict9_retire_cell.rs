// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 9, lane `evict9` -- a baked direct call made
//! through a RETIRE cell
//! (`evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
//!
//! `static int f(int x) { return g(x); }` is compiled with its `invokestatic`
//! row marked `JIT_RETIRE_CELL_GUARD` (the row the compile doors plan for an
//! in-loop bind under `CRATONVM_JIT_RETIRE_CELL=1`) and executed:
//!
//! * before `f` is published its cell is empty, so the call DISPATCHES (the
//!   stubbed `invoke_dispatch` answers `5000 + x`);
//! * `f`'s publication fills the cell with the body it baked, so the call
//!   enters `g` directly (`7000 + x`, no dispatch);
//! * evicting `g` clears the cell, so the SAME compiled `f` -- a frame that
//!   would still be running it, in real life -- dispatches again instead of
//!   re-entering the withdrawn body.
//!
//! The third test (h23, 2026-09-22) is the one the review index asked for and
//! wave 9 did not write: the same withdrawal made CONCURRENTLY with threads
//! already inside the loop that reads the cell.

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{
    CompiledMethod, ExecutableBuffer, JitCache, JitCycleEdgeCell, JitDirectCall, JitInvokeInfo,
    JIT_RETIRE_CELL_GUARD,
};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const CTX: i64 = 0x1234_5678;

thread_local! {
    /// Per test THREAD: the stub runs on the thread that runs the compiled code.
    static DISPATCHES: Cell<usize> = const { Cell::new(0) };
}

unsafe extern "C" fn counting_dispatch(
    vm: i64,
    _info: *const JitInvokeInfo,
    args: *const i64,
    n: usize,
) -> i64 {
    DISPATCHES.with(|d| d.set(d.get() + 1));
    assert_eq!(vm, CTX, "the fallback passes the frame's VM context");
    assert_eq!(n, 1, "one argument");
    // SAFETY: the compiled caller passes its service range, `n` live slots.
    5000 + unsafe { *args }
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w9 evict9 test invoked an unwired runtime helper");
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
        invoke_dispatch: counting_dispatch as *const () as usize,
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

/// `g(ctx, x) = 7000 + x`, hand-encoded with the context entry ABI: the VM
/// context in the first argument register, `x` in the second.
fn callee_g() -> CompiledMethod {
    let mut buf = ExecutableBuffer::new(64).expect("executable buffer");
    #[cfg(target_os = "windows")]
    buf.emit(&[0x48, 0x8D, 0x82, 0x58, 0x1B, 0x00, 0x00]); // LEA RAX,[RDX+7000]
    #[cfg(not(target_os = "windows"))]
    buf.emit(&[0x48, 0x8D, 0x86, 0x58, 0x1B, 0x00, 0x00]); // LEA RAX,[RSI+7000]
    buf.emit(&[0xC3]); // RET
    CompiledMethod::new_with_context(buf)
}

fn g_info() -> Box<JitInvokeInfo> {
    Box::new(JitInvokeInfo {
        class_name: "R9W9Ev",
        method_name: "g",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
    })
}

/// `static int f(int x) { return g(x); }`
///   0: iload_0  1: invokestatic #1  4: ireturn
fn compile_f(info: &JitInvokeInfo, row: JitDirectCall) -> CompiledMethod {
    let code: Vec<u8> = vec![0x1a, 0xb8, 0x00, 0x01, 0xac, 0, 0];
    compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,          // num_params
        1,          // max_locals
        true,       // needs_heap: the fallback dispatch reads the VM pointer
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(1, info as *const JitInvokeInfo)],
        vec![(1, row)],     // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,           // string_layout
        &[],            // param_jvm_slots: identity
        0,              // param_slot_span
        0,              // param_oop_mask
        Vec::new(),
        "R9W9Ev.f:(I)I",
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .expect("JIT compilation of R9W9Ev.f failed")
}

/// Run `f(x)`; return (answer, dispatches made).
fn run(m: &CompiledMethod, x: i64) -> (i64, usize) {
    let before = DISPATCHES.with(Cell::get);
    // SAFETY: JIT code compiled by this test from valid bytecode; the only
    // things it can call are `g` (hand-encoded above, alive in the cache or
    // through the caller's root) and the counting dispatch stub.
    let got = unsafe { m.try_call_with_context(CTX, &[x]) }.expect("test JIT call");
    (got, DISPATCHES.with(Cell::get) - before)
}

#[test]
fn a_retire_cell_call_enters_the_pinned_body_until_its_eviction_then_dispatches() {
    let cid = cratonvm_types::ClassId::new(7);
    let cache = JitCache::new();
    cache.put(
        Arc::from("R9W9Ev"),
        Arc::from("g"),
        Arc::from("(I)I"),
        cid,
        callee_g(),
    );
    let g_entry = cache
        .get("R9W9Ev", "g", "(I)I", cid)
        .expect("g published")
        .entry_ptr() as usize;

    let info = g_info();
    let cell = JitCycleEdgeCell::new_retire(g_entry, "R9W9Ev", "g", "(I)I", true);
    let row = JitDirectCall {
        entry: Arc::as_ptr(&cell) as usize,
        needs_context: true,
        num_params: 1,
        return_type: b'I',
        guard_class_id: JIT_RETIRE_CELL_GUARD,
    };
    let mut f = compile_f(&info, row);
    let cell_bytes = (Arc::as_ptr(&cell) as u64).to_le_bytes();
    assert!(
        f._buffer_slice_for_debug()
            .windows(8)
            .any(|w| w == cell_bytes),
        "f's code must load its target through the cell"
    );

    // Unpublished: the cell is empty, the call dispatches.
    assert_eq!(run(&f, 3), (5003, 1));

    // Published: the cell holds g, the call is direct.
    f._jit_cycle_cells.push(Arc::clone(&cell));
    f._direct_callee_entries = vec![g_entry];
    cache.put(
        Arc::from("R9W9Ev"),
        Arc::from("f"),
        Arc::from("(I)I"),
        cid,
        f,
    );
    assert_eq!(cell.current_entry() as usize, g_entry);
    let f = cache.get("R9W9Ev", "f", "(I)I", cid).expect("f published");
    assert_eq!(run(&f, 3), (7003, 0));
    assert_eq!(run(&f, -10), (6990, 0));

    // g is evicted (and f with it). The running `f` must not re-enter g.
    cache.remove("R9W9Ev", "g", "(I)I", cid);
    assert_eq!(cell.current_entry(), 0);
    assert_eq!(run(&f, 3), (5003, 1));
    drop(f);
    cache.clear_all();
}

/// The unmarked row is the plain direct CALL, byte-for-byte what the doors
/// emitted before wave 9: it enters g with no dispatch, published or not.
#[test]
fn the_plain_row_calls_the_entry_directly() {
    let cid = cratonvm_types::ClassId::new(8);
    let cache = JitCache::new();
    cache.put(
        Arc::from("R9W9Ev2"),
        Arc::from("g"),
        Arc::from("(I)I"),
        cid,
        callee_g(),
    );
    let g = cache.get("R9W9Ev2", "g", "(I)I", cid).expect("g published");
    let info = g_info();
    let f = compile_f(
        &info,
        JitDirectCall {
            entry: g.entry_ptr() as usize,
            needs_context: true,
            num_params: 1,
            return_type: b'I',
            guard_class_id: 0,
        },
    );
    assert_eq!(run(&f, 4), (7004, 0));
    drop(f);
    drop(g);
    cache.clear_all();
}

/// The window wave 9 designed for but never executed: a withdrawal that lands
/// while other threads are *inside* the sequence that reads the cell.
///
/// The site is `mov r11,[cell] ; test r11,r11 ; jnz .call ; <dispatch setup> ;
/// .call: call r11`. Between the load and the `CALL` there is no blocking
/// point, so a thread that loaded a non-zero word is committed to entering
/// that body — after `JitCache::remove` has already cleared the cell and
/// released the callee. What makes that safe is that `JitCycleEdgeCell::clear`
/// hands the owner to the retirement queue (`defer_jit_owner`) instead of
/// dropping it, so the mapping outlives every thread that could be inside it.
/// This test is what fails if that ever becomes a plain drop: the loaded entry
/// would be unmapped under the `CALL`.
///
/// Each round publishes a fresh `g`, a fresh cell pinned to it and a fresh `f`
/// compiled against that cell, starts workers calling `f` in a loop while
/// holding the caller's `Arc` — wave 9's "a frame that would still be running
/// it" — and withdraws `g` underneath them. Every answer must be exactly one
/// of the two legal ones; a third value, or a fault, is the bug.
#[test]
fn a_withdrawal_under_running_callers_is_never_observed_half_done() {
    const ROUNDS: usize = 48;
    const WORKERS: usize = 3;

    let direct = AtomicUsize::new(0);
    let dispatched = AtomicUsize::new(0);
    let rounds_that_straddled = AtomicUsize::new(0);

    for _round in 0..ROUNDS {
        let cid = cratonvm_types::ClassId::new(9);
        let cache = JitCache::new();
        cache.put(
            Arc::from("R9W9Ev3"),
            Arc::from("g"),
            Arc::from("(I)I"),
            cid,
            callee_g(),
        );
        let g_entry = cache
            .get("R9W9Ev3", "g", "(I)I", cid)
            .expect("g published")
            .entry_ptr() as usize;

        let info = g_info();
        let cell = JitCycleEdgeCell::new_retire(g_entry, "R9W9Ev3", "g", "(I)I", true);
        let mut f = compile_f(
            &info,
            JitDirectCall {
                entry: Arc::as_ptr(&cell) as usize,
                needs_context: true,
                num_params: 1,
                return_type: b'I',
                guard_class_id: JIT_RETIRE_CELL_GUARD,
            },
        );
        f._jit_cycle_cells.push(Arc::clone(&cell));
        f._direct_callee_entries = vec![g_entry];
        cache.put(
            Arc::from("R9W9Ev3"),
            Arc::from("f"),
            Arc::from("(I)I"),
            cid,
            f,
        );
        let f = cache.get("R9W9Ev3", "f", "(I)I", cid).expect("f published");
        assert_eq!(cell.current_entry() as usize, g_entry);

        let stop = AtomicBool::new(false);
        let armed = AtomicUsize::new(0);
        let saw_direct = AtomicBool::new(false);
        let saw_dispatch = AtomicBool::new(false);

        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                let f = Arc::clone(&f);
                let (stop, armed) = (&stop, &armed);
                let (saw_direct, saw_dispatch) = (&saw_direct, &saw_dispatch);
                let (direct, dispatched) = (&direct, &dispatched);
                scope.spawn(move || {
                    // One call BEFORE arming, so the withdrawal below cannot
                    // race ahead of every worker's first answer and leave the
                    // round with nothing to compare against.
                    let mut x: i64 = 1;
                    let mut classify = |got: i64, x: i64| {
                        if got == 7000 + x {
                            saw_direct.store(true, Ordering::Relaxed);
                            direct.fetch_add(1, Ordering::Relaxed);
                        } else if got == 5000 + x {
                            saw_dispatch.store(true, Ordering::Relaxed);
                            dispatched.fetch_add(1, Ordering::Relaxed);
                        } else {
                            panic!(
                                "f({x}) answered {got}: neither the pinned body (7000+x) \
                                 nor the dispatch fallback (5000+x)"
                            );
                        }
                    };
                    classify(run(&f, x).0, x);
                    armed.fetch_add(1, Ordering::SeqCst);
                    while !stop.load(Ordering::Relaxed) {
                        x = (x + 1) & 0x3ff;
                        classify(run(&f, x).0, x);
                    }
                });
            }
            while armed.load(Ordering::SeqCst) < WORKERS {
                std::hint::spin_loop();
            }
            // Withdraw with every worker already in the loop.
            cache.remove("R9W9Ev3", "g", "(I)I", cid);
            assert_eq!(cell.current_entry(), 0, "the withdrawal clears the cell");
            // Let them run on past it, so the post-withdrawal answer is
            // observed in this round too.
            for _ in 0..200_000 {
                std::hint::spin_loop();
            }
            stop.store(true, Ordering::Relaxed);
        });

        if saw_direct.load(Ordering::Relaxed) && saw_dispatch.load(Ordering::Relaxed) {
            rounds_that_straddled.fetch_add(1, Ordering::Relaxed);
        }
        drop(f);
        cache.clear_all();
    }

    // Not a vacuous pass: the withdrawal has to have landed mid-loop often
    // enough that workers saw the pinned body before it AND the dispatch
    // fallback after it, in the same round.
    let straddled = rounds_that_straddled.load(Ordering::Relaxed);
    assert!(
        straddled * 2 >= ROUNDS,
        "only {straddled}/{ROUNDS} rounds straddled the withdrawal \
         (direct={}, dispatched={}); the window is not being exercised",
        direct.load(Ordering::Relaxed),
        dispatched.load(Ordering::Relaxed),
    );
}
