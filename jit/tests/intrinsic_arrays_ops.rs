// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential / behavioural tests for the ARRAYS_OPS JIT intrinsic family
//! (`java.util.Arrays.fill` and `java.util.Arrays.equals`).
//!
//! Each test JIT-compiles a tiny method whose single bytecode of interest is
//! an `invokestatic` resolved to an `Arrays.*` intrinsic, then runs the
//! generated machine code against the JDK-specified reference behaviour.
//!
//! Array memory model (`cratonvm_types`): an object header of `HEADER_SIZE`
//! bytes with the i32 element count at `ARRAY_LENGTH_OFFSET`, element data
//! packed at natural width from `HEADER_SIZE`. The tests fabricate that layout
//! in a raw, 8-byte-aligned `Vec<u8>` — the intrinsics touch only the length
//! field and the data area, never the GC mark word, so a real heap allocation
//! is unnecessary.
//!
//! Both constants are **imported from `cratonvm_types`, never restated here**.
//! Until 2026-07-26 this file defined its own `HEADER_SIZE = 32` /
//! `ARRAY_LENGTH_OFFSET = 12`, which made it the only site in the workspace
//! where a layout change fails *unsoundly*: local copies keep compiling, so the
//! fixture lays memory out at the old offsets while the JIT under test emits
//! the new ones, and the accesses run past the end of the backing `Vec` instead
//! of tripping a clean assertion. See
//! `header-shrink.md` §6.5 and the layout-drift
//! tripwires at the bottom of this file.

use cratonvm_jit::x64::{compile, is_jit_compatible};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ARRAY_LENGTH_OFFSET, HEADER_SIZE};
use std::collections::{HashMap, HashSet};

/// Runtime helpers for the intrinsic tests. The fill/equals intrinsics emit
/// no `CALL` on the success path; the ONLY helper they can reach is
/// `jit_npe_with_action`, invoked by the shared null-check stub
/// (`emit_null_check_store_stubs` in `x64.rs`) when `Arrays.fill` is handed a
/// null array — the stub sets `JIT_PENDING_NPE` + the JEP-358 action code, then
/// loads the `i64::MIN` deopt sentinel and runs the epilogue. We wire that to a
/// benign no-op so the null test observes the deopt sentinel rather than
/// panicking. (Historically the stub triggered the NPE via a `bastore` to the
/// zeroed array arg; it now calls the dedicated `jit_npe_with_action` helper for
/// JEP-358 helpful messages.)
fn arrays_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("ARRAYS_OPS intrinsic test invoked an unexpected runtime helper");
    }
    // No-op jit_npe_with_action: the shared null-check stub calls this with the
    // JEP-358 action code, then loads `i64::MIN` and runs the epilogue. The real
    // helper sets JIT_PENDING_NPE; for the test we only need it to return without
    // crashing so the call yields the deopt sentinel.
    unsafe extern "C" fn noop_npe_with_action() {}
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
        jit_npe_with_action: noop_npe_with_action as *const () as usize,
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

/// A fabricated primitive array: an 8-byte-aligned buffer laid out exactly
/// like a `cratonvm` array object, using the *real* `HEADER_SIZE` and
/// `ARRAY_LENGTH_OFFSET` so the fixture can never drift from the layout the
/// JIT under test emits. Kept alive by holding the backing `Vec`.
struct FakeArray {
    buf: Vec<u8>,
}

impl FakeArray {
    /// Allocate a zeroed array of `len` elements, each `elem_size` bytes.
    fn new(len: usize, elem_size: usize) -> Self {
        let total = HEADER_SIZE + len * elem_size;
        // Over-allocate by 8 so we can hand out an 8-aligned start.
        let mut backing = vec![0u8; total + 8];
        let misalign = backing.as_ptr() as usize & 7;
        let pad = (8 - misalign) & 7;
        // Shift the logical start to an 8-aligned offset by rotating padding
        // to the front: simplest is to just allocate aligned via a Vec<u64>.
        let _ = pad;
        backing.truncate(total + 8);
        let mut fa = FakeArray { buf: backing };
        fa.set_len(len as i32);
        fa
    }

