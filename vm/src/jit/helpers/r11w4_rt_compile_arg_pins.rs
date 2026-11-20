use super::*;
use crate::config::VmConfig;
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

/// Receiver 0, int 1, `byte[]` 2, long 3, `String` 4, double 5.
pub static MIXED_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "p/Probe",
    method_name: "m",
    descriptor: "(I[BJLjava/lang/String;D)V",
    num_jit_args: 6,
    return_type: b'V',
    invoke_kind: 0,
    declaring_class_id: 0,
    owner_class_id: 0,
};

/// No receiver: `Object[][]` 0, int 1, `int[]` 2.
pub static STATIC_INFO: JitInvokeInfo = JitInvokeInfo {
    class_name: "p/Probe",
    method_name: "s",
    descriptor: "([[Ljava/lang/Object;I[I)V",
    num_jit_args: 3,
    return_type: b'V',
    invoke_kind: 3,
    declaring_class_id: 0,
    owner_class_id: 0,
};

pub fn reference_positions(info: &JitInvokeInfo, len: usize) -> Vec<usize> {
    let args = vec![0i64; len];
    let mut seen = Vec::new();
    for_each_jit_reference_arg(info, &args, |index, _| seen.push(index));
    seen
}

/// JIT round 23 wave 3d (lane L1): the IR's direct self-call row (kind 4)
/// has a receiver only when its argument count says so. The walk read
/// `invoke_kind != 3`, so a STATIC self-call `(I[B)V` named its `int` a
/// receiver and the `byte[]` at word 1 was walked as word 2 (past the
/// buffer: missed). The decoder already answered "no receiver" for it.
#[test]
pub fn a_kind_4_row_names_a_receiver_only_when_it_carries_one() {
    let static_self = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "s",
        descriptor: "(I[B)V",
        num_jit_args: 2,
        return_type: b'V',
        invoke_kind: 4,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    let instance_self = JitInvokeInfo {
        class_name: "p/Probe",
        method_name: "m",
        descriptor: "(I[B)V",
        num_jit_args: 3,
        return_type: b'V',
        invoke_kind: 4,
        declaring_class_id: 0,
        owner_class_id: 0,
    };
    let instance_twin = JitInvokeInfo {
        invoke_kind: 1,
        ..instance_self
    };
    assert!(!jit_info_has_receiver(&static_self));
    assert_eq!(reference_positions(&static_self, 2), vec![1], "the byte[] only");
    assert!(jit_info_has_receiver(&instance_self));
    assert_eq!(reference_positions(&instance_self, 3), vec![0, 2]);
    // The kind-1 twin the VM is normally handed answers the same.
    assert!(jit_info_has_receiver(&instance_twin));
    assert_eq!(reference_positions(&instance_twin, 3), vec![0, 2]);
    // Kinds 0-3 keep `invoke_site_has_receiver`'s answer.
    assert!(jit_info_has_receiver(&MIXED_INFO));
    assert!(!jit_info_has_receiver(&STATIC_INFO));
}

#[test]
pub fn the_reference_walk_names_the_receiver_and_every_reference_parameter() {
    assert_eq!(reference_positions(&MIXED_INFO, 6), vec![0, 2, 4]);
    assert_eq!(reference_positions(&STATIC_INFO, 3), vec![0, 2]);
    // A short buffer stops the walk instead of reading past it.
    assert_eq!(reference_positions(&MIXED_INFO, 2), vec![0]);
}

#[test]
pub fn raw_pins_follow_a_move_and_release_to_the_watermark() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "r11w4-compile-arg-pins");
    let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let moved_to = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    // An enclosing pin that neither round may disturb.
    thread.native_pin_roots.push(outer);
    // The `String` slot is null; the int and long slots are never pinned.
    let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

    let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
    assert_eq!(thread.native_pin_roots, vec![outer, recv, arr]);
    assert_eq!(
        pins.release_raw(&mut thread, &args),
        None,
        "nothing moved, so the helper keeps reading its own buffer",
    );
    assert_eq!(thread.native_pin_roots, vec![outer]);

    let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
    // The collection inside the compile moves the array.
    thread.native_pin_roots[2] = moved_to;
    let moved = pins.release_raw(&mut thread, &args);
    let mut expected = args.to_vec();
    expected[2] = moved_to.as_ptr() as i64;
    assert_eq!(
        moved,
        Some(expected),
        "the moved argument must come back at its new address, the rest unchanged",
    );
    assert_eq!(thread.native_pin_roots, vec![outer]);
}

