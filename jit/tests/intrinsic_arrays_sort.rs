// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the ARRAYS_SORT JIT intrinsic family — the
//! `java.util.Arrays.sort(prim[])` single-argument overloads (Phase 4b).
//!
//! The intrinsic emits an in-place insertion sort directly into the JITted
//! code (no `CALL`). Each test JIT-compiles a tiny synthetic method
//!
//!     void f(int[] a) { Arrays.sort(a); }
//!
//! constructs a heap-layout array object in Rust-owned memory, passes its
//! pointer as the method argument, runs the JITted code, and asserts the
//! element data is now sorted ascending — bit-identical to what
//! `<[T]>::sort()` produces. The input matrix covers every edge case the
//! task calls out: empty, single element, already sorted, reverse sorted,
//! duplicates, and negative values.
//!
//! ## Heap-object layout
//!
//! The JIT reads an array as: a fixed `HEADER_SIZE`-byte object header with
//! the element count at `ARRAY_LENGTH_OFFSET`, followed by tightly packed
//! element data starting at `HEADER_SIZE`. We synthesize exactly that
//! layout in an 8-byte-aligned buffer and hand the JIT a raw pointer to it,
//! so the inline sort mutates our buffer directly.
//!
//! The stub `JitRuntimeHelpers` is never invoked: insertion sort touches
//! only the array memory and uses no runtime helper or call site.

use cratonvm_jit::x64::compile;
use cratonvm_jit::JitDirectCall;
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_LENGTH_OFFSET, HEADER_SIZE};
use std::collections::{HashMap, HashSet};

/// Stub runtime helpers — no ARRAYS_SORT intrinsic invokes a helper.
fn stub_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("ARRAYS_SORT intrinsic test invoked an unwired runtime helper");
    }
    let s = stub as *const () as usize;
    // `set_throw_bci` only records the throwing bci in a thread-local; the
    // backend calls it on the throw path of EVERY method that has an exception
    // check, so reaching it is normal rather than a sign of missing wiring.
    // Give it a real no-op instead of the panicking stub.
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
        // Reached through emit_call_absolute, so a 0 here is a null CALL
        // (SIGSEGV), not an inert "unwired" sentinel. Neither is a sign of
        // missing wiring: `set_throw_bci` just records the throwing bci, and
        // `service_callee_deopt` is the normal IR direct-call path when a
        // callee returns the i64::MIN "threw" sentinel. The deopt stub returns
        // that sentinel unchanged -- exactly what the real helper does for a
        // vm/info it cannot service -- so the caller propagates the throw.
        set_throw_bci: throw_bci,
        service_callee_deopt: deopt_unserviceable,
        ..Default::default()
    }
}

/// An 8-byte-aligned heap-layout array object: a `HEADER_SIZE` header (with
/// the element count written at `ARRAY_LENGTH_OFFSET`) followed by packed
/// element data. Keeps the backing storage alive for the call's duration.
struct FakeArray {
    /// `u64`-backed for guaranteed 8-byte alignment (long[] needs it).
    storage: Vec<u64>,
    byte_len: usize,
}

impl FakeArray {
    /// Build an array object holding `n` elements of `elem_size` bytes each.
    fn new(n: usize, elem_size: usize) -> Self {
        let byte_len = HEADER_SIZE + n * elem_size;
        let words = byte_len.div_ceil(8).max(1);
        let mut storage = vec![0u64; words];
        // Write the element count into the header as a 32-bit field.
        let base = storage.as_mut_ptr() as *mut u8;
        // SAFETY: `base + ARRAY_LENGTH_OFFSET` is within the allocation
        // (ARRAY_LENGTH_OFFSET < HEADER_SIZE <= byte_len), and writes through
        // a freshly allocated, uniquely owned buffer.
        unsafe {
            (base.add(ARRAY_LENGTH_OFFSET) as *mut u32).write_unaligned(n as u32);
        }
        FakeArray { storage, byte_len }
    }

    /// Raw pointer to the object header — what the JIT sees as the array ref.
    fn ptr(&self) -> *mut u8 {
        self.storage.as_ptr() as *mut u8
    }

