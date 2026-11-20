use super::*;
use crate::test_utils::mock_ctx;
use crate::try_alloc_concurrent_synthetic;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn param(kind: UpcallKind) -> UpcallParam {
    UpcallParam {
        kind,
        target_size: 0,
        target_align: 1,
    }
}

fn fake_target(ctx: &mut dyn NativeContext) -> ObjectRef {
    try_alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 2).unwrap()
}

// The fake targets below run inside an `extern "C"` callback, where a
// panic aborts the whole test binary. So none panics: an unexpected
// argument list answers a value the test's own assertion then rejects.

/// `(int, int) -> int`: the sum, wrapping.
fn add_ints(
    _ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    Ok(Some(match args {
        [Value::Int(a), Value::Int(b)] => Value::Int(a.wrapping_add(*b)),
        _ => Value::Int(-999),
    }))
}

/// `(double, int) -> double`: `x * k + 0.25`.
fn scale(
    _ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    Ok(Some(match args {
        [Value::Double(x), Value::Int(k)] => Value::Double(x * f64::from(*k) + 0.25),
        _ => Value::Double(f64::NAN),
    }))
}

/// `(address, address) -> int`: a `qsort` comparator over two `int`s. A
/// segment that is not 4 bytes (the target layout's size) answers "equal"
/// for everything, which leaves the array unsorted.
fn compare_ints(
    ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    let [Value::Object(Some(a)), Value::Object(Some(b))] = args else {
        return Ok(Some(Value::Int(0)));
    };
    if ctx.get_field(*a, 1) != Value::Long(4) || ctx.get_field(*b, 1) != Value::Long(4) {
        return Ok(Some(Value::Int(0)));
    }
    let (Value::Long(pa), Value::Long(pb)) = (ctx.get_field(*a, 0), ctx.get_field(*b, 0))
    else {
        return Ok(Some(Value::Int(0)));
    };
    // SAFETY: the test hands the stub pointers into a live `i32` array.
    let (x, y) = unsafe { (*(pa as *const i32), *(pb as *const i32)) };
    Ok(Some(Value::Int(x.cmp(&y) as i32)))
}

/// Throws: the result of every call is an exception.
fn throws(
    _ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    _args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    Err(RuntimeError::IllegalStateException {
        message: "boom".into(),
    }
    .into())
}

#[test]
fn layout_kinds_map_to_the_java_carriers() {
    assert_eq!(
        UpcallKind::from_layout_kind(LAYOUT_INT),
        Some(UpcallKind::Int)
    );
    assert_eq!(
        UpcallKind::from_layout_kind(LAYOUT_CHAR),
        Some(UpcallKind::Char)
    );
    assert_eq!(
        UpcallKind::from_layout_kind(LAYOUT_ADDRESS),
        Some(UpcallKind::Address)
    );
    assert_eq!(UpcallKind::from_layout_kind(LAYOUT_STRUCT), None);
    assert_eq!(UpcallKind::from_layout_kind(plf::LAYOUT_UNKNOWN), None);
    let shape = UpcallShape {
        params: vec![
            param(UpcallKind::Address),
            param(UpcallKind::Char),
            param(UpcallKind::Boolean),
        ],
        ret: Some(UpcallKind::Double),
    };
    assert_eq!(
        shape.method_descriptor(),
        "(Ljava/lang/foreign/MemorySegment;CZ)D"
    );
    let void = UpcallShape {
        params: Vec::new(),
        ret: None,
    };
    assert_eq!(void.method_descriptor(), "()V");
}

#[test]
fn type_mismatch_refuses_only_what_it_is_sure_of() {
    let want = "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;)I";
    assert!(!upcall_type_mismatch(want, want, false));
    // A parameter too many or too few.
    assert!(upcall_type_mismatch(
        want,
        "(Ljava/lang/foreign/MemorySegment;)I",
        false
    ));
    // A primitive where the descriptor wants another.
    assert!(upcall_type_mismatch(
        want,
        "(Ljava/lang/foreign/MemorySegment;Ljava/lang/foreign/MemorySegment;)J",
        false
    ));
    assert!(upcall_type_mismatch("(I)V", "(J)V", false));
    // `Object` is what an unreadable mirror renders as: not evidence.
    assert!(!upcall_type_mismatch(
        want,
        "(Ljava/lang/Object;Ljava/lang/foreign/MemorySegment;)I",
        false
    ));
    // Unparseable: not evidence either.
    assert!(!upcall_type_mismatch(want, "garbage", false));
}

/// Round 13 wave 5 (lane ffm3): when every mirror of the target's type was
/// named, an `Object` is a real `Object` -- HotSpot's `MethodType.equals`
/// refuses `(Object)V` for a `(JAVA_INT)V` descriptor. Equal types and
/// unparseable ones still pass.
#[test]
fn a_named_object_is_evidence_of_a_mismatch() {
    assert!(upcall_type_mismatch("(I)V", "(Ljava/lang/Object;)V", true));
    assert!(upcall_type_mismatch(
        "(Ljava/lang/foreign/MemorySegment;)I",
        "(Ljava/lang/foreign/MemorySegment;)Ljava/lang/Object;",
        true
    ));
    assert!(!upcall_type_mismatch(
        "(I)V",
        "(Ljava/lang/Object;)V",
        false
    ));
    assert!(!upcall_type_mismatch("(I)V", "(I)V", true));
    assert!(!upcall_type_mismatch("(I)V", "garbage", true));
}

#[test]
fn return_bits_widen_as_libffi_expects() {
    let ctx = mock_ctx();
    let ctx: &dyn NativeContext = &ctx;
    assert_eq!(
        return_bits(ctx, UpcallKind::Int, Value::Int(-2)).unwrap(),
        u64::MAX - 1
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Byte, Value::Int(-1)).unwrap(),
        u64::MAX
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Byte, Value::Int(0x17F)).unwrap(),
        0x7F
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Short, Value::Int(-3)).unwrap(),
        u64::MAX - 2
    );
    // `char` is unsigned: 0xFFFE stays 0xFFFE.
    assert_eq!(
        return_bits(ctx, UpcallKind::Char, Value::Int(0xFFFE)).unwrap(),
        0xFFFE
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Boolean, Value::Int(1)).unwrap(),
        1
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Long, Value::Long(-5)).unwrap(),
        (-5i64) as u64
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Float, Value::Float(1.5)).unwrap(),
        u64::from(1.5f32.to_bits())
    );
    assert_eq!(
        return_bits(ctx, UpcallKind::Double, Value::Double(-0.0)).unwrap(),
        (-0.0f64).to_bits()
    );
    // A mismatched result is refused, not coerced to 0.
    assert!(return_bits(ctx, UpcallKind::Int, Value::Double(1.0)).is_err());
    // A null segment is the JDK's NPE.
    assert!(matches!(
        return_bits(ctx, UpcallKind::Address, Value::Object(None)),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(RuntimeError::NullPointerException { .. })
        ))
    ));
}

