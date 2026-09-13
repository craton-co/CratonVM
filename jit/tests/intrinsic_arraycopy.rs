// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Differential tests for the ARRAYCOPY JIT intrinsic family — the
//! `java.lang.System.arraycopy` call-site intrinsic (Phase 2).
//!
//! Unlike the pure-leaf bit-op intrinsics, `arraycopy` touches the heap: the
//! emitted code dereferences real array objects laid out exactly like the
//! VM's `ObjectHeader` (`cratonvm_types::heap_types`). Each test therefore
//! builds a byte buffer with that precise layout — a `HEADER_SIZE`-byte header
//! followed by compact element data — passes raw pointers to the JIT-compiled
//! wrapper,
//! and asserts the destination buffer matches a host-computed reference.
//!
//! Coverage:
//!   * forward copy between two distinct primitive arrays,
//!   * overlapping copy within one array, dst > src (must copy backward),
//!   * overlapping copy within one array, dst < src (forward is correct),
//!   * empty copy (len == 0) — a no-op,
//!   * out-of-bounds copy — must NOT corrupt memory; the inline guard
//!     branches to the uncommon-trap deopt stub (the interpreter then
//!     re-runs the call via native `System.arraycopy`, which throws AIOOBE),
//!   * the matcher registers exactly the one type-erased descriptor.
//!
//! The inline fast path runs entirely without calling a runtime helper, so a
//! stub `JitRuntimeHelpers` is sufficient. The single exception is the
//! out-of-bounds test, whose deopt stub does call `uncommon_trap`; a counting
//! stub there confirms the trap fired and that no copy was performed.

use cratonvm_jit::x64::{compile, compile_with_param_slots};
use cratonvm_jit::{CompiledMethod, JitDirectCall, JitInvokeInfo};
use cratonvm_jit_api::JitRuntimeHelpers;
use cratonvm_types::{ArrayElementType, ClassId, ObjectHeader, ObjectKind, HEADER_SIZE};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Records every `uncommon_trap` invocation so the deopt tests can assert the
/// inline guard bailed instead of copying. `uncommon_trap` is a fixed
/// `extern "C"` pointer baked into every compiled method, so the counter must
/// be a process-global; the `DEOPT_LOCK` below serialises the deopt tests so
/// their before/after reads of this counter are not interleaved.
static TRAP_COUNT: AtomicU64 = AtomicU64::new(0);

/// Serialises the deopt-path tests. They share the global `TRAP_COUNT`, so
/// without this lock two of them running in parallel would each observe the
/// other's increment and the `before + 1` assertion would spuriously fail.
static DEOPT_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DispatchRecord {
    vm_ptr: i64,
    info_ptr: usize,
    num_args: usize,
    args: [i64; 5],
    return_type: u8,
    invoke_kind: u8,
}

static DISPATCH_RECORD: Mutex<Option<DispatchRecord>> = Mutex::new(None);

fn deopt_lock() -> MutexGuard<'static, ()> {
    DEOPT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clear_deopt_signals() -> u64 {
    let _ = cratonvm_jit::deopt::take_last_deopt();
    TRAP_COUNT.load(Ordering::SeqCst)
}

fn assert_no_deopt_after(before_traps: u64, context: &str) {
    let trap_delta = TRAP_COUNT
        .load(Ordering::SeqCst)
        .saturating_sub(before_traps);
    let frame_deopt = cratonvm_jit::deopt::take_last_deopt();
    assert_eq!(
        trap_delta, 0,
        "{context} must not trigger an uncommon-trap deopt"
    );
    assert!(
        frame_deopt.is_none(),
        "{context} must not trigger a real-frame deopt"
    );
}

fn assert_one_deopt_after(before_traps: u64, context: &str) {
    let trap_delta = TRAP_COUNT
        .load(Ordering::SeqCst)
        .saturating_sub(before_traps);
    let frame_deopt = cratonvm_jit::deopt::take_last_deopt();
    let signal_count = trap_delta + u64::from(frame_deopt.is_some());
    assert_eq!(
        signal_count,
        1,
        "{context} must trigger exactly one deopt; legacy_traps={trap_delta}, real_frame_deopt={}",
        frame_deopt.is_some()
    );
}