    /// Pointer to element index `i` of `elem_size` bytes.
    fn elem_ptr(&self, i: usize, elem_size: usize) -> *mut u8 {
        let off = HEADER_SIZE + i * elem_size;
        assert!(off + elem_size <= self.byte_len, "element OOB");
        // SAFETY: bounds-checked against the allocation length above.
        unsafe { self.ptr().add(off) }
    }
}

/// JIT-compile `void f(<array>) { Arrays.sort(arr); }`.
///
/// Bytecode: `aload_0` (0x2a), `invokestatic` (0xb8 0x00 0x01),
/// `return` (0xb1). The `invokestatic` opcode sits at pc 1 — that is the
/// `direct_calls` key. The method takes one reference parameter and is void.
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

/// Resolve an `Arrays.sort` overload to its intrinsic entry sentinel.
fn resolve(descriptor: &str) -> Option<usize> {
    cratonvm_jit::try_resolve_intrinsic("java/util/Arrays", "sort", descriptor)
        .map(|(entry, _, _)| entry)
}

/// The shared edge-case input matrix: empty, single, sorted, reverse,
/// duplicates, negatives, and a longer mixed run that exceeds any plausible
/// "tiny array" threshold (the intrinsic has no threshold — it must sort
/// arrays of every length correctly).
fn input_cases() -> Vec<Vec<i64>> {
    vec![
        vec![],
        vec![42],
        vec![1, 2, 3, 4, 5],
        vec![5, 4, 3, 2, 1],
        vec![3, 1, 3, 2, 1, 3, 2],
        vec![-5, 3, -1, 0, -100, 7, -7],
        vec![0, 0, 0, 0],
        vec![-1, -2, -3],
        (0..40).rev().collect(),
        vec![7, -3, 7, 7, -3, 0, 100, -100, 50, 50, 1, -1],
    ]
}

#[test]
fn arrays_sort_int_matches_reference() {
    let entry = resolve("([I)V").expect("Arrays.sort([I)V must register");
    let f = compile_sort(entry);
    for case in input_cases() {
        let n = case.len();
        let arr = FakeArray::new(n, 4);
        for (i, &v) in case.iter().enumerate() {
            // SAFETY: index < n; elem_ptr bounds-checks the offset.
            unsafe {
                (arr.elem_ptr(i, 4) as *mut i32).write_unaligned(v as i32);
            }
        }
        f(arr.ptr());
        let mut got: Vec<i32> = (0..n)
            // SAFETY: index < n.
            .map(|i| unsafe { (arr.elem_ptr(i, 4) as *const i32).read_unaligned() })
            .collect();
        let mut want: Vec<i32> = case.iter().map(|&v| v as i32).collect();
        want.sort();
        assert_eq!(got, want, "sort([I) mismatch on {case:?}");
        got.clear();
    }
}

#[test]
fn arrays_sort_long_matches_reference() {
    let entry = resolve("([J)V").expect("Arrays.sort([J)V must register");
    let f = compile_sort(entry);
    // Long-specific values that exercise the full 64-bit signed range.
    let mut cases = input_cases();
    cases.push(vec![i64::MAX, i64::MIN, 0, -1, 1]);
    cases.push(vec![i64::MIN, i64::MIN, i64::MAX, i64::MAX]);
    for case in cases {
        let n = case.len();
        let arr = FakeArray::new(n, 8);
        for (i, &v) in case.iter().enumerate() {
            // SAFETY: index < n.
            unsafe {
                (arr.elem_ptr(i, 8) as *mut i64).write_unaligned(v);
            }
        }
        f(arr.ptr());
        let got: Vec<i64> = (0..n)
            // SAFETY: index < n.
            .map(|i| unsafe { (arr.elem_ptr(i, 8) as *const i64).read_unaligned() })
            .collect();
        let mut want = case.clone();
        want.sort();
        assert_eq!(got, want, "sort([J) mismatch on {case:?}");
    }
}