/// Round 15 wave 6 (lane sweepC): an `Address` return refuses only a
/// class the VM says is definitely no `MemorySegment`, never an unknown
/// answer or CratonVM's own segment stamps.
#[test]
fn address_return_refuses_only_a_definite_non_segment() {
    assert!(upcall_return_class_refused(Some(false), "java/lang/String"));
    assert!(!upcall_return_class_refused(None, "java/lang/String"));
    assert!(!upcall_return_class_refused(
        Some(true),
        "jdk/internal/foreign/NativeMemorySegmentImpl"
    ));
    assert!(!upcall_return_class_refused(
        Some(false),
        crate::panama::CRATON_SEGMENT_CLASS
    ));
    assert!(!upcall_return_class_refused(
        Some(false),
        crate::panama::PE_SEGMENT_INTERFACE
    ));
}

/// j27 wave 4 (lane FFMMSG): an upcall returning a heap segment for an
/// `ADDRESS` fails in the return binding's `SharedUtils.unboxSegment` on
/// HotSpot, whose uncaught-exception report prints
/// `IllegalArgumentException: Heap segment not allowed: <segment>`; this
/// said `Heap segment not allowed as an upcall return`.
#[test]
fn a_heap_address_return_refuses_with_hotspot_text() {
    let mut ctx = mock_ctx();
    let array = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    let heap = match crate::panama::pe_of_array_alias(&mut ctx, &[Value::Object(Some(array))])
    {
        Ok(Some(Value::Object(Some(heap)))) => heap,
        other => panic!("ofArray answered {other:?}"),
    };
    let ctx: &dyn NativeContext = &ctx;
    let message = match return_bits(ctx, UpcallKind::Address, Value::Object(Some(heap))) {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => message,
        other => panic!("expected IllegalArgumentException, got {other:?}"),
    };
    assert_eq!(
        message,
        format!(
            "Heap segment not allowed: MemorySegment{{ kind: heap, heapBase: [I@{:x}, address: 0x0, byteSize: 8 }}",
            ctx.identity_hash_code(array) as u32
        )
    );
}

#[test]
fn decode_arg_reads_each_c_type_with_its_extension() {
    let b: i8 = -7;
    let s: i16 = -300;
    let c: u16 = 0xFFFE;
    let z: u8 = 1;
    let l: i64 = -1 << 40;
    let f: f32 = 2.5;
    let d: f64 = -0.125;
    let p: usize = 0x1234_5678;
    /// `decode_arg` of a live local, which is the C type `kind` names.
    fn read<T>(kind: UpcallKind, value: &T) -> Value {
        // SAFETY: `value` is a live `T`, and every call below pairs `T`
        // with the C type of `kind`.
        unsafe { decode_arg(kind, (value as *const T).cast()) }
    }
    assert_eq!(read(UpcallKind::Byte, &b), Value::Int(-7));
    assert_eq!(read(UpcallKind::Short, &s), Value::Int(-300));
    assert_eq!(read(UpcallKind::Char, &c), Value::Int(0xFFFE));
    assert_eq!(read(UpcallKind::Boolean, &z), Value::Int(1));
    assert_eq!(read(UpcallKind::Long, &l), Value::Long(-1 << 40));
    assert_eq!(read(UpcallKind::Float, &f), Value::Float(2.5));
    assert_eq!(read(UpcallKind::Double, &d), Value::Double(-0.125));
    assert_eq!(read(UpcallKind::Address, &p), Value::Long(0x1234_5678));
}

/// The whole path: a libffi closure called as a C function pointer, with
/// the downcall's context installed, reaching the invoker and returning.
#[test]
fn a_stub_called_from_c_runs_the_target_and_returns_its_int() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, add_ints);
    assert!(upcall_stub_registered(code));
    let result = {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: the stub's CIF is `int(int, int)`.
        let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(code) };
        (f(40, 2), f(i32::MAX, 1))
    };
    assert_eq!(result, (42, i32::MIN));
    // Not followed by `!upcall_stub_registered(code)`: a test running in
    // parallel may be handed the freed trampoline's address at once.
    remove_upcall_stub(code);
}

/// The attach hook the cross-VM test installs: a fresh context of the
/// requested VM on the calling (helper) thread, as the VM's hook attaches
/// that thread. No other test in this binary reaches the hook.
fn mock_attach_hook(vm: usize, run: &mut dyn FnMut(&mut dyn NativeContext)) -> bool {
    let mut ctx = mock_ctx();
    ctx.set_vm_identity(vm);
    run(&mut ctx);
    true
}

/// Round 13 wave 8 (lane ffm4): a stub of VM B called inside a downcall
/// of VM A runs (on a helper attached to B) instead of exiting. A's
/// thread waits in one blocking region of A (round 14 wave 1, lane ffm:
/// `r13w8-ffm4-cross-vm-upcall-wait-gc-safe-patch-FIXED-20260929.md`).
#[test]
fn another_vms_stub_inside_a_downcall_runs_on_a_helper_of_that_vm() {
    set_upcall_attach_hook(mock_attach_hook);
    let mut vm_a = mock_ctx();
    vm_a.set_vm_identity(0x7A80_0810);
    let mut vm_b = mock_ctx();
    vm_b.set_vm_identity(0x7A80_0811);
    let target = fake_target(&mut vm_b);
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&vm_b, target, shape, None, add_ints);
    let got = {
        let _guard = plf::ActiveContextGuard::install(&mut vm_a);
        // SAFETY: the stub's CIF is `int(int, int)`.
        let f: extern "C" fn(i32, i32) -> i32 = unsafe { std::mem::transmute(code) };
        f(40, 2)
    };
    assert_eq!(got, 42);
    assert_eq!(vm_a.blocking_region_counts(), (1, 1));
    remove_upcall_stub(code);
}