unsafe extern "C" fn recording_uncommon_trap(_vm: i64, _reason: i64, _bci: i64) -> i64 {
    TRAP_COUNT.fetch_add(1, Ordering::SeqCst);
    0 // deopt action code; the JIT stub then returns i64::MIN itself
}

/// Runtime helpers. The arraycopy fast path never calls a helper; only the
/// deopt stub calls `uncommon_trap`, so that one slot is wired to a real
/// recording function and the rest are panic stubs.
fn helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn stub() {
        panic!("ARRAYCOPY intrinsic test invoked an unwired runtime helper");
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
        uncommon_trap: recording_uncommon_trap as *const () as usize,
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

fn helpers_with_dispatch(dispatch: usize) -> JitRuntimeHelpers {
    let mut h = helpers();
    h.invoke_dispatch = dispatch;
    h
}

unsafe extern "C" fn recording_invoke_dispatch(
    vm_ptr: i64,
    info: *const JitInvokeInfo,
    args_ptr: *const i64,
    num_args: usize,
) -> i64 {
    let mut args = [0i64; 5];
    if !args_ptr.is_null() {
        let n = num_args.min(args.len());
        unsafe {
            std::ptr::copy_nonoverlapping(args_ptr, args.as_mut_ptr(), n);
        }
    }

    let (return_type, invoke_kind) = if info.is_null() {
        (0, 0)
    } else {
        unsafe { ((*info).return_type, (*info).invoke_kind) }
    };

    let mut record = DISPATCH_RECORD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *record = Some(DispatchRecord {
        vm_ptr,
        info_ptr: info as usize,
        num_args,
        args,
        return_type,
        invoke_kind,
    });
    0
}

/// A heap array object laid out byte-for-byte like the VM's compact array:
/// a `HEADER_SIZE`-byte `ObjectHeader` followed by tightly packed element
/// data. Backed by a `Vec<u64>` so the base address is 8-byte aligned.
struct FakeArray {
    storage: Vec<u64>,
    elem_size: usize,
    length: usize,
}

impl FakeArray {
    /// Build an array of `length` elements of the given primitive kind,
    /// element data initialised from `init` (one byte value per element,
    /// broadcast across the element's bytes is *not* done — only byte arrays
    /// use 1-byte elements; `init` is written into the low byte and the rest
    /// zeroed, which is enough to make the differential comparison precise).
    fn new(kind: ArrayElementType, elem_size: usize, length: usize) -> Self {
        let data_bytes = length * elem_size;
        let total = HEADER_SIZE + data_bytes;
        let words = total.div_ceil(8).max(1);
        let mut storage = vec![0u64; words];
        // Build the header through `ObjectHeader::new` rather than writing its
        // fields at literal offsets.
        //
        // This fixture used to poke `kind` at offset 4, `element_type` at
        // offset 5 and `array_length` at offset 12 — the pre-`header-16`
        // layout. The 2026-08-07 shrink (24 -> 16) moved every one of them:
        // `shape` now occupies offsets 4..8 and IS the array length for an
        // array, and the kind/element_type/gc_age/gc_flags quartet moved into
        // `mark_word` bits 48..63. Those writes therefore set the array's
        // length to `ObjectKind::Array as u8` == 1 and left the element type
        // unset, so the intrinsic's inline guard saw a 1-element array of the
        // wrong kind, deopted instead of copying, and every value assertion
        // failed against an untouched destination — a red that reads exactly
        // like an arraycopy miscompile but was entirely in the fixture.
        //
        // Restating a layout in a test is what let that happen; going through
        // the constructor is what stops the next shrink from doing it again.
        let base = storage.as_mut_ptr() as *mut ObjectHeader;
        unsafe {
            // Primitive arrays carry ClassId(0). `num_slots` is unused for an
            // array shape — the constructor stores `array_length` there.
            std::ptr::write(
                base,
                ObjectHeader::new(
                    ClassId::new(0),
                    ObjectKind::Array,
                    kind,
                    length as u32, // Cast: fixture array lengths are small
                    0,
                ),
            );
        }
        FakeArray {
            storage,
            elem_size,
            length,
        }
    }

    fn ptr(&self) -> i64 {
        self.storage.as_ptr() as i64
    }

    fn data_ptr(&self) -> *mut u8 {
        unsafe { (self.storage.as_ptr() as *mut u8).add(HEADER_SIZE) }
    }

    /// Write element `idx` from the low `elem_size` bytes of `value`.
    fn set(&mut self, idx: usize, value: u64) {
        assert!(idx < self.length);
        let p = self.data_ptr();
        let le = value.to_le_bytes();
        unsafe {
            std::ptr::copy_nonoverlapping(le.as_ptr(), p.add(idx * self.elem_size), self.elem_size);
        }
    }

    /// Read element `idx` zero-extended into a u64.
    fn get(&self, idx: usize) -> u64 {
        assert!(idx < self.length);
        let p = self.data_ptr();
        let mut buf = [0u8; 8];
        unsafe {
            std::ptr::copy_nonoverlapping(
                p.add(idx * self.elem_size),
                buf.as_mut_ptr(),
                self.elem_size,
            );
        }
        u64::from_le_bytes(buf)
    }
}

/// JIT-compile the wrapper method
///   `void f(Object src, int srcPos, Object dst, int dstPos, int len)
///        { System.arraycopy(src, srcPos, dst, dstPos, len); }`
/// and return a closure that runs it.
///
/// Bytecode (invokestatic at pc 6):
///   aload_0 (2a), iload_1 (1b), aload_2 (2c), iload_3 (1d),
///   iload 4 (15 04), invokestatic (b8 00 01), return (b1)
fn compile_arraycopy() -> impl Fn(i64, i32, i64, i32, i32) {
    let entry = cratonvm_jit::try_resolve_intrinsic(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    )
    .expect("System.arraycopy must register as an intrinsic")
    .0;

    let code: Vec<u8> = vec![
        0x2a, 0x1b, 0x2c, 0x1d, 0x15, 0x04, 0xb8, 0x00, 0x01, 0xb1, 0, 0,
    ];
    let compiled = compile(
        &code,
        code.len(),
        5, // num_params: src, srcPos, dst, dstPos, len
        5, // max_locals
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(
            6,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 5,
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
        &helpers(),
        HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("JIT compilation of the arraycopy wrapper failed");

    move |src: i64, src_pos: i32, dst: i64, dst_pos: i32, len: i32| {
        // SAFETY: `compiled` was produced by the JIT from valid bytecode and
        // the mmap region is executable. The pointer args reference live
        // `FakeArray` storage owned by the caller for the call's duration.
        unsafe {
            compiled
                .try_call(&[src, src_pos as i64, dst, dst_pos as i64, len as i64])
                .expect("test JIT call");
        }
    }
}

fn compile_despec_arraycopy_with_dispatch(
    method_key: &str,
    despec: &std::sync::Arc<cratonvm_jit::deopt::DespecRegistry>,
    info: &JitInvokeInfo,
    helpers: &JitRuntimeHelpers,
) -> CompiledMethod {
    let entry = cratonvm_jit::try_resolve_intrinsic(
        "java/lang/System",
        "arraycopy",
        "(Ljava/lang/Object;ILjava/lang/Object;II)V",
    )
    .expect("System.arraycopy must register as an intrinsic")
    .0;

    let code: Vec<u8> = vec![
        0x2a, 0x1b, 0x2c, 0x1d, 0x15, 0x04, 0xb8, 0x00, 0x01, 0xb1, 0, 0,
    ];
    compile_with_param_slots(
        // Not a door: this test hands the backend hand-built bytecode with no
        // method identity to admit. See `CompileAdmission::for_backend_test`.
        &cratonvm_jit::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        code.len(),
        5, // num_params: src, srcPos, dst, dstPos, len
        5, // max_locals
        true,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(6, info as *const JitInvokeInfo)],
        vec![(
            6,
            JitDirectCall {
                entry,
                needs_context: false,
                num_params: 5,
                return_type: b'V',
                guard_class_id: 0,
            },
        )],
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
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
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &[],
        0,
        0,
        Vec::new(),
        method_key,
        Some(despec),
        Vec::new(), // indy_info
        // elidable_init_pcs: hand-built bytecode with no constant pool, so
        // nothing is PROVEN to be an empty `<init>` and nothing may be elided.
        None,
        &cratonvm_jit::DirectHelperTable::EMPTY,
    )
    .expect("JIT compilation of the despecialized arraycopy wrapper failed")
}

#[test]
fn arraycopy_matcher_registers_only_the_erased_descriptor() {
    // The single type-erased descriptor is registered.
    assert!(
        cratonvm_jit::try_resolve_intrinsic(
            "java/lang/System",
            "arraycopy",
            "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        )
        .is_some(),
        "the canonical System.arraycopy descriptor must register"
    );
    // Nothing else on System is an arraycopy intrinsic.
    assert!(
        cratonvm_jit::try_resolve_intrinsic("java/lang/System", "currentTimeMillis", "()J")
            .is_none()
    );
    assert!(cratonvm_jit::try_resolve_intrinsic("java/lang/Object", "arraycopy", "()V").is_none());
}

#[test]
fn arraycopy_despec_uses_dispatch_not_intrinsic_sentinel() {
    let _g = deopt_lock();
    let method_key = "ArraycopyDespec.wrapper:(Ljava/lang/Object;ILjava/lang/Object;II)V";
    // This compile's own registry, standing in for one VM's: nothing another
    // test records can reach it, so there is nothing to clear afterwards.
    let despec = std::sync::Arc::new(cratonvm_jit::deopt::DespecRegistry::new());
    despec.insert(method_key, 6);
    {
        let mut record = DISPATCH_RECORD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *record = None;
    }

    let info = JitInvokeInfo {
        class_name: "java/lang/System",
        method_name: "arraycopy",
        descriptor: "(Ljava/lang/Object;ILjava/lang/Object;II)V",
        num_jit_args: 5,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
    };
    let helpers = helpers_with_dispatch(recording_invoke_dispatch as *const () as usize);
    let compiled = compile_despec_arraycopy_with_dispatch(method_key, &despec, &info, &helpers);

    let before = clear_deopt_signals();
    let vm_ptr = 0x1234_5678_i64;
    unsafe {
        compiled
            .try_call_with_context(vm_ptr, &[11, 22, 33, 44, 55])
            .expect("test JIT call");
    }
    assert_no_deopt_after(before, "de-specialized arraycopy dispatch");

    let record = *DISPATCH_RECORD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let record = record.expect("despecialized arraycopy must call invoke_dispatch");
    assert_eq!(record.vm_ptr, vm_ptr);
    assert_eq!(record.info_ptr, &info as *const JitInvokeInfo as usize);
    assert_eq!(record.num_args, 5);
    assert_eq!(record.args, [11, 22, 33, 44, 55]);
    assert_eq!(record.return_type, b'V');
    assert_eq!(record.invoke_kind, 3);
}

#[test]
fn arraycopy_forward_distinct_int_arrays() {
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Int, 4, 8);
    let mut dst = FakeArray::new(ArrayElementType::Int, 4, 8);
    for i in 0..8 {
        src.set(i, 0x1000 + i as u64);
        dst.set(i, 0xEEEE_0000 + i as u64);
    }
    // Copy src[1..6] -> dst[2..7].
    f(src.ptr(), 1, dst.ptr(), 2, 5);
    for i in 0..8 {
        let expected = if (2..7).contains(&i) {
            0x1000 + (i as u64 - 1)
        } else {
            0xEEEE_0000 + i as u64
        };
        assert_eq!(dst.get(i), expected, "dst[{i}] after forward copy");
    }
    // src is untouched.
    for i in 0..8 {
        assert_eq!(src.get(i), 0x1000 + i as u64, "src[{i}] must be unchanged");
    }
}

#[test]
fn arraycopy_overlapping_same_array_dst_greater_than_src() {
    // dst > src within one array: regions overlap "forward", so a naive
    // forward byte copy would corrupt the tail. The intrinsic must copy
    // backward (STD) and behave like memmove.
    let f = compile_arraycopy();
    let mut a = FakeArray::new(ArrayElementType::Int, 4, 10);
    for i in 0..10 {
        a.set(i, i as u64);
    }
    // a[0..6] -> a[3..9].
    f(a.ptr(), 0, a.ptr(), 3, 6);
    let mut expected = [0u64; 10];
    for i in 0..10 {
        expected[i] = i as u64;
    }
    // memmove reference.
    let snapshot = expected;
    for i in 0..6 {
        expected[3 + i] = snapshot[i];
    }
    for i in 0..10 {
        assert_eq!(a.get(i), expected[i], "a[{i}] after overlap dst>src");
    }
}

#[test]
fn arraycopy_overlapping_same_array_dst_less_than_src() {
    // dst < src within one array: a forward copy is already correct, but the
    // intrinsic must still produce memmove semantics.
    let f = compile_arraycopy();
    let mut a = FakeArray::new(ArrayElementType::Int, 4, 10);
    for i in 0..10 {
        a.set(i, 100 + i as u64);
    }
    // a[4..10] -> a[1..7].
    f(a.ptr(), 4, a.ptr(), 1, 6);
    let mut expected = [0u64; 10];
    for i in 0..10 {
        expected[i] = 100 + i as u64;
    }
    let snapshot = expected;
    for i in 0..6 {
        expected[1 + i] = snapshot[4 + i];
    }
    for i in 0..10 {
        assert_eq!(a.get(i), expected[i], "a[{i}] after overlap dst<src");
    }
}

#[test]
fn arraycopy_byte_arrays_overlap_backward() {
    // 1-byte elements stress the REP MOVSB path with shift == 0.
    let f = compile_arraycopy();
    let mut a = FakeArray::new(ArrayElementType::Byte, 1, 16);
    for i in 0..16 {
        a.set(i, i as u64);
    }
    // a[0..10] -> a[5..15], overlapping, dst > src.
    f(a.ptr(), 0, a.ptr(), 5, 10);
    let mut expected = [0u64; 16];
    for i in 0..16 {
        expected[i] = i as u64;
    }
    let snapshot = expected;
    for i in 0..10 {
        expected[5 + i] = snapshot[i];
    }
    for i in 0..16 {
        assert_eq!(a.get(i), expected[i], "byte a[{i}] after overlap");
    }
}

#[test]
fn arraycopy_long_arrays_distinct() {
    // 8-byte elements stress the REP MOVSB path with shift == 3.
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Long, 8, 6);
    let mut dst = FakeArray::new(ArrayElementType::Long, 8, 6);
    for i in 0..6 {
        src.set(i, 0xDEAD_0000_0000_0000 | i as u64);
        dst.set(i, 0);
    }
    f(src.ptr(), 0, dst.ptr(), 0, 6);
    for i in 0..6 {
        assert_eq!(
            dst.get(i),
            0xDEAD_0000_0000_0000 | i as u64,
            "long dst[{i}]"
        );
    }
}

#[test]
fn arraycopy_empty_copy_is_a_noop() {
    let _g = deopt_lock();
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Int, 4, 4);
    let mut dst = FakeArray::new(ArrayElementType::Int, 4, 4);
    for i in 0..4 {
        src.set(i, 7 + i as u64);
        dst.set(i, 0xABCD_0000 + i as u64);
    }
    let before_traps = clear_deopt_signals();
    // len == 0 with valid in-range positions: no copy, no deopt.
    f(src.ptr(), 1, dst.ptr(), 2, 0);
    assert_no_deopt_after(before_traps, "empty copy with valid positions");
    for i in 0..4 {
        assert_eq!(
            dst.get(i),
            0xABCD_0000 + i as u64,
            "dst[{i}] after empty copy"
        );
    }
}