#[test]
fn arrays_sort_char_matches_reference() {
    // char is unsigned 0..=65535 — sorted by unsigned value.
    let entry = resolve("([C)V").expect("Arrays.sort([C)V must register");
    let f = compile_sort(entry);
    let cases: Vec<Vec<u16>> = vec![
        vec![],
        vec![1000],
        vec![5, 4, 3, 2, 1],
        vec![3, 1, 3, 2, 1],
        // 0xFFFF would be negative as i16 — must sort LAST as a char.
        vec![0xFFFF, 0, 0x8000, 1, 0x7FFF],
        (0..40u16).rev().collect(),
    ];
    for case in cases {
        let n = case.len();
        let arr = FakeArray::new(n, 2);
        for (i, &v) in case.iter().enumerate() {
            // SAFETY: index < n.
            unsafe {
                (arr.elem_ptr(i, 2) as *mut u16).write_unaligned(v);
            }
        }
        f(arr.ptr());
        let got: Vec<u16> = (0..n)
            // SAFETY: index < n.
            .map(|i| unsafe { (arr.elem_ptr(i, 2) as *const u16).read_unaligned() })
            .collect();
        let mut want = case.clone();
        want.sort();
        assert_eq!(got, want, "sort([C) mismatch on {case:?}");
    }
}

#[test]
fn arrays_sort_short_matches_reference() {
    // short is signed -32768..=32767.
    let entry = resolve("([S)V").expect("Arrays.sort([S)V must register");
    let f = compile_sort(entry);
    let cases: Vec<Vec<i16>> = vec![
        vec![],
        vec![-1],
        vec![5, 4, 3, 2, 1],
        vec![3, -1, 3, -2, 1],
        vec![i16::MIN, i16::MAX, 0, -1, 1],
        (-20..20i16).rev().collect(),
    ];
    for case in cases {
        let n = case.len();
        let arr = FakeArray::new(n, 2);
        for (i, &v) in case.iter().enumerate() {
            // SAFETY: index < n.
            unsafe {
                (arr.elem_ptr(i, 2) as *mut i16).write_unaligned(v);
            }
        }
        f(arr.ptr());
        let got: Vec<i16> = (0..n)
            // SAFETY: index < n.
            .map(|i| unsafe { (arr.elem_ptr(i, 2) as *const i16).read_unaligned() })
            .collect();
        let mut want = case.clone();
        want.sort();
        assert_eq!(got, want, "sort([S) mismatch on {case:?}");
    }
}

#[test]
fn arrays_sort_byte_matches_reference() {
    // byte is signed -128..=127.
    let entry = resolve("([B)V").expect("Arrays.sort([B)V must register");
    let f = compile_sort(entry);
    let cases: Vec<Vec<i8>> = vec![
        vec![],
        vec![7],
        vec![5, 4, 3, 2, 1],
        vec![3, -1, 3, -2, 1],
        vec![i8::MIN, i8::MAX, 0, -1, 1],
        (-30..30i8).rev().collect(),
    ];
    for case in cases {
        let n = case.len();
        let arr = FakeArray::new(n, 1);
        for (i, &v) in case.iter().enumerate() {
            // SAFETY: index < n.
            unsafe {
                (arr.elem_ptr(i, 1) as *mut i8).write_unaligned(v);
            }
        }
        f(arr.ptr());
        let got: Vec<i8> = (0..n)
            // SAFETY: index < n.
            .map(|i| unsafe { (arr.elem_ptr(i, 1) as *const i8).read_unaligned() })
            .collect();
        let mut want = case.clone();
        want.sort();
        assert_eq!(got, want, "sort([B) mismatch on {case:?}");
    }
}

#[test]
fn arrays_sort_only_registers_integral_single_arg_overloads() {
    // The five integral single-arg overloads are intrinsified.
    for d in ["([I)V", "([J)V", "([C)V", "([S)V", "([B)V"] {
        assert!(
            resolve(d).is_some(),
            "Arrays.sort{d} should be registered as an intrinsic"
        );
    }
    // float[]/double[] are out of scope (NaN / -0.0 ordering) — must bail.
    for d in ["([F)V", "([D)V"] {
        assert!(
            resolve(d).is_none(),
            "Arrays.sort{d} must NOT be intrinsified (FP ordering subtleties)"
        );
    }
    // Reference-array sort and the 3-arg range overloads are out of scope.
    for d in ["([Ljava/lang/Object;)V", "([II I)V", "([III)V", "([JII)V"] {
        assert!(
            resolve(d).is_none(),
            "Arrays.sort{d} must NOT be intrinsified (out of scope)"
        );
    }
    // A non-Arrays class with the same method name must not match.
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/util/Collections", "sort", "([I)V").is_none(),
        "only java/util/Arrays.sort is an ARRAYS_SORT intrinsic"
    );
}