/// `(int) -> void`: marks the target, so a test can tell the invoker ran.
fn mark_target(
    ctx: &mut dyn NativeContext,
    target: ObjectRef,
    _args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    ctx.set_field(target, 0, Value::Int(77));
    Ok(None)
}

/// Round 14 wave 1 (lane ffm): a stub with a [`DirectStatic`] answers
/// through `invoke_static_settling` and never reaches its invoker
/// (`r13w13-ffm7-upcall-per-call-cost`); without one the invoker runs.
#[test]
fn a_direct_static_stub_skips_its_invoker() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    ctx.set_field(target, 0, Value::Int(0));
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int)],
        ret: None,
    };
    let direct_code = install_upcall_stub(&ctx, target, shape.clone(), None, mark_target);
    let set = registry().lock().get(&direct_code).is_some_and(|entry| {
        entry
            .data()
            .direct
            .set(DirectStatic {
                class: "p/Callbacks".into(),
                name: "onInt".into(),
                desc: "(I)V".into(),
                owner: None,
                settled: std::sync::atomic::AtomicU32::new(0),
                params: 1,
                real: false,
            })
            .is_ok()
    });
    assert!(set);
    {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: the stub's CIF is `void(int)`.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(direct_code) };
        f(5);
    }
    assert_eq!(ctx.get_field(target, 0), Value::Int(0), "the invoker ran");
    let plain_code = install_upcall_stub(&ctx, target, shape, None, mark_target);
    {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: as above.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(plain_code) };
        f(5);
    }
    assert_eq!(ctx.get_field(target, 0), Value::Int(77));
    remove_upcall_stub(direct_code);
    remove_upcall_stub(plain_code);
}

/// Round 15 wave 5 (lane upcall1; `r15w5-mh4-review-findings` item 7): a
/// [`DirectStatic`] whose member takes more parameters than the upcall
/// decodes is never entered through `invoke_static_settling` (which would
/// hand the member an uninitialized local); the invoker runs instead. And
/// the creation filter admits only the stub's own descriptor and count.
#[test]
fn a_direct_static_member_longer_than_the_upcall_is_not_entered() {
    let one_int = UpcallShape {
        params: vec![param(UpcallKind::Int)],
        ret: None,
    };
    assert!(direct_static_fits("(I)V", &one_int));
    assert!(!direct_static_fits("(II)V", &one_int));
    assert!(!direct_static_fits("()V", &one_int));
    assert!(!direct_static_fits("(J)V", &one_int));

    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    ctx.set_field(target, 0, Value::Int(0));
    let code = install_upcall_stub(&ctx, target, one_int, None, mark_target);
    let set = registry().lock().get(&code).is_some_and(|entry| {
        entry
            .data()
            .direct
            .set(DirectStatic {
                class: "p/Callbacks".into(),
                name: "onTwoInts".into(),
                desc: "(II)V".into(),
                owner: None,
                settled: std::sync::atomic::AtomicU32::new(0),
                params: 2,
                real: false,
            })
            .is_ok()
    });
    assert!(set);
    {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: the stub's CIF is `void(int)`.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(code) };
        f(5);
    }
    assert_eq!(ctx.get_field(target, 0), Value::Int(77), "the invoker ran");
    remove_upcall_stub(code);
}

/// Stubs registered for `vm` (tests give themselves a private VM identity,
/// so this is not disturbed by tests running in parallel).
fn stubs_of(vm: usize) -> usize {
    registry().lock().values().filter(|e| e.vm == vm).count()
}

#[test]
fn a_stub_returning_double_hands_c_the_double() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Double), param(UpcallKind::Int)],
        ret: Some(UpcallKind::Double),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, scale);
    let got = {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: the stub's CIF is `double(double, int)`.
        let f: extern "C" fn(f64, i32) -> f64 = unsafe { std::mem::transmute(code) };
        f(1.5, 3)
    };
    assert_eq!(got, 1.5 * 3.0 + 0.25);
    remove_upcall_stub(code);
}

/// `qsort` itself, with the stub as its comparator: address arguments
/// become segments of the target layout's size.
#[test]
fn qsort_sorts_through_an_address_comparator_stub() {
    extern "C" {
        fn qsort(
            base: *mut c_void,
            n: usize,
            size: usize,
            cmp: extern "C" fn(*const c_void, *const c_void) -> i32,
        );
    }
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let address = UpcallParam {
        kind: UpcallKind::Address,
        target_size: 4,
        target_align: 4,
    };
    let shape = UpcallShape {
        params: vec![address, address],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, compare_ints);
    let mut data: Vec<i32> = (0..200).map(|i| (i * 7919 % 211) - 100).collect();
    let mut want = data.clone();
    want.sort_unstable();
    {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: the stub's CIF is `int(void*, void*)`.
        let cmp: extern "C" fn(*const c_void, *const c_void) -> i32 =
            unsafe { std::mem::transmute(code) };
        // SAFETY: `data` is a live, correctly sized `int` array.
        unsafe { qsort(data.as_mut_ptr().cast(), data.len(), 4, cmp) };
    }
    assert_eq!(data, want);
    // Every segment the comparator received was unpinned again.
    remove_upcall_stub(code);
}

#[test]
fn an_exception_from_the_target_is_reported_as_uncaught() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int)],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, throws);
    let registry_data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData);
    let data = registry_data.expect("registered");
    let arg: i32 = 5;
    let args = [(&arg as *const i32).cast::<c_void>()];
    // SAFETY: `data` is alive while its entry is registered; `args` holds
    // one pointer to an `int`, the stub's one parameter.
    let outcome = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    assert!(matches!(outcome, Err(UpcallFatal::Uncaught(_))));
    remove_upcall_stub(code);
}

