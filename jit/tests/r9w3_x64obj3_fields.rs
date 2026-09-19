// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT review round 9, wave 3, lane `x64obj3` — executed tests for the
//! single-pass field fast paths this lane touched:
//!
//! * `perf-spliced-callee-getfield-always-calls-the-helper` — a `getfield` in
//!   a callee the single-pass backend SPLICES now takes the guarded inline load
//!   the top-level arms use (`Compiler::emit_spliced_inline_getfield`), with
//!   `jit_getfield` as its slow edge only;
//! * `single-pass-primitive-putfield-is-always-a-helper-call` — the executed,
//!   per-type half the wave-2 page left open: under
//!   `CRATONVM_JIT_INLINE_PRIM_PUTFIELD=1` every primitive descriptor stores
//!   inline into BOTH object layouts, the bytes are exactly the helper's, and
//!   `jit_putfield_*` is never called.
//!
//! Receivers are hand-built 8-aligned buffers inside a wide-open READ bounds
//! table (`WIDE_OPEN`), the same arrangement `ir_vs_singlepass.rs` uses. The
//! helper entries are COUNTING markers: a routing mistake shows up as a
//! non-zero count or a marker value, never as a crash, and a stub never panics
//! (an `extern "C"` panic aborts the whole test binary).

use cratonvm_jit::JitRuntimeHelpers;
use cratonvm_jit::{try_compile, CachedBytecodeMethod, CompiledMethod, InlineSite};
use cratonvm_types::{
    ClassId, FIELD_CELL_PAYLOAD32_OFFSET, GC_FLAGS_BYTE_OFFSET, GC_FLAG_COMPACT, HEADER_SIZE,
    NUM_SLOTS_OFFSET, SLOT_SIZE,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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

/// What every helper this file does not care about answers. Not a panic (see
/// the module doc) and not a plausible field value.
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

/// A helper table whose every call target is [`benign`], READ and STORE
/// bounds on [`WIDE_OPEN`]. Callers overwrite the entries they count.
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
    class: &str,
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
        class_name: Arc::from(class),
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

/// An 8-aligned object buffer: `num_fields` legacy cells' worth of body,
/// `num_slots = num_fields`, the compact flag as asked, and every body byte
/// `0xAA` so a store's exact footprint is visible.
fn make_object(num_fields: usize, compact: bool) -> Vec<u64> {
    let bytes = HEADER_SIZE + num_fields.max(1) * SLOT_SIZE;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    // SAFETY: every offset below is inside the `bytes`-long buffer.
    unsafe {
        for i in HEADER_SIZE..bytes {
            *base.add(i) = 0xAA;
        }
        // Cast: a handful of fields.
        std::ptr::write_unaligned(base.add(NUM_SLOTS_OFFSET) as *mut u32, num_fields as u32);
        if compact {
            *base.add(GC_FLAGS_BYTE_OFFSET) |= GC_FLAG_COMPACT;
        }
    }
    buf
}

fn body(obj: &[u64]) -> Vec<u8> {
    // SAFETY: reading the buffer's own bytes.
    let all = unsafe { std::slice::from_raw_parts(obj.as_ptr() as *const u8, obj.len() * 8) };
    all[HEADER_SIZE..].to_vec()
}

/// Call a compiled static method, supplying the hidden context slot when the
/// artifact has one (field methods do: their helper edge wants `vm_ptr`).
/// The fast paths under test never dereference it.
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

// ---------------------------------------------------------------------------
// Spliced-callee getfield
// ---------------------------------------------------------------------------

/// `static int get(Object c) { return c.v; }` — `aload_0; getfield #1;
/// ireturn` — with field `v` at slot 0, and optionally a compact row placing it
/// at packed byte offset 8.
fn getter_site(compact: Option<(u32, bool)>) -> InlineSite {
    let code = vec![0x2a, 0xb4, 0x00, 0x01, 0xac, 0x00, 0x00];
    InlineSite {
        callee_code_len: code.len() - 2,
        callee_code: code,
        callee_max_locals: 1,
        callee_num_args: 1,
        callee_is_static: true,
        return_type: b'I',
        field_info: vec![(1, 0, b'I')],
        compact_field_info: compact.map(|(o, r)| vec![(1, o, r)]).unwrap_or_default(),
        needs_heap: true,
        class_name: "r9w3/Cell".to_string(),
        method_name: "get".to_string(),
        descriptor: "(Ljava/lang/Object;)I".to_string(),
        ..InlineSite::default()
    }
}

/// `static int run(Object c) { return Cell.get(c); }` compiled single-pass
/// with `get` handed to the single-pass inliner.
fn compile_caller(site: InlineSite, h: &JitRuntimeHelpers) -> CompiledMethod {
    let cm = cached(
        "r9w3/Caller",
        "run",
        "(Ljava/lang/Object;)I",
        vec![0x2a, 0xb8, 0x00, 0x01, 0xac],
        1,
        1,
    );
    let invoke = |idx: u16| -> Option<(String, String, String)> {
        (idx == 1).then(|| {
            (
                "r9w3/Cell".to_string(),
                "get".to_string(),
                "(Ljava/lang/Object;)I".to_string(),
            )
        })
    };
    let inline = |c: &str, m: &str, _d: &str| -> Option<InlineSite> {
        (c == "r9w3/Cell" && m == "get").then(|| site.clone())
    };
    try_compile(
        &cm,
        None,          // cp_class_name_resolver
        None,          // cp_field_resolver (the caller has no field)
        None,          // cp_static_field_resolver
        Some(&invoke), // cp_invoke_resolver
        None,          // callee_compiler
        None,          // cp_new_resolver
        None,          // cp_ldc_resolver
        None,          // cp_ldc2w_resolver
        None,          // profile
        h,
        Some(&inline), // inline_resolver (single-pass)
        None,          // string_layout_resolver
        None,          // cp_invoke_class_id_resolver
        None,          // cp_elidable_init_resolver
        false,         // optimize: single-pass
        false,
        false,
        false,
        false,
        false,
        None,
    )
    .expect("the caller compiles single-pass")
}

/// A legacy (16-byte `Value` cell) receiver: the spliced `c.v` is read
/// inline, and `jit_getfield` is reached only by a receiver the guard refuses.
#[test]
fn a_spliced_getfield_reads_a_legacy_cell_inline() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        424_242
    }
    let mut h = helpers();
    h.getfield = counting_getfield as *const () as usize;
    let m = compile_caller(getter_site(None), &h);

    let mut obj = make_object(1, false);
    // Slot 0 = `Value::Int(42)`: tag 0, payload at +4, high word zero.
    // SAFETY: inside the one-cell body.
    unsafe {
        let cell = (obj.as_mut_ptr() as *mut u8).add(HEADER_SIZE);
        std::ptr::write_unaligned(cell as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(8) as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32, 42);
    }
    let r = call(&m, &[obj.as_ptr() as i64]);
    assert_ne!(
        r, BENIGN,
        "the call to `get` was not spliced, so this test exercised nothing"
    );
    assert_eq!(r as i32, 42, "the spliced getfield read the wrong value");
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "an in-bounds legacy receiver must not reach jit_getfield from a splice"
    );

    // The slow edge still works: a null receiver is the helper's to answer.
    let r = call(&m, &[0]);
    assert_eq!(r, 424_242, "a null receiver must reach the helper");
    assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    drop(obj);
}