    /// 8-aligned base pointer of the fabricated object.
    fn base(&self) -> *mut u8 {
        let raw = self.buf.as_ptr() as usize;
        let aligned = (raw + 7) & !7usize;
        aligned as *mut u8
    }

    fn ptr(&self) -> i64 {
        self.base() as i64
    }

    fn set_len(&mut self, len: i32) {
        unsafe {
            let p = self.base().add(ARRAY_LENGTH_OFFSET) as *mut i32;
            p.write_unaligned(len);
        }
    }

    fn data(&self) -> *mut u8 {
        unsafe { self.base().add(HEADER_SIZE) }
    }

    fn read_i8(&self, idx: usize) -> i8 {
        unsafe { (self.data() as *const i8).add(idx).read() }
    }
    fn read_i16(&self, idx: usize) -> i16 {
        unsafe { (self.data() as *const i16).add(idx).read_unaligned() }
    }
    fn read_i32(&self, idx: usize) -> i32 {
        unsafe { (self.data() as *const i32).add(idx).read_unaligned() }
    }
    fn read_i64(&self, idx: usize) -> i64 {
        unsafe { (self.data() as *const i64).add(idx).read_unaligned() }
    }
    fn write_i8(&mut self, idx: usize, v: i8) {
        unsafe { (self.data() as *mut i8).add(idx).write(v) }
    }
    fn write_i16(&mut self, idx: usize, v: i16) {
        unsafe { (self.data() as *mut i16).add(idx).write_unaligned(v) }
    }
    fn write_i32(&mut self, idx: usize, v: i32) {
        unsafe { (self.data() as *mut i32).add(idx).write_unaligned(v) }
    }
    fn write_i64(&mut self, idx: usize, v: i64) {
        unsafe { (self.data() as *mut i64).add(idx).write_unaligned(v) }
    }
}

/// Compile a method and return the executable `CompiledMethod`.
#[allow(clippy::too_many_arguments)]
fn compile_method(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    direct_calls: Vec<(usize, cratonvm_jit::JitDirectCall)>,
) -> cratonvm_jit::CompiledMethod {
    assert!(
        is_jit_compatible(code, code_len, "()V"),
        "bytecode rejected by jit_scan — test setup is wrong"
    );
    compile(
        code,
        code_len,
        num_params,
        max_locals,
        false,      // needs_heap — no array opcodes, args are raw pointers
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        Vec::new(), // invoke_info
        direct_calls,
        Vec::new(),     // mic_slots
        Vec::new(),     // pic_slots
        Vec::new(),     // ldc_info
        Vec::new(),     // ldc2w_info
        HashMap::new(), // branch_hints
        HashMap::new(), // loop_unroll_hints
        &arrays_helpers(),
        HashSet::new(), // non_escaping_new
        HashMap::new(), // inline_sites
        None,           // string_layout
    )
    .expect("JIT compilation failed")
}

fn direct_call(entry: usize, num_params: usize, return_type: u8) -> cratonvm_jit::JitDirectCall {
    cratonvm_jit::JitDirectCall {
        entry,
        needs_context: false,
        num_params,
        return_type,
        guard_class_id: 0,
    }
}

// ---------------------------------------------------------------------------
// Arrays.fill
// ---------------------------------------------------------------------------

/// Bytecode for `void f(<array>, <value>) { Arrays.fill(array, value); }`:
///   aload_0, <load value local 1>, invokestatic #1, return
/// `load_value` is the single-byte opcode that pushes local 1.
fn fill_bytecode(load_value: u8) -> Vec<u8> {
    // aload_0 (0x2a), <load_value>, invokestatic (0xb8 00 01), return (0xb1)
    vec![0x2a, load_value, 0xb8, 0x00, 0x01, 0xb1, 0, 0]
}