/// A stub whose arena's close walk ran refuses to run, and the sweep
/// frees it -- but not while an upcall of it is in flight. A state word
/// of 0 alone (a close still deciding, which may reopen the session) is
/// not death (round 13 wave 8, lane ffm4).
#[test]
fn a_closed_arenas_stub_is_dead_and_is_reclaimed() {
    let mut ctx = mock_ctx();
    // Private to this test: the registry is process-wide and the sweep
    // reads the session through this context.
    ctx.set_vm_identity(0x7A80_0701);
    let target = fake_target(&mut ctx);
    let Value::Object(Some(session)) = foreign_ffm::p67_memory_session(&mut ctx).unwrap()
    else {
        panic!("no session");
    };
    let state_slot = foreign_ffm::p67_session_slots(&ctx, session).state;
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, Some((session, state_slot)), add_ints);
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    // Open: runs, and is not reclaimable.
    assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
    let (a, b) = (3i32, 4i32);
    let args = [
        (&a as *const i32).cast::<c_void>(),
        (&b as *const i32).cast::<c_void>(),
    ];
    // SAFETY: registered, and `args` matches `int(int, int)`.
    let open = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    assert_eq!(open.unwrap(), 7);
    // State word 0, walk not run: still alive, not reclaimable.
    ctx.set_field(session, state_slot, Value::Int(0));
    // SAFETY: as above.
    let closing = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    assert_eq!(closing.unwrap(), 7);
    assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
    // In flight when the walk retires it: kept, but refuses to run.
    // SAFETY: still registered.
    unsafe { (*data).in_flight.fetch_add(1, Ordering::SeqCst) };
    assert_eq!(retire_upcall_stubs_of_session(&ctx, session), 0);
    assert!(upcall_stub_registered(code));
    // SAFETY: still registered.
    let closed = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    assert!(matches!(closed, Err(UpcallFatal::Unsupported(_))));
    // SAFETY: still registered.
    unsafe { (*data).in_flight.fetch_sub(1, Ordering::SeqCst) };
    // Idle and closed: freed.
    assert_eq!(reclaim_closed_upcall_stubs(&ctx), 1);
    assert_eq!(stubs_of(0x7A80_0701), 0);
}

/// The GC halves root and remap the target AND the session, for the
/// owning VM only.
#[test]
fn roots_and_remap_cover_target_and_session_per_vm() {
    const VM: usize = 0x7A80_0702;
    const OTHER_VM: usize = 0x7A80_0703;
    let ctx = mock_ctx();
    ctx.set_vm_identity(VM);
    // Fake, never-dereferenced addresses: the scan and the remap only move
    // integers around.
    // SAFETY: used only as opaque keys below.
    let target = unsafe { ObjectRef::from_raw(0xABCD_0000usize as *mut u8) };
    // SAFETY: as above.
    let session = unsafe { ObjectRef::from_raw(0xABCE_0000usize as *mut u8) };
    let shape = UpcallShape {
        params: Vec::new(),
        ret: None,
    };
    let code = install_upcall_stub(&ctx, target, shape, Some((session, 0)), add_ints);

    let mut roots = Vec::new();
    gc_scan_roots(VM, &mut roots);
    let addrs: Vec<usize> = roots.iter().map(|r| r.as_ptr() as usize).collect();
    assert!(addrs.contains(&0xABCD_0000) && addrs.contains(&0xABCE_0000));
    let mut other = Vec::new();
    gc_scan_roots(OTHER_VM, &mut other);
    assert!(!other.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000));

    let mut map = cratonvm_types::PointerMap::default();
    map.insert(0xABCD_0000usize, 0xABCD_8000usize);
    map.insert(0xABCE_0000usize, 0xABCE_8000usize);
    gc_update_refs(OTHER_VM, &map);
    let mut still = Vec::new();
    gc_scan_roots(VM, &mut still);
    assert!(still.iter().any(|r| r.as_ptr() as usize == 0xABCD_0000));
    gc_update_refs(VM, &map);
    let mut moved = Vec::new();
    gc_scan_roots(VM, &mut moved);
    let addrs: Vec<usize> = moved.iter().map(|r| r.as_ptr() as usize).collect();
    assert!(addrs.contains(&0xABCD_8000) && addrs.contains(&0xABCE_8000));
    // The panama.rs entry points the VM root table calls reach the same set.
    let mut via_panama = Vec::new();
    crate::panama::gc_scan_upcall_target_roots(VM, &mut via_panama);
    assert!(via_panama
        .iter()
        .any(|r| r.as_ptr() as usize == 0xABCD_8000));
    remove_upcall_stub(code);
}

/// The descriptor carrier's layouts become the shape; an address layout's
/// target layout sizes its segment; a group layout is refused.
fn alloc_obj(ctx: &mut dyn NativeContext, class: &str, slots: usize) -> ObjectRef {
    try_alloc_concurrent_synthetic(ctx, class, slots).unwrap()
}

#[test]
fn the_shape_is_read_off_the_function_descriptor() {
    let mut ctx = mock_ctx();
    let int_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallLayout", 2);
    ctx.set_field(int_layout, 0, Value::Int(LAYOUT_INT));
    let target_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallTargetLayout", 2);
    ctx.set_field(target_layout, 0, Value::Long(16));
    ctx.set_field(target_layout, 1, Value::Long(8));
    let address_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallAddressLayout", 5);
    ctx.set_field(address_layout, 0, Value::Int(LAYOUT_ADDRESS));
    let target_slot = foreign_ffm::p67_layout_slots(&ctx, address_layout).target;
    ctx.set_field(
        address_layout,
        target_slot,
        Value::Object(Some(target_layout)),
    );
    let object_class = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
    let params = ctx.new_ref_array(object_class, 2);
    ctx.set_array_element(params, 0, Value::Object(Some(address_layout)));
    ctx.set_array_element(params, 1, Value::Object(Some(int_layout)));
    let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
    ctx.set_field(descriptor, 0, Value::Object(Some(int_layout)));
    ctx.set_field(descriptor, 1, Value::Object(Some(params)));
    let shape = upcall_shape(&ctx, descriptor).unwrap();
    assert_eq!(
        shape,
        UpcallShape {
            params: vec![
                UpcallParam {
                    kind: UpcallKind::Address,
                    target_size: 16,
                    target_align: 8,
                },
                param(UpcallKind::Int),
            ],
            ret: Some(UpcallKind::Int),
        }
    );

    let group = alloc_obj(&mut ctx, "cratonvm/test/UpcallGroup", 2);
    ctx.set_field(group, 0, Value::Int(LAYOUT_STRUCT));
    ctx.set_field(descriptor, 0, Value::Object(Some(group)));
    assert!(matches!(
        upcall_shape(&ctx, descriptor),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                RuntimeError::UnsupportedOperationException { .. }
            )
        ))
    ));
}