/// A compact receiver at a splice whose callee carries a compact row: the
/// packed 4-byte field is read in place, and a legacy receiver at the same
/// site still reads its cell.
#[test]
fn a_spliced_getfield_reads_a_compact_field_inline() {
    if !cratonvm_types::compact_ref_fields_enabled() {
        return; // the compact layout is off for this process
    }
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        CALLS.fetch_add(1, Ordering::SeqCst);
        424_242
    }
    let mut h = helpers();
    h.getfield = counting_getfield as *const () as usize;
    let m = compile_caller(getter_site(Some((8, false))), &h);

    let mut compact = make_object(1, true);
    // SAFETY: HEADER_SIZE + 8 + 4 is inside the 16-byte body.
    unsafe {
        let at = (compact.as_mut_ptr() as *mut u8).add(HEADER_SIZE + 8);
        std::ptr::write_unaligned(at as *mut i32, -1234);
    }
    let r = call(&m, &[compact.as_ptr() as i64]);
    assert_ne!(r, BENIGN, "the call to `get` was not spliced");
    assert_eq!(
        r as i32, -1234,
        "the packed field must be read at its compact offset"
    );

    let mut legacy = make_object(1, false);
    // SAFETY: inside the one-cell body.
    unsafe {
        let cell = (legacy.as_mut_ptr() as *mut u8).add(HEADER_SIZE);
        std::ptr::write_unaligned(cell as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(8) as *mut u64, 0);
        std::ptr::write_unaligned(cell.add(FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32, 99);
    }
    let r = call(&m, &[legacy.as_ptr() as i64]);
    assert_eq!(
        r as i32, 99,
        "a legacy receiver at a compact site reads its cell"
    );
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        0,
        "no helper call on either layout"
    );
    drop((compact, legacy));
}