#[test]
fn arraycopy_out_of_bounds_deopts_without_corruption() {
    let _g = deopt_lock();
    // srcPos + len exceeds src.length. The inline bounds guard must branch
    // to the uncommon-trap deopt stub — NOT perform a partial/over-run copy.
    // The interpreter would then re-run the call via native arraycopy, which
    // throws ArrayIndexOutOfBoundsException; here we only verify the JIT side
    // bailed cleanly and left the destination untouched.
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Int, 4, 4);
    let mut dst = FakeArray::new(ArrayElementType::Int, 4, 8);
    for i in 0..4 {
        src.set(i, 1 + i as u64);
    }
    for i in 0..8 {
        dst.set(i, 0x5555_0000 + i as u64);
    }
    let before = clear_deopt_signals();
    // srcPos=2, len=5 -> 2+5 = 7 > src.length(4): out of bounds.
    f(src.ptr(), 2, dst.ptr(), 0, 5);
    assert_one_deopt_after(before, "out-of-bounds arraycopy");
    // The destination must be byte-identical to its pre-call contents.
    for i in 0..8 {
        assert_eq!(
            dst.get(i),
            0x5555_0000 + i as u64,
            "dst[{i}] must be untouched after an out-of-bounds bail"
        );
    }
}

#[test]
fn arraycopy_negative_position_deopts() {
    let _g = deopt_lock();
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Int, 4, 4);
    let mut dst = FakeArray::new(ArrayElementType::Int, 4, 4);
    for i in 0..4 {
        src.set(i, 9 + i as u64);
        dst.set(i, 0x3333_0000 + i as u64);
    }
    let before = clear_deopt_signals();
    // srcPos = -1: negative position must bail.
    f(src.ptr(), -1, dst.ptr(), 0, 2);
    assert_one_deopt_after(before, "negative srcPos");
    for i in 0..4 {
        assert_eq!(dst.get(i), 0x3333_0000 + i as u64, "dst[{i}] untouched");
    }
}