/// `Linker.upcallStub` with an option is HotSpot's IAE, before anything
/// is built.
#[test]
fn an_upcall_option_is_refused() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
    let arena = alloc_obj(&mut ctx, "cratonvm/test/UpcallArena", 2);
    let option = alloc_obj(&mut ctx, "java/lang/foreign/Linker$Option", 2);
    let object_class = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
    let options = ctx.new_ref_array(object_class, 1);
    ctx.set_array_element(options, 0, Value::Object(Some(option)));
    let refused = create_upcall_stub(&mut ctx, target, descriptor, arena, Some(options));
    assert!(matches!(
        refused,
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { .. }
            )
        ))
    ));
}

/// Round 13 wave 9 (lane ffm5): `AbstractLinker.checkLayouts` -- a value
/// layout in the other byte order or without natural alignment is
/// `Unsupported layout: <stripped layout>`, a top-level sequence is
/// refused the same way, and a canonical layout (or a kind-tagged
/// carrier, which records nothing to check) passes.
#[test]
fn a_non_canonical_value_layout_is_refused_like_check_layouts() {
    fn message(result: Result<(), MethodCallFailed>) -> Option<String> {
        match result {
            Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
                RuntimeError::IllegalArgumentException { message },
            ))) => Some(message),
            _ => None,
        }
    }
    let mut ctx = mock_ctx();
    // A fabricated carrier (`p67_layout_object`'s four slots); the class
    // name only has to classify as an `int` layout.
    let int_layout = alloc_obj(&mut ctx, "cratonvm/test/UpcallOfIntLayout", 4);
    ctx.set_field(int_layout, 0, Value::Long(4));
    ctx.set_field(int_layout, 1, Value::Long(4));
    ctx.set_field(
        int_layout,
        2,
        Value::Int(i32::from(cfg!(target_endian = "little"))),
    );
    ctx.set_field(int_layout, 3, Value::Object(None));
    let descriptor = alloc_obj(&mut ctx, "java/lang/foreign/FunctionDescriptor", 2);
    ctx.set_field(descriptor, 0, Value::Object(Some(int_layout)));
    ctx.set_field(descriptor, 1, Value::Object(None));
    assert!(
        check_linker_layouts(&ctx, descriptor).is_ok(),
        "canonical JAVA_INT"
    );

    // The other byte order: upper-case letter.
    ctx.set_field(
        int_layout,
        2,
        Value::Int(i32::from(!cfg!(target_endian = "little"))),
    );
    let little = foreign_ffm::p67_layout_is_little(&ctx, int_layout);
    assert_ne!(
        little,
        cfg!(target_endian = "little"),
        "fixture reads as foreign order"
    );
    let want = if little {
        "Unsupported layout: i4"
    } else {
        "Unsupported layout: I4"
    };
    assert_eq!(
        message(check_linker_layouts(&ctx, descriptor)).as_deref(),
        Some(want)
    );

    // Native order, alignment 1: the `1%` prefix.
    ctx.set_field(
        int_layout,
        2,
        Value::Int(i32::from(cfg!(target_endian = "little"))),
    );
    ctx.set_field(int_layout, 1, Value::Long(1));
    assert_eq!(
        message(check_linker_layouts(&ctx, descriptor)).as_deref(),
        Some("Unsupported layout: 1%i4")
    );

    // A kind-tagged carrier records no size: nothing to check.
    let tagged = alloc_obj(&mut ctx, "cratonvm/test/UpcallLayout", 2);
    ctx.set_field(tagged, 0, Value::Int(LAYOUT_INT));
    ctx.set_field(descriptor, 0, Value::Object(Some(tagged)));
    assert!(
        check_linker_layouts(&ctx, descriptor).is_ok(),
        "kind-tagged"
    );

    // A sequence at the top level.
    // Five slots: the render reads the payload (2) and count (4) slots.
    let sequence = alloc_obj(&mut ctx, "cratonvm/test/UpcallSequenceLayout", 5);
    ctx.set_field(sequence, 0, Value::Int(LAYOUT_SEQUENCE));
    ctx.set_field(descriptor, 0, Value::Object(Some(sequence)));
    let refused = message(check_linker_layouts(&ctx, descriptor));
    assert!(
        refused
            .as_deref()
            .is_some_and(|m| m.starts_with("Unsupported layout: ")),
        "{refused:?}"
    );
}

/// Round 13 wave 9 (lane ffm5): a non-closeable arena's stub is freed by
/// the sweep once its weak session handle is cleared -- not before, not
/// while an upcall of it runs, and never by the closed-arena sweep.
#[test]
fn an_auto_arenas_stub_is_freed_once_its_session_is_collected() {
    let mut ctx = mock_ctx();
    // Private to this test: the registry is process-wide.
    ctx.set_vm_identity(0x7A80_0901);
    let target = fake_target(&mut ctx);
    let Value::Object(Some(session)) = foreign_ffm::p67_memory_session(&mut ctx).unwrap()
    else {
        panic!("no session");
    };
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Int), param(UpcallKind::Int)],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, add_ints);
    // The mock has no weak handles: a strong one stands in, and removing
    // it is the collection clearing it.
    let handle = ctx.add_global_root(session);
    assert_ne!(handle, 0);
    assert!(set_upcall_stub_auto_session(code, handle));
    // Session alive: kept by both sweeps.
    assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
    assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
    assert!(upcall_stub_registered(code));
    assert!(ctx.remove_global_root(handle));
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    // Collected, but an upcall is running: kept.
    // SAFETY: still registered.
    unsafe { (*data).in_flight.fetch_add(1, Ordering::SeqCst) };
    assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
    assert_eq!(reclaim_closed_upcall_stubs(&ctx), 0);
    assert!(upcall_stub_registered(code));
    // SAFETY: still registered.
    unsafe { (*data).in_flight.fetch_sub(1, Ordering::SeqCst) };
    // Collected and idle: freed, once.
    assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 1);
    assert_eq!(stubs_of(0x7A80_0901), 0);
    assert_eq!(reclaim_auto_upcall_stubs(&mut ctx), 0);
}

