// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 2, lane `invoke2` — the inline PIC cascade's
//! SHARED post-call tail (`pic-cascade-repeats-the-post-call-tail-per-way`).
//!
//! Each of the four ways used to carry its own no-context marshalling block,
//! `CALL R11`, RBP republish, callee-deopt check and `JMP .done`. They now
//! branch (`JC .call` / `JMP .noctx`) to ONE tail emitted after the cascade.
//! What that can break, and what these tests execute:
//!
//! * every way still reaches its OWN target (the target stays in R11 across
//!   the jump to the shared tail);
//! * each way still selects the right ABI — a context callee gets
//!   `(vm, recv, x)`, a context-free one `(recv, x)` — so the argument arrives
//!   in the register the callee reads (the callee returns `x + K`, which is
//!   wrong under the other ABI);
//! * a way whose word is zero (a class-only profile seed) and an unknown
//!   receiver class still take the miss path (here: `invoke_dispatch`,
//!   stubbed to answer 777).

use cratonvm_jit::x64::compile_with_param_slots;
use cratonvm_jit::{CompiledMethod, JitInvokeInfo, JitPICSlot, JIT_IC_NEEDS_CONTEXT_TAG};
use cratonvm_jit_api::JitRuntimeHelpers;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;

const MISS: i64 = 777;

unsafe extern "C" fn miss_dispatch(
    _vm: i64,
    _info: *const JitInvokeInfo,
    _args: *const i64,
    _n: usize,
) -> i64 {
    MISS
}

fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("r9w2 invoke2 test invoked an unwired runtime helper");
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
        invoke_dispatch: miss_dispatch as *const () as usize,
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

/// Compile a two-parameter method (`this`, `int x`) from hand-built bytecode.
fn compile_method(
    code: &[u8],
    code_len: usize,
    needs_heap: bool,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    pic_slots: Vec<(usize, *const JitPICSlot)>,
    method_key: &str,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    compile_with_param_slots(
        // Not a door: hand-built bytecode with no method identity to admit.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code_len,
        2, // num_params: receiver, x
        2, // max_locals
        needs_heap,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        invoke_info,
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        pic_slots,
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,           // string_layout
        &[],            // param_jvm_slots: identity
        0,              // param_slot_span
        0,              // param_oop_mask
        Vec::new(),     // compact_field_info
        method_key,
        None,       // despec
        Vec::new(), // indy_info
        None,       // elidable_init_pcs
        cratonvm_jit::x64::BackendRequest::default(),
    )
    .unwrap_or_else(|| panic!("JIT compilation of {method_key} failed"))
}

/// `int m(int x) { return x + k; }` — `iload_1; bipush k; iadd; ireturn`.
fn compile_callee(k: i8, needs_heap: bool, helpers: &JitRuntimeHelpers) -> CompiledMethod {
    let code: Vec<u8> = vec![0x1b, 0x10, k as u8, 0x60, 0xac, 0, 0];
    compile_method(
        &code,
        5,
        needs_heap,
        Vec::new(),
        Vec::new(),
        "R9W2Callee.m:(I)I",
        helpers,
    )
}

/// A zeroed fake object whose header carries `class_id` at offset 0 and
/// `ObjectKind::Object` (0) in the kind byte.
fn fake_receiver(class_id: u32) -> Box<[u64; 8]> {
    let mut obj = Box::new([0u64; 8]);
    obj[0] = u64::from(class_id);
    obj
}

fn publish_way(pic: &JitPICSlot, way: usize, class_id: u32, callee: Option<&CompiledMethod>) {
    let word = callee.map_or(0, |c| {
        let entry = c.entry_ptr() as u64;
        if c.needs_context() {
            entry | JIT_IC_NEEDS_CONTEXT_TAG
        } else {
            entry
        }
    });
    // Word first, then the class id — the order `publish_way_locked` uses.
    pic.entry_words[way].store(word, Ordering::Release);
    pic.class_ids[way].store(class_id, Ordering::Release);
}

#[test]
fn every_pic_way_reaches_its_own_target_through_the_shared_tail() {
    let helpers = helpers();
    // Ways 0 and 2 context-free, ways 1 and 3 context-using: both ABI arms of
    // the shared tail, reached from both an early way and the fall-through
    // last way.
    let callees = [
        compile_callee(10, false, &helpers),
        compile_callee(20, true, &helpers),
        compile_callee(30, false, &helpers),
        compile_callee(40, true, &helpers),
    ];
    assert!(!callees[0].needs_context() && callees[1].needs_context());
    let class_ids = [101u32, 102, 103, 104];

    let pic = Box::new(JitPICSlot::new());
    for (way, callee) in callees.iter().enumerate() {
        publish_way(&pic, way, class_ids[way], Some(callee));
    }

    let info = Box::new(JitInvokeInfo {
        class_name: "R9W2Base",
        method_name: "m",
        descriptor: "(I)I",
        num_jit_args: 2, // receiver + x
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
    });
    // static int f(R9W2Base o, int x) { return o.m(x); }
    //   0: aload_0  1: iload_1  2: invokevirtual #1  5: ireturn
    let code: Vec<u8> = vec![0x2a, 0x1b, 0xb6, 0x00, 0x01, 0xac, 0, 0];
    let caller = compile_method(
        &code,
        6,
        true,
        vec![(2, &*info as *const JitInvokeInfo)],
        vec![(2, &*pic as *const JitPICSlot)],
        "R9W2Caller.f:(LR9W2Base;I)I",
        &helpers,
    );

    let vm_ptr = 0x1234_5678_i64;
    for (way, &cid) in class_ids.iter().enumerate() {
        let recv = fake_receiver(cid);
        let recv_ptr = recv.as_ptr() as i64;
        // SAFETY: machine code produced by the JIT from valid bytecode; the
        // receiver is a live, 8-byte-aligned buffer shaped like a header.
        let got =
            unsafe { caller.try_call_with_context(vm_ptr, &[recv_ptr, 5]) }.expect("test JIT call");
        let want = 5 + 10 * (way as i64 + 1);
        assert_eq!(
            got, want,
            "way {way} (class {cid}) must call its own callee"
        );
    }

    // An unknown receiver class misses every way and the hashed table.
    let stranger = fake_receiver(999);
    let got = unsafe { caller.try_call_with_context(vm_ptr, &[stranger.as_ptr() as i64, 5]) }
        .expect("test JIT call");
    assert_eq!(
        got, MISS,
        "an unknown receiver class must take the miss path"
    );

    // A class-only way (profile seed, zero word) must miss rather than CALL 0.
    publish_way(&pic, 1, class_ids[1], None);
    let seeded = fake_receiver(class_ids[1]);
    let got = unsafe { caller.try_call_with_context(vm_ptr, &[seeded.as_ptr() as i64, 5]) }
        .expect("test JIT call");
    assert_eq!(got, MISS, "a zero-word way must take the miss path");

    // The other ways are unaffected by that miss.
    let last = fake_receiver(class_ids[3]);
    let got = unsafe { caller.try_call_with_context(vm_ptr, &[last.as_ptr() as i64, 5]) }
        .expect("test JIT call");
    assert_eq!(got, 45);
    drop(callees);
}
