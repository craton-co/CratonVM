// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 5, lane `volatile5` — executed tests for
//! `volatile-instance-fields-are-plain-accesses-in-compiled-code`.
//!
//! The VM now tells the JIT which `getfield`/`putfield` sites name a volatile
//! field (`CompileRequest::cp_field_volatile_resolver`). The single-pass
//! backend follows every volatile `putfield` with the JMM's StoreLoad fence
//! (`MFENCE`), and the optimizing tier refuses such a method, so it cannot
//! merge or hoist a volatile read. These tests pin:
//!
//! * the fence, as a byte difference between a volatile and a plain store, on
//!   the helper arm, the inline primitive arm and the reference arm — and its
//!   absence after a volatile LOAD, which x86-64 already orders;
//! * a compiled spin loop on a volatile field that another thread sets
//!   terminates, with the optimizing tier requested and not (a hoisted read
//!   would spin forever; the test has a timeout rather than hanging);
//! * a Dekker / store-buffering litmus on two compiled volatile handshakes:
//!   the outcome where both threads read the other's flag as 0 — which the
//!   missing StoreLoad fence allows on x86-64 — never occurs.
//!
//! Harness: the `r9w3_x64obj3_fields.rs` one. Receivers are hand-built
//! 8-aligned legacy-layout buffers inside a wide-open READ bounds table; the
//! helper entries are non-panicking stubs, and the field helpers read and
//! write the real cell so the helper arm and the inline arm observe the same
//! memory.

use cratonvm_jit::JitRuntimeHelpers;
use cratonvm_jit::{try_compile_request, CachedBytecodeMethod, CompileRequest, CompiledMethod};
use cratonvm_types::{ClassId, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, NUM_SLOTS_OFFSET, SLOT_SIZE};
use std::sync::atomic::{AtomicI32, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

/// One region spanning `[0x1000, usize::MAX)`: every 8-aligned non-null test
/// buffer passes the guarded receiver check.
static WIDE_OPEN: [AtomicUsize; 6] = [
    AtomicUsize::new(0x1000),
    AtomicUsize::new(usize::MAX),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

/// What every helper this file does not care about answers.
const BENIGN: i64 = 777_777;

unsafe extern "C" fn benign() -> i64 {
    BENIGN
}

unsafe extern "C" fn record_throw_bci(_bci: i64) {}

unsafe extern "C" fn self_guard(_vm: i64) -> i64 {
    0
}

unsafe extern "C" fn native_stack_floor() -> i64 {
    0
}

/// The `int` payload word of legacy cell `idx` of `obj`.
fn payload(obj: i64, idx: i64) -> &'static AtomicI32 {
    // Cast: test buffer address arithmetic.
    let at = obj as usize + HEADER_SIZE + idx as usize * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
    // SAFETY: every receiver these helpers see is a live, 8-aligned test
    // buffer with at least `idx + 1` legacy cells (the payload word is
    // 4-aligned), and it outlives every compiled call made on it.
    unsafe { &*(at as *const AtomicI32) }
}

/// `jit_getfield(vm, obj, idx)`: reads the legacy cell the inline arm reads.
unsafe extern "C" fn cell_getfield(_vm: i64, obj: i64, idx: i64) -> i64 {
    if obj == 0 {
        return BENIGN;
    }
    i64::from(payload(obj, idx).load(Ordering::Relaxed))
}

/// `jit_putfield_int(obj, idx, val)`: writes the legacy cell with a RELAXED
/// store, exactly the ordering the VM's helper uses. Any StoreLoad ordering
/// the Dekker test observes therefore comes from the compiled code's fence.
unsafe extern "C" fn cell_putfield_int(obj: i64, idx: i64, val: i64) {
    if obj == 0 {
        return;
    }
    // Cast: an `int` field keeps the low 32 bits.
    payload(obj, idx).store(val as i32, Ordering::Relaxed);
}

/// A helper table whose every call target is [`benign`], READ and STORE
/// bounds on [`WIDE_OPEN`], and the two `int` field helpers on the real cell.
fn helpers() -> JitRuntimeHelpers {
    let s = benign as *const () as usize;
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
        getfield: cell_getfield as *const () as usize,
        putfield_int: cell_putfield_int as *const () as usize,
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
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: WIDE_OPEN.as_ptr() as usize,
        read_bounds_addr: WIDE_OPEN.as_ptr() as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
        ldc_string: s,
        set_throw_bci: record_throw_bci as *const () as usize,
        service_callee_deopt: s,
        ..Default::default()
    }
}