// -----------------------------------------------------------------------
// Round 13 wave 11 (lane ffm6): group layouts
// -----------------------------------------------------------------------

/// A fabricated value layout (`p67_layout_object`'s four slots).
fn value_layout(
    ctx: &mut dyn NativeContext,
    class: &str,
    size: i64,
    align: i64,
    little: bool,
) -> ObjectRef {
    let layout = alloc_obj(ctx, class, 4);
    ctx.set_field(layout, 0, Value::Long(size));
    ctx.set_field(layout, 1, Value::Long(align));
    ctx.set_field(layout, 2, Value::Int(i32::from(little)));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

const NATIVE_LITTLE: bool = cfg!(target_endian = "little");

fn int4(ctx: &mut dyn NativeContext) -> ObjectRef {
    value_layout(ctx, "cratonvm/test/UpcallOfIntLayout", 4, 4, NATIVE_LITTLE)
}

fn long8(ctx: &mut dyn NativeContext) -> ObjectRef {
    value_layout(ctx, "cratonvm/test/UpcallOfLongLayout", 8, 8, NATIVE_LITTLE)
}

fn padding(ctx: &mut dyn NativeContext, size: i64) -> ObjectRef {
    let layout = alloc_obj(ctx, "cratonvm/test/UpcallPaddingLayout", 4);
    ctx.set_field(layout, 0, Value::Long(size));
    ctx.set_field(layout, 1, Value::Long(1));
    ctx.set_field(layout, 2, Value::Object(None));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

/// A fabricated group carrier: `[0]` size, `[1]` alignment, `[2]` the
/// member array, `[3]` no name. `class` decides struct or union.
fn group(
    ctx: &mut dyn NativeContext,
    class: &str,
    members: &[ObjectRef],
    size: i64,
    align: i64,
) -> ObjectRef {
    let object_class = crate::lang_class::reflection_component_id(ctx, "java/lang/Object");
    let array = ctx.new_ref_array(object_class, members.len());
    for (i, member) in members.iter().enumerate() {
        ctx.set_array_element(array, i, Value::Object(Some(*member)));
    }
    let layout = alloc_obj(ctx, class, 4);
    ctx.set_field(layout, 0, Value::Long(size));
    ctx.set_field(layout, 1, Value::Long(align));
    ctx.set_field(layout, 2, Value::Object(Some(array)));
    ctx.set_field(layout, 3, Value::Object(None));
    layout
}

fn struct_of(
    ctx: &mut dyn NativeContext,
    members: &[ObjectRef],
    size: i64,
    align: i64,
) -> ObjectRef {
    group(
        ctx,
        "cratonvm/test/UpcallStructLayout",
        members,
        size,
        align,
    )
}

/// `FunctionDescriptor.of(ret)` (no parameters).
fn returning(ctx: &mut dyn NativeContext, ret: ObjectRef) -> ObjectRef {
    let descriptor = alloc_obj(ctx, "java/lang/foreign/FunctionDescriptor", 2);
    ctx.set_field(descriptor, 0, Value::Object(Some(ret)));
    ctx.set_field(descriptor, 1, Value::Object(None));
    descriptor
}

fn refusal(result: Result<(), MethodCallFailed>) -> Option<String> {
    match result {
        Err(MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(
            RuntimeError::IllegalArgumentException { message },
        ))) => Some(message),
        _ => None,
    }
}



/// SBR2 wave 8 (lane FFM): a REAL `MemoryLayout.unionLayout(a, b)` stores
/// `List.of(elements)`, an `ImmutableCollections$List12` (`e0`, `e1`, no
/// backing array; `e1` is a plain `Object` sentinel for one element). The
/// group reader must still find both members, and a real
/// `FunctionDescriptorImpl`'s `argLayouts` must read the same way.
#[test]
fn a_group_held_in_a_list12_is_decoded() {
    use cratonvm_native_api::FieldMetadata;
    use cratonvm_types::ClassId;
    let mut ctx = mock_ctx();
    let (a, b) = (int4(&mut ctx), int4(&mut ctx));
    let union = struct_of(&mut ctx, &[a, b], 4, 4);
    let list12 = alloc_obj(&mut ctx, "java/util/ImmutableCollections$List12", 2);
    let cid = ctx.class_id_of_object(list12);
    let field = |name: &str, slot: usize, cid: ClassId| FieldMetadata {
        name: name.to_string(),
        descriptor: "Ljava/lang/Object;".to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(cid, vec![field("e0", 0, cid), field("e1", 1, cid)]);
    ctx.set_field(list12, 0, Value::Object(Some(a)));
    ctx.set_field(list12, 1, Value::Object(Some(b)));
    ctx.set_field(union, 2, Value::Object(Some(list12)));
    let (store, n) = foreign_ffm::p67_group_members(&ctx, union).expect("a List12 is decoded");
    assert_eq!(n, 2);
    assert_eq!(
        foreign_ffm::p67_group_member_at(&ctx, store, 0),
        Value::Object(Some(a))
    );
    assert_eq!(
        foreign_ffm::p67_group_member_at(&ctx, store, 1),
        Value::Object(Some(b))
    );
    // One element: `e1` is the JDK's `EMPTY` sentinel, a plain `Object`.
    let one = alloc_obj(&mut ctx, "java/util/ImmutableCollections$List12", 2);
    ctx.set_field(one, 0, Value::Object(Some(a)));
    let sentinel = alloc_obj(&mut ctx, "java/lang/Object", 0);
    ctx.set_field(one, 1, Value::Object(Some(sentinel)));
    ctx.set_field(union, 2, Value::Object(Some(one)));
    assert_eq!(
        foreign_ffm::p67_group_members(&ctx, union).map(|(_, n)| n),
        Some(1)
    );
    // A real `FunctionDescriptorImpl`: slot 0 `resLayout`, slot 1 the list.
    let fd = alloc_obj(&mut ctx, "jdk/internal/foreign/FunctionDescriptorImpl", 2);
    ctx.set_field(fd, 0, Value::Object(Some(a)));
    ctx.set_field(fd, 1, Value::Object(Some(list12)));
    assert_eq!(plf::descriptor_return_layout(&ctx, fd), Some(a));
    assert_eq!(
        plf::descriptor_param_layouts(&ctx, fd),
        vec![Some(a), Some(b)]
    );
}


/// A target that returns a native segment over `size` bytes of the
/// pattern `k, k+1, ..` (`k` its one `int` argument), `size` read off the
/// shape's group return. The image is leaked: a test's worth of bytes.
fn returns_pattern(
    ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    args: &[Value],
    shape: &UpcallShape,
) -> MethodCallResult {
    let k = match args {
        [Value::Int(k)] => *k as u8,
        _ => 0,
    };
    let size = match shape.ret {
        Some(UpcallKind::Group { size, .. }) => size as usize,
        _ => 0,
    };
    let image: &'static mut [u8] = Box::leak(
        (0..size)
            .map(|i| k.wrapping_add(i as u8))
            .collect::<Vec<u8>>()
            .into_boxed_slice(),
    );
    let segment = crate::panama::alloc_segment_carrier(ctx, 6)?;
    ctx.set_field(segment, 0, Value::Long(image.as_mut_ptr() as i64));
    ctx.set_field(segment, 1, Value::Long(size as i64));
    ctx.set_field(segment, 2, Value::Object(None));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(segment))))
}