// ---------------------------------------------------------------------------
// Inline primitive putfield, per type, both layouts
// ---------------------------------------------------------------------------

static PUT_CALLS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn counting_put(_obj: i64, _idx: i64, _val: i64) {
    PUT_CALLS.fetch_add(1, Ordering::SeqCst);
}

/// `static void set(Object o, int x) { o.f = <conv>(x); }` with `f` of
/// descriptor `tag` at slot 1 (legacy) / packed byte offset 8 (compact),
/// compiled single-pass with the inline store switched on for the compile.
fn compile_setter(tag: u8, h: &JitRuntimeHelpers) -> CompiledMethod {
    let mut code = vec![0x2a, 0x1b]; // aload_0; iload_1
    match tag {
        b'J' => code.push(0x85), // i2l
        b'F' => code.push(0x86), // i2f
        b'D' => code.push(0x87), // i2d
        _ => {}                  // an unnarrowed int: the store must narrow
    }
    code.extend_from_slice(&[0xb5, 0x00, 0x01, 0xb1]); // putfield #1; return
    let cm = cached("r9w3/Setter", "set", "(Ljava/lang/Object;I)V", code, 2, 2);
    let field = move |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        (cp == 1).then_some((1, tag, Some((8, false))))
    };
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_INLINE_PRIM_PUTFIELD", Some("1"))],
        || {
            try_compile(
                &cm,
                None,
                Some(&field),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                h,
                None,
                None,
                None,
                None,
                false, // single-pass
                false,
                false,
                false,
                false,
                false,
                None,
            )
        },
    )
    .expect("the setter compiles single-pass")
}

/// `(tag, int argument, legacy cell words, compact bytes)`.
fn cases() -> Vec<(u8, i64, [u64; 2], Vec<u8>)> {
    let f = 3.0f32.to_bits();
    let d = (-2.0f64).to_bits();
    vec![
        (
            b'I',
            -5,
            [0xFFFF_FFFB_0000_0000, 0],
            (-5i32).to_le_bytes().to_vec(),
        ),
        (
            b'J',
            -7,
            [1, (-7i64) as u64],
            (-7i64).to_le_bytes().to_vec(),
        ),
        (
            b'F',
            3,
            [2 | (u64::from(f) << 32), 0],
            f.to_le_bytes().to_vec(),
        ),
        (b'D', -2, [3, d], d.to_le_bytes().to_vec()),
        // Narrowing: 0x1FF -> (byte)-1; 0x18001 -> (short)0x8001 / (char)0x8001;
        // 3 -> (boolean)1.
        (b'B', 0x1FF, [0xFFFF_FFFF_0000_0000, 0], vec![0xFF]),
        (b'S', 0x18001, [0xFFFF_8001_0000_0000, 0], vec![0x01, 0x80]),
        (b'C', 0x18001, [0x0000_8001_0000_0000, 0], vec![0x01, 0x80]),
        (b'Z', 3, [0x0000_0001_0000_0000, 0], vec![0x01]),
    ]
}