/// `code` padded with the VM's two trailing zero bytes, as a static method.
fn cached(
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("r9w5/Volatile"),
        method_name: Arc::from(name),
        method_descriptor: Arc::from(descriptor),
        source_file: None,
        code: Arc::from(code.as_slice()),
        exception_table: Arc::from(Vec::new().as_slice()),
        max_stack: 8,
        max_locals,
        num_params,
        is_synchronized: false,
        is_static: true,
        force_native_cache: std::sync::OnceLock::new(),
        descriptor_facts_cache: std::sync::OnceLock::new(),
        intercept_shape_cache: std::sync::OnceLock::new(),
        interp_invocations: std::sync::atomic::AtomicU32::new(0),
        tiering_settled: std::sync::atomic::AtomicU32::new(0),
        branch_profile_armed: std::sync::atomic::AtomicBool::new(false),
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// Compile `cm`, whose only field is constant-pool entry 1 — `field_index`
/// 0, descriptor `tag`, compact row at packed offset 8 — declared volatile or
/// not. `optimize` requests the optimizing tier, which a volatile access must
/// decline in favour of single-pass.
fn compile(
    cm: &CachedBytecodeMethod,
    h: &JitRuntimeHelpers,
    tag: u8,
    volatile: bool,
    optimize: bool,
) -> CompiledMethod {
    let field = move |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        (cp == 1).then_some((0, tag, Some((8, tag == b'L' || tag == b'['))))
    };
    let is_volatile = move |cp: u16| -> bool { volatile && cp == 1 };
    try_compile_request(&CompileRequest {
        cp_field_resolver: Some(&field),
        cp_field_volatile_resolver: Some(&is_volatile),
        optimize,
        ..CompileRequest::new(cm, h)
    })
    .unwrap_or_else(|| panic!("{} must compile (volatile={volatile})", cm.method_name))
}

fn mfences(m: &CompiledMethod) -> usize {
    m.code_bytes()
        .windows(3)
        .filter(|w| w[0] == 0x0F && w[1] == 0xAE && w[2] == 0xF0)
        .count()
}

/// Call a compiled static method, supplying the hidden context slot when the
/// artifact has one. The paths under test never dereference it.
fn call(m: &CompiledMethod, args: &[i64]) -> i64 {
    let dummy_vm = [0u64; 16];
    // SAFETY: JIT-compiled from valid bytecode; every reference argument is a
    // live, 8-aligned test buffer inside `WIDE_OPEN`, and every reachable
    // helper is a non-panicking stub.
    unsafe {
        if m.needs_context() {
            m.try_call_with_context(dummy_vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    }
    .expect("jit call")
}

/// A legacy-layout (16-byte `Value` cell) object with `num_fields` int cells,
/// every one `Value::Int(0)`.
fn make_object(num_fields: usize) -> Vec<u64> {
    let bytes = HEADER_SIZE + num_fields.max(1) * SLOT_SIZE;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    // SAFETY: inside the `bytes`-long buffer.
    unsafe {
        // Cast: a handful of fields.
        std::ptr::write_unaligned(base.add(NUM_SLOTS_OFFSET) as *mut u32, num_fields as u32);
    }
    buf
}

/// `static void set(Object o, int x) { o.f = x; }` for an `I` field,
/// `o.f = (long) x` for a `J` field, and `static void set(Object o, Object x)`
/// for an `L` field.
fn setter(tag: u8) -> CachedBytecodeMethod {
    let (mut code, desc) = match tag {
        // aload_0; iload_1; i2l
        b'J' => (vec![0x2a, 0x1b, 0x85], "(Ljava/lang/Object;I)V"),
        // aload_0; aload_1
        b'L' => (vec![0x2a, 0x2b], "(Ljava/lang/Object;Ljava/lang/Object;)V"),
        // aload_0; iload_1
        _ => (vec![0x2a, 0x1b], "(Ljava/lang/Object;I)V"),
    };
    code.extend_from_slice(&[0xb5, 0x00, 0x01, 0xb1]); // putfield #1; return
    cached("set", desc, code, 2, 2)
}

/// Exactly one more `MFENCE` in the volatile store than in the identical plain
/// one, on every arm: the default helper arm (`I`, `J`), the inline primitive
/// arm (`CRATONVM_JIT_INLINE_PRIM_PUTFIELD=1`) and the reference arm (`L`).
/// Compared rather than counted from zero, because the byte pattern could in
/// principle occur inside a baked 64-bit address identically in both bodies.
#[test]
fn a_volatile_putfield_is_followed_by_a_store_load_fence() {
    let h = helpers();
    for (tag, inline_prim) in [(b'I', None), (b'I', Some("1")), (b'J', None), (b'L', None)] {
        let cm = setter(tag);
        let (plain, fenced) = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_INLINE_PRIM_PUTFIELD", inline_prim)],
            || (compile(&cm, &h, tag, false, false), compile(&cm, &h, tag, true, false)),
        );
        assert_eq!(
            mfences(&fenced),
            mfences(&plain) + 1,
            "`{}` putfield (inline primitive store: {inline_prim:?}): a volatile store \
             owes exactly one StoreLoad fence",
            tag as char
        );
    }
}

/// The fence does not break the store: a volatile `int` store through both
/// the helper arm and the inline arm lands in the cell.
#[test]
fn a_fenced_volatile_int_store_still_stores() {
    let h = helpers();
    let cm = setter(b'I');
    for inline_prim in [None, Some("1")] {
        let m = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_INLINE_PRIM_PUTFIELD", inline_prim)],
            || compile(&cm, &h, b'I', true, false),
        );
        let obj = make_object(1);
        call(&m, &[obj.as_ptr() as i64, -42]);
        assert_eq!(
            payload(obj.as_ptr() as i64, 0).load(Ordering::SeqCst),
            -42,
            "volatile store (inline primitive store: {inline_prim:?})"
        );
        drop(obj);
    }
}