/// The bytes of a `#[repr(C)]` value.
fn bytes_of<T>(value: &T) -> Vec<u8> {
    // SAFETY: `value` is a live `T` of `size_of::<T>()` initialised bytes
    // (the structs below have no padding).
    unsafe {
        std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
    }
    .to_vec()
}

/// Round 13 wave 11 (lane ffm6): a stub returning a struct by value hands
/// C exactly the struct's bytes, in registers or through the hidden
/// return pointer: 16 bytes (two integer registers on SysV, hidden
/// pointer on Win64), 12 (hidden pointer on Win64, never an 8-byte
/// `ffi_arg` write past it) and 3 (hidden pointer on Win64).
#[test]
fn a_stub_returning_a_struct_hands_c_its_bytes() {
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // read as bytes
    struct S16 {
        a: i64,
        b: i64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // read as bytes
    struct S12 {
        a: i32,
        b: i32,
        c: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    #[allow(dead_code)] // read as bytes
    struct S3 {
        a: u8,
        b: u8,
        c: u8,
    }
    fn pattern(k: u8, size: usize) -> Vec<u8> {
        (0..size).map(|i| k.wrapping_add(i as u8)).collect()
    }
    fn stub(
        ctx: &dyn NativeContext,
        target: ObjectRef,
        ret: Type,
        size: u32,
        align: u32,
    ) -> usize {
        let shape = UpcallShape {
            params: vec![param(UpcallKind::Int)],
            ret: Some(UpcallKind::Group { size, align }),
        };
        let types = UpcallFfiTypes {
            params: vec![Type::i32()],
            ret,
        };
        install_upcall_stub_typed(ctx, target, shape, types, None, returns_pattern)
    }
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let c16 = stub(
        &ctx,
        target,
        Type::structure(vec![Type::i64(), Type::i64()]),
        16,
        8,
    );
    let c12 = stub(
        &ctx,
        target,
        Type::structure(vec![Type::i32(), Type::i32(), Type::i32()]),
        12,
        4,
    );
    let c3 = stub(
        &ctx,
        target,
        Type::structure(vec![Type::u8(), Type::u8(), Type::u8()]),
        3,
        1,
    );
    let (g16, g12, g3) = {
        let _guard = plf::ActiveContextGuard::install(&mut ctx);
        // SAFETY: each stub's CIF is `S(int)` for its `S`.
        let f16: extern "C" fn(i32) -> S16 = unsafe { std::mem::transmute(c16) };
        // SAFETY: as above.
        let f12: extern "C" fn(i32) -> S12 = unsafe { std::mem::transmute(c12) };
        // SAFETY: as above.
        let f3: extern "C" fn(i32) -> S3 = unsafe { std::mem::transmute(c3) };
        (f16(10), f12(40), f3(90))
    };
    assert_eq!(bytes_of(&g16), pattern(10, 16));
    assert_eq!(bytes_of(&g12), pattern(40, 12));
    assert_eq!(bytes_of(&g3), pattern(90, 3));
    for code in [c16, c12, c3] {
        remove_upcall_stub(code);
    }
}

/// The copy out of the returned segment refuses what HotSpot's copy
/// refuses: `null`, and a segment shorter than the layout.
#[test]
fn a_group_return_refuses_null_and_short_segments() {
    let mut ctx = mock_ctx();
    let mut buffer = [0u8; 8];
    let dest = Some(GroupReturn {
        ptr: buffer.as_mut_ptr(),
        len: buffer.len(),
    });
    assert!(matches!(
        copy_group_return(&mut ctx, Value::Object(None), 8, dest),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(RuntimeError::NullPointerException { .. })
        ))
    ));
    let image: [u8; 4] = [1, 2, 3, 4];
    let segment = crate::panama::alloc_segment_carrier(&mut ctx, 6).unwrap();
    ctx.set_field(segment, 0, Value::Long(image.as_ptr() as i64));
    ctx.set_field(segment, 1, Value::Long(4));
    ctx.set_field(segment, 2, Value::Object(None));
    ctx.set_field(segment, 3, Value::Int(0));
    ctx.set_field(segment, 4, Value::Int(1));
    ctx.set_field(segment, 5, Value::Long(0));
    assert!(matches!(
        copy_group_return(&mut ctx, Value::Object(Some(segment)), 8, dest),
        Err(MethodCallFailed::InternalError(
            cratonvm_types::error::VmError::Runtime(
                RuntimeError::IndexOutOfBoundsException { .. }
            )
        ))
    ));
    assert!(copy_group_return(&mut ctx, Value::Object(Some(segment)), 4, dest).is_ok());
    assert_eq!(buffer, [1, 2, 3, 4, 0, 0, 0, 0]);
}

/// Round 13 wave 13 (lane ffm7): `Utils.isAligned`, the predicate the
/// upcall's address boxing applies.
#[test]
fn misalignment_is_judged_against_the_target_alignment() {
    assert!(misaligned_address(6, 4));
    assert!(misaligned_address(0x1001, 8));
    assert!(!misaligned_address(8, 4));
    assert!(!misaligned_address(0, 8), "NULL is aligned for everything");
    assert!(!misaligned_address(3, 1), "no target layout: alignment 1");
}