/// Every primitive descriptor, into a legacy object: the whole 16-byte cell
/// of slot 1 is `write_value_atomic`'s two words, slot 0 is untouched, and no
/// `jit_putfield_*` runs. Into a compact object: exactly the field's own width
/// at the packed offset and not one byte more.
#[test]
fn every_primitive_putfield_stores_inline_into_both_layouts() {
    let mut h = helpers();
    let put = counting_put as *const () as usize;
    h.putfield_int = put;
    h.putfield_long = put;
    h.putfield_float = put;
    h.putfield_double = put;
    for (tag, arg, words, compact_bytes) in cases() {
        let t = tag as char;
        let m = compile_setter(tag, &h);
        let before = PUT_CALLS.load(Ordering::SeqCst);

        // Legacy receiver: two cells, the store lands in cell 1.
        let legacy = make_object(2, false);
        call(&m, &[legacy.as_ptr() as i64, arg]);
        let b = body(&legacy);
        let w0 = u64::from_le_bytes(b[SLOT_SIZE..SLOT_SIZE + 8].try_into().unwrap());
        let w1 = u64::from_le_bytes(b[SLOT_SIZE + 8..SLOT_SIZE + 16].try_into().unwrap());
        assert_eq!([w0, w1], words, "`{t}` legacy cell words");
        assert!(
            b[..SLOT_SIZE].iter().all(|&x| x == 0xAA),
            "`{t}` legacy store touched the neighbouring cell"
        );

        if cratonvm_types::compact_ref_fields_enabled() {
            let compact = make_object(2, true);
            call(&m, &[compact.as_ptr() as i64, arg]);
            let b = body(&compact);
            let n = compact_bytes.len();
            assert_eq!(&b[8..8 + n], &compact_bytes[..], "`{t}` compact bytes");
            assert!(
                b[..8].iter().chain(b[8 + n..].iter()).all(|&x| x == 0xAA),
                "`{t}` compact store is wider than the field"
            );
        }
        assert_eq!(
            PUT_CALLS.load(Ordering::SeqCst),
            before,
            "`{t}`: jit_putfield_* was called although the store is inline"
        );
        drop(legacy);
    }
}

/// A receiver the guard refuses (null) still reaches the unchanged helper.
#[test]
fn a_refused_primitive_putfield_receiver_takes_the_helper() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn counting(_obj: i64, _idx: i64, _val: i64) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }
    let mut h = helpers();
    h.putfield_int = counting as *const () as usize;
    let m = compile_setter(b'I', &h);
    // Null is thrown by the precise null check before the store is reached,
    // so use another refused shape: an unaligned pointer fails the guard's
    // alignment clause without ever being dereferenced.
    let mut backing = vec![0u64; 8];
    let unaligned = (backing.as_mut_ptr() as i64) + 4;
    call(&m, &[unaligned, 1]);
    assert_eq!(
        CALLS.load(Ordering::SeqCst),
        1,
        "the refused receiver must reach the helper"
    );
    drop(backing);
}

// ---------------------------------------------------------------------------
// Volatile static loads: no MFENCE unless asked (irlower2 request 1)
// ---------------------------------------------------------------------------

/// `static int get() { return V; }` with `V` a resolved `volatile int`,
/// compiled single-pass with `CRATONVM_JIT_VOLATILE_LOAD_FENCE` as given.
fn compile_volatile_get(fence: Option<&str>) -> CompiledMethod {
    let cm = cached("r9w3/Vol", "get", "()I", vec![0xb2, 0x00, 0x01, 0xac], 0, 0);
    let statics =
        |cp: u16| -> Option<(u32, usize, u8, bool)> { (cp == 1).then_some((7, 0, b'I', true)) };
    let h = helpers();
    cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_JIT_VOLATILE_LOAD_FENCE", fence)],
        || {
            try_compile(
                &cm,
                None,
                None,
                Some(&statics),
                None,
                None,
                None,
                None,
                None,
                None,
                &h,
                None,
                None,
                None,
                None,
                false, // single-pass
                false,
                false,
                false,
                false,
                false,
                None,
            )
        },
    )
    .expect("the volatile getter compiles single-pass")
}

fn mfences(m: &CompiledMethod) -> usize {
    m.code_bytes()
        .windows(3)
        .filter(|w| w[0] == 0x0F && w[1] == 0xAE && w[2] == 0xF0)
        .count()
}

/// x86-64 loads are already acquire, and the JMM's StoreLoad fence sits after
/// every volatile STORE, so a volatile static READ carries no `MFENCE` — in
/// both the inline load and the helper arm — unless the opt-in flag asks for
/// one, exactly as the IR tier decides it.
#[test]
fn a_volatile_static_load_has_no_fence_unless_asked() {
    // Compared, not counted from zero: the byte pattern could in principle
    // also occur inside a baked 64-bit address, identically in both bodies.
    let plain = compile_volatile_get(Some("0"));
    let fenced = compile_volatile_get(Some("1"));
    assert_eq!(
        mfences(&fenced),
        mfences(&plain) + 1,
        "exactly the one read-side MFENCE is controlled by CRATONVM_JIT_VOLATILE_LOAD_FENCE"
    );
}