/// A volatile LOAD carries no fence: x86-64 loads are acquires, and the
/// StoreLoad obligation sits after the volatile store. Same rule the static
/// side follows (`a_volatile_static_load_has_no_fence_unless_asked`).
#[test]
fn a_volatile_getfield_carries_no_fence() {
    let h = helpers();
    // static int get(Object o) { return o.f; }
    let cm = cached("get", "(Ljava/lang/Object;)I", vec![0x2a, 0xb4, 0x00, 0x01, 0xac], 1, 1);
    let plain = compile(&cm, &h, b'I', false, false);
    let volatile = compile(&cm, &h, b'I', true, false);
    assert_eq!(mfences(&volatile), mfences(&plain));
    let obj = make_object(1);
    payload(obj.as_ptr() as i64, 0).store(9, Ordering::SeqCst);
    assert_eq!(call(&volatile, &[obj.as_ptr() as i64]) as i32, 9);
    drop(obj);
}

/// `static int spin(Object o) { int n = 0; while (o.f == 0) n++; return n; }`
/// on a volatile `f`, compiled with and without the optimizing tier
/// requested, run on its own thread while this one sets `f` 50 ms later. A
/// compiled body that hoisted the read out of the loop — what the optimizing
/// tier's LICM did to an unordered `Op::Load` in a read-only loop — never
/// returns; the test fails at its timeout instead of hanging the suite.
#[test]
fn a_compiled_spin_loop_on_a_volatile_field_terminates() {
    let h = helpers();
    let code = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x2a, // 2: aload_0
        0xb4, 0x00, 0x01, // 3: getfield #1
        0x9a, 0x00, 0x09, // 6: ifne +9 -> 15
        0x84, 0x01, 0x01, // 9: iinc 1, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1b, // 15: iload_1
        0xac, // 16: ireturn
    ];
    let cm = cached("spin", "(Ljava/lang/Object;)I", code, 2, 1);
    for optimize in [false, true] {
        let m = compile(&cm, &h, b'I', true, optimize);
        let obj = make_object(1);
        let obj_addr = obj.as_ptr() as i64;
        let m_addr = &m as *const CompiledMethod as usize;
        let (tx, rx) = std::sync::mpsc::channel();
        let spinner = std::thread::spawn(move || {
            // SAFETY: `m` outlives this thread — it is joined below, or, on a
            // timeout, `m` and `obj` are leaked so the still-spinning body
            // keeps valid code and a valid receiver.
            let m = unsafe { &*(m_addr as *const CompiledMethod) };
            let _ = tx.send(call(m, &[obj_addr]));
        });
        std::thread::sleep(Duration::from_millis(50));
        payload(obj_addr, 0).store(1, Ordering::SeqCst);
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(n) => {
                assert!(n >= 0, "iteration count {n}");
                spinner.join().expect("spinner thread");
                drop((m, obj));
            }
            Err(_) => {
                std::mem::forget(m);
                std::mem::forget(obj);
                panic!(
                    "the compiled spin loop (optimizing tier requested: {optimize}) never \
                     observed the volatile store: the read was hoisted out of the loop"
                );
            }
        }
    }
}

