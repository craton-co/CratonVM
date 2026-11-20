// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! intr21 (JIT r21 intrinsics page review): the single-pass `Arrays.sort(prim[])`
//! intrinsic writes the whole array in place with no `*astore` opcode, so it
//! owes the GPU input-residency barrier exactly as the `Arrays.fill` and
//! `System.arraycopy` arms emit it (`cratonvm_jit::gpu_barrier`). Before this
//! fix a `--gpu` run left the device mirror of a sorted array stale.
//!
//! # Why this is its own binary
//!
//! `gpu_barrier::arm` publishes two process-global atomics that every backend
//! reads at emit time (see `gpu_barrier_arming.rs`). A test that arms must not
//! share a process with another codegen test, so this file holds ONE `#[test]`
//! and nothing else. The expected values are derived from the barrier's
//! documented sequence (bucket = `(addr >> 3) & 63`), not measured.

use cratonvm_jit::x64::compile;
use cratonvm_jit::{gpu_barrier, JitDirectCall};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_LENGTH_OFFSET, HEADER_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Stub runtime helpers: no `Arrays.sort` intrinsic invokes a helper.
fn stub_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("Arrays.sort intrinsic test invoked an unwired runtime helper");
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

/// JIT-compile `void f(int[] a) { Arrays.sort(a); }` (the same synthetic
/// method `intrinsic_arrays_sort.rs` compiles).
fn compile_sort(entry: usize) -> impl Fn(*mut u8) {
    let code: Vec<u8> = vec![0x2a, 0xb8, 0x00, 0x01, 0xb1, 0, 0];
    let compiled = compile(
        &code,
        5,
        1, // num_params (the array ref)
        1, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            1,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 1,
                return_type: b'V',
                guard_class_id: 0,
            },
        )],
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &stub_helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("JIT compilation of Arrays.sort intrinsic failed");
    move |arr: *mut u8| {
        // SAFETY: `compiled` was produced by the JIT from valid bytecode and
        // the mmap region is executable. `arr` points at a live heap-layout
        // array object owned by the caller for the duration of the call.
        unsafe {
            compiled.try_call(&[arr as i64]).expect("test JIT call");
        }
    }
}

/// An 8-byte-aligned heap-layout `int[]` of `n` elements holding `n, n-1, .. 1`.
fn descending_int_array(n: usize) -> Vec<u64> {
    let byte_len = HEADER_SIZE + n * 4;
    let mut storage = vec![0u64; byte_len.div_ceil(8).max(1)];
    let base = storage.as_mut_ptr() as *mut u8;
    // SAFETY: every write is inside the freshly allocated, uniquely owned buffer.
    unsafe {
        (base.add(ARRAY_LENGTH_OFFSET) as *mut u32).write_unaligned(n as u32);
        for i in 0..n {
            (base.add(HEADER_SIZE + i * 4) as *mut i32).write_unaligned((n - i) as i32);
        }
    }
    storage
}

#[test]
fn intr21_arrays_sort_marks_the_gpu_dirty_bucket_when_the_barrier_is_armed() {
    let entry = cratonvm_jit::try_resolve_intrinsic("java/util/Arrays", "sort", "([I)V")
        .map(|(entry, _, _)| entry)
        .expect("Arrays.sort([I)V must register");

    // The residency filter word: every bucket "cached" (all ones). The dirty
    // table: 64 bytes, one per bucket. Both leaked so the emitted code's
    // immediates stay valid for the process.
    let filter: &'static AtomicUsize = Box::leak(Box::new(AtomicUsize::new(usize::MAX)));
    let dirty: &'static [AtomicU8; 64] =
        Box::leak(Box::new(std::array::from_fn(|_| AtomicU8::new(0))));
    assert!(
        gpu_barrier::barrier_bytes().is_none(),
        "a fresh process starts unarmed"
    );
    assert!(gpu_barrier::arm(
        filter as *const AtomicUsize as usize,
        dirty.as_ptr() as usize
    ));

    // 10 elements takes the emitted insertion sort, 100 the emitted heapsort.
    for n in [10usize, 100] {
        for d in dirty.iter() {
            d.store(0, Ordering::Relaxed);
        }
        let f = compile_sort(entry);
        let mut arr = descending_int_array(n);
        let ptr = arr.as_mut_ptr() as *mut u8;
        f(ptr);
        // Sorted ascending: 1..=n.
        for i in 0..n {
            // SAFETY: i < n, inside the buffer.
            let v = unsafe { (ptr.add(HEADER_SIZE + i * 4) as *const i32).read_unaligned() };
            assert_eq!(v, (i + 1) as i32, "n={n} element {i}");
        }
        let bucket = ((ptr as usize) >> 3) & 63;
        for (b, d) in dirty.iter().enumerate() {
            let want = u8::from(b == bucket);
            assert_eq!(
                d.load(Ordering::Relaxed),
                want,
                "n={n}: only the sorted array's bucket {bucket} may be marked (bucket {b})"
            );
        }
    }

    // A disarmed compile emits no barrier: the dirty table stays clean.
    assert!(!gpu_barrier::arm(0, 0));
    for d in dirty.iter() {
        d.store(0, Ordering::Relaxed);
    }
    let f = compile_sort(entry);
    let mut arr = descending_int_array(10);
    f(arr.as_mut_ptr() as *mut u8);
    assert!(dirty.iter().all(|d| d.load(Ordering::Relaxed) == 0));
}