#[test]
pub fn value_pins_write_the_current_address_back() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "r11w4-compile-arg-pins");
    let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let other = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let moved_to = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let mut values = [
        Value::Object(Some(recv)),
        Value::Int(3),
        Value::Object(None),
        Value::Object(Some(other)),
    ];

    let pins = CompileArgPins::pin_values(&shared, &mut thread, &values);
    assert_eq!(thread.native_pin_roots, vec![recv, other]);
    // The collection moves the receiver.
    thread.native_pin_roots[0] = moved_to;
    pins.release_values(&mut thread, &mut values);
    assert_eq!(
        values,
        [
            Value::Object(Some(moved_to)),
            Value::Int(3),
            Value::Object(None),
            Value::Object(Some(other)),
        ],
    );
    assert!(thread.native_pin_roots.is_empty());
}

/// Round 11 wave 5 (r11w4-rt-callee-rerun-reads-pre-call-argument-words):
/// the re-run doors read the arguments back through pins that stay in
/// place across several GC-capable steps. `current_raw` / `args_now`
/// answer the words as they stand at each read, WITHOUT releasing, and
/// `release` then truncates to the watermark.
#[test]
pub fn pins_read_back_current_words_without_releasing() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "r11w5-rerun-arg-pins");
    let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let moved_recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let moved_arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    thread.native_pin_roots.push(outer);
    let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

    let pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
    // Nothing moved: the caller's own buffer is read, nothing is copied.
    assert!(matches!(
        args_now(Some(&pins), &thread, &args),
        std::borrow::Cow::Borrowed(_)
    ));
    // The callee's collection moves the array ...
    thread.native_pin_roots[2] = moved_arr;
    let mut expected = args.to_vec();
    expected[2] = moved_arr.as_ptr() as i64;
    assert_eq!(&*args_now(Some(&pins), &thread, &args), &expected[..]);
    // ... the pins are still there, and a later allocation moves the
    // receiver too; the next read sees both moves.
    assert_eq!(thread.native_pin_roots, vec![outer, recv, moved_arr]);
    thread.native_pin_roots[1] = moved_recv;
    expected[0] = moved_recv.as_ptr() as i64;
    assert_eq!(pins.current_raw(&thread, &args), Some(expected));
    // No pins: the buffer as given.
    assert_eq!(&*args_now(None, &thread, &args), &args[..]);
    pins.release(&mut thread);
    assert_eq!(thread.native_pin_roots, vec![outer]);
}

/// gen r5w4/jit8: a pin scope left WITHOUT its explicit release -- an
/// unwind out of `compile()` / `call()`, or the `jit_thread_mut() == None`
/// arm -- truncates to its watermark on drop. It used to leave the
/// arguments rooted for the life of the thread when no enclosing pin scope
/// sat below them, keeping whatever they reached alive
/// (`docs/feature-designs/deopt-osr.md`).
#[test]
pub fn a_pin_scope_dropped_without_release_truncates_to_its_watermark() {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let mut thread = JvmThread::new(ThreadId(0), "r5w4-jit8-pins-drop");
    let recv = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let arr = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    let outer = shared.mem.heap.alloc_object(ClassId::new(0), 0);
    thread.native_pin_roots.push(outer);
    let args = [recv.as_ptr() as i64, 64, arr.as_ptr() as i64, 7, 0, 0];

    // Raw pins, and a nested scope's leftovers above them, both dropped.
    {
        let _pins = CompileArgPins::pin_raw(&shared, &mut thread, &MIXED_INFO, &args);
        thread.native_pin_roots.push(arr);
    }
    assert_eq!(
        thread.native_pin_roots,
        vec![outer],
        "the dropped scope and everything above its watermark must be released",
    );

    // Value pins, dropped the way the `jit_thread_mut() == None` arm of
    // `compile_with_ref_args_pinned` drops them: moved into an `Option`
    // and never released. (An unwind drops them by the same `Drop`.)
    let values = [Value::Object(Some(recv)), Value::Int(1)];
    let pins = Some(CompileArgPins::pin_values(&shared, &mut thread, &values));
    assert_eq!(thread.native_pin_roots, vec![outer, recv]);
    drop(pins);
    assert_eq!(thread.native_pin_roots, vec![outer]);
}