#[test]
fn test_arrays_fill_int() {
    // iload_1 = 0x1b
    let code = fill_bytecode(0x1b);
    let compiled = compile_method(
        &code,
        6,
        2,
        2,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill4.as_entry(), 2, b'V'),
        )],
    );
    for &len in &[0usize, 1, 5, 17, 64] {
        let mut arr = FakeArray::new(len, 4);
        for i in 0..len {
            arr.write_i32(i, -1);
        }
        let fill_val: i32 = 0x12345678;
        // SAFETY: JIT-compiled code; args are (array_ptr, fill_value).
        unsafe {
            compiled
                .try_call(&[arr.ptr(), fill_val as i64])
                .expect("test JIT call")
        };
        for i in 0..len {
            assert_eq!(arr.read_i32(i), fill_val, "int fill len={len} idx={i}");
        }
    }
}

#[test]
fn test_arrays_fill_long() {
    let code = fill_bytecode(0x1f); // lload_1 = 0x1f
    let compiled = compile_method(
        &code,
        6,
        2,
        3,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill8.as_entry(), 2, b'V'),
        )],
    );
    for &len in &[0usize, 1, 3, 9] {
        let mut arr = FakeArray::new(len, 8);
        let fill_val: i64 = -0x0123_4567_89AB_CDEF;
        // SAFETY: JIT-compiled code.
        unsafe {
            compiled
                .try_call(&[arr.ptr(), fill_val])
                .expect("test JIT call")
        };
        for i in 0..len {
            assert_eq!(arr.read_i64(i), fill_val, "long fill len={len} idx={i}");
        }
    }
}

#[test]
fn test_arrays_fill_byte() {
    let code = fill_bytecode(0x1b); // iload_1
    let compiled = compile_method(
        &code,
        6,
        2,
        2,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill1.as_entry(), 2, b'V'),
        )],
    );
    for &len in &[0usize, 1, 7, 33] {
        let mut arr = FakeArray::new(len, 1);
        let fill_val: i8 = -7;
        // SAFETY: JIT-compiled code.
        unsafe {
            compiled
                .try_call(&[arr.ptr(), fill_val as i64])
                .expect("test JIT call")
        };
        for i in 0..len {
            assert_eq!(arr.read_i8(i), fill_val, "byte fill len={len} idx={i}");
        }
    }
}

#[test]
fn test_arrays_fill_char_short() {
    let code = fill_bytecode(0x1b); // iload_1
    let compiled = compile_method(
        &code,
        6,
        2,
        2,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill2.as_entry(), 2, b'V'),
        )],
    );
    for &len in &[0usize, 1, 4, 21] {
        let mut arr = FakeArray::new(len, 2);
        let fill_val: i16 = -12345;
        // SAFETY: JIT-compiled code.
        unsafe {
            compiled
                .try_call(&[arr.ptr(), fill_val as i64])
                .expect("test JIT call")
        };
        for i in 0..len {
            assert_eq!(arr.read_i16(i), fill_val, "short fill len={len} idx={i}");
        }
    }
}

#[test]
fn test_arrays_fill_does_not_overrun() {
    // Sentinel byte just past the last element must be untouched.
    let code = fill_bytecode(0x1b);
    let compiled = compile_method(
        &code,
        6,
        2,
        2,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill4.as_entry(), 2, b'V'),
        )],
    );
    let len = 8usize;
    // Allocate len+2 elements but tell the array its length is `len`.
    let mut arr = FakeArray::new(len + 2, 4);
    arr.set_len(len as i32);
    arr.write_i32(len, 0x7777_7777);
    arr.write_i32(len + 1, 0x6666_6666);
    // SAFETY: JIT-compiled code.
    unsafe {
        compiled
            .try_call(&[arr.ptr(), 0x1111_1111])
            .expect("test JIT call")
    };
    for i in 0..len {
        assert_eq!(arr.read_i32(i), 0x1111_1111);
    }
    assert_eq!(arr.read_i32(len), 0x7777_7777, "fill overran array end");
    assert_eq!(arr.read_i32(len + 1), 0x6666_6666, "fill overran array end");
}