/// Store buffering (Dekker): `static int shake(Object mine, Object other) {
/// mine.f = 1; return other.f; }` with `f` volatile, run by two threads on
/// crossed receivers, many times. Without a StoreLoad fence each thread's
/// store can sit in its store buffer while its load reads the other flag's old
/// 0, so both return 0 — forbidden for volatiles by the JMM (sequential
/// consistency). The fence makes it impossible.
#[test]
fn a_dekker_handshake_on_compiled_volatile_stores_never_reads_both_zero() {
    const ROUNDS: usize = 20_000;
    let h = helpers();
    let code = vec![
        0x2a, // 0: aload_0
        0x04, // 1: iconst_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0x2b, // 5: aload_1
        0xb4, 0x00, 0x01, // 6: getfield #1
        0xac, // 9: ireturn
    ];
    let cm = cached("shake", "(Ljava/lang/Object;Ljava/lang/Object;)I", code, 2, 2);
    let m = compile(&cm, &h, b'I', true, false);
    let a = make_object(1);
    let b = make_object(1);
    let (a_addr, b_addr) = (a.as_ptr() as i64, b.as_ptr() as i64);
    let m_addr = &m as *const CompiledMethod as usize;
    let start = Arc::new(Barrier::new(3));
    let end = Arc::new(Barrier::new(3));
    let results = Arc::new([AtomicI64::new(-1), AtomicI64::new(-1)]);
    let workers: Vec<_> = [(a_addr, b_addr), (b_addr, a_addr)]
        .into_iter()
        .enumerate()
        .map(|(i, (mine, other))| {
            let (start, end, results) = (start.clone(), end.clone(), results.clone());
            std::thread::spawn(move || {
                // SAFETY: `m`, `a` and `b` outlive both workers (joined below).
                let m = unsafe { &*(m_addr as *const CompiledMethod) };
                for _ in 0..ROUNDS {
                    start.wait();
                    results[i].store(call(m, &[mine, other]), Ordering::SeqCst);
                    end.wait();
                }
            })
        })
        .collect();
    let mut both_zero = 0usize;
    for _ in 0..ROUNDS {
        payload(a_addr, 0).store(0, Ordering::SeqCst);
        payload(b_addr, 0).store(0, Ordering::SeqCst);
        start.wait();
        end.wait();
        let r0 = results[0].load(Ordering::SeqCst);
        let r1 = results[1].load(Ordering::SeqCst);
        if r0 == 0 && r1 == 0 {
            both_zero += 1;
        }
    }
    for w in workers {
        w.join().expect("worker");
    }
    drop((m, a, b));
    assert_eq!(
        both_zero, 0,
        "{both_zero} of {ROUNDS} rounds read both volatile flags as 0: the compiled \
         volatile store is missing its StoreLoad fence"
    );
}

/// `static int f(Object o, Object[] a, int i) { int n = 0; while (o.f == 0)
/// { Object r = a[i]; n++; } return n; }` with `f` volatile. The loop-invariant
/// `aload; iload; aaload` is the single-pass backend's `LoopHoist` shape, but
/// the volatile read in the same loop is an acquire the hoisted read may not
/// move above, so the body must be exactly the one compiled with that hoist
/// switched off (`CRATONVM_DISABLE_AALOAD_LICM=1`). Asserted as equal lengths,
/// which holds whether or not the plain loop would have hoisted.
#[test]
fn an_aaload_is_not_hoisted_out_of_a_loop_with_a_volatile_read() {
    let h = helpers();
    let code = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3
        0x2a, // 2: aload_0
        0xb4, 0x00, 0x01, // 3: getfield #1
        0x9a, 0x00, 0x0d, // 6: ifne +13 -> 19
        0x2b, // 9: aload_1
        0x1c, // 10: iload_2
        0x32, // 11: aaload
        0x57, // 12: pop
        0x84, 0x03, 0x01, // 13: iinc 3, 1
        0xa7, 0xff, 0xf2, // 16: goto -14 -> 2
        0x1d, // 19: iload_3
        0xac, // 20: ireturn
    ];
    let cm = cached("wait_then_read", "(Ljava/lang/Object;[Ljava/lang/Object;I)I", code, 4, 3);
    let default_body = compile(&cm, &h, b'I', true, false);
    let unhoisted_body = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_DISABLE_AALOAD_LICM", Some("1"))],
        || compile(&cm, &h, b'I', true, false),
    );
    assert_eq!(
        default_body.code_bytes().len(),
        unhoisted_body.code_bytes().len(),
        "the aaload was hoisted out of a loop that reads a volatile field"
    );
}