#[test]
fn arraycopy_mismatched_element_kinds_deopts() {
    let _g = deopt_lock();
    // src is int[], dst is long[]: incompatible element kinds. The inline
    // element-kind guard must bail so the interpreter can throw
    // ArrayStoreException via native arraycopy.
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Int, 4, 4);
    let mut dst = FakeArray::new(ArrayElementType::Long, 8, 4);
    for i in 0..4 {
        src.set(i, i as u64);
        dst.set(i, 0x7777_0000 + i as u64);
    }
    let before = clear_deopt_signals();
    f(src.ptr(), 0, dst.ptr(), 0, 2);
    assert_one_deopt_after(before, "mismatched element kinds");
    for i in 0..4 {
        assert_eq!(dst.get(i), 0x7777_0000 + i as u64, "dst[{i}] untouched");
    }
}

#[test]
fn arraycopy_reference_array_deopts() {
    let _g = deopt_lock();
    // A reference array (element_type == Reference == 0) is never inlined:
    // the GC store barrier and ArrayStoreException semantics make it unsafe.
    // The element-kind guard's "< 4" primitive check must bail.
    let f = compile_arraycopy();
    let mut src = FakeArray::new(ArrayElementType::Reference, 8, 4);
    let mut dst = FakeArray::new(ArrayElementType::Reference, 8, 4);
    for i in 0..4 {
        src.set(i, 0);
        dst.set(i, 0xBEEF_0000 + i as u64);
    }
    let before = clear_deopt_signals();
    f(src.ptr(), 0, dst.ptr(), 0, 2);
    assert_one_deopt_after(before, "reference-array arraycopy");
    for i in 0..4 {
        assert_eq!(dst.get(i), 0xBEEF_0000 + i as u64, "dst[{i}] untouched");
    }
}