/// Round 13 wave 13 (lane ffm7; `r12w7-upcall-residuals` item 5, bullet
/// 4): an address C passes that its target layout's alignment does not
/// divide is `Utils.longToAddress`'s `IllegalArgumentException` inside the
/// upcall -- the uncaught-exception exit -- and the target never runs; an
/// aligned one runs.
#[test]
fn a_misaligned_address_argument_is_refused_like_long_to_address() {
    let mut ctx = mock_ctx();
    let target = fake_target(&mut ctx);
    let address = UpcallParam {
        kind: UpcallKind::Address,
        target_size: 4,
        target_align: 4,
    };
    let shape = UpcallShape {
        params: vec![address, address],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, compare_ints);
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    let ints: [i32; 3] = [5, 7, 9];
    let base = ints.as_ptr() as usize;
    let aligned: [usize; 2] = [base, base + 4];
    let misaligned: [usize; 2] = [base, base + 2];
    let aligned_args = [
        (&aligned[0] as *const usize).cast::<c_void>(),
        (&aligned[1] as *const usize).cast::<c_void>(),
    ];
    let misaligned_args = [
        (&misaligned[0] as *const usize).cast::<c_void>(),
        (&misaligned[1] as *const usize).cast::<c_void>(),
    ];
    let pins_before = ctx.native_pin_count_for_test();
    // SAFETY: registered; each slot points at a pointer-sized address, the
    // C type of an address parameter, into the live `ints`.
    let ran = unsafe { upcall_dispatch(&mut ctx, &*data, aligned_args.as_ptr()) };
    assert_eq!(ran.unwrap() as i32, -1, "5 < 7");
    // SAFETY: as above; the second address is refused before any read.
    let refused = unsafe { upcall_dispatch(&mut ctx, &*data, misaligned_args.as_ptr()) };
    let text = format!("{refused:?}");
    assert!(
        matches!(refused, Err(UpcallFatal::Uncaught(_)))
            && text.contains("Invalid alignment constraint for address: 0x"),
        "{text}"
    );
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
    remove_upcall_stub(code);
}

thread_local! {
    /// The group argument [`sums_struct_arg`] last received.
    static KEPT_ARG: std::cell::Cell<Option<ObjectRef>> = const { std::cell::Cell::new(None) };
}

/// `(struct { long a; long b; }) -> int`: `a * 10 + b`, read through the
/// segment the stub made, which must be open and owned by this thread and
/// 16 bytes long (negative answers name the failed check). Keeps the
/// segment in [`KEPT_ARG`].
fn sums_struct_arg(
    ctx: &mut dyn NativeContext,
    _target: ObjectRef,
    args: &[Value],
    _shape: &UpcallShape,
) -> MethodCallResult {
    let [Value::Object(Some(segment))] = args else {
        return Ok(Some(Value::Int(-1)));
    };
    let segment = *segment;
    if crate::panama::pe_segment_check_scope(ctx, segment).is_err() {
        return Ok(Some(Value::Int(-2)));
    }
    if plf::segment_byte_size(ctx, segment) != 16 {
        return Ok(Some(Value::Int(-3)));
    }
    let address = plf::segment_address(ctx, segment);
    if address == 0 {
        return Ok(Some(Value::Int(-4)));
    }
    // SAFETY: a live 16-byte block of the per-upcall arena (checked above).
    let (a, b) = unsafe {
        (
            std::ptr::read_unaligned(address as usize as *const i64),
            std::ptr::read_unaligned((address as usize + 8) as *const i64),
        )
    };
    KEPT_ARG.with(|k| k.set(Some(segment)));
    Ok(Some(Value::Int((a * 10 + b) as i32)))
}

/// Round 13 wave 13 (lane ffm7; testable since FFM6-1 gave the mock a
/// stable thread): a by-value struct PARAMETER reaches the target as a
/// segment of a confined arena made for the one upcall -- open and this
/// thread's during the call, `Already closed` after it (HotSpot's
/// `newBoundedArena` frame) -- and the upcall leaves no pin behind.
#[test]
fn a_struct_parameter_is_a_confined_segment_closed_after_the_upcall() {
    #[repr(C)]
    #[allow(dead_code)] // read as bytes
    struct S16 {
        a: i64,
        b: i64,
    }
    let mut ctx = mock_ctx();
    ctx.stable_current_thread();
    let target = fake_target(&mut ctx);
    let shape = UpcallShape {
        params: vec![param(UpcallKind::Group { size: 16, align: 8 })],
        ret: Some(UpcallKind::Int),
    };
    let code = install_upcall_stub(&ctx, target, shape, None, sums_struct_arg);
    let data = registry()
        .lock()
        .get(&code)
        .map(|e| e.data as *const UpcallStubData)
        .expect("registered");
    let s = S16 { a: 3, b: 4 };
    let args = [(&s as *const S16).cast::<c_void>()];
    KEPT_ARG.with(|k| k.set(None));
    let pins_before = ctx.native_pin_count_for_test();
    // SAFETY: registered; the one slot points at the aggregate's 16 bytes,
    // which is how libffi hands a closure a by-value struct.
    let got = unsafe { upcall_dispatch(&mut ctx, &*data, args.as_ptr()) };
    assert_eq!(got.unwrap() as i32, 34);
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
    let kept = KEPT_ARG.with(|k| k.take()).expect("the target ran");
    let after = crate::panama::pe_segment_check_scope(&mut ctx, kept);
    assert!(format!("{after:?}").contains("Already closed"), "{after:?}");
    remove_upcall_stub(code);
}

#[test]
fn the_row_is_a_stated_bridge_on_the_linker_interface() {
    let mut r = NativeMethodRegistry::new();
    register_panama_upcall(&mut r);
    let rows = r.census();
    let row = rows
        .iter()
        .find(|row| row.class == LINKER_CLASS && row.name == "upcallStub")
        .expect("Linker.upcallStub registered");
    assert_eq!(row.descriptor, UPCALL_STUB_DESCRIPTOR);
    assert_eq!(row.kind, NativeKind::Bridge);
    assert!(row.kind_chosen, "the kind must be stated, not inherited");
}