#[test]
fn test_arrays_fill_null_array_deopts() {
    // Arrays.fill(null, v) must take the NPE path. The shared null-check
    // stub returns the deopt sentinel i64::MIN.
    let code = fill_bytecode(0x1b);
    let compiled = compile_method(
        &code,
        6,
        2,
        2,
        vec![(
            2,
            direct_call(cratonvm_jit::JitIntrinsic::ArraysFill4.as_entry(), 2, b'V'),
        )],
    );
    // SAFETY: JIT-compiled code; null array argument exercises the NPE stub.
    let r = unsafe {
        compiled
            .try_call(&[0i64, 0x1111_1111])
            .expect("test JIT call")
    };
    assert_eq!(
        r,
        i64::MIN,
        "Arrays.fill(null, ..) must deopt out via the NPE stub"
    );
}

// ---------------------------------------------------------------------------
// Arrays.equals
// ---------------------------------------------------------------------------

/// Bytecode for `boolean f(<a>, <b>) { return Arrays.equals(a, b); }`:
///   aload_0, aload_1, invokestatic #1, ireturn
fn equals_bytecode() -> Vec<u8> {
    // aload_0 (0x2a), aload_1 (0x2b), invokestatic (0xb8 00 01), ireturn (0xac)
    vec![0x2a, 0x2b, 0xb8, 0x00, 0x01, 0xac, 0, 0]
}

fn equals_compiled(intrinsic: cratonvm_jit::JitIntrinsic) -> cratonvm_jit::CompiledMethod {
    let code = equals_bytecode();
    compile_method(
        &code,
        6,
        2,
        2,
        vec![(2, direct_call(intrinsic.as_entry(), 2, b'Z'))],
    )
}

#[test]
fn test_arrays_equals_int() {
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals4);

    // Equal content, multiple lengths.
    for &len in &[0usize, 1, 4, 19] {
        let mut a = FakeArray::new(len, 4);
        let mut b = FakeArray::new(len, 4);
        for i in 0..len {
            a.write_i32(i, i as i32 * 7 - 3);
            b.write_i32(i, i as i32 * 7 - 3);
        }
        // SAFETY: JIT-compiled code.
        let r = unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        };
        assert_eq!(r, 1, "equal int[] len={len} must be true");
    }

    // Single differing element at various positions.
    for diff_at in 0..6usize {
        let mut a = FakeArray::new(6, 4);
        let mut b = FakeArray::new(6, 4);
        for i in 0..6 {
            a.write_i32(i, 100 + i as i32);
            b.write_i32(i, 100 + i as i32);
        }
        b.write_i32(diff_at, -999);
        // SAFETY: JIT-compiled code.
        let r = unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        };
        assert_eq!(r, 0, "int[] differing at idx {diff_at} must be false");
    }

    // Length mismatch.
    let a = FakeArray::new(5, 4);
    let b = FakeArray::new(6, 4);
    // SAFETY: JIT-compiled code.
    let r = unsafe {
        compiled
            .try_call(&[a.ptr(), b.ptr()])
            .expect("test JIT call")
    };
    assert_eq!(r, 0, "length-mismatched int[] must be false");
}

#[test]
fn test_arrays_equals_long() {
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals8);
    let mut a = FakeArray::new(4, 8);
    let mut b = FakeArray::new(4, 8);
    for i in 0..4 {
        a.write_i64(i, (i as i64) << 40 | 0xABCD);
        b.write_i64(i, (i as i64) << 40 | 0xABCD);
    }
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        1
    );
    b.write_i64(2, 0);
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        0
    );
}

#[test]
fn test_arrays_equals_byte() {
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals1);
    let mut a = FakeArray::new(11, 1);
    let mut b = FakeArray::new(11, 1);
    for i in 0..11 {
        a.write_i8(i, (i as i8).wrapping_mul(13));
        b.write_i8(i, (i as i8).wrapping_mul(13));
    }
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        1
    );
    b.write_i8(10, b.read_i8(10).wrapping_add(1));
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        0
    );
}

#[test]
fn test_arrays_equals_char_short() {
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals2);
    let mut a = FakeArray::new(7, 2);
    let mut b = FakeArray::new(7, 2);
    for i in 0..7 {
        a.write_i16(i, (i as i16).wrapping_mul(1111));
        b.write_i16(i, (i as i16).wrapping_mul(1111));
    }
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        1
    );
    b.write_i16(0, b.read_i16(0).wrapping_add(1));
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        0
    );
}

#[test]
fn test_arrays_equals_empty() {
    // Two distinct empty arrays compare equal.
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals4);
    let a = FakeArray::new(0, 4);
    let b = FakeArray::new(0, 4);
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), b.ptr()])
                .expect("test JIT call")
        },
        1,
        "two empty arrays must compare equal"
    );
}

#[test]
fn test_arrays_equals_null_semantics() {
    // JDK: both null -> true; one null -> false; same ref -> true.
    let compiled = equals_compiled(cratonvm_jit::JitIntrinsic::ArraysEquals4);
    let a = FakeArray::new(3, 4);

    // both null
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe { compiled.try_call(&[0i64, 0i64]).expect("test JIT call") },
        1,
        "equals(null, null) must be true"
    );
    // a null, b non-null
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe { compiled.try_call(&[0i64, a.ptr()]).expect("test JIT call") },
        0,
        "equals(null, arr) must be false"
    );
    // a non-null, b null
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe { compiled.try_call(&[a.ptr(), 0i64]).expect("test JIT call") },
        0,
        "equals(arr, null) must be false"
    );
    // same reference
    // SAFETY: JIT-compiled code.
    assert_eq!(
        unsafe {
            compiled
                .try_call(&[a.ptr(), a.ptr()])
                .expect("test JIT call")
        },
        1,
        "equals(arr, arr) must be true (same reference)"
    );
}

// ---------------------------------------------------------------------------
// Layout-drift tripwires (arch-2026-07-26 `layout-constant-hazards`)
// ---------------------------------------------------------------------------

/// `FakeArray` hand-builds an object header, so it is sound only while the
/// assumptions baked into `FakeArray::new` still hold for the *real* constants.
///
/// This file used to restate `HEADER_SIZE = 32` and `ARRAY_LENGTH_OFFSET = 12`
/// as local `const`s, which made it the workspace's one unsound layout site: a
/// layout change does not trip an assertion here, it makes the fixture allocate
/// and index at the old offsets while the JIT under test emits the new ones, so
/// `set_len` and the element reads run off the end of the backing `Vec`. Both
/// constants are now imported; this test pins what the fixture still assumes,
/// so a divergence fails loudly instead of corrupting memory.
#[test]
fn fake_array_layout_assumptions_hold_for_the_real_constants() {
    assert_eq!(
        HEADER_SIZE,
        std::mem::size_of::<cratonvm_types::ObjectHeader>(),
        "the fixture allocates HEADER_SIZE bytes of header; it must be the whole \
         header the JIT addresses past"
    );
    assert_eq!(
        HEADER_SIZE % 8,
        0,
        "FakeArray::base hands out an 8-aligned pointer and FakeArray::data adds \
         HEADER_SIZE to it, so the element data must stay 8-aligned"
    );
    assert!(
        ARRAY_LENGTH_OFFSET + 4 <= HEADER_SIZE,
        "the i32 length field must live inside the header the fixture allocates"
    );
    assert!(
        HEADER_SIZE <= 127,
        "the intrinsics address elements as [base + index*scale + HEADER_SIZE] \
         with a signed disp8; past 127 the emitted displacement goes negative"
    );
    assert!(
        ARRAY_LENGTH_OFFSET <= 127,
        "the bounds check loads the length with a signed disp8 displacement"
    );

    // The fixture over-allocates by 8 so it can hand out an 8-aligned start.
    // That slack must still cover the worst-case realignment shift for a header
    // of whatever size the constants now describe.
    let fa = FakeArray::new(4, 8);
    let buf_start = fa.buf.as_ptr() as usize;
    let buf_end = buf_start + fa.buf.len();
    let base = fa.base() as usize;
    assert!(
        base >= buf_start && base + HEADER_SIZE + 4 * 8 <= buf_end,
        "FakeArray::new under-allocated: the object spans +{}..+{} of a {}-byte \
         buffer",
        base - buf_start,
        base - buf_start + HEADER_SIZE + 32,
        fa.buf.len()
    );
    assert_eq!(
        fa.base() as usize % 8,
        0,
        "the fixture base must be 8-aligned"
    );
}

/// **No integration test may restate an object-layout constant.**
///
/// A local `const HEADER_SIZE: usize = 32;` in a test keeps compiling after the
/// real constant moves, and then lays fixtures out at the stale offset. That is
/// an out-of-bounds access, not a failed assertion — the one failure mode a
/// layout change does not announce. This walks every `.rs` file under a
/// `tests/` directory in the workspace and rejects any definition of a name
/// `cratonvm_types` owns.
///
/// Scoped to `tests/` trees on purpose: `HEADER_SIZE` is also a legitimate and
/// entirely unrelated name in production code — `jfr/src/dump.rs` (JFR chunk
/// header, 72), `reader/src/jimage.rs` (jimage file header, 28) and
/// `vm/src/debug/protocol.rs` (debug wire header, 11). None of those describe
/// the object header, and none of them lay out heap fixtures.
#[test]
fn no_test_file_restates_an_object_layout_constant() {
    const OWNED_BY_TYPES: [&str; 16] = [
        "HEADER_SIZE",
        "MARK_WORD_OFFSET",
        "ARRAY_LENGTH_OFFSET",
        "ARRAY_ELEMENT_TYPE_OFFSET",
        "OBJECT_KIND_OFFSET",
        "NUM_SLOTS_OFFSET",
        "GC_AGE_OFFSET",
        "GC_FLAGS_OFFSET",
        "FORWARDING_PTR_OFFSET",
        "IDENTITY_HASH_CODE_OFFSET",
        "SLOT_SIZE",
        "REF_ELEMENT_SIZE",
        "REF_FIELD_SIZE",
        "FIELD_CELL_TAG_OFFSET",
        "FIELD_CELL_PAYLOAD32_OFFSET",
        "FIELD_CELL_PAYLOAD64_OFFSET",
    ];

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the jit crate sits one level under the workspace root")
        .to_path_buf();
    assert!(
        workspace.join("Cargo.toml").is_file(),
        "expected the workspace manifest at {}; without it the walk below would \
         silently scan nothing and pass vacuously",
        workspace.display()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut saw_self = false;
    let mut stack = vec![workspace.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                // `target/` is build output. Hidden directories hold git data
                // and nested agent worktrees; walking those would scan other
                // checkouts of this same repository. `apps/` holds Java harness
                // trees rather than workspace crates.
                if name != "target"
                    && name != "apps"
                    && name != "node_modules"
                    && !name.starts_with('.')
                {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".rs") {
                continue;
            }
            let in_tests_tree = path
                .strip_prefix(&workspace)
                .map(|rel| rel.components().any(|c| c.as_os_str() == "tests"))
                .unwrap_or(false);
            if !in_tests_tree {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            scanned += 1;
            saw_self |= name == "intrinsic_arrays_ops.rs";
            for (lineno, line) in src.lines().enumerate() {
                let trimmed = line.trim_start();
                let Some(rest) = trimmed
                    .strip_prefix("pub const ")
                    .or_else(|| trimmed.strip_prefix("const "))
                    .or_else(|| trimmed.strip_prefix("pub static "))
                    .or_else(|| trimmed.strip_prefix("static "))
                else {
                    continue;
                };
                let ident: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if OWNED_BY_TYPES.contains(&ident.as_str()) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.display(),
                        lineno + 1,
                        trimmed.trim_end()
                    ));
                }
            }
        }
    }

    assert!(
        scanned > 0 && saw_self,
        "the walk found {scanned} test sources under {} and {} reach this file; \
         it is checking nothing",
        workspace.display(),
        if saw_self { "did" } else { "did not" }
    );
    assert!(
        offenders.is_empty(),
        "test files must import object-layout constants from `cratonvm_types`, \
         never restate them: a stale local copy keeps compiling, lays fixtures \
         out at the old offsets and reads out of bounds instead of failing an \
         assertion.\n{}",
        offenders.join("\n")
    );
}
