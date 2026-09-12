// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! activate-ir-optimizer step 2 — IR-vs-single-pass differential self-check.
//!
//! The optimizing IR pipeline and the single-pass `x64` backend are two
//! independent code generators for the same bytecode. Before the IR gate is
//! relaxed to run on more method shapes (step 3), they must be proven to agree:
//! a divergence is a miscompile. This harness compiles a corpus of pure-integer
//! methods through BOTH backends via the per-call `optimize` toggle
//! (`try_compile(.., optimize=true)` = IR pipeline, `optimize=false` =
//! single-pass) and asserts they return identical results for every sample
//! input — plus a host-computed correctness anchor so a *shared* bug is caught
//! too.
//!
//! Pure-integer methods are used deliberately: the IR pipeline only handles
//! 32-bit-int, call-free code (category-2 long/double and `invoke*` methods fall
//! back to single-pass, making the comparison trivially identical), so an int
//! corpus is exactly where the two backends genuinely differ.

use cratonvm_jit::{
    try_compile, CachedBytecodeMethod, CompiledMethod, InlineSite, JitRuntimeHelpers,
};
use cratonvm_types::{
    ClassId, ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, HEADER_SIZE, SLOT_SIZE,
};
use std::sync::Arc;

/// Dummy runtime helpers — the corpus is pure arithmetic / branches / counted
/// loops, so no helper (alloc, field, dispatch) is ever invoked; the stub
/// pointer is baked but never called.
/// Wide-open region bounds table for the guarded inline getfield: one region
/// spanning [0x1000, usize::MAX) so every 8-aligned non-null test object
/// passes the inline receiver guard and is raw-loaded (no helper call) —
/// the same "no runtime helper reachable" contract these tests were written
/// against when raw-inline getfield was the default.
static TEST_REGION_BOUNDS: [std::sync::atomic::AtomicUsize; 6] = [
    std::sync::atomic::AtomicUsize::new(0x1000),
    std::sync::atomic::AtomicUsize::new(usize::MAX),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
    std::sync::atomic::AtomicUsize::new(0),
];

/// The largest slot index any receiver this file builds actually has. Nothing
/// here allocates a wide object; `make_object` is called with at most a handful
/// of fields.
const STUB_MAX_FIELD_SLOT: i64 = 64;

/// Faults recorded by the helper stubs below, as human-readable lines.
///
/// A stub is an `extern "C"` fn, so a panic inside one CANNOT unwind: it aborts
/// the process. That is not a test failure, it is the loss of every test after
/// it in the file — which is exactly what happened here (see
/// [`stub_field_slot`]). So a stub never panics and never indexes with a value
/// it has not checked; it records the fault, returns a benign answer, and lets
/// the calling test's own assertion fail with a real message. The recorded line
/// is what says WHY.
fn stub_faults() -> &'static std::sync::Mutex<Vec<String>> {
    static FAULTS: std::sync::OnceLock<std::sync::Mutex<Vec<String>>> = std::sync::OnceLock::new();
    FAULTS.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Decode a field-helper slot argument, or record a fault and answer `None`.
///
/// # The decode
///
/// `jit_getfield`'s third argument is a slot index PLUS the flag bits in
/// `GETFIELD_FLAG_BITS` — `GETFIELD_RECEIVER_PROVEN_OOP` (bit 62) and, since
/// 2026-08-23, `GETFIELD_EXPECT_REFERENCE` (bit 61). A stub is a test double
/// for that helper and has to decode its argument the same way, through the one
/// decoder `cratonvm_jit_api::getfield_index_of`.
///
/// Missing that strip is what made
/// `ir_vs_singlepass_reference_putfield_then_getfield` abort the whole test
/// binary: `getfield_index_arg` sets `GETFIELD_EXPECT_REFERENCE` on the
/// REFERENCE path only, so every int test in this file passed while the one
/// reference read arrived with `idx = 0x2000_0000_0000_0000` and
/// `idx as usize * SLOT_SIZE` overflowed. The identical defect had already been
/// found and fixed in `jit/src/x64/tests.rs`'s `stub_getfield` four days
/// earlier; this file's twin was not updated with it.
///
/// # The bound
///
/// Stripping alone would have turned that abort into a wrong answer rather than
/// a crash, which is better but still not a diagnosis. The bound is what makes
/// a future bad argument REPORT itself: anything outside `0..STUB_MAX_FIELD_SLOT`
/// is a fault, not an address.
fn stub_field_slot(who: &str, raw: i64) -> Option<usize> {
    if let Some(idx) = decode_field_slot(raw) {
        return Some(idx);
    }
    let idx = cratonvm_jit_api::getfield_index_of(raw);
    if let Ok(mut faults) = stub_faults().lock() {
        faults.push(format!(
            "{who}: slot argument {raw:#x} decodes to {idx}, which is not a field slot \
             (0..{STUB_MAX_FIELD_SLOT}). If a new flag bit joined GETFIELD_FLAG_BITS, \
             `cratonvm_jit_api::getfield_index_of` is the one place that has to learn it."
        ));
    }
    None
}

/// The pure half of [`stub_field_slot`]: decode and bounds-check, no recording.
///
/// Split out so it can be tested directly — the fault list is process-wide and
/// the harness runs tests in parallel threads, so a test that deliberately
/// provoked a recording could clear another test's list.
fn decode_field_slot(raw: i64) -> Option<usize> {
    let idx = cratonvm_jit_api::getfield_index_of(raw);
    (0..STUB_MAX_FIELD_SLOT)
        .contains(&idx)
        .then_some(idx as usize)
}

/// Fail the calling test if any stub recorded a fault, and clear the list.
///
/// Call this from a test that drives the field helpers. It turns "the stub was
/// handed something it could not use" into a named assertion failure at the
/// point of use, instead of a wrong value the test then reports as a backend
/// divergence — or, before the bound existed, a process abort.
#[track_caller]
fn assert_no_stub_faults() {
    let taken: Vec<String> = match stub_faults().lock() {
        Ok(mut faults) => std::mem::take(&mut *faults),
        Err(_) => return,
    };
    assert!(
        taken.is_empty(),
        "a JIT runtime-helper stub was handed an argument it could not decode:\n  {}",
        taken.join("\n  "),
    );
}

/// `jit_getfield` for this harness's synthetic receivers, which use the LEGACY
/// uniform layout (`HEADER_SIZE + field_index * SLOT_SIZE`) that `make_object`
/// writes — see `compile_opt_fields`: "no registered CompactLayout for this
/// test's synthetic buffers". Null receiver → 0, matching the real helper's
/// null path and the inline lowering's.
///
/// # Safety
/// `obj` is either 0 or one of `make_object`'s live buffers, and `idx` is
/// within its field count (the corpus resolves only fields it allocated).
unsafe extern "C" fn legacy_getfield(_vm: i64, obj: i64, idx: i64) -> i64 {
    if obj == 0 {
        return 0;
    }
    let Some(idx) = stub_field_slot("legacy_getfield", idx) else {
        return 0;
    };
    let at = (obj as *const u8).add(HEADER_SIZE + idx * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET);
    std::ptr::read_unaligned(at as *const i32) as i64
}

/// `jit_putfield_int` for the same buffers. Writes the identical three-part
/// `Value::Int` cell the IR tier's inline store emits (discriminant, 32-bit
/// payload, cleared high qword) so the two lowerings are byte-comparable.
///
/// # Safety
/// Same contract as [`legacy_getfield`].
unsafe extern "C" fn legacy_putfield_int(obj: i64, idx: i64, val: i64) {
    if obj == 0 {
        return;
    }
    let Some(idx) = stub_field_slot("legacy_putfield_int", idx) else {
        return;
    };
    let base = (obj as *mut u8).add(HEADER_SIZE + idx * SLOT_SIZE);
    std::ptr::write_unaligned(base as *mut u32, 0);
    std::ptr::write_unaligned(
        base.add(FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32,
        val as i32,
    );
    std::ptr::write_unaligned(base.add(8) as *mut u64, 0);
}

fn dummy_helpers() -> JitRuntimeHelpers {
    // guarded_inline_getfield_enabled() is default-ON (jit/src/x64.rs) --
    // this whole file's contract ("no runtime helper reachable" via the
    // wide-open TEST_REGION_BOUNDS above) relies on the guarded-inline path
    // being taken, which now happens without an explicit opt-in.
    unsafe extern "C" fn stub() {
        panic!("ir_vs_singlepass invoked an unwired runtime helper");
    }
    unsafe extern "C" fn self_guard(_vm_ptr: i64) -> i64 {
        0
    }
    unsafe extern "C" fn native_stack_floor() -> i64 {
        0
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
        // NOT the panicking stub. `ir_lower` lowers `Op::Load` through
        // `getfield` whenever the address is present, and `Op::Store` through
        // `putfield_int` whenever compact layout is on (it is, by default) —
        // so the field corpus below genuinely reaches these. They used to be
        // unreachable for a reason that made the corpus vacuous rather than
        // safe: the IR builder refused every getfield/putfield method under
        // compact layout, so `compile_opt_fields(.., optimize=true)` handed
        // back a SINGLE-PASS body and the differential compared single-pass
        // with itself. These two implement exactly the layout `make_object`
        // writes, so the comparison is now real.
        getfield: legacy_getfield as *const () as usize,
        putfield_int: legacy_putfield_int as *const () as usize,
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
        // Panic stub by default: only consulted on a J/D call's `RAX == i64::MIN`
        // branch, which no existing test reaches. The long-return tests below
        // override this per-test with a real returns-0/1 stub.
        dispatch_threw: s,
        jit_frem: s,
        jit_drem: s,
        self_call_stack_guard: self_guard as *const () as usize,
        region_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        // Same table through the READ-side slot: the guarded getfield arms
        // (both tiers) bake `read_bounds_addr`, while the inline reference
        // putfield arms keep baking `region_bounds_addr` above.
        read_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: native_stack_floor as *const () as usize,
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

fn cached(
    name: &str,
    descriptor: &str,
    mut code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
) -> CachedBytecodeMethod {
    // VM convention: `CachedBytecodeMethod.code` is the bytecode padded with two
    // trailing 0x00 bytes (`vm/src/.../interpreter.rs` "padded_bytecode adds 2";
    // `frame.rs` asserts the two trailing zeros). `jit::try_compile` recovers the
    // real length via `code.len() - 2`. Without the padding the last two opcodes
    // are dropped — the method emits without a `ret` and crashes when called.
    code.push(0x00);
    code.push(0x00);
    CachedBytecodeMethod {
        declaring_class_id: ClassId::new(1),
        class_name: Arc::from("Corpus"),
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
        native_callback_cache: std::sync::OnceLock::new(),
        invoc_key: std::sync::OnceLock::new(),
        jit_probe_generation: std::sync::atomic::AtomicU64::new(0),
        quickened: std::sync::OnceLock::new(),
    }
}

/// This harness is about ROUTING and about the two backends AGREEING. The
/// C1->C2 acceptance gate is a POLICY layered on top of that, and every probe
/// here is a handful of bytecodes that by construction applies no transform the
/// baseline tier lacks -- so under the default policy the gate refuses the IR
/// body, `try_compile` falls back to single-pass, and this harness compares
/// single-pass against itself.
///
/// That is not a weaker differential test, it is one that CANNOT FAIL: both
/// arms are the same backend. On 2026-09-06 it turned 17 of these tests green
/// for the wrong reason and red for the right one, which is how it was found.
///
/// Every compile helper in this file calls this first. Idempotent, and
/// deliberately one-way -- see `force_accept_always_for_this_process`.
fn routing_not_policy() {
    cratonvm_jit::ir_evidence::force_accept_always_for_this_process();
}

fn compile_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, false, false, false, None,
    )
}

/// Compile `(name, code)` both ways and assert IR == single-pass == `expected`
/// (low 32 bits — an int method) for every `(args, expected)` case.
fn check(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid pure-int
        // bytecode into executable memory; the System V i64-arg / i64-ret entry
        // ABI matches `try_call` (the same path the VM uses to invoke both
        // backends), and no runtime helper is reachable for these methods.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {args:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {}, expected {expected}",
            r_ir as i32,
        );
    }
}

/// Like [`compile_opt`] but with the `ir_emit_long` gate ON (inc 25), so the IR
/// pipeline admits long-using methods. The single-pass side is unaffected
/// (`ir_emit_long` is consulted only on the optimizing path).
fn compile_long_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, true, false, false, None,
    )
}

/// Like [`compile_opt`] but with the `ir_emit_fp` gate ON (inc 30), so the IR
/// pipeline admits a method that uses `float`/`double` internally (FP-free
/// signature). The single-pass side is unaffected.
fn compile_fp_opt(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm, None, None, None, None, None, None, None, None, None, helpers, None, None, None, None,
        optimize, false, false, false, false, true, None,
    )
}

/// Compile an FP-using `(name, code)` both ways (IR with `ir_emit_fp` on vs
/// single-pass) and assert IR == single-pass == `expected` (low 32 bits — every
/// FP corpus method takes int args and returns an int, so the GPR i64-arg /
/// i64-ret `try_call` ABI is exact and the FP work stays internal to XMM). The
/// host-computed `expected` is the IEEE-754 anchor catching a *shared* bug.
fn check_fp(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_fp_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR, FP) failed to compile"));
    let sp = compile_fp_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid FP bytecode
        // with an int signature; the i64-arg / i64-ret ABI matches `try_call`,
        // no helper is reachable.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {args:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {}, expected {expected}",
            r_ir as i32,
        );
    }
}

/// Compile a long-using `(name, code)` both ways and assert IR == single-pass ==
/// `expected` over the FULL 64 bits (a `long` method). The JIT ABI passes each
/// parameter as one i64 register and returns the result in RAX, so `try_call`
/// args/return are full i64. (Routing through the IR path — not a vacuous
/// single-pass fall-through — is separately proven by `ir_long_wiring_…` in
/// `lib.rs` via `IR_LOWER_COMPILES`.)
fn check_long(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i64)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_long_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR, long) failed to compile"));
    let sp = compile_long_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, expected) in cases {
        // SAFETY: both bodies were produced by the JIT from valid long bytecode;
        // the i64-arg / i64-ret ABI matches `try_call`, no helper is reachable.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir, r_sp,
            "{name}: IR vs single-pass DIVERGE (full i64) for {args:?}: IR={r_ir}, single-pass={r_sp}",
        );
        assert_eq!(
            r_ir, *expected,
            "{name}: both backends agree but disagree with host for {args:?}: got {r_ir}, expected {expected}",
        );
    }
}

#[test]
fn ir_vs_singlepass_long_add() {
    // long add(long a, long b) { return a + b; }
    //   lload_0; lload_2; ladd; lreturn
    let code = vec![0x1e, 0x20, 0x61, 0xad];
    check_long(
        "ladd",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 7),
            (vec![i64::MAX, 1], i64::MIN), // 64-bit wrap
            // genuinely 64-bit (a 32-bit ADD would drop the high word):
            (vec![0x1_0000_0000, 0x2_0000_0000], 0x3_0000_0000),
            (vec![-5, -7], -12),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_two_slot_params() {
    // long f(long a, long b) { return a*b - b; }  — exercises lload_2 (b at JVM
    // slot 2, the heart of the cat-2 two-slot param layout fix).
    //   lload_0; lload_2; lmul; lload_2; lsub; lreturn
    let code = vec![0x1e, 0x20, 0x69, 0x20, 0x65, 0xad];
    check_long(
        "lmulsub",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 8),                             // 12 - 4
            (vec![0x1_0000_0000, 3], 0x3_0000_0000 - 3), // 64-bit mul
            (vec![-2, 5], -15),                          // -10 - 5
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_mixed_int_long_param() {
    // long f(int a, long b) { return (long)a + b; }  — int param `a` at slot 0
    // (1 slot), long `b` at slots 1-2; lload_1 must read b.
    //   iload_0; i2l; lload_1; ladd; lreturn
    let code = vec![0x1a, 0x85, 0x1f, 0x61, 0xad];
    check_long(
        "mixedil",
        "(IJ)J",
        code,
        3,
        2,
        &[
            (vec![5, 7], 12),
            (vec![-1, 0x1_0000_0000], 0x1_0000_0000 - 1), // i2l sign-extends a
            (vec![100, -50], 50),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_to_int_return() {
    // int f(long a, long b) { return (int)(a + b); }  — l2i truncates to low 32.
    //   lload_0; lload_2; ladd; l2i; ireturn
    let code = vec![0x1e, 0x20, 0x61, 0x88, 0xac];
    let helpers = dummy_helpers();
    let cm = cached("l2iret", "(JJ)I", code, 4, 2);
    let ir = compile_long_opt(&cm, &helpers, true).expect("IR long");
    let sp = compile_long_opt(&cm, &helpers, false).expect("single-pass");
    for (a, b) in [(3i64, 4i64), (0x1_0000_0005, 0x1_0000_0002), (i64::MAX, 1)] {
        let r_ir = unsafe { ir.try_call(&[a, b]) }.unwrap();
        let r_sp = unsafe { sp.try_call(&[a, b]) }.unwrap();
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "l2i IR vs single-pass for ({a},{b})"
        );
        assert_eq!(
            r_ir as i32,
            a.wrapping_add(b) as i32,
            "l2i vs host for ({a},{b})"
        );
    }
}

#[test]
fn ir_vs_singlepass_long_wide_load_store() {
    // inc 26: long params/locals at JVM slots >= 4 use the WIDE `lload` (0x16) /
    // `lstore` (0x37) forms (not the short `lload_2`/`lstore_3`).
    // long f(long a, long b, long c) { long d = a + b; long e = c - d; return d * e; }
    //   a@0-1 b@2-3 c@4-5 d@6-7 e@8-9
    //   lload_0; lload_2; ladd; lstore 6; lload 4; lload 6; lsub; lstore 8;
    //   lload 6; lload 8; lmul; lreturn
    let code = vec![
        0x1e, 0x20, 0x61, // d = a + b
        0x37, 0x06, // lstore 6 (d)
        0x16, 0x04, // lload 4 (c)
        0x16, 0x06, // lload 6 (d)
        0x65, // lsub  -> c - d
        0x37, 0x08, // lstore 8 (e)
        0x16, 0x06, // lload 6 (d)
        0x16, 0x08, // lload 8 (e)
        0x69, // lmul -> d * e
        0xad, // lreturn
    ];
    check_long(
        "lwide",
        "(JJJ)J",
        code,
        10,
        3,
        &[
            // host uses wrapping ops to match the JVM's 64-bit `long` semantics.
            (vec![3, 4, 100], {
                let d = 3i64.wrapping_add(4);
                let e = 100i64.wrapping_sub(d);
                d.wrapping_mul(e) // 7 * 93 = 651
            }),
            (vec![0x1_0000_0000, 1, 0x4_0000_0000], {
                let d = 0x1_0000_0000i64.wrapping_add(1);
                let e = 0x4_0000_0000i64.wrapping_sub(d);
                d.wrapping_mul(e) // overflows 64-bit → wraps
            }),
            (vec![-2, -3, 10], {
                let d = (-2i64).wrapping_add(-3);
                let e = 10i64.wrapping_sub(d);
                d.wrapping_mul(e)
            }),
        ],
    );
}

/// Like [`compile_long_opt`] but also supplies an `ldc2_w` long-constant resolver
/// (inc 26) so the IR builder can lower long constants from the constant pool.
fn compile_long_ldc2w(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    ldc2w: &dyn Fn(u16) -> Option<(i64, bool)>, // inc 35: (bits, is_double)
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(ldc2w),
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        true,
        false, // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,
    )
}

/// Like [`compile_long_ldc2w`] but with the FP gate on (inc 35) so a `double`
/// `ldc2_w` constant is admitted and lowered to `dconst`.
fn compile_fp_ldc2w(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    ldc2w: &dyn Fn(u16) -> Option<(i64, bool)>,
) -> Option<CompiledMethod> {
    try_compile(
        cm,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(ldc2w),
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        true,
        false,
        true, // ir_emit_fp ON
        None,
    )
}

#[test]
fn ir_vs_singlepass_double_ldc2w_constant() {
    // inc 35: a `double` `ldc2_w` constant. double f(double a) { return a * 1.5; }
    //   dload_0; ldc2_w #1 (1.5); dmul; dreturn
    const C: f64 = 1.5;
    let code = vec![0x26, 0x14, 0x00, 0x01, 0x6b, 0xaf];
    let ldc2w = |cp: u16| -> Option<(i64, bool)> {
        match cp {
            1 => Some((C.to_bits() as i64, true)), // double → is_double = true
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("dldc", "(D)D", code, 2, 1);
    let ir = compile_fp_ldc2w(&cm, &helpers, true, &ldc2w).expect("IR double ldc2_w");
    let sp = compile_fp_ldc2w(&cm, &helpers, false, &ldc2w).expect("single-pass double ldc2_w");
    for a in [3.0f64, 0.0, -7.25, 1e10, -0.5] {
        let args = [a.to_bits() as i64];
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let host = a * C;
        assert_eq!(r_ir.to_bits(), host.to_bits(), "double ldc2_w IR for a={a}");
        assert_eq!(
            r_sp.to_bits(),
            host.to_bits(),
            "double ldc2_w single-pass for a={a}"
        );
    }
}

#[test]
fn ir_vs_singlepass_long_ldc2w_constant() {
    // inc 26: `ldc2_w` long constants. long f(long a) { return a * C1 + C2; }
    //   lload_0; ldc2_w #1; lmul; ldc2_w #2; ladd; lreturn
    const C1: i64 = 1_000_000_007;
    const C2: i64 = 0x7FFF_FFFF_0000_002A; // a large 64-bit constant (not 32-bit)
    let code = vec![
        0x1e, // lload_0 (a)
        0x14, 0x00, 0x01, // ldc2_w #1 (C1)
        0x69, // lmul
        0x14, 0x00, 0x02, // ldc2_w #2 (C2)
        0x61, // ladd
        0xad, // lreturn
    ];
    let ldc2w = |cp: u16| -> Option<(i64, bool)> {
        match cp {
            1 => Some((C1, false)), // long constants → is_double = false
            2 => Some((C2, false)),
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("lldc", "(J)J", code, 2, 1);
    let ir = compile_long_ldc2w(&cm, &helpers, true, &ldc2w).expect("IR long ldc2_w");
    let sp = compile_long_ldc2w(&cm, &helpers, false, &ldc2w).expect("single-pass");
    for a in [3i64, 0, -7, 0x1_0000_0000, i64::MAX] {
        let r_ir = unsafe { ir.try_call(&[a]) }.unwrap();
        let r_sp = unsafe { sp.try_call(&[a]) }.unwrap();
        let host = a.wrapping_mul(C1).wrapping_add(C2);
        assert_eq!(r_ir, r_sp, "ldc2_w IR vs single-pass for a={a}");
        assert_eq!(r_ir, host, "ldc2_w vs host for a={a}");
    }
}

// ── cov-01: `ldc` / `ldc_w` (0x12 / 0x13) ────────────────────────────────
//
// `ldc` + `ldc_w` + `getstatic` was 189 of the 273 opcode-gap events measured
// on 2026-08-03 (`ir-coverage-survey-20260803.md`) — 69%
// of every opcode `IrBuilder::build` had no arm for. Increment 1 is the
// IMMEDIATE case: an `int` or `float` constant the caller's `cp_ldc_resolver`
// already reduced to bits.

/// [`compile_opt`] plus a `cp_ldc_resolver`, so the IR builder can lower
/// `ldc` / `ldc_w`. `fp` turns the `ir_emit_fp` gate on — a `float` constant is
/// admitted only under it, exactly as a `double` `ldc2_w` is.
fn compile_ldc(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    fp: bool,
    ldc: &dyn Fn(u16) -> Option<cratonvm_jit::JitLdcConstant>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(ldc),
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        false,
        false,
        fp,
        None,
    )
}

#[test]
fn ir_vs_singlepass_int_ldc_constant() {
    // cov-01 inc 1. int f(int a) { return a * C1 + C2; }
    //   iload_0; ldc #1; imul; ldc_w #2; iadd; ireturn
    //
    // Both widths in one body on purpose: `ldc` is 2 bytes and `ldc_w` is 3,
    // and a wrong `pc` advance in either arm desyncs the abstract walk rather
    // than producing a wrong number, so a shared-length bug would show up as a
    // build failure on one backend and not the other.
    const C1: i32 = 1_000_003;
    const C2: i32 = -2_000_000_011;
    let code = vec![
        0x1a, // iload_0 (a)
        0x12, 0x01, // ldc #1 (C1)
        0x68, // imul
        0x13, 0x00, 0x02, // ldc_w #2 (C2)
        0x60, // iadd
        0xac, // ireturn
    ];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::Immediate {
                bits: C1 as i64,
                is_float: false,
            }),
            2 => Some(cratonvm_jit::JitLdcConstant::Immediate {
                bits: C2 as i64,
                is_float: false,
            }),
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("ildc", "(I)I", code, 2, 1);
    let ir = compile_ldc(&cm, &helpers, true, false, &ldc).expect("IR int ldc");
    let sp = compile_ldc(&cm, &helpers, false, false, &ldc).expect("single-pass int ldc");
    assert!(
        ir.used_ir_backend,
        "cov-01: an int `ldc` must reach the optimizing backend, not fall through"
    );
    for a in [3i32, 0, -7, i32::MAX, i32::MIN, 65_536] {
        let r_ir = unsafe { ir.try_call(&[a as i64]) }.unwrap() as i32;
        let r_sp = unsafe { sp.try_call(&[a as i64]) }.unwrap() as i32;
        let host = a.wrapping_mul(C1).wrapping_add(C2);
        assert_eq!(r_ir, r_sp, "int ldc IR vs single-pass for a={a}");
        assert_eq!(r_ir, host, "int ldc vs host for a={a}");
    }
}

#[test]
fn ir_vs_singlepass_float_ldc_constant() {
    // cov-01 inc 1, the float half. int f(int a) { return (int)(a * 2.5f); }
    //   iload_0; i2f; ldc #1 (2.5f); fmul; f2i; ireturn
    //
    // Returns an int so the GPR `try_call` ABI is exact; the FP work stays
    // internal. The point of the case is the TYPE: `is_float` has to reach the
    // builder, because `is_float_opcode` does not list `ldc` and the value is
    // otherwise indistinguishable from an int with the same bit pattern.
    const C: f32 = 2.5;
    let code = vec![
        0x1a, // iload_0 (a)
        0x86, // i2f
        0x12, 0x01, // ldc #1 (2.5f)
        0x6a, // fmul
        0x8b, // f2i
        0xac, // ireturn
    ];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::Immediate {
                bits: C.to_bits() as i64,
                is_float: true,
            }),
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("fldc", "(I)I", code, 2, 1);
    let ir = compile_ldc(&cm, &helpers, true, true, &ldc).expect("IR float ldc");
    let sp = compile_ldc(&cm, &helpers, false, true, &ldc).expect("single-pass float ldc");
    assert!(
        ir.used_ir_backend,
        "cov-01: a float `ldc` must reach the optimizing backend under the FP gate"
    );
    for a in [3i32, 0, -7, 1000, -1_000_001] {
        let r_ir = unsafe { ir.try_call(&[a as i64]) }.unwrap() as i32;
        let r_sp = unsafe { sp.try_call(&[a as i64]) }.unwrap() as i32;
        let host = (a as f32 * C) as i32;
        assert_eq!(r_ir, r_sp, "float ldc IR vs single-pass for a={a}");
        assert_eq!(r_ir, host, "float ldc vs host for a={a}");
    }
}

#[test]
fn float_ldc_stays_on_single_pass_with_the_fp_gate_off() {
    // The fail-closed half, and the edit that trips it: delete the
    // `!is_float || ir_emit_fp` guard in `try_compile`'s cov-01 feed and this
    // method reaches the optimizing backend with an `Op::ConstF`/`Float` node
    // in a graph the FP tier is switched off for.
    //
    // `is_float_opcode` does NOT list `ldc`, so `fp_in_body` is false for a
    // body that contains NO other FP opcode — which is exactly why this method
    // is ADMITTED to the optimizing pipeline (through the int clause) and has
    // to be refused by the builder rather than by admission. The `pop` keeps
    // the body FP-opcode-free while still containing the constant; without it
    // the refusal would come from the admission gate and the test would pass
    // vacuously with the guard deleted.
    //
    // `float f(){ 2.5f; return 0; }` — ldc #1; pop; iconst_0; ireturn.
    let code = vec![
        0x12, 0x01, // ldc #1 (2.5f)
        0x57, // pop
        0x03, // iconst_0
        0xac, // ireturn
    ];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::Immediate {
                bits: 2.5f32.to_bits() as i64,
                is_float: true,
            }),
            _ => None,
        }
    };
    let helpers = dummy_helpers();
    let cm = cached("fldcoff", "()I", code.clone(), 1, 0);
    let off = compile_ldc(&cm, &helpers, true, false, &ldc)
        .expect("the method still compiles — on the single-pass backend");
    assert!(
        !off.used_ir_backend,
        "cov-01: a float `ldc` with the FP gate off must bail to single-pass"
    );
    // The positive control for the same body: with the gate ON it IS lowered.
    // Without this rung, "the gate is off" and "the builder cannot lower this
    // shape at all" are indistinguishable and the assertion above proves
    // nothing about the guard.
    let cm_on = cached("fldcon", "()I", code, 1, 0);
    let on = compile_ldc(&cm_on, &helpers, true, true, &ldc).expect("IR body under the FP gate");
    assert!(
        on.used_ir_backend,
        "cov-01: the same body must reach the optimizing backend with the FP gate on"
    );
    assert_eq!(unsafe { on.try_call(&[]) }.unwrap() as i32, 0);
}

#[test]
fn class_ldc_with_the_helper_unwired_stays_on_single_pass() {
    // `ldc <Class>` is served by `helpers.ldc_class_cp`, which is an OptionalPtr
    // — a hand-built table leaves it 0. The IR feed must then OMIT the site so
    // the builder bails, exactly as the single-pass arm refuses the same
    // condition, rather than planning a CALL through address zero.
    //
    // The edit that trips it: drop the `helpers.ldc_class_cp != 0` guard from
    // the cov-01 feed in `try_compile`.
    //
    // (A `String` / `Class` `ldc` with its helper WIRED is lowered — increments
    // 2 and 3 — and is covered by `ir_vs_singlepass_string_ldc` /
    // `ir_vs_singlepass_class_ldc` further down. What stays refused for good is
    // a `MethodHandle` / `MethodType` / condy site, whose resolver returns
    // `None`; that is the next case.)
    let code = vec![
        0x12, 0x01, // ldc #1
        0xb0, // areturn
    ];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::ClassMirror {
                holder_class_id: 1,
                cp_idx: 1,
            }),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.ldc_class_cp = 0;
    let cm = cached("cldcoff", "()Ljava/lang/Object;", code, 1, 0);
    // The single-pass backend bails the WHOLE compile on this condition
    // (`ldc_class_helper_unwired`), so `None` is the expected outcome and is
    // itself fail-closed. Either way the optimizing backend must not have
    // produced a body.
    if let Some(compiled) = compile_ldc(&cm, &helpers, true, false, &ldc) {
        assert!(
            !compiled.used_ir_backend,
            "cov-01: an `ldc <Class>` whose helper is unwired must not reach the \
             optimizing backend"
        );
    }
}

#[test]
fn an_unresolvable_ldc_stays_on_single_pass() {
    // The permanent fail-closed half, and the one no later increment lifts. A
    // `MethodHandle` / `MethodType` / condy `ldc` is what `cp_ldc_resolver`
    // answers `None` for: not an immediate, not a String, not a Class mirror.
    // It is absent from all three of the builder's tables and must bail the
    // method — "a constant materialised without its resolution side effects is
    // a wrong-code bug, not a missing optimisation".
    //
    // The edit that trips it: give the builder's 0x12 arm a fallback that
    // pushes anything at all for a pc absent from every table.
    let code = vec![
        0x12, 0x01, // ldc #1
        0xb0, // areturn
    ];
    let ldc = |_cp: u16| -> Option<cratonvm_jit::JitLdcConstant> { None };
    let helpers = dummy_helpers();
    let cm = cached("mhldc", "()Ljava/lang/Object;", code, 1, 0);
    // `try_compile` records this as a PERMANENT bail on the single-pass side
    // too (RBC.7 — the constant-pool entry's kind never changes), so `None`
    // here is the expected shape.
    if let Some(compiled) = compile_ldc(&cm, &helpers, true, false, &ldc) {
        assert!(
            !compiled.used_ir_backend,
            "cov-01: an unresolvable `ldc` must not reach the optimizing backend"
        );
    }
}

#[test]
fn ir_vs_singlepass_long_ldiv() {
    // long signed division. long f(long a, long b) { return a / b; }
    //   lload_0; lload_2; ldiv; lreturn
    // Non-zero divisors only — a zero divisor deopts (returns the i64::MIN
    // sentinel) and is validated live (the interpreter throws ArithmeticException).
    let code = vec![0x1e, 0x20, 0x6d, 0xad];
    check_long(
        "ldiv",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![7, 2], 3),
            (vec![-7, 2], -3), // Java truncates toward zero
            (vec![7, -2], -3),
            (vec![-7, -2], 3),
            // genuinely 64-bit: a 32-bit IDIV would mis-divide these.
            (vec![0x7FFF_FFFF_FFFF_FFFF, 3], 0x7FFF_FFFF_FFFF_FFFF / 3),
            (vec![0x1_0000_0000, 2], 0x8000_0000),
            // JVMS §6.5.ldiv overflow: LONG_MIN / -1 == LONG_MIN (no #DE / no
            // exception). The lowerer's overflow guard must synthesise this.
            (vec![i64::MIN, -1], i64::MIN),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_lrem() {
    // long signed remainder (mirrors ldiv). long f(long a,long b){return a%b;}
    //   lload_0; lload_2; lrem; lreturn
    let code = vec![0x1e, 0x20, 0x71, 0xad];
    check_long(
        "lrem",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![7, 2], 1),
            (vec![-7, 2], -1), // Java: remainder sign follows the dividend
            (vec![7, -2], 1),
            (vec![-7, -2], -1),
            (
                vec![0x7FFF_FFFF_FFFF_FFFF, 1_000_000_007],
                0x7FFF_FFFF_FFFF_FFFF % 1_000_000_007,
            ),
            // JVMS §6.5.lrem overflow: LONG_MIN % -1 == 0 (no #DE / no exception).
            (vec![i64::MIN, -1], 0),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_shifts() {
    // inc 27: long shifts. long f(long a, int n){ return (a<<n) + (a>>n) + (a>>>n); }
    //   a@0-1, n@2.  lload_0; iload_2; lshl; lload_0; iload_2; lshr; ladd;
    //   lload_0; iload_2; lushr; ladd; lreturn
    let code = vec![
        0x1e, 0x1c, 0x79, // a << n
        0x1e, 0x1c, 0x7b, 0x61, // + (a >> n)
        0x1e, 0x1c, 0x7d, 0x61, // + (a >>> n)
        0xad,
    ];
    check_long(
        "lshifts",
        "(JI)J",
        code,
        3,
        2,
        &[
            // host: JVM masks the shift count to 6 bits (count & 0x3f) for long.
            (vec![1, 1], {
                let a = 1i64;
                (a << (1 & 63))
                    .wrapping_add(a >> (1 & 63))
                    .wrapping_add((a as u64 >> (1 & 63)) as i64)
            }),
            (vec![-1, 4], {
                let a = -1i64;
                (a << 4)
                    .wrapping_add(a >> 4)
                    .wrapping_add((a as u64 >> 4) as i64)
            }),
            (vec![0x1234_5678_9abc_def0u64 as i64, 40], {
                let a = 0x1234_5678_9abc_def0u64 as i64;
                (a << (40 & 63))
                    .wrapping_add(a >> (40 & 63))
                    .wrapping_add((a as u64 >> (40 & 63)) as i64)
            }),
            (vec![i64::MIN, 1], {
                let a = i64::MIN;
                (a << 1)
                    .wrapping_add(a >> 1)
                    .wrapping_add((a as u64 >> 1) as i64)
            }),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_bitwise() {
    // inc 27: long bitwise. long f(long a, long b){ return (a & b) | (a ^ b); }
    //   lload_0; lload_2; land; lload_0; lload_2; lxor; lor; lreturn
    let code = vec![
        0x1e, 0x20, 0x7f, // a & b
        0x1e, 0x20, 0x83, // a ^ b
        0x81, // |
        0xad,
    ];
    check_long(
        "lbitwise",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (
                vec![
                    0x0f0f_0f0f_0f0f_0f0fu64 as i64,
                    0x00ff_00ff_00ff_00ffu64 as i64,
                ],
                {
                    let (a, b) = (
                        0x0f0f_0f0f_0f0f_0f0fu64 as i64,
                        0x00ff_00ff_00ff_00ffu64 as i64,
                    );
                    (a & b) | (a ^ b)
                },
            ),
            (vec![-1, 0], -1),
            (vec![0, -1], -1),
            (vec![i64::MIN, i64::MAX], {
                let (a, b) = (i64::MIN, i64::MAX);
                (a & b) | (a ^ b)
            }),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_lcmp_branch() {
    // inc 27: lcmp + a long-fed branch. long min(long a, long b){ return a<b?a:b; }
    //   lload_0; lload_2; lcmp; ifge ELSE; lload_0; lreturn; ELSE: lload_2; lreturn
    //   pc0 lload_0; pc1 lload_2; pc2 lcmp; pc3 ifge +5(->pc8);
    //   pc6 lload_0; pc7 lreturn; pc8 lload_2; pc9 lreturn
    let code = vec![
        0x1e, 0x20, 0x94, // a, b, lcmp
        0x9c, 0x00, 0x05, // ifge +5 -> pc 8 (else: a >= b -> return b)
        0x1e, 0xad, // then (a < b): return a
        0x20, 0xad, // else: return b
    ];
    check_long(
        "lmin",
        "(JJ)J",
        code,
        4,
        2,
        &[
            (vec![3, 4], 3),
            (vec![4, 3], 3),
            (vec![5, 5], 5),
            (vec![-1, i64::MIN], i64::MIN),
            (vec![i64::MAX, i64::MIN], i64::MIN),
            (vec![0x1_0000_0000, 0xffff_ffff], 0xffff_ffff),
        ],
    );
}

#[test]
fn ir_vs_singlepass_long_loop_phi_lcmp() {
    // inc 27: a long loop-carried accumulator with a long-fed loop condition —
    // exercises Op::LCmp + a backward long branch + a long loop phi (now typed
    // Long, not Int) + wide lload/lstore, all together. No int counter (avoids
    // the not-yet-handled wide iload), no idiv/call (stays safepoint-free).
    //   long f(long a, long limit){ long s=0; while (s < limit) s = s + a; return s; }
    //   a@0-1, limit@2-3, s@4-5
    let code = vec![
        0x09, // lconst_0
        0x37, 0x04, // lstore 4 (s=0)
        // LOOP (pc 3):
        0x16, 0x04, // lload 4 (s)
        0x20, // lload_2 (limit)
        0x94, // lcmp
        0x9c, 0x00, 0x0c, // ifge +12 -> END (pc 19) if s >= limit
        0x16, 0x04, // lload 4 (s)
        0x1e, // lload_0 (a)
        0x61, // ladd
        0x37, 0x04, // lstore 4 (s = s + a)
        0xa7, 0xff, 0xf3, // goto -13 -> LOOP (pc 3)
        // END (pc 19):
        0x16, 0x04, // lload 4 (s)
        0xad, // lreturn
    ];
    check_long(
        "lloop",
        "(JJ)J",
        code,
        6,
        2,
        &[
            (vec![3, 10], {
                let (a, limit) = (3i64, 10i64);
                let mut s = 0i64;
                while s < limit {
                    s = s.wrapping_add(a);
                }
                s
            }),
            (vec![5, 5], 5),
            (vec![1, 64], 64),
            (vec![7, 50], 56),
            (vec![100, 1], 100),
        ],
    );
}

#[test]
fn ir_vs_singlepass_add() {
    // int add(int a, int b) { return a + b; }
    //   iload_0; iload_1; iadd; ireturn
    check(
        "add",
        "(II)I",
        vec![0x1a, 0x1b, 0x60, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 7),
            (vec![10, -3], 7),
            (vec![-5, -6], -11),
            (vec![i32::MAX as i64, 1], i32::MIN), // wraps
        ],
    );
}

#[test]
fn ir_vs_singlepass_poly() {
    // int poly(int a) { return a*a - 2*a + 1; }   (== (a-1)^2)
    //   iload_0; iload_0; imul; iconst_2; iload_0; imul; isub; iconst_1; iadd; ireturn
    check(
        "poly",
        "(I)I",
        vec![
            0x1a, 0x1a, 0x68, // a*a
            0x05, 0x1a, 0x68, // 2*a
            0x64, // a*a - 2*a
            0x04, 0x60, // + 1
            0xac,
        ],
        1,
        1,
        &[
            (vec![1], 0),
            (vec![3], 4),
            (vec![0], 1),
            (vec![-2], 9),
            (vec![5], 16),
        ],
    );
}

// ── Multiple return points (conditional early return) ──────────────────
//
// These were the harness's first catch: the IR pipeline used to root DCE only
// from `graph.exit` (the LAST `Op::Return`, since each `ireturn` overwrites it),
// deleting every other return path and collapsing the conditional into a
// single-successor branch that always took the surviving return. Fixed by
// rooting `eliminate_dead_nodes` from ALL `Op::Return` nodes (ir_optimize.rs).

#[test]
fn ir_vs_singlepass_conditional_early_return() {
    // int sgn2(int a) { if (a<0) return -1; return 1; }   (one branch, two returns)
    check(
        "sgn2",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x05, // iload_0; ifge +5 → 6
            0x02, 0xac, // iconst_m1; ireturn
            0x04, 0xac, // iconst_1; ireturn
        ],
        1,
        1,
        &[
            (vec![-5], -1),
            (vec![0], 1),
            (vec![7], 1),
            (vec![i32::MIN as i64], -1),
            (vec![i32::MAX as i64], 1),
        ],
    );
}

/// `ifnull` / `if_acmpeq` must compare all 64 bits of a reference.
///
/// The IR `Op::Cmp` was written for the int comparisons and emitted a 32-bit
/// `CMP EAX, ECX`. A heap pointer whose low word happens to be zero would then
/// test equal to null, and two objects exactly 4 GiB apart would test equal to
/// each other — a silently wrong branch, not a crash. The operands here are
/// deliberately chosen so a 32-bit compare gives the opposite answer.
#[test]
fn ir_vs_singlepass_ifnull_compares_all_64_bits() {
    // int isNull(Object a) { return a == null ? 0 : 1; }
    check(
        "isNull",
        "(Ljava/lang/Object;)I",
        vec![
            0x2a, 0xc6, 0x00, 0x05, // aload_0; ifnull +5 → 6
            0x04, 0xac, // iconst_1; ireturn
            0x03, 0xac, // iconst_0; ireturn
        ],
        1,
        1,
        &[
            (vec![0], 0),
            (vec![0x7f_1234_5678], 1),
            // Low 32 bits are zero: a 32-bit compare calls this null.
            (vec![0x1_0000_0000], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_if_acmp_compares_all_64_bits() {
    // int same(Object a, Object b) { return a == b ? 1 : 0; }
    check(
        "same",
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        vec![
            0x2a, 0x2b, 0xa5, 0x00, 0x05, // aload_0; aload_1; if_acmpeq +5 → 7
            0x03, 0xac, // iconst_0; ireturn
            0x04, 0xac, // iconst_1; ireturn
        ],
        2,
        2,
        &[
            (vec![0x7f_1234_5678, 0x7f_1234_5678], 1),
            (vec![0x7f_1234_5678, 0x7f_1234_5680], 0),
            // Differ only above bit 32: a 32-bit compare calls these equal.
            (vec![0x1_0000_0000, 0x2_0000_0000], 0),
            (vec![0, 0], 1),
        ],
    );
}

/// `aconst_null` + `areturn`: both were unlowered, so any method returning an
/// object — or terminating a structure with a null literal — was refused at
/// stage one of the optimizing pipeline.
#[test]
fn ir_vs_singlepass_aconst_null_areturn() {
    // Object pick(Object a, int flag) { return flag != 0 ? a : null; }
    // `check` compares the low 32 bits, which is enough to tell the returned
    // pointer from null here.
    check(
        "pick",
        "(Ljava/lang/Object;I)Ljava/lang/Object;",
        vec![
            0x1b, 0x99, 0x00, 0x05, // iload_1; ifeq +5 → 6
            0x2a, 0xb0, // aload_0; areturn
            0x01, 0xb0, // aconst_null; areturn
        ],
        2,
        2,
        &[
            (vec![0x7f_1234_5678, 1], 0x1234_5678),
            (vec![0x7f_1234_5678, 0], 0),
        ],
    );
}

#[test]
fn ir_vs_singlepass_two_branch_three_returns() {
    // int sgn3(int a) { if (a<0) return -1; if (a>0) return 1; return 0; }
    check(
        "sgn3",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x05, // iload_0; ifge +5 → 6
            0x02, 0xac, // iconst_m1; ireturn
            0x1a, 0x9e, 0x00, 0x05, // iload_0; ifle +5 → 12
            0x04, 0xac, // iconst_1; ireturn
            0x03, 0xac, // iconst_0; ireturn
        ],
        1,
        1,
        &[
            (vec![-5], -1),
            (vec![0], 0),
            (vec![7], 1),
            (vec![i32::MIN as i64], -1),
            (vec![i32::MAX as i64], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_abs_early_return() {
    // int abs(int a) { if (a<0) return -a; return a; }   (computed early return)
    check(
        "abs",
        "(I)I",
        vec![
            0x1a, 0x9c, 0x00, 0x06, // iload_0; ifge +6 → 7
            0x1a, 0x74, 0xac, // iload_0; ineg; ireturn
            0x1a, 0xac, // iload_0; ireturn
        ],
        1,
        1,
        &[(vec![-5], 5), (vec![0], 0), (vec![7], 7), (vec![-100], 100)],
    );
}

// ── int truncation conversions (i2b / i2c / i2s) ───────────────────────
//
// Step 3 (IR gate relaxation): the IR builder now lowers i2b/i2c/i2s (it used to
// bail → single-pass). These decompose to existing shift/and ops, so the
// optimizing IR path handles byte/char/short-truncating int methods. The
// harness confirms IR == single-pass for the sign/zero-extension edge cases.

#[test]
fn ir_vs_singlepass_i2b() {
    // int f(int a) { return (byte)a; }   iload_0; i2b; ireturn
    check(
        "i2b",
        "(I)I",
        vec![0x1a, 0x91, 0xac],
        1,
        1,
        &[
            (vec![127], 127),
            (vec![128], -128),
            (vec![256], 0),
            (vec![-1], -1),
            (vec![300], 44),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2c() {
    // int f(int a) { return (char)a; }   iload_0; i2c; ireturn
    check(
        "i2c",
        "(I)I",
        vec![0x1a, 0x92, 0xac],
        1,
        1,
        &[
            (vec![65], 65),
            (vec![-1], 65535),
            (vec![65536], 0),
            (vec![-65536], 0),
            (vec![0xABCD], 0xABCD),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2s() {
    // int f(int a) { return (short)a; }   iload_0; i2s; ireturn
    check(
        "i2s",
        "(I)I",
        vec![0x1a, 0x93, 0xac],
        1,
        1,
        &[
            (vec![32767], 32767),
            (vec![32768], -32768),
            (vec![65536], 0),
            (vec![-1], -1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_i2b_chained() {
    // int g(int a) { return (byte)a + 1000; }   — the truncated value must feed
    // a following int op correctly (sipush 1000; iadd).
    check(
        "i2b_chained",
        "(I)I",
        vec![0x1a, 0x91, 0x11, 0x03, 0xe8, 0x60, 0xac],
        1,
        1,
        &[(vec![128], 872), (vec![127], 1127), (vec![-1], 999)],
    );
}

// ── switches (tableswitch / lookupswitch) ──────────────────────────────
//
// Step 3 second slice: the IR builder now lowers tableswitch/lookupswitch as a
// CMP-equality chain (the same shape single-pass emits), so int switch methods
// take the optimizing IR path. The harness confirms IR == single-pass for hits
// and the default.

#[test]
fn ir_vs_singlepass_tableswitch() {
    // int f(int x) { switch (x) { case 0: return 10; case 1: return 20;
    //                             case 2: return 30; default: return 99; } }
    // tableswitch at pc 1 → padding to pc 4; table low=0 high=2.
    check(
        "tswitch",
        "(I)I",
        vec![
            0x1a, // 0: iload_0
            0xaa, // 1: tableswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x24, // 4-7: default = +36 → 37
            0x00, 0x00, 0x00, 0x00, // 8-11: low = 0
            0x00, 0x00, 0x00, 0x02, // 12-15: high = 2
            0x00, 0x00, 0x00, 0x1b, // 16-19: case 0 = +27 → 28
            0x00, 0x00, 0x00, 0x1e, // 20-23: case 1 = +30 → 31
            0x00, 0x00, 0x00, 0x21, // 24-27: case 2 = +33 → 34
            0x10, 0x0a, 0xac, // 28-30: bipush 10; ireturn
            0x10, 0x14, 0xac, // 31-33: bipush 20; ireturn
            0x10, 0x1e, 0xac, // 34-36: bipush 30; ireturn
            0x10, 0x63, 0xac, // 37-39: bipush 99; ireturn (default)
        ],
        1,
        1,
        &[
            (vec![0], 10),
            (vec![1], 20),
            (vec![2], 30),
            (vec![3], 99),
            (vec![-1], 99),
            (vec![100], 99),
        ],
    );
}

#[test]
fn ir_vs_singlepass_lookupswitch() {
    // int g(int x) { switch (x) { case 10: return 1; case 20: return 2;
    //                             default: return 0; } }
    check(
        "lswitch",
        "(I)I",
        vec![
            0x1a, // 0: iload_0
            0xab, // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1f, // 4-7: default = +31 → 32
            0x00, 0x00, 0x00, 0x02, // 8-11: npairs = 2
            0x00, 0x00, 0x00, 0x0a, // 12-15: match 10
            0x00, 0x00, 0x00, 0x1b, // 16-19: offset +27 → 28
            0x00, 0x00, 0x00, 0x14, // 20-23: match 20
            0x00, 0x00, 0x00, 0x1d, // 24-27: offset +29 → 30
            0x04, 0xac, // 28-29: iconst_1; ireturn
            0x05, 0xac, // 30-31: iconst_2; ireturn
            0x03, 0xac, // 32-33: iconst_0; ireturn (default)
        ],
        1,
        1,
        &[
            (vec![10], 1),
            (vec![20], 2),
            (vec![0], 0),
            (vec![15], 0),
            (vec![-5], 0),
        ],
    );
}

#[test]
fn ir_vs_singlepass_sum_loop() {
    // int sum(int n) { int s=0; for (int i=0;i<n;i++) s+=i; return s; }
    //   locals: 0=n(param), 1=s, 2=i
    check(
        "sum",
        "(I)I",
        vec![
            0x03, 0x3c, // iconst_0; istore_1   (s)
            0x03, 0x3d, // iconst_0; istore_2   (i)
            0x1c, 0x1a, 0xa2, 0x00, 0x0d, // iload_2; iload_0; if_icmpge +13 → 19
            0x1b, 0x1c, 0x60, 0x3c, // iload_1; iload_2; iadd; istore_1
            0x84, 0x02, 0x01, // iinc 2, 1
            0xa7, 0xff, 0xf4, // goto -12 → 4
            0x1b, 0xac, // iload_1; ireturn
        ],
        3,
        1,
        &[
            (vec![0], 0),
            (vec![1], 0),
            (vec![5], 10),
            (vec![10], 45),
            (vec![100], 4950),
        ],
    );
}

// ── instance-field reads (getfield → Op::Load) ─────────────────────────
//
// Step 3 (field/call frontier, slice 1): the IR builder now lowers an
// int-category `getfield` into `Op::Load`, so a method whose only heap op is a
// field read takes the optimizing IR path. Unlike the pure-arithmetic corpus
// above, these execute a real memory access, so the harness builds a synthetic
// heap object laid out exactly as the VM lays one out — header + 16-byte
// `Value` cells — and passes its address as the receiver. Both backends read
// the same bytes, so IR == single-pass == host proves the inline-getfield ABI
// (null → 0, else MOVSXD the 32-bit `Value::Int` payload) is byte-faithful.
//
// Only int-category fields are exercised (the only shape the builder lowers);
// every `putfield` still bails to single-pass (no IR `Op::Store` yet).

/// Build a synthetic heap object with `fields.len()` int fields, each holding
/// the given value. Mirrors the VM object layout: a `HEADER_SIZE`-byte header
/// followed by one `SLOT_SIZE`-byte `Value` cell per field; an int value lives
/// at `FIELD_CELL_PAYLOAD32_OFFSET` within its cell with a zero (`Value::Int`)
/// discriminant tag (the all-zero buffer supplies the tag). The returned
/// buffer must outlive every call that reads it.
fn make_object(fields: &[i32]) -> Vec<u64> {
    // u64 backing so the buffer is 8-aligned: the guarded inline getfield's
    // receiver check (alignment bit-test + region bounds) requires real
    // object headers to be 8-aligned, and Vec<u8> guarantees nothing.
    let bytes = HEADER_SIZE + fields.len() * SLOT_SIZE;
    let mut buf = vec![0u64; bytes.div_ceil(8)];
    let base = buf.as_mut_ptr() as *mut u8;
    for (i, &v) in fields.iter().enumerate() {
        let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        // SAFETY: off + 4 <= bytes <= buf.len()*8 by construction.
        unsafe { std::ptr::write_unaligned(base.add(off) as *mut i32, v) };
    }
    buf
}

/// Compile via the per-call `optimize` toggle, supplying a constant-pool field
/// resolver (`cp_idx → (field_index, type_tag)`) both backends need to resolve
/// the field layout.
fn compile_opt_fields(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    optimize: bool,
) -> Option<CompiledMethod> {
    routing_not_policy();
    // No registered CompactLayout for this test's synthetic buffers -- None
    // (not a fabricated (0, false)) is the correct "no compact slot" value;
    // see cp_field_resolver's Option<(u32, bool)> contract in jit/src/lib.rs.
    let field_resolver_with_compact =
        |cp_idx| field_resolver(cp_idx).map(|(field_index, tag)| (field_index, tag, None));
    try_compile(
        cm,
        None,
        Some(&field_resolver_with_compact),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        false,
        false,
        false, // ir_emit_fp
        None,
    )
}

/// Compile a single-object-parameter `(L…;)I` method both ways, then for each
/// `(field_values, expected)` case build an object from `field_values`, call
/// both bodies with its address, and assert IR == single-pass == `expected`.
fn check_field(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    cases: &[(Vec<i32>, i32)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, 1);
    let ir = compile_opt_fields(&cm, &helpers, field_resolver, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt_fields(&cm, &helpers, field_resolver, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    // getfield methods are `needs_heap` (the checked-helper fallback wants the
    // hidden vm_ptr), so the artifact ABI has the context in ARG0 and the
    // receiver in ARG1 — calling `try_call` without a context would feed the
    // receiver INTO the context slot and a garbage register into local 0. The
    // int getfield's guarded-inline fast path never dereferences the context,
    // so a zeroed dummy buffer is a safe placeholder (a receiver that fails
    // the inline guard reaches the panicking stub helper, which is exactly
    // the signal these tests want).
    let dummy_vm = [0u8; 64];
    for (fields, expected) in cases {
        let obj = make_object(fields);
        let args = [obj.as_ptr() as i64];
        // SAFETY: both bodies were JIT-compiled from valid getfield bytecode;
        // `obj` is a live, 8-aligned, correctly-laid-out object whose address
        // is the sole (reference) Java argument, and it outlives both calls
        // (dropped at the end of this iteration). The guarded inline getfield
        // raw-loads it (inside the wide-open TEST_REGION_BOUNDS); no runtime
        // helper is reachable for a valid receiver.
        let call = |m: &CompiledMethod| unsafe {
            if m.needs_context() {
                m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
            } else {
                m.try_call(&args)
            }
        };
        let r_sp =
            call(&sp).unwrap_or_else(|e| panic!("{name}: single-pass call {fields:?}: {e:?}"));
        let r_ir = call(&ir).unwrap_or_else(|e| panic!("{name}: IR call {fields:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "{name}: IR vs single-pass DIVERGE for {fields:?}: IR={}, single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: both backends agree but disagree with host for {fields:?}: got {}, expected {expected}",
            r_ir as i32,
        );
        drop(obj); // keep the object alive until after both calls
    }
}

#[test]
fn ir_vs_singlepass_getfield_simple() {
    // static int get(Corpus o) { return o.x; }   (field 0 = int)
    //   aload_0; getfield #2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_simple",
        "(Lpkg/Corpus;)I",
        vec![0x2a, 0xb4, 0x00, 0x02, 0xac],
        1,
        &resolver,
        &[
            (vec![5], 5),
            (vec![-7], -7),
            (vec![0], 0),
            (vec![i32::MIN], i32::MIN),
            (vec![i32::MAX], i32::MAX),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_via_astore_local() {
    // static int get(Corpus o) { Corpus p = o; return p.x; }
    // Round-trips the receiver ref through a LOCAL via `astore`/`aload` — the
    // shape real javac emits for an object local. The IR builder had no `astore`
    // handler, so any method storing a ref to a local bailed to single-pass;
    // this proves the now-lowered `astore`/`aload` round-trip executes
    // identically (IR == single-pass == host) on the int-field path.
    //   aload_0; astore_1; aload_1; getfield #2; ireturn
    //   2a 4c 2b b4 00 02 ac
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_via_astore",
        "(Lpkg/Corpus;)I",
        vec![0x2a, 0x4c, 0x2b, 0xb4, 0x00, 0x02, 0xac],
        2, // local 0 = receiver param, local 1 = p
        &resolver,
        &[
            (vec![5], 5),
            (vec![-7], -7),
            (vec![0], 0),
            (vec![i32::MIN], i32::MIN),
            (vec![i32::MAX], i32::MAX),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_two_fields_sum() {
    // static int sum(Corpus o) { return o.x + o.y; }   (fields 0,1 = int)
    //   aload_0; getfield #2; aload_0; getfield #3; iadd; ireturn
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')),
        3 => Some((1, b'I')),
        _ => None,
    };
    check_field(
        "getfield_sum",
        "(Lpkg/Corpus;)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, // aload_0; getfield #2 (x)
            0x2a, 0xb4, 0x00, 0x03, // aload_0; getfield #3 (y)
            0x60, 0xac, // iadd; ireturn
        ],
        1,
        &resolver,
        &[
            (vec![3, 4], 7),
            (vec![10, -3], 7),
            (vec![-5, -6], -11),
            (vec![i32::MAX, 1], i32::MIN), // wraps
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_branch() {
    // static int sign(Corpus o) { int v = o.x; if (v < 0) return -1; return 1; }
    // A field read feeding a conditional branch + two return points — proves the
    // Load is pinned to the right control path and the value flows into a φ-free
    // multi-return shape.
    //   0: aload_0           2a
    //   1: getfield #2       b4 00 02
    //   4: istore_1          3c
    //   5: iload_1           1b
    //   6: iflt +5 → 11      9b 00 05
    //   9: iconst_1; ireturn 04 ac
    //  11: iconst_m1;ireturn 02 ac
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field(
        "getfield_branch",
        "(Lpkg/Corpus;)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, 0x3c, 0x1b, 0x9b, 0x00, 0x05, 0x04, 0xac, 0x02, 0xac,
        ],
        2,
        &resolver,
        &[
            (vec![5], 1),
            (vec![-5], -1),
            (vec![0], 1),
            (vec![i32::MIN], -1),
            (vec![i32::MAX], 1),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_loop() {
    // static int f(Corpus o) { int s=0; for (int i=0;i<o.n;i++) s += o.x; return s; }
    // getfield in BOTH the loop condition (o.n) and body (o.x), re-read each
    // iteration (LICM is default-off), under a loop with carried phis + a memory
    // phi — proves Op::Load schedules correctly inside a loop body.
    //   locals: 0=o, 1=s, 2=i ; fields: 0=x, 1=n
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')), // x
        3 => Some((1, b'I')), // n
        _ => None,
    };
    check_field(
        "getfield_loop",
        "(Lpkg/Corpus;)I",
        vec![
            0x03, 0x3c, // 0: iconst_0; istore_1   (s = 0)
            0x03, 0x3d, // 2: iconst_0; istore_2   (i = 0)
            0x1c, 0x2a, 0xb4, 0x00, 0x03, // 4: iload_2; aload_0; getfield #3 (n)
            0xa2, 0x00, 0x10, // 9: if_icmpge +16 → 25
            0x1b, 0x2a, 0xb4, 0x00, 0x02, // 12: iload_1; aload_0; getfield #2 (x)
            0x60, 0x3c, // 17: iadd; istore_1
            0x84, 0x02, 0x01, // 19: iinc 2, 1
            0xa7, 0xff, 0xee, // 22: goto -18 → 4
            0x1b, 0xac, // 25: iload_1; ireturn
        ],
        3,
        &resolver,
        &[
            (vec![5, 3], 15),
            (vec![5, 0], 0),
            (vec![7, 4], 28),
            (vec![-2, 3], -6),
            (vec![3, 10], 30),
        ],
    );
}

// ── instance-field writes (putfield → Op::Store) ───────────────────────
//
// Step 3 slice 2: the IR builder lowers an int-category `putfield` to
// `Op::Store`, the first store the production IR path emits. Soundness rests on
// the memory token chain: every memory op consumes the prior token and produces
// a new one, so the scheduler's input-edge topological sort serialises them
// (RAW/WAR/WAW all preserved). The lowerer inlines the `jit_putfield_int` heap
// write; the harness gives single-pass (which lowers to `CALL jit_putfield_int`)
// a faithful stub, so both backends produce identical cells. Each case runs each
// backend against its OWN fresh object and compares the return value AND the
// post-call object state (so a dropped store is caught).

/// `dummy_helpers` with a real `putfield_int` that writes a `Value::Int` cell
/// into the synthetic object — single-pass lowers an int `putfield` to a
/// `CALL jit_putfield_int`, so it needs a live helper. Matches the inline IR
/// store byte-for-byte (discriminant 0 + 32-bit payload, high qword cleared).
fn field_helpers() -> JitRuntimeHelpers {
    unsafe extern "C" fn putfield_int(obj: i64, field_index: i64, val: i64) {
        if obj == 0 {
            return;
        }
        let p = obj as *mut u8;
        let off = HEADER_SIZE + field_index as usize * SLOT_SIZE;
        // The synthetic object is a byte-aligned `Vec<u8>`, so use unaligned
        // writes (the JIT's own `MOV` stores are unaligned-safe on x86; a real
        // heap object is aligned, where `jit_putfield_int`'s aligned write is
        // fine). The resulting cell bytes are identical either way.
        std::ptr::write_unaligned(p.add(off) as *mut u32, 0); // Value::Int discriminant
        std::ptr::write_unaligned(
            p.add(off + FIELD_CELL_PAYLOAD32_OFFSET) as *mut i32,
            val as i32,
        );
        std::ptr::write_unaligned(p.add(off + 8) as *mut u64, 0); // high qword
    }
    let mut h = dummy_helpers();
    h.putfield_int = putfield_int as *const () as usize;
    h
}

/// Read the int payload of field `i` from a synthetic object buffer.
fn read_field(buf: &[u64], i: usize) -> i32 {
    let off = HEADER_SIZE + i * SLOT_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
    let base = buf.as_ptr() as *const u8;
    // SAFETY: off + 4 is within the buffer by make_object's construction.
    unsafe { std::ptr::read_unaligned(base.add(off) as *const i32) }
}

/// Compile a `(L…; <int args>)I` method both ways; for each case run each
/// backend against its OWN fresh object built from `init`, with `extra` int
/// args after the receiver, then assert the return value AND the post-call
/// field state agree across backends and with the host expectation.
fn check_field_rw(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    field_resolver: &dyn Fn(u16) -> Option<(usize, u8)>,
    n_fields: usize,
    cases: &[(Vec<i32>, Vec<i64>, i32, Vec<i32>)],
) {
    let helpers = field_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt_fields(&cm, &helpers, field_resolver, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    let sp = compile_opt_fields(&cm, &helpers, field_resolver, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (init, extra, expected, after) in cases {
        // A `putfield` method is `needs_heap` in the single-pass backend, so its
        // body takes a hidden VM-context pointer as the first argument
        // (`try_call_with_context`) — the receiver and value follow. The inline
        // IR store needs no context (`needs_context() == false`) and is called
        // via `try_call`. An int putfield never dereferences the context, so a
        // zeroed dummy buffer is a safe placeholder. Each backend mutates its
        // own fresh object.
        let dummy_vm = [0u8; 64];
        let run = |m: &CompiledMethod| -> (i32, Vec<i32>) {
            let mut obj = make_object(init);
            let mut args = vec![obj.as_mut_ptr() as i64];
            args.extend_from_slice(extra);
            // SAFETY: `m` is JIT-compiled from valid field bytecode; `obj` is a
            // live, exclusively-owned, correctly-laid-out object whose address
            // is the receiver arg; the int `extra` args follow. The only helper
            // reachable is the live `putfield_int` stub. The context pointer (if
            // taken) is a live 64-byte buffer the int path never reads.
            let r = unsafe {
                if m.needs_context() {
                    m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
                } else {
                    m.try_call(&args)
                }
            }
            .unwrap_or_else(|e| panic!("{name}: call {init:?},{extra:?}: {e:?}"));
            let fields: Vec<i32> = (0..n_fields).map(|i| read_field(&obj, i)).collect();
            (r as i32, fields)
        };
        let (r_sp, after_sp) = run(&sp);
        let (r_ir, after_ir) = run(&ir);
        assert_eq!(
            r_ir, r_sp,
            "{name}: return IR vs single-pass DIVERGE for {init:?},{extra:?}: IR={r_ir}, sp={r_sp}",
        );
        assert_eq!(
            after_ir, after_sp,
            "{name}: object state IR vs single-pass DIVERGE for {init:?},{extra:?}: IR={after_ir:?}, sp={after_sp:?}",
        );
        assert_eq!(
            r_ir, *expected,
            "{name}: both backends agree but return disagrees with host for {init:?},{extra:?}: got {r_ir}, expected {expected}",
        );
        assert_eq!(
            &after_ir, after,
            "{name}: both backends agree but object state disagrees with host for {init:?},{extra:?}: got {after_ir:?}, expected {after:?}",
        );
    }
}

#[test]
fn ir_vs_singlepass_putfield_then_getfield() {
    // static int setget(Corpus o, int v) { o.x = v; return o.x; }
    // Store-then-load (RAW): the load MUST observe the store. A mis-ordered
    // load returns the initial field value instead.
    //   aload_0; iload_1; putfield #2; aload_0; getfield #2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "setget",
        "(Lpkg/Corpus;I)I",
        vec![
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // aload_0; iload_1; putfield #2
            0x2a, 0xb4, 0x00, 0x02, 0xac, // aload_0; getfield #2; ireturn
        ],
        2,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 5, vec![5]),
            (vec![0], vec![-7], -7, vec![-7]),
            (vec![3], vec![0], 0, vec![0]),
            (vec![7], vec![i32::MIN as i64], i32::MIN, vec![i32::MIN]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_getfield_then_putfield() {
    // static int getset(Corpus o, int v) { int t = o.x; o.x = v; return t; }
    // Load-then-store (WAR): the returned old value MUST be read BEFORE the
    // store overwrites the field. A mis-ordered store returns the new value.
    //   aload_0; getfield #2; istore_2; aload_0; iload_1; putfield #2; iload_2; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "getset",
        "(Lpkg/Corpus;I)I",
        vec![
            0x2a, 0xb4, 0x00, 0x02, 0x3d, // aload_0; getfield #2; istore_2
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // aload_0; iload_1; putfield #2
            0x1c, 0xac, // iload_2; ireturn
        ],
        3,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 99, vec![5]),
            (vec![42], vec![-1], 42, vec![-1]),
            (vec![0], vec![7], 0, vec![7]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_putfield_pure_write() {
    // static int set(Corpus o, int v) { o.x = v; return v; }
    // The store's memory result is UNUSED (the method returns v, not a read of
    // the field), so DCE must still keep it — the post-call field state proves
    // the write happened. A dropped store leaves the field at its initial value.
    //   aload_0; iload_1; putfield #2; iload_1; ireturn
    let resolver = |cp: u16| if cp == 2 { Some((0, b'I')) } else { None };
    check_field_rw(
        "set",
        "(Lpkg/Corpus;I)I",
        vec![0x2a, 0x1b, 0xb5, 0x00, 0x02, 0x1b, 0xac],
        2,
        2,
        &resolver,
        1,
        &[
            (vec![99], vec![5], 5, vec![5]),
            (vec![0], vec![-9], -9, vec![-9]),
            (vec![123], vec![123], 123, vec![123]),
        ],
    );
}

#[test]
fn ir_vs_singlepass_putfield_two_fields() {
    // static int set2(Corpus o, int a, int b) { o.x=a; o.y=b; return o.x+o.y; }
    // Two stores then two loads — WAW between the stores, RAW from each load.
    //   aload_0; iload_1; putfield #2; aload_0; iload_2; putfield #3;
    //   aload_0; getfield #2; aload_0; getfield #3; iadd; ireturn
    let resolver = |cp: u16| match cp {
        2 => Some((0, b'I')),
        3 => Some((1, b'I')),
        _ => None,
    };
    check_field_rw(
        "set2",
        "(Lpkg/Corpus;II)I",
        vec![
            0x2a, 0x1b, 0xb5, 0x00, 0x02, // o.x = a
            0x2a, 0x1c, 0xb5, 0x00, 0x03, // o.y = b
            0x2a, 0xb4, 0x00, 0x02, // o.x
            0x2a, 0xb4, 0x00, 0x03, // o.y
            0x60, 0xac, // iadd; ireturn
        ],
        3,
        3,
        &resolver,
        2,
        &[
            (vec![0, 0], vec![3, 4], 7, vec![3, 4]),
            (vec![1, 2], vec![10, 20], 30, vec![10, 20]),
            (vec![5, 5], vec![-3, 3], 0, vec![-3, 3]),
        ],
    );
}

// ── COV-03: REFERENCE putfield ─────────────────────────────────────────
//
// `getfield` learned about reference fields; `putfield` twenty lines below it
// did not. This section is the differential half of closing that: a method that
// stores a reference field, reads it back, and returns it must give the same
// answer from both backends.
//
// The layout both backends must agree on is the legacy 16-byte `Value` cell —
// `Value::Object`'s tag dword (4) at `FIELD_CELL_TAG_OFFSET`, the raw pointer at
// `FIELD_CELL_PAYLOAD64_OFFSET` — because that is what the single-pass backend's
// inline reference `getfield` reads (`MOV RAX, [RAX + cell + 8]`) for a
// non-compact receiver, which is what `make_object` builds. The stubs below
// implement exactly that, so single-pass's INLINE read and the IR tier's HELPER
// read are being compared against the same bytes rather than against each
// other's private conventions.
//
// The reference STORE goes through `jit_putfield_object` on both sides — that is
// the point: it is the only lowering either backend has for a reference field
// write, because it is the one that carries the SATB pre-barrier and the
// collector's post-write barrier.

/// `jit_getfield` for a REFERENCE field in the synthetic legacy-layout objects:
/// the raw pointer at `FIELD_CELL_PAYLOAD64_OFFSET`. Null receiver → 0, matching
/// both the real helper's null path and the inline lowering's.
///
/// # Safety
/// Same contract as [`legacy_getfield`].
unsafe extern "C" fn legacy_getfield_ref(_vm: i64, obj: i64, idx: i64) -> i64 {
    if obj == 0 {
        return 0;
    }
    let Some(idx) = stub_field_slot("legacy_getfield_ref", idx) else {
        return 0;
    };
    let at = (obj as *const u8).add(HEADER_SIZE + idx * SLOT_SIZE + 8);
    std::ptr::read_unaligned(at as *const i64)
}

/// `jit_putfield_object` for the same buffers. Writes the `Value::Object` tag
/// (4) and the raw pointer, which is the cell shape the single-pass inline
/// reference read decodes. No barrier is modelled — there is no collector here;
/// what this harness proves is that both backends route the store through THIS
/// helper and agree on the bytes. That the helper carries the barriers is
/// asserted separately, on the emitted artifact, in `ir_lower`'s
/// `a_reference_putfield_lowers_only_through_the_barrier_helper`.
///
/// # Safety
/// Same contract as [`legacy_getfield`]; `val` is 0 or a live object address.
unsafe extern "C" fn legacy_putfield_object(_vm: i64, obj: i64, idx: i64, val: i64) {
    if obj == 0 {
        return;
    }
    // No decode here: `jit_putfield_object`'s index argument carries no flag
    // bits (there is no `putfield_index_arg`), so this is a plain slot. The
    // bound is still checked — see `stub_field_slot`.
    let Some(idx) = stub_field_slot("legacy_putfield_object", idx) else {
        return;
    };
    let base = (obj as *mut u8).add(HEADER_SIZE + idx * SLOT_SIZE);
    std::ptr::write_unaligned(base as *mut u32, 4); // Value::Object tag
    std::ptr::write_unaligned(base.add(8) as *mut i64, val);
}

fn ref_field_helpers() -> JitRuntimeHelpers {
    let mut h = dummy_helpers();
    h.getfield = legacy_getfield_ref as *const () as usize;
    h.putfield_object = legacy_putfield_object as *const () as usize;
    h
}

/// Read the 64-bit reference payload of field `i` from a synthetic object.
fn read_ref_field(buf: &[u64], i: usize) -> i64 {
    let off = HEADER_SIZE + i * SLOT_SIZE + 8;
    let base = buf.as_ptr() as *const u8;
    // SAFETY: off + 8 is within the buffer by make_object's construction.
    unsafe { std::ptr::read_unaligned(base.add(off) as *const i64) }
}

/// A helper stub decodes `jit_getfield`'s slot argument the way the real helper
/// does — flags stripped — and refuses anything that is not a slot instead of
/// indexing with it.
///
/// This is the unit-level guard on the defect that aborted this whole test
/// binary: `getfield_index_arg` sets `GETFIELD_EXPECT_REFERENCE` on the
/// reference path, the stub multiplied the raw argument by `SLOT_SIZE`, and
/// `0x2000_0000_0000_0000 * 16` overflows. Every int test in the file passed
/// while the one reference read killed the process, because the flag is only
/// set for references.
#[test]
fn a_stub_decodes_the_getfield_slot_argument_like_the_real_helper() {
    // Every flag combination the encoder can produce must decode back to the
    // slot. `receiver_proven_oop` is only encoded on the reference path, which
    // is why the (false, true) row still decodes to a bare slot.
    for slot in [0u32, 1, 7, 63] {
        for is_ref in [false, true] {
            for proven in [false, true] {
                // Every key too: the NPE trap-site key is the widest passenger
                // in this argument, so a stub that strips only the two flags
                // indexes the object with a 24-bit number.
                for key in [0u32, 1, 0x00ff_ffff] {
                    let arg =
                        cratonvm_jit_api::getfield_index_arg(slot, is_ref, proven, key) as i64;
                    assert_eq!(
                        decode_field_slot(arg),
                        Some(slot as usize),
                        "slot {slot} (is_ref={is_ref}, proven={proven}, key={key}) must survive                          the round trip",
                    );
                }
            }
        }
    }

    // The exact argument that used to abort the process: slot 0 with
    // `GETFIELD_EXPECT_REFERENCE`. Decoding it is the difference between a
    // read of field 0 and a multiply overflow.
    let flagged = cratonvm_jit_api::getfield_index_arg(0, true, false, 0) as i64;
    assert_eq!(flagged as u64, cratonvm_jit_api::GETFIELD_EXPECT_REFERENCE);
    assert_eq!(decode_field_slot(flagged), Some(0));

    // And an argument that is not a slot at all is REFUSED rather than used.
    // Before this, the stub indexed with it — inside an `extern "C"` fn, where
    // the resulting panic cannot unwind and takes the process down.
    assert_eq!(decode_field_slot(-1), None);
    assert_eq!(decode_field_slot(STUB_MAX_FIELD_SLOT), None);
    assert_eq!(decode_field_slot(i64::MAX), None);
}

/// The COV-03 differential: `static Object setget(Corpus o, Object v) { o.r = v;
/// return o.r; }`. A reference store followed by a reference read of the same
/// field — the exact shape the lane's brief names — run through both backends
/// against their own fresh objects, comparing the returned reference AND the
/// resulting field bytes.
#[test]
fn ir_vs_singlepass_reference_putfield_then_getfield() {
    // aload_0; aload_1; putfield #2; aload_0; getfield #2; areturn
    let code = vec![
        0x2a, 0x2b, 0xb5, 0x00, 0x02, // o.r = v
        0x2a, 0xb4, 0x00, 0x02, 0xb0, // return o.r
    ];
    let resolver = |cp: u16| if cp == 2 { Some((0usize, b'L')) } else { None };
    let helpers = ref_field_helpers();
    let cm = cached(
        "ref_setget",
        "(Lpkg/Corpus;Ljava/lang/Object;)Ljava/lang/Object;",
        code,
        2,
        2,
    );
    let ir = compile_opt_fields(&cm, &helpers, &resolver, true)
        .expect("ref_setget: optimize=true (IR pipeline) failed to compile");
    let sp = compile_opt_fields(&cm, &helpers, &resolver, false)
        .expect("ref_setget: optimize=false (single-pass) failed to compile");

    // The values stored: a live object address, and null.
    let target = make_object(&[7]);
    let dummy_vm = [0u8; 64];
    for value in [target.as_ptr() as i64, 0i64] {
        let run = |m: &CompiledMethod| -> (i64, i64) {
            let mut obj = make_object(&[0]);
            let args = [obj.as_mut_ptr() as i64, value];
            // SAFETY: `m` is JIT-compiled from valid field bytecode; `obj` is a
            // live, exclusively-owned, correctly-laid-out receiver and `value`
            // is 0 or the address of the live `target` buffer. The only helpers
            // reachable are the two live stubs above; the context pointer is a
            // live 64-byte buffer neither stub reads.
            let r = unsafe {
                if m.needs_context() {
                    m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
                } else {
                    m.try_call(&args)
                }
            }
            .unwrap_or_else(|e| panic!("ref_setget: call value={value:#x}: {e:?}"));
            (r, read_ref_field(&obj, 0))
        };
        let (r_sp, cell_sp) = run(&sp);
        let (r_ir, cell_ir) = run(&ir);
        // Before the divergence assertions: a stub that could not decode its
        // argument answers 0, which would otherwise be reported as "the IR tier
        // returned null" — the wrong defect.
        assert_no_stub_faults();
        assert_eq!(
            r_ir, r_sp,
            "ref_setget: returned reference DIVERGES for value={value:#x}: IR={r_ir:#x}, sp={r_sp:#x}",
        );
        assert_eq!(
            cell_ir, cell_sp,
            "ref_setget: stored cell DIVERGES for value={value:#x}: IR={cell_ir:#x}, sp={cell_sp:#x}",
        );
        assert_eq!(
            r_ir, value,
            "ref_setget: both backends agree but disagree with the host for \
             value={value:#x}: got {r_ir:#x}",
        );
        assert_eq!(
            cell_ir, value,
            "ref_setget: the store did not land for value={value:#x}: cell={cell_ir:#x}",
        );
    }
    drop(target);
}

/// The write-only case: the store's memory result is never read back inside the
/// method, so nothing but the post-call object state proves it happened. This is
/// what a dropped reference store looks like — and a dropped reference store is
/// also what a missing write barrier looks like to the collector, one
/// collection later.
#[test]
fn ir_vs_singlepass_reference_putfield_pure_write() {
    // aload_0; aload_1; putfield #2; return
    let code = vec![0x2a, 0x2b, 0xb5, 0x00, 0x02, 0xb1];
    let resolver = |cp: u16| if cp == 2 { Some((0usize, b'L')) } else { None };
    let helpers = ref_field_helpers();
    let cm = cached("ref_set", "(Lpkg/Corpus;Ljava/lang/Object;)V", code, 2, 2);
    let ir = compile_opt_fields(&cm, &helpers, &resolver, true)
        .expect("ref_set: optimize=true (IR pipeline) failed to compile");
    let sp = compile_opt_fields(&cm, &helpers, &resolver, false)
        .expect("ref_set: optimize=false (single-pass) failed to compile");
    let target = make_object(&[1]);
    let dummy_vm = [0u8; 64];
    for value in [target.as_ptr() as i64, 0i64] {
        let run = |m: &CompiledMethod| -> i64 {
            let mut obj = make_object(&[0]);
            let args = [obj.as_mut_ptr() as i64, value];
            // SAFETY: as in `ir_vs_singlepass_reference_putfield_then_getfield`.
            unsafe {
                if m.needs_context() {
                    m.try_call_with_context(dummy_vm.as_ptr() as i64, &args)
                } else {
                    m.try_call(&args)
                }
            }
            .unwrap_or_else(|e| panic!("ref_set: call value={value:#x}: {e:?}"));
            read_ref_field(&obj, 0)
        };
        let cell_sp = run(&sp);
        let cell_ir = run(&ir);
        assert_eq!(
            cell_ir, cell_sp,
            "ref_set: stored cell DIVERGES for value={value:#x}: IR={cell_ir:#x}, sp={cell_sp:#x}",
        );
        assert_eq!(
            cell_ir, value,
            "ref_set: the store was dropped for value={value:#x}: cell={cell_ir:#x}",
        );
    }
    drop(target);
}

// ── Gap B: invokestatic → Op::Call via the invoke_dispatch helper ──────
//
// The IR builder lowers an int-only `invokestatic` in an oop-free method to
// `Op::Call`, which the lowerer dispatches as
//   i64 helper(vm_ptr, info_ptr, args_ptr, num_args)
// (the same ABI as single-pass), marshalling the Java args contiguously into a
// frame staging region. These tests supply a REAL stub `invoke_dispatch` so the
// generated code actually runs — validating the marshalling (values, order,
// count), the `needs_context` prologue, and the `i64::MIN` exception sentinel —
// without the full VM (the handoff's "real dispatch stub for a direct static
// call" option).

/// Compile a method through the IR pipeline WITH `ir_emit_calls` on, supplying an
/// invoke resolver and helpers (whose `invoke_dispatch` the caller has wired to a
/// stub). Returns the compiled body (needs_context, since it contains a call).
fn compile_with_dispatch(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true,  // optimize (C2 / IR pipeline)
        true,  // ir_emit_calls (Gap B, invokestatic)
        true,  // ir_emit_special_calls (inc 24, invokespecial — inert without 0xb7)
        true,  // ir_emit_long (inc 28 — long call args; inert for non-long callers)
        true,  // ir_emit_virtual_calls (inc 26, invokevirtual/interface)
        false, // ir_emit_fp (inc 30 — inert for these int/long callers)
        None,
    )
}

/// Like [`compile_with_dispatch`] but with BOTH the call gate AND the FP gate on
/// (inc 32) — for a caller that consumes a `double`-returning `Op::Call` result.
fn compile_with_dispatch_fp(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true, // optimize
        true, // ir_emit_calls
        true, // ir_emit_special_calls
        true, // ir_emit_long
        true, // ir_emit_virtual_calls
        true, // ir_emit_fp (inc 32 — D call returns)
        None,
    )
}

/// Stub `invoke_dispatch` that reads `num_args` i64 args from `args_ptr` and
/// returns an order- AND count-sensitive function of them, so a marshalling bug
/// (wrong order, wrong base, wrong count) produces a divergent result:
///   result = Σ arg[i] * 100^(n-1-i)   +   n * 1_000_000
unsafe extern "C" fn positional_dispatch(
    _vm: i64,
    _info: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let p = args_ptr as *const i64;
    let n = num_args as usize;
    let mut acc: i64 = 0;
    for i in 0..n {
        acc = acc * 100 + (*p.add(i) as i32 as i64);
    }
    acc + (n as i64) * 1_000_000
}

fn host_positional(args: &[i64]) -> i64 {
    let mut acc: i64 = 0;
    for &a in args {
        acc = acc * 100 + (a as i32 as i64);
    }
    acc + (args.len() as i64) * 1_000_000
}

#[test]
fn ir_vs_singlepass_invokestatic_two_int_args() {
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return g(a, b); }
    //   iload_0; iload_1; invokestatic #2; ireturn
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of int invokestatic method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context (VM ptr as hidden arg 0)"
    );
    let dummy_vm = [0u8; 64];
    for (a, b) in [
        (3i64, 4i64),
        (0, 0),
        (7, 9),
        (-1, 2),
        (100, 200),
        (i32::MAX as i64, 1),
    ] {
        // SAFETY: `ir` is a finalized IR-compiled body taking (vm_ptr, a, b); the
        // dummy VM buffer is never dereferenced by the stub dispatch helper.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call ({a},{b}): {e:?}"));
        assert_eq!(
            r,
            host_positional(&[a, b]),
            "invokestatic dispatch result for ({a}, {b})"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_three_args_and_arith() {
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b, int c) { return g(a, b, c) + 1; }
    //   iload_0; iload_1; iload_2; invokestatic #2; iconst_1; iadd; ireturn
    let code = vec![0x1a, 0x1b, 0x1c, 0xb8, 0x00, 0x02, 0x04, 0x60, 0xac];
    let cm = cached("f", "(III)I", code, 3, 3);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(III)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("IR compile of 3-arg method");
    let dummy_vm = [0u8; 64];
    for (a, b, c) in [(1i64, 2i64, 3i64), (9, 8, 7), (0, 0, 0), (-1, -2, -3)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b, c]) }
            .unwrap_or_else(|e| panic!("call ({a},{b},{c}): {e:?}"));
        // host: g(a,b,c) result, then + 1 (the trailing iconst_1; iadd).
        let expected = (host_positional(&[a, b, c]) as i32).wrapping_add(1) as i64;
        assert_eq!(r, expected, "3-arg dispatch + arith for ({a},{b},{c})");
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_exception_sentinel() {
    // A callee that throws makes `invoke_dispatch` return the `i64::MIN` sentinel;
    // the JIT method must detect it and bail (returning the sentinel unchanged so
    // the VM takes the pending exception) rather than use it as a result.
    unsafe extern "C" fn throwing_dispatch(_vm: i64, _info: i64, _args: i64, _n: i64) -> i64 {
        i64::MIN
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = throwing_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing callee (i64::MIN) must bail and return the sentinel"
    );
}

#[test]
fn ir_vs_singlepass_invokestatic_reference_arg() {
    // inc 22: a REFERENCE argument is marshalled as its raw pointer. The method
    // takes an object + an int and passes both to a static call; the stub reads
    // field 0 off the object pointer and returns `o.x + n`, proving the pointer
    // is marshalled intact (not truncated/swapped) and the int arg follows it.
    //   static int f(Corpus o, int n) { return g(o, n); }
    //   aload_0; iload_1; invokestatic #2; ireturn
    unsafe extern "C" fn ref_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "ref_dispatch expects (object, int)");
        let p = args_ptr as *const i64;
        let obj = *p; // arg0 = the object pointer (a reference)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((obj as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = ref_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(Lpkg/Corpus;I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of reference-arg invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is arg0 (a reference), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "reference-arg marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_long_arg() {
    // inc 28: a LONG argument is marshalled as ONE i64 slot (full 64-bit). The
    // method passes a long + an int to a static call returning int; the stub
    // reads BOTH halves of the long arg0 (so a truncated marshalling would
    // diverge) plus the int arg1.
    //   static int f(long a, int n) { return g(a, n); }   // g:(JI)I
    //   lload_0; iload_2; invokestatic #2; ireturn
    unsafe extern "C" fn longarg_dispatch(
        _vm: i64,
        _info: i64,
        args_ptr: i64,
        num_args: i64,
    ) -> i64 {
        assert_eq!(num_args, 2, "longarg_dispatch expects (long, int)");
        let p = args_ptr as *const i64;
        let a = *p; // arg0 = the FULL 64-bit long (one slot)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let hi = (a >> 32) as i32 as i64; // high 32 bits — 0 if truncated
        let lo = a as i32 as i64; // low 32 bits
        hi.wrapping_add(lo).wrapping_add(n) as i32 as i64
    }
    fn host(a: i64, n: i64) -> i32 {
        let hi = (a >> 32) as i32;
        let lo = a as i32;
        hi.wrapping_add(lo).wrapping_add(n as i32)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = longarg_dispatch as *const () as usize;
    let code = vec![0x1e, 0x1c, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(JI)I", code, 3, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JI)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of long-arg invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, n) in [
        (3i64, 7i64),
        (0x1234_5678_9abc_def0u64 as i64, 11), // high bits non-zero
        (-1, 2),
        (i64::MIN, 1),
        (0, 0),
    ] {
        // SAFETY: finalized Op::Call body taking (vm, long a, int n); the dummy
        // VM buffer is never dereferenced by the stub.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, n]) }
            .unwrap_or_else(|e| panic!("call a={a},n={n}: {e:?}"));
        assert_eq!(
            r as i32,
            host(a, n),
            "long-arg marshalling (full 64-bit) for a={a}, n={n}"
        );
    }
}

// ── J (long) call RETURNS — the i64::MIN-sentinel disambiguation (post-inc-29) ──
//
// `static_call_shape` now accepts a `J` return, so an `Op::Call` may produce a
// long result in RAX. The dispatch helper signals a callee exception/deopt by
// returning the `i64::MIN` sentinel — which is bit-identical to a legitimate
// `Long.MIN_VALUE` return. These tests pin the call-site disambiguation: on the
// `RAX == i64::MIN` branch the JIT consults the out-of-band `dispatch_threw`
// peek, bailing ONLY when a genuine exception/deopt is pending and otherwise
// keeping the real value. The methods do a trailing `+ 1` after the call so the
// "kept the value (then +1)" and "bailed (returned the sentinel unchanged)"
// outcomes are observably different.

#[test]
fn ir_vs_singlepass_invokestatic_long_return() {
    // static long f(long a, long b) { return g(a, b) + 1; }   // g:(JJ)J
    //   lload_0; lload_2; invokestatic #2; lconst_1; ladd; lreturn
    // Common path (RAX != i64::MIN): a variety of long results — including
    // negative and full-64-bit values — must round-trip through the `JNE .keep`
    // fast path untouched. `dispatch_threw` must NEVER be consulted here.
    unsafe extern "C" fn sum_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "sum_dispatch expects (long, long)");
        let p = args_ptr as *const i64;
        (*p).wrapping_add(*p.add(1)) // full 64-bit long sum
    }
    unsafe extern "C" fn poison_threw() -> i64 {
        panic!("dispatch_threw must not run when the call result is not i64::MIN");
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = sum_dispatch as *const () as usize;
    helpers.dispatch_threw = poison_threw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of long-returning invokestatic method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, b) in [
        (3i64, 4i64),
        (-5, 2),
        (0x0123_4567_89ab_cdefu64 as i64, 0x10),
        (i64::MAX, 0), // sum = MAX (not the sentinel)
        (-1, -1),      // sum = -2 (high bit set, not the sentinel)
    ] {
        // SAFETY: finalized Op::Call body taking (vm, long a, long b); the dummy
        // VM buffer is never dereferenced by the stub.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call a={a},b={b}: {e:?}"));
        assert_eq!(
            r,
            a.wrapping_add(b).wrapping_add(1),
            "long-return common path for a={a}, b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_long_return_min_value_legit() {
    // The collision case: a callee that LEGITIMATELY returns Long.MIN_VALUE
    // (== i64::MIN) must NOT be misread as the deopt/exception sentinel.
    // `dispatch_threw` reports "no signal pending" (0), so the caller keeps the
    // value and runs the trailing `+ 1` → MIN_VALUE + 1. Pre-fix this returned
    // i64::MIN (the caller silently bailed at the call, skipping the `+ 1`).
    //   static long f(long a, long b) { return g(a, b) + 1; }
    unsafe extern "C" fn min_value_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // a real Long.MIN_VALUE result — no exception
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0 // no exception/deopt pending
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = min_value_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN.wrapping_add(1),
        "a legitimate Long.MIN_VALUE return must be kept (then +1), not bailed as a deopt"
    );
}

#[test]
fn ir_vs_singlepass_invokestatic_long_return_min_value_exception() {
    // The genuine-sentinel case: the callee threw, so the dispatch helper
    // returns i64::MIN AND `dispatch_threw` reports a pending signal (1). The
    // caller must BAIL — propagate the sentinel unchanged, skipping the trailing
    // `+ 1` — so the VM's post-JIT path routes the pending exception.
    //   static long f(long a, long b) { return g(a, b) + 1; }
    unsafe extern "C" fn throwing_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // the deopt/exception sentinel
    }
    unsafe extern "C" fn did_throw() -> i64 {
        1 // a real exception/deopt is pending
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = throwing_dispatch as *const () as usize;
    helpers.dispatch_threw = did_throw as *const () as usize;
    let code = vec![0x1e, 0x20, 0xb8, 0x00, 0x02, 0x0a, 0x61, 0xad];
    let cm = cached("f", "(JJ)J", code, 4, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(JJ)J".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing callee (i64::MIN + pending signal) must bail and propagate the sentinel \
         (the trailing +1 must NOT run)"
    );
}

#[test]
fn ir_vs_singlepass_invokespecial_instance_call() {
    // inc 24: a resolved non-`<init>` `invokespecial` (a private / `super.` /
    // otherwise non-virtual instance call) lowers to `Op::Call` with the
    // receiver marshalled as arg0 and `invoke_kind == 1`. Structurally this
    // mirrors the inc-22 reference-arg test, but the object is the IMPLICIT
    // receiver: the target descriptor is `(I)I`, not `(Lpkg/Corpus;I)I`, so the
    // IR builder must compute `num_jit_args = 1 (descriptor) + 1 (receiver)` and
    // place the receiver first. The stub reads field 0 off the receiver pointer
    // and returns `recv.x + n`, so a wrong receiver/arg order, base, or count
    // diverges.
    //   int f(Corpus o, int n) { return o.g(n); }   // g private → invokespecial
    //   aload_0; iload_1; invokespecial #2; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p; // arg0 = the receiver pointer (invokespecial `this`)
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb7, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            // The invokespecial target: instance method `g(I)I` (receiver implicit).
            Some(("pkg/Corpus".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokespecial instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is the receiver (arg0), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokespecial receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokevirtual_instance_call() {
    // inc 25: a resolved `invokevirtual` lowers to `Op::Call` with the receiver
    // marshalled as arg0 and `invoke_kind == 0`. Identical machine-level shape
    // to the inc-24 invokespecial test (receiver-first, `num_jit_args == 2`,
    // `needs_context`); only the opcode (0xb6) and dispatch kind differ. The
    // stub reads field 0 off the receiver and returns `recv.x + n`.
    //   int f(Corpus o, int n) { return o.g(n); }   // g virtual → invokevirtual
    //   aload_0; iload_1; invokevirtual #2; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p; // arg0 = the receiver pointer
        let n = *p.add(1) as i32 as i64; // arg1 = int
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET; // field 0 int payload
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    unsafe extern "C" fn recv_mic_dispatch(
        vm: i64,
        info: i64,
        args_ptr: i64,
        num_args: i64,
        _mic: i64,
        _pic: i64,
    ) -> i64 {
        recv_dispatch(vm, info, args_ptr, num_args)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    helpers.invoke_virtual_mic = recv_mic_dispatch as *const () as usize;
    let code = vec![0x2a, 0x1b, 0xb6, 0x00, 0x02, 0xac];
    let cm = cached("f", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Corpus".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokevirtual instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: `obj` is a live, correctly-laid-out synthetic object; its
        // address is the receiver (arg0), `n` is arg1; the stub only reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokevirtual receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

#[test]
fn ir_vs_singlepass_invokeinterface_instance_call() {
    // inc 25: a resolved `invokeinterface` lowers to `Op::Call` with the receiver
    // marshalled as arg0 and `invoke_kind == 2`. The decisive extra coverage vs.
    // the invokevirtual test is the FIVE-byte instruction encoding
    // (0xb9, cp_hi, cp_lo, count, 0): the builder and both length walkers must
    // advance pc += 5, or the trailing `ireturn` is mis-located and the method
    // either bails or miscompiles. The stub reads field 0 off the receiver.
    //   int f(Iface o, int n) { return o.g(n); }   // g interface → invokeinterface
    //   aload_0; iload_1; invokeinterface #2, 2, 0; ireturn
    unsafe extern "C" fn recv_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "recv_dispatch expects (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p;
        let n = *p.add(1) as i32 as i64;
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        let x = std::ptr::read_unaligned((recv as *const u8).add(off) as *const i32) as i64;
        x + n
    }
    unsafe extern "C" fn recv_mic_dispatch(
        vm: i64,
        info: i64,
        args_ptr: i64,
        num_args: i64,
        _mic: i64,
        _pic: i64,
    ) -> i64 {
        recv_dispatch(vm, info, args_ptr, num_args)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = recv_dispatch as *const () as usize;
    helpers.invoke_virtual_mic = recv_mic_dispatch as *const () as usize;
    // invokeinterface is 5 bytes: opcode, cp_hi, cp_lo, count(=2: receiver+int), 0.
    let code = vec![0x2a, 0x1b, 0xb9, 0x00, 0x02, 0x02, 0x00, 0xac];
    let cm = cached("f", "(Lpkg/Iface;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Iface".into(), "g".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("IR compile of invokeinterface instance method");
    assert!(
        ir.needs_context(),
        "an Op::Call method must report needs_context"
    );
    let dummy_vm = [0u8; 64];
    for (xval, n) in [(5i32, 7i64), (-3, 2), (0, 0), (i32::MAX, 1)] {
        let obj = make_object(&[xval]);
        let args = [obj.as_ptr() as i64, n];
        // SAFETY: as above — receiver is arg0, `n` is arg1; the stub reads field 0.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("call x={xval},n={n}: {e:?}"));
        assert_eq!(
            r,
            xval as i64 + n,
            "invokeinterface (5-byte) receiver marshalling for x={xval}, n={n}"
        );
        drop(obj);
    }
}

// ── inc 30: double/float XMM value tier ──────────────────────────────────
//
// Each corpus method takes `int` args and returns an `int`, computing with
// `float`/`double` internally — so the GPR i64-arg / i64-ret `try_call` ABI is
// exact and the FP work stays in XMM (FP params/returns are a follow-on). The
// host anchor is computed with Rust's float-to-int `as` cast, which saturates
// (NaN→0, ±overflow→MIN/MAX, truncate toward zero) — exactly the JVM `f2i`/`d2i`
// semantics the JIT must reproduce, so it catches a bug shared by both backends.

#[test]
fn ir_vs_singlepass_fadd_round_trip() {
    // int f(int a, int b) { return (int)((float)a + (float)b); }
    //   iload_0; i2f; iload_1; i2f; fadd; f2i; ireturn
    check_fp(
        "fadd",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x62, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 7),
            (vec![-5, 10], 5),
            // both < 2^24, so the float sum is exact
            (vec![1_000_000, 2_000_000], 3_000_000),
        ],
    );
}

#[test]
fn ir_vs_singlepass_fsub_fmul() {
    // int f(int a, int b) { return (int)((float)a - (float)b); }
    check_fp(
        "fsub",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x66, 0x8b, 0xac],
        2,
        2,
        &[(vec![10, 3], 7), (vec![3, 10], -7), (vec![-4, -9], 5)],
    );
    // int f(int a, int b) { return (int)((float)a * (float)b); }
    check_fp(
        "fmul",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x6a, 0x8b, 0xac],
        2,
        2,
        &[(vec![3, 4], 12), (vec![-5, 6], -30), (vec![7, 0], 0)],
    );
}

#[test]
fn ir_vs_singlepass_fdiv_with_inf_overflow() {
    // int f(int a, int b) { return (int)((float)a / (float)b); }
    //   iload_0; i2f; iload_1; i2f; fdiv; f2i; ireturn
    check_fp(
        "fdiv",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x6e, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![7, 2], 3),         // 3.5 → 3 (toward zero)
            (vec![-7, 2], -3),       // -3.5 → -3
            (vec![10, 3], 3),        // 3.333…
            (vec![5, 0], i32::MAX),  // +inf → INT_MAX (NaN/overflow fixup)
            (vec![-5, 0], i32::MIN), // -inf → INT_MIN
        ],
    );
}

#[test]
fn ir_vs_singlepass_dmul_with_overflow() {
    // int f(int a, int b) { return (int)((double)a * (double)b); }
    //   iload_0; i2d; iload_1; i2d; dmul; d2i; ireturn
    check_fp(
        "dmul",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x6b, 0x8e, 0xac],
        2,
        2,
        &[
            (vec![3, 4], 12),
            (vec![-7, 6], -42),
            (vec![100_000, 100_000], i32::MAX),  // 1e10 → INT_MAX
            (vec![-100_000, 100_000], i32::MIN), // -1e10 → INT_MIN
        ],
    );
    // int f(int a, int b) { return (int)((double)a - (double)b); }  — dsub
    check_fp(
        "dsub",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x67, 0x8e, 0xac],
        2,
        2,
        &[(vec![10, 3], 7), (vec![3, 10], -7)],
    );
}

#[test]
fn ir_vs_singlepass_fp_negation() {
    // int f(int a) { return (int)(-((float)a)); }  — fneg (sign-bit flip)
    check_fp(
        "fneg",
        "(I)I",
        vec![0x1a, 0x86, 0x76, 0x8b, 0xac],
        1,
        1,
        &[
            (vec![5], -5),
            (vec![-5], 5),
            (vec![0], 0),
            (vec![123456], -123456),
        ],
    );
    // int f(int a) { return (int)(-((double)a)); }  — dneg
    check_fp(
        "dneg",
        "(I)I",
        vec![0x1a, 0x87, 0x77, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], -5), (vec![-5], 5), (vec![0], 0)],
    );
}

// ── FP remainder (frem/drem) — Slice A ──────────────────────────────────────
//
// JVMS FP remainder is `fmod`-style (truncated, sign of the dividend) with no
// single SSE instruction, so the IR `Op::Rem` Float/Double arm lowers to a
// `CALL` of the `jit_frem`/`jit_drem` runtime helper (operands in XMM0/XMM1,
// result XMM0). The SINGLE-PASS backend has no `frem`/`drem` arm — it bails such
// a method to the interpreter — so unlike the other FP ops there is no
// IR-vs-single-pass comparison; the contract is IR == host IEEE anchor. Rust's
// `%` on floats is itself `fmod`, the same reference the production helper
// delegates to, so it catches a miscompile in the call sequence / NaN handling.

/// Real `frem`/`drem` helper stubs for the FP-remainder tests — `extern "C"`
/// with the float ABI (args in XMM0/XMM1, return XMM0), exactly what the IR
/// `Op::Rem` site `CALL`s. Mirrors the production `jit_frem`/`jit_drem`.
unsafe extern "C" fn test_frem(a: f32, b: f32) -> f32 {
    a % b
}
unsafe extern "C" fn test_drem(a: f64, b: f64) -> f64 {
    a % b
}

/// [`dummy_helpers`] with the two FP-remainder helpers wired to real stubs (the
/// rest stay panic stubs — a `frem`/`drem` method calls no other helper).
fn frem_helpers() -> JitRuntimeHelpers {
    JitRuntimeHelpers {
        safepoint_flag_addr: 0,
        safepoint_slow_path: 0,
        jit_frem: test_frem as *const () as usize,
        jit_drem: test_drem as *const () as usize,
        self_call_stack_guard: 0,
        region_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        read_bounds_addr: TEST_REGION_BOUNDS.as_ptr() as usize,
        local_handler_lookup: 0,
        native_stack_floor_fn: 0,
        ..dummy_helpers()
    }
}

/// Compile an FP-remainder method through the IR pipeline ONLY and assert
/// IR == host (Rust float `%`). Single-pass bails `frem`/`drem`, so there is no
/// IR-vs-single-pass leg; the assert that single-pass returns `None` pins that
/// assumption (a future single-pass `frem` should switch this to `check_fp`).
fn check_frem(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i32)],
) {
    let helpers = frem_helpers();
    // Compile the IR body FIRST: a single-pass attempt bails (no frem arm) and
    // adds the method to the shared permanent bail-list (`mark_jit_bail_listed`),
    // which would then short-circuit this IR compile to `None`. So the
    // single-pass-bails check below runs on a DISTINCTLY-named twin method.
    let cm = cached(name, descriptor, code.clone(), max_locals, num_params);
    let ir = compile_fp_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: IR (FP) failed to compile frem/drem — gate/builder?"));
    // Pin the "single-pass bails frem/drem" assumption (a future single-pass
    // frem arm should switch this test to `check_fp`). Separate name so the
    // bail-list entry it creates can't shadow `cm` above.
    let twin = cached(
        &format!("{name}_spbail"),
        descriptor,
        code,
        max_locals,
        num_params,
    );
    assert!(
        compile_fp_opt(&twin, &helpers, false).is_none(),
        "{name}: single-pass unexpectedly compiled frem/drem — switch this test to check_fp",
    );
    for (args, expected) in cases {
        // SAFETY: IR-produced body from valid FP bytecode with an int signature;
        // the i64-arg/i64-ret ABI matches try_call and only frem/drem (wired
        // above) is reachable.
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir as i32, *expected,
            "{name}: IR vs host DIVERGE for {args:?}: IR={}, host={expected}",
            r_ir as i32,
        );
    }
}

#[test]
fn ir_fp_frem_integer_operands() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // int f(int a, int b) { return (int)((float)a % (float)b); }
    //   iload_0; i2f; iload_1; i2f; frem; f2i; ireturn
    check_frem(
        "frem",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x72, 0x8b, 0xac],
        2,
        2,
        &[
            (vec![7, 3], 1),   // 7 % 3 = 1
            (vec![-7, 3], -1), // sign of dividend
            (vec![7, -3], 1),  // sign of dividend (not divisor)
            (vec![-7, -3], -1),
            (vec![8, 4], 0),
            (vec![10, 3], 1),
            (vec![5, 0], 0), // x % 0 = NaN; f2i(NaN) = 0
        ],
    );
}

#[test]
fn ir_fp_drem_integer_operands() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // int f(int a, int b) { return (int)((double)a % (double)b); }
    //   iload_0; i2d; iload_1; i2d; drem; d2i; ireturn
    check_frem(
        "drem",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x73, 0x8e, 0xac],
        2,
        2,
        &[
            (vec![7, 3], 1),
            (vec![-7, 3], -1),
            (vec![7, -3], 1),
            (vec![-7, -3], -1),
            (vec![8, 4], 0),
            (vec![10, 3], 1),
            (vec![5, 0], 0), // NaN → 0
        ],
    );
}

#[test]
fn ir_fp_frem_fractional() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // int f(int a, int b) { return (int)(((a/4f) % (b/4f)) * 4f); } — genuinely
    // fractional intermediate remainders (0.25, …) prove the helper computes a
    // real fmod, not just integer-operand agreement. The *4 rescale lands on an
    // integer so f2i is lossless, and every value (n/4) is exact in binary FP.
    //   iload_0;i2f; iconst_4;i2f;fdiv; iload_1;i2f; iconst_4;i2f;fdiv; frem;
    //   iconst_4;i2f;fmul; f2i; ireturn
    check_frem(
        "frem_frac",
        "(II)I",
        vec![
            0x1a, 0x86, 0x07, 0x86, 0x6e, // (float)a / 4
            0x1b, 0x86, 0x07, 0x86, 0x6e, // (float)b / 4
            0x72, // frem
            0x07, 0x86, 0x6a, // * 4
            0x8b, 0xac, // f2i; ireturn
        ],
        2,
        2,
        &[
            (vec![7, 2], 1),   // 1.75 % 0.5 = 0.25 → *4 = 1
            (vec![9, 4], 1),   // 2.25 % 1.0 = 0.25 → 1
            (vec![-7, 2], -1), // -1.75 % 0.5 = -0.25 → -1
            (vec![10, 3], 1),  // 2.5 % 0.75 = 0.25 → 1
        ],
    );
}

#[test]
fn ir_fp_drem_fractional() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // Double analogue of `ir_fp_frem_fractional`.
    //   iload_0;i2d; iconst_4;i2d;ddiv; iload_1;i2d; iconst_4;i2d;ddiv; drem;
    //   iconst_4;i2d;dmul; d2i; ireturn
    check_frem(
        "drem_frac",
        "(II)I",
        vec![
            0x1a, 0x87, 0x07, 0x87, 0x6f, // (double)a / 4
            0x1b, 0x87, 0x07, 0x87, 0x6f, // (double)b / 4
            0x73, // drem
            0x07, 0x87, 0x6b, // * 4
            0x8e, 0xac, // d2i; ireturn
        ],
        2,
        2,
        &[
            (vec![7, 2], 1),
            (vec![9, 4], 1),
            (vec![-7, 2], -1),
            (vec![10, 3], 1),
        ],
    );
}

// ── FP method with int-div (Slice C: FP-slot deopt resume) ──────────────────
//
// Dropping the FP gate's `!method_has_int_div` exclusion admits an FP method
// containing an `idiv`/`irem` to the IR path. The int division carries a
// div-by-zero deopt guard; with FP-slot resume wired, an FP value live ACROSS
// that guard is reconstructed precisely on a deopt (the div-by-zero path is
// validated E2E vs HotSpot — caught ArithmeticException + correct FP value).
// Here, with a non-zero divisor (no deopt), the method must compile via IR and
// agree with single-pass (which also compiles idiv + FP) and the host anchor.

#[test]
fn ir_fp_method_with_int_div() {
    // int f(int a, int b) { float x = (float)a * 2; int q = a / b;
    //                       return (int)(x + (float)q); }
    // The FP value `x` is live on the operand stack ACROSS the idiv (the deopt
    // point), exercising an FP stack slot at the guard.
    //   iload_0; i2f; fconst_2; fmul;   // x = a*2.0f  [FP live]
    //   iload_0; iload_1; idiv;          // q = a/b     [idiv deopt guard]
    //   i2f; fadd; f2i; ireturn          // (int)(x + (float)q)
    check_fp(
        "fp_intdiv",
        "(II)I",
        vec![
            0x1a, 0x86, 0x0d, 0x6a, // (float)a * 2.0
            0x1a, 0x1b, 0x6c, // a / b
            0x86, 0x62, 0x8b, 0xac, // (float)q ; +x ; (int) ; return
        ],
        2,
        2,
        &[
            (vec![10, 2], 25),   // x=20.0, q=5  → 25
            (vec![7, 3], 16),    // x=14.0, q=2  → 16
            (vec![-8, 4], -18),  // x=-16.0, q=-2 → -18
            (vec![100, 7], 214), // x=200.0, q=14 → 214
        ],
    );
}

// ── FP array element access (faload/daload/fastore/dastore) — Slice B ───────
//
// The IR lowers an FP array access inline: array→RAX, index→RCX, the JVMS null
// + bounds deopt guards, then a `MOVSS`/`MOVSD` at `[RAX + RCX*elem_size +
// HEADER_SIZE]`. Single-pass compiles the same opcodes inline, so the
// non-faulting path is a true IR==single-pass==host differential (a synthetic
// array buffer is passed as the receiver, mirroring the getfield harness). The
// fault paths (null array / OOB index) deopt → the interpreter re-throws the
// exact NPE/AIOOBE; that parity is validated E2E vs HotSpot.

/// Build a synthetic `float[]` buffer: `length` at `ARRAY_LENGTH_OFFSET`, the
/// elements (4 bytes each) packed from `HEADER_SIZE`.
fn make_f32_array(elems: &[f32]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * 4];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * 4;
        buf[off..off + 4].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    buf
}

/// Build a synthetic `double[]` buffer (8 bytes per element).
fn make_f64_array(elems: &[f64]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * 8];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * 8;
        buf[off..off + 8].copy_from_slice(&v.to_bits().to_le_bytes());
    }
    buf
}

#[test]
fn ir_fp_faload() {
    // static float fget(float[] a, int i) { return a[i]; }
    //   aload_0; iload_1; faload; freturn
    let helpers = dummy_helpers();
    let cm = cached("fget", "([FI)F", vec![0x2a, 0x1b, 0x30, 0xae], 2, 2);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("fget IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("fget single-pass");
    let elems = [1.5f32, -2.25, 0.0, 1234.5, f32::INFINITY];
    let arr = make_f32_array(&elems);
    for (i, &want) in elems.iter().enumerate() {
        let args = [arr.as_ptr() as i64, i as i64];
        // SAFETY: both bodies are JIT-compiled faload; `arr` is a live, correctly
        // laid-out float[] whose address is arg0, the index in-bounds.
        let r_sp = f32::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u32);
        let r_ir = f32::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u32);
        assert_eq!(r_ir.to_bits(), r_sp.to_bits(), "faload IR vs sp at {i}");
        assert_eq!(r_ir.to_bits(), want.to_bits(), "faload vs host at {i}");
    }
}

#[test]
fn ir_fp_daload() {
    // static double dget(double[] a, int i) { return a[i]; }
    //   aload_0; iload_1; daload; dreturn
    let helpers = dummy_helpers();
    let cm = cached("dget", "([DI)D", vec![0x2a, 0x1b, 0x31, 0xaf], 2, 2);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("dget IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("dget single-pass");
    let elems = [1.5f64, -2.25, 0.0, 1e300, f64::NEG_INFINITY];
    let arr = make_f64_array(&elems);
    for (i, &want) in elems.iter().enumerate() {
        let args = [arr.as_ptr() as i64, i as i64];
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        assert_eq!(r_ir.to_bits(), r_sp.to_bits(), "daload IR vs sp at {i}");
        assert_eq!(r_ir.to_bits(), want.to_bits(), "daload vs host at {i}");
    }
}

#[test]
fn ir_fp_fastore() {
    // static void fset(float[] a, int i, float v) { a[i] = v; }
    //   aload_0; iload_1; fload_2; fastore; return
    let helpers = dummy_helpers();
    let cm = cached("fset", "([FIF)V", vec![0x2a, 0x1b, 0x24, 0x51, 0xb1], 3, 3);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("fset IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("fset single-pass");
    for (i, v) in [(0usize, 9.5f32), (2, -1.25), (4, 1e30)] {
        // Each backend writes its OWN fresh array; the element must agree.
        let read_back = |m: &CompiledMethod| -> u32 {
            let mut arr = make_f32_array(&[0.0; 5]);
            let args = [arr.as_mut_ptr() as i64, i as i64, v.to_bits() as i64];
            // SAFETY: JIT-compiled fastore; live float[] arg0, in-bounds index,
            // float value passed as bits (compact GPR FP ABI). No helper reached.
            unsafe { m.try_call(&args) }.unwrap();
            let off = HEADER_SIZE + i * 4;
            u32::from_le_bytes([arr[off], arr[off + 1], arr[off + 2], arr[off + 3]])
        };
        let got_sp = read_back(&sp);
        let got_ir = read_back(&ir);
        assert_eq!(got_ir, got_sp, "fastore IR vs sp at {i}");
        assert_eq!(got_ir, v.to_bits(), "fastore vs host at {i}");
    }
}

#[test]
fn ir_fp_dastore() {
    // static void dset(double[] a, int i, double v) { a[i] = v; }
    //   aload_0; iload_1; dload_2; dastore; return  (double v is cat-2 → slots 2-3;
    //   dload_2 == 0x28, NOT dload_0/0x26)
    let helpers = dummy_helpers();
    let cm = cached("dset", "([DID)V", vec![0x2a, 0x1b, 0x28, 0x52, 0xb1], 4, 3);
    let ir = compile_fp_opt(&cm, &helpers, true).expect("dset IR (FP)");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("dset single-pass");
    for (i, v) in [(0usize, 9.5f64), (2, -1.25), (4, 1e300)] {
        let read_back = |m: &CompiledMethod| -> u64 {
            let mut arr = make_f64_array(&[0.0; 5]);
            let args = [arr.as_mut_ptr() as i64, i as i64, v.to_bits() as i64];
            unsafe { m.try_call(&args) }.unwrap();
            let off = HEADER_SIZE + i * 8;
            u64::from_le_bytes([
                arr[off],
                arr[off + 1],
                arr[off + 2],
                arr[off + 3],
                arr[off + 4],
                arr[off + 5],
                arr[off + 6],
                arr[off + 7],
            ])
        };
        let got_sp = read_back(&sp);
        let got_ir = read_back(&ir);
        assert_eq!(got_ir, got_sp, "dastore IR vs sp at {i}");
        assert_eq!(got_ir, v.to_bits(), "dastore vs host at {i}");
    }
}

// ── FP 3-way compares (fcmpl/fcmpg/dcmpl/dcmpg) — item 3 next slice ──────────
//
// The compare yields int {-1,0,1} feeding the existing `if<cond>`. The
// `ucomis`-based branchless lowering must agree with single-pass AND the host
// IEEE anchor for the ordered cases, AND honour the JVMS NaN-unordered rule
// (NaN → -1 for the `l` variants, +1 for the `g` variants), for a NaN in EITHER
// operand position.

#[test]
fn ir_vs_singlepass_fcmp_ordered() {
    // int f(int a, int b) { return Float.compare-ish: ((float)a) <cmp> ((float)b); }
    //   iload_0; i2f; iload_1; i2f; fcmp{l,g}; ireturn
    let ordered = &[
        (vec![1i64, 2], -1i32),
        (vec![2, 1], 1),
        (vec![5, 5], 0),
        (vec![-3, -3], 0),
        (vec![-5, 2], -1),
        (vec![2, -5], 1),
    ];
    // fcmpl (0x95) and fcmpg (0x96) are identical for ordered operands.
    check_fp(
        "fcmpl_ord",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x95, 0xac],
        2,
        2,
        ordered,
    );
    check_fp(
        "fcmpg_ord",
        "(II)I",
        vec![0x1a, 0x86, 0x1b, 0x86, 0x96, 0xac],
        2,
        2,
        ordered,
    );
}

#[test]
fn ir_vs_singlepass_dcmp_ordered() {
    //   iload_0; i2d; iload_1; i2d; dcmp{l,g}; ireturn
    let ordered = &[
        (vec![1i64, 2], -1i32),
        (vec![2, 1], 1),
        (vec![7, 7], 0),
        (vec![-9, 4], -1),
        (vec![4, -9], 1),
    ];
    check_fp(
        "dcmpl_ord",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x97, 0xac],
        2,
        2,
        ordered,
    );
    check_fp(
        "dcmpg_ord",
        "(II)I",
        vec![0x1a, 0x87, 0x1b, 0x87, 0x98, 0xac],
        2,
        2,
        ordered,
    );
}

#[test]
fn ir_vs_singlepass_fcmp_nan() {
    // NaN is synthesized as 0.0f/0.0f (fconst_0 fconst_0 fdiv). fcmpl → -1 and
    // fcmpg → +1 for a NaN in either position.
    // int f() { return (0/0f) fcmpl 1f; }  — NaN as LEFT operand
    check_fp(
        "fcmpl_nan_lhs",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x0c, 0x95, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    // int f() { return (0/0f) fcmpg 1f; }
    check_fp(
        "fcmpg_nan_lhs",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x0c, 0x96, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
    // int f() { return 1f fcmpl (0/0f); }  — NaN as RIGHT operand
    check_fp(
        "fcmpl_nan_rhs",
        "()I",
        vec![0x0c, 0x0b, 0x0b, 0x6e, 0x95, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    // int f() { return 1f fcmpg (0/0f); }
    check_fp(
        "fcmpg_nan_rhs",
        "()I",
        vec![0x0c, 0x0b, 0x0b, 0x6e, 0x96, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
}

#[test]
fn ir_vs_singlepass_dcmp_nan() {
    // NaN as 0.0/0.0 (dconst_0 dconst_0 ddiv). dcmpl → -1, dcmpg → +1.
    check_fp(
        "dcmpl_nan_lhs",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x0f, 0x97, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    check_fp(
        "dcmpg_nan_lhs",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x0f, 0x98, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
    check_fp(
        "dcmpl_nan_rhs",
        "()I",
        vec![0x0f, 0x0e, 0x0e, 0x6f, 0x97, 0xac],
        0,
        0,
        &[(vec![], -1)],
    );
    check_fp(
        "dcmpg_nan_rhs",
        "()I",
        vec![0x0f, 0x0e, 0x0e, 0x6f, 0x98, 0xac],
        0,
        0,
        &[(vec![], 1)],
    );
}

// ── double RETURNS — method-level dreturn + D call returns (inc 32) ──────────
//
// A `double` result rides RAX as clean 64-bit bits (the JIT i64 return ABI; the
// interpreter reads `result as u64` → `f64::from_bits`). The `dreturn` builder
// arm + `Op::Return`'s `load_to_rax` need no XMM return marshalling, and a `D`
// call result's `-0.0`/`i64::MIN` bit collision is disambiguated by the item-1
// `dispatch_threw` peek (the call-site check matches `IrType::Double`).

#[test]
fn ir_vs_singlepass_double_return() {
    // static double f(int x) { return (double)x + 1.0; }
    //   iload_0; i2d; dconst_1; dadd; dreturn
    let code = vec![0x1a, 0x87, 0x0f, 0x63, 0xaf];
    let cm = cached("dret", "(I)D", code, 1, 1);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true)
        .expect("IR compile of double-returning method (FP gate)");
    let sp = compile_fp_opt(&cm, &helpers, false)
        .expect("single-pass compile of double-returning method");
    for x in [0i64, 5, -3, 100, -100, i32::MIN as i64] {
        // SAFETY: both bodies take one int arg and return a double whose bits
        // ride RAX; `try_call` returns that i64 (the f64 bit pattern).
        let r_ir = f64::from_bits(unsafe { ir.try_call(&[x]) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&[x]) }.unwrap() as u64);
        let expected = x as f64 + 1.0;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR double-return for x={x}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass double-return for x={x}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_double_return() {
    // static double f(int x) { return g(x) + 1.0; }   // g:(I)D
    //   iload_0; invokestatic #2; dconst_1; dadd; dreturn
    // The caller consumes a `double`-returning Op::Call result — exercises the
    // `static_call_shape` D-return admission + `IrType::Double` call-result
    // typing + the item-1 sentinel disambiguation on a D call result.
    unsafe extern "C" fn dret_dispatch(_vm: i64, _i: i64, args_ptr: i64, _n: i64) -> i64 {
        // g(x) = (double)x * 0.5, returned as raw f64 bits in RAX.
        let x = unsafe { *(args_ptr as *const i64) } as i32;
        ((x as f64) * 0.5).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = dret_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0f, 0x63, 0xaf];
    let cm = cached("f", "(I)D", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)D".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of double-call-returning method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for x in [4i64, 5, -6, 0, 1000] {
        let r = f64::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[x]) }.unwrap() as u64,
        );
        let expected = (x as f64) * 0.5 + 1.0;
        assert_eq!(r.to_bits(), expected.to_bits(), "D call-return for x={x}");
    }
}

// ── float RETURNS — method-level freturn + F call returns (inc 33) ───────────
//
// A `float` result rides the LOW 32 of RAX; consumers read only the low 32
// (interpreter `result as u32`; downstream `MOVSS`), so stale upper bits are
// harmless EXCEPT the `+0.0f`/`i64::MIN`-bits collision on a call result, which
// the call-site `dispatch_threw` peek (now `IrType::Float`) catches.

#[test]
fn ir_vs_singlepass_float_return() {
    // static float f(int x) { return (float)x + 1.0f; }
    //   iload_0; i2f; fconst_1; fadd; freturn
    let code = vec![0x1a, 0x86, 0x0c, 0x62, 0xae];
    let cm = cached("fret", "(I)F", code, 1, 1);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true)
        .expect("IR compile of float-returning method (FP gate)");
    let sp = compile_fp_opt(&cm, &helpers, false)
        .expect("single-pass compile of float-returning method");
    for x in [0i64, 5, -3, 100, -100, i32::MIN as i64] {
        // float bits ride the low 32 of RAX (stale upper bits are masked).
        let r_ir = f32::from_bits(unsafe { ir.try_call(&[x]) }.unwrap() as u32);
        let r_sp = f32::from_bits(unsafe { sp.try_call(&[x]) }.unwrap() as u32);
        let expected = x as f32 + 1.0f32;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR float-return for x={x}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass float-return for x={x}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_float_return() {
    // static float f(int x) { return g(x) + 1.0f; }   // g:(I)F
    //   iload_0; invokestatic #2; fconst_1; fadd; freturn
    unsafe extern "C" fn fret_dispatch(_vm: i64, _i: i64, args_ptr: i64, _n: i64) -> i64 {
        let x = unsafe { *(args_ptr as *const i64) } as i32;
        // g(x) = (float)x * 0.5f, returned as float bits.
        ((x as f32) * 0.5f32).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = fret_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0c, 0x62, 0xae];
    let cm = cached("f", "(I)F", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)F".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of float-call-returning method");
    let dummy_vm = [0u8; 64];
    for x in [4i64, 5, -6, 0, 1000] {
        let r = f32::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[x]) }.unwrap() as u32,
        );
        let expected = (x as f32) * 0.5f32 + 1.0f32;
        assert_eq!(r.to_bits(), expected.to_bits(), "F call-return for x={x}");
    }
}

#[test]
fn ir_vs_singlepass_float_return_min_bits_collision() {
    // The float collision: a `+0.0f` call result whose RAX bits == i64::MIN (low
    // 32 = 0 = +0.0f, upper 32 = 0x8000_0000 stale) must NOT be misread as the
    // deopt sentinel. `dispatch_threw` reports no signal, the caller keeps it, and
    // the low 32 (+0.0f) flows on → +0.0f + 1.0f = 1.0f. (i64::MIN has low-63 = 0,
    // so ONLY +0.0f can ever collide for a float.)
    unsafe extern "C" fn min_bits_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        i64::MIN // low 32 = 0 = +0.0f bits; upper 32 = stale 0x8000_0000
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = min_bits_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    // static float f(int x) { return g(x) + 1.0f; }
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0x0c, 0x62, 0xae];
    let cm = cached("f", "(I)F", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(I)F".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = f32::from_bits(
        unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[7]) }.unwrap() as u32,
    );
    assert_eq!(
        r.to_bits(),
        1.0f32.to_bits(),
        "a +0.0f call-return whose RAX bits == i64::MIN must be kept (→ +0.0f + 1.0f = 1.0f), not bailed"
    );
}

// ── FP PARAMS + D/F call-ARGS (inc 34) ──────────────────────────────────────
//
// The VM uses the compact all-GPR i64 ABI: an FP param/arg arrives as bits in an
// INTEGER register (`to_bits() as i64`), NOT XMM. The prologue stores it to the
// param slot like any other param; `set_param_types` types it Float/Double (cat-2
// two-slot layout for `D`), so a `dload`/`fload` reads it via `fp_load`. So a
// method with an FP SIGNATURE (not just FP-internal) now takes the IR path.

#[test]
fn ir_vs_singlepass_double_param() {
    // static double f(double a, int b) { return a + (double)b; }
    //   dload_0; iload_2; i2d; dadd; dreturn   (a: D @ jvm 0-1, b: I @ jvm 2)
    let code = vec![0x26, 0x1c, 0x87, 0x63, 0xaf];
    let cm = cached("dparam", "(DI)D", code, 3, 2);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true).expect("IR double-param");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("single-pass double-param");
    for (a, b) in [(1.5f64, 2i64), (-3.25, 7), (1e10, -5), (0.0, 0)] {
        let args = [a.to_bits() as i64, b];
        let r_ir = f64::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u64);
        let r_sp = f64::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u64);
        let expected = a + b as f64;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR double-param a={a},b={b}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass double-param a={a},b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_float_param() {
    // static float f(float a, int b) { return a + (float)b; }
    //   fload_0; iload_1; i2f; fadd; freturn   (a: F @ jvm 0, b: I @ jvm 1)
    let code = vec![0x22, 0x1b, 0x86, 0x62, 0xae];
    let cm = cached("fparam", "(FI)F", code, 2, 2);
    let helpers = dummy_helpers();
    let ir = compile_fp_opt(&cm, &helpers, true).expect("IR float-param");
    let sp = compile_fp_opt(&cm, &helpers, false).expect("single-pass float-param");
    for (a, b) in [(1.5f32, 2i64), (-3.25, 7), (100.0, -5), (0.0, 0)] {
        let args = [a.to_bits() as i64, b];
        let r_ir = f32::from_bits(unsafe { ir.try_call(&args) }.unwrap() as u32);
        let r_sp = f32::from_bits(unsafe { sp.try_call(&args) }.unwrap() as u32);
        let expected = a + b as f32;
        assert_eq!(
            r_ir.to_bits(),
            expected.to_bits(),
            "IR float-param a={a},b={b}"
        );
        assert_eq!(
            r_sp.to_bits(),
            expected.to_bits(),
            "single-pass float-param a={a},b={b}"
        );
    }
}

#[test]
fn ir_vs_singlepass_invokestatic_fp_args() {
    // static double f(double a, float b) { return g(a, b); }   // g:(DF)D
    //   dload_0; fload_2; invokestatic #2; dreturn   (a: D @ 0-1, b: F @ 2)
    // Exercises a `double` arg + a `float` arg through `Op::Call` (marshalled as
    // bits to the staging region) + the FP params of the caller + a `D` call return.
    unsafe extern "C" fn dfarg_dispatch(_vm: i64, _i: i64, args_ptr: i64, n: i64) -> i64 {
        assert_eq!(n, 2, "dfarg_dispatch expects (double, float)");
        let p = args_ptr as *const i64;
        let a = f64::from_bits(unsafe { *p } as u64); // arg0 = double (full 64 bits)
        let b = f32::from_bits(unsafe { *p.add(1) } as u32); // arg1 = float (low 32)
        (a + b as f64).to_bits() as i64
    }
    unsafe extern "C" fn never_threw() -> i64 {
        0
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = dfarg_dispatch as *const () as usize;
    helpers.dispatch_threw = never_threw as *const () as usize;
    let code = vec![0x26, 0x24, 0xb8, 0x00, 0x02, 0xaf];
    let cm = cached("f", "(DF)D", code, 3, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(DF)D".into()))
        } else {
            None
        }
    };
    let ir = compile_with_dispatch_fp(&cm, &helpers, &resolver)
        .expect("IR compile of FP-call-args method");
    assert!(ir.needs_context());
    let dummy_vm = [0u8; 64];
    for (a, b) in [(1.5f64, 2.5f32), (-3.0, 0.25), (1e9, -1.0), (0.0, 0.0)] {
        let args = [a.to_bits() as i64, b.to_bits() as i64];
        let r = f64::from_bits(
            unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }.unwrap() as u64,
        );
        let expected = a + b as f64;
        assert_eq!(r.to_bits(), expected.to_bits(), "FP call-args a={a},b={b}");
    }
}

#[test]
fn ir_vs_singlepass_fp_constants() {
    // int f(int a) { return (int)((float)a + 2.0f); }  — fconst_2
    check_fp(
        "fconst2",
        "(I)I",
        vec![0x1a, 0x86, 0x0d, 0x62, 0x8b, 0xac],
        1,
        1,
        &[(vec![5], 7), (vec![-3], -1), (vec![100], 102)],
    );
    // int f(int a) { return (int)((double)a + 1.0); }  — dconst_1
    check_fp(
        "dconst1",
        "(I)I",
        vec![0x1a, 0x87, 0x0f, 0x63, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], 6), (vec![-3], -2)],
    );
}

#[test]
fn ir_vs_singlepass_fp_fp_conversions() {
    // int f(int a) { return (int)(double)(float)a; }  — i2f; f2d; d2i
    check_fp(
        "f2d",
        "(I)I",
        vec![0x1a, 0x86, 0x8d, 0x8e, 0xac],
        1,
        1,
        &[(vec![5], 5), (vec![-3], -3), (vec![16_777_216], 16_777_216)],
    );
    // int f(int a) { double d = a; float fv = (float)d; return (int)fv; }  — d2f
    check_fp(
        "d2f",
        "(I)I",
        vec![0x1a, 0x87, 0x90, 0x8b, 0xac],
        1,
        1,
        &[(vec![5], 5), (vec![-100], -100)],
    );
}

#[test]
fn ir_vs_singlepass_long_fp_conversions() {
    // int f(int a) { long L = a; float fv = L; return (int)fv; }  — i2l; l2f; f2i
    check_fp(
        "l2f",
        "(I)I",
        vec![0x1a, 0x85, 0x89, 0x8b, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7)],
    );
    // int f(int a) { float fv = a; long L = (long)fv; return (int)L; }  — i2f; f2l; l2i
    check_fp(
        "f2l",
        "(I)I",
        vec![0x1a, 0x86, 0x8c, 0x88, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7)],
    );
    // int f(int a) { long L=a; double d=L; long r=(long)d; return (int)r; }
    //   i2l; l2d; d2l; l2i
    check_fp(
        "l2d_d2l",
        "(I)I",
        vec![0x1a, 0x85, 0x8a, 0x8f, 0x88, 0xac],
        1,
        1,
        &[(vec![1000], 1000), (vec![-7], -7), (vec![123_456], 123_456)],
    );
}

#[test]
fn ir_vs_singlepass_double_locals() {
    // int f(int a) { double x = a; double y = x*x; return (int)(y + x); }
    //   iload_0; i2d; dstore_1; dload_1; dload_1; dmul; dstore_3; dload_3;
    //   dload_1; dadd; d2i; ireturn   — exercises dstore_1/3 + dload_1/3 (cat-2
    //   locals: x at slots 1-2, y at slots 3-4).
    check_fp(
        "dlocals",
        "(I)I",
        vec![
            0x1a, 0x87, 0x48, 0x27, 0x27, 0x6b, 0x4a, 0x29, 0x27, 0x63, 0x8e, 0xac,
        ],
        5,
        1,
        &[(vec![5], 30), (vec![10], 110), (vec![-3], 6), (vec![0], 0)],
    );
}

#[test]
fn ir_vs_singlepass_wide_fp_load_store() {
    // int f(int a) { float fv = a; return (int)fv; }  with fv at slot 5, forcing
    // the wide fstore/fload forms: iload_0; i2f; fstore 5; fload 5; f2i; ireturn.
    check_fp(
        "wide_fls",
        "(I)I",
        vec![0x1a, 0x86, 0x38, 0x05, 0x17, 0x05, 0x8b, 0xac],
        6,
        1,
        &[(vec![5], 5), (vec![-100], -100), (vec![42], 42)],
    );
}

#[test]
fn ir_vs_singlepass_fp_nan_to_zero() {
    // int f() { float x = 0.0f; return (int)(x / x); }  — 0.0/0.0 = NaN → 0.
    //   fconst_0; fconst_0; fdiv; f2i; ireturn
    check_fp(
        "fnan",
        "()I",
        vec![0x0b, 0x0b, 0x6e, 0x8b, 0xac],
        0,
        0,
        &[(vec![], 0)],
    );
    // int f() { double x = 0.0; return (int)(x / x); }  — dconst_0; dconst_0;
    //   ddiv; d2i; ireturn
    check_fp(
        "dnan",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x8e, 0xac],
        0,
        0,
        &[(vec![], 0)],
    );
}

#[test]
fn ir_vs_singlepass_fp_to_long_fixup() {
    // Exercise the 64-bit (`is_long`) NaN/overflow fixup branch — the long-result
    // conversions f2l/d2l — across all three arms (NaN→0, +inf→LONG_MAX,
    // -inf→LONG_MIN). The result is returned as `(int)(long)…` = the low 32 bits
    // of the saturated long: LONG_MAX (0x7FFF…FF) → -1, LONG_MIN (0x8000…0) → 0,
    // 0L → 0. Inf/NaN are built deterministically from `x/0` (no float rounding).

    // (int)(long)(1.0/0.0) = (int)(+inf) → (int)LONG_MAX = -1.   d2l, +overflow
    check_fp(
        "d2l_pinf",
        "()I",
        vec![0x0f, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_1; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], -1)],
    );
    // (int)(long)(-1.0/0.0) = (int)(-inf) → (int)LONG_MIN = 0.   d2l, -overflow
    check_fp(
        "d2l_ninf",
        "()I",
        vec![0x0f, 0x77, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_1; dneg; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], 0)],
    );
    // (int)(long)(0.0/0.0) = (int)(long)NaN = 0.   d2l, NaN
    check_fp(
        "d2l_nan",
        "()I",
        vec![0x0e, 0x0e, 0x6f, 0x8f, 0x88, 0xac], // dconst_0; dconst_0; ddiv; d2l; l2i; ireturn
        0,
        0,
        &[(vec![], 0)],
    );
    // (int)(long)(1.0f/0.0f) = (int)(+inf) → (int)LONG_MAX = -1.   f2l, +overflow
    check_fp(
        "f2l_pinf",
        "()I",
        vec![0x0c, 0x0b, 0x6e, 0x8c, 0x88, 0xac], // fconst_1; fconst_0; fdiv; f2l; l2i; ireturn
        0,
        0,
        &[(vec![], -1)],
    );
}

// ── Backend-routing guards for the fib44 self-recursion fix ──
//
// With the direct-call opt-out forced for this test, a SELF-RECURSIVE
// wide-return (J/D/F) call must bail the method to single-pass, NOT lower to an
// IR `Op::Call` routed through the generic dispatch helper per call.
// The gate is keyed on `CompiledMethod::used_ir_backend` (true = optimizing IR
// pipeline produced the body; false = single-pass, incl. a bail). These pin the
// gate's behaviour AND its specificity (it fires ONLY for self-recursive J/D/F).

#[test]
fn selfrec_long_return_bails_to_singlepass() {
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(false));

    // static long fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }  // (I)J
    let helpers = dummy_helpers();
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x06, // iload_0; iconst_1; if_icmpgt 8
        0x1a, 0x85, 0xad, // iload_0; i2l; lreturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x61, 0xad, // ladd; lreturn
    ];
    let cm = cached("fib", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        // CP #2 resolves to THIS method (self-recursion). `cached` uses class
        // "Corpus", method = the given name, descriptor = the given descriptor.
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled =
        compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles (via single-pass)");
    assert!(
        !compiled.used_ir_backend,
        "fib44 regression guard: a self-recursive long-returning method must bail to \
         single-pass (direct self-call), not the IR Op::Call/jit_invoke_dispatch path"
    );
}

#[test]
fn selfrec_int_return_uses_ir() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // static int f(int n) { return n < 2 ? n : f(n-1) + f(n-2); }  // (I)I
    // The gate is wide-return-specific: an INT self-recursive call is unaffected
    // and still lowers to the IR Op::Call path (pre-existing behaviour).
    let helpers = dummy_helpers();
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt 7
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x60, 0xac, // iadd; ireturn
    ];
    let cm = cached("f", "(I)I", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "f".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles");
    assert!(
        compiled.used_ir_backend,
        "an int (non-wide) self-recursive call is not gated and should use the IR backend"
    );
}

#[test]
fn crossmethod_long_return_uses_ir() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // static long f(int n) { return g(n); }   // g:(I)J, NOT self-recursive
    // Confirms the gate is SELF-recursion specific: a cross-method long-returning
    // call is the intended inc-29 unblock and still lowers to IR Op::Call.
    let helpers = dummy_helpers();
    let code = vec![0x1a, 0xb8, 0x00, 0x02, 0xad]; // iload_0; invokestatic #2 (g); lreturn
    let cm = cached("f", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        // CP #2 resolves to a DIFFERENT method `g` (not the compiling method `f`).
        if cp == 2 {
            Some(("Corpus".into(), "g".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver).expect("compiles");
    assert!(
        compiled.used_ir_backend,
        "a cross-method long-returning call must NOT be gated (intended IR unblock)"
    );
}

#[test]
fn selfrec_int_direct_call_executes_correctly() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // Integer-returning self-recursion is the common QuickBench fib shape.
    // With the direct-call gate on it must stay on IR without routing every
    // recursive edge through jit_invoke_dispatch.
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(true));

    let helpers = dummy_helpers();
    // static int fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x05, // iload_0; iconst_1; if_icmpgt 7
        0x1a, 0xac, // iload_0; ireturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2
        0x60, 0xac, // iadd; ireturn
    ];
    let cm = cached("fib", "(I)I", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)I".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("self-recursive int compiles on the IR direct-call path");
    assert!(
        compiled.used_ir_backend,
        "integer self-recursion should use the IR direct-call backend"
    );

    let dummy_vm = [0u8; 64];
    for (n, expect) in [
        (0i64, 0i64),
        (1, 1),
        (2, 1),
        (5, 5),
        (10, 55),
        (20, 6765),
        (30, 832040),
    ] {
        let r = unsafe { compiled.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("call fib({n}): {e:?}"));
        assert_eq!(r, expect, "fib({n}) via IR direct self-call");
    }
}

/// A self-recursive method with MORE ARGUMENTS THAN THE ENTRY REGISTER FILE.
///
/// `ir_lower::emit_self_recursive_call` marshals the hidden VM context pointer
/// plus every Java argument into an entry-ABI register — `abi[0]` for the
/// context, `abi[1 + i]` for arg `i`. That file is FOUR registers on Win64
/// (RCX, RDX, R8, R9) and SIX on SysV, so the direct route can carry at most 3
/// Java arguments on Windows and 5 on Linux. Nothing checked the arity, and
/// compiling a 4-arg self-recursive method on Windows PANICKED the lowerer:
///
///   index out of bounds: the len is 4 but the index is 4
///
/// In the VM that panic lands on the BACKGROUND COMPILER THREAD, which does not
/// come back — so the first such method silently drops the whole process to the
/// interpreter for the rest of its life. MEASURED with
/// `CRATONVM_DBG_JIT_COMPILED=1` on `probes/SelfRecArgs.java`: f1, f2 and f3
/// compile, f4 panics, and then nothing compiles at all — not f5, not f6, not
/// `main`'s OSR. It reached a real workload as a HANG, Hibernate's
/// `DefaultCatalogAndSchemaTest` running to a 600 s timeout interpreted.
///
/// # Why this asserts the COMPILE and not the call
///
/// Both arities below are refused the direct route on at least one supported
/// platform, and a refused self-call routes its recursive edge through
/// `jit_invoke_dispatch` — which this file stubs with a panic on purpose (see
/// `dummy_helpers`: "no runtime helper reachable" is this harness's contract).
/// So `try_call_with_context` cannot run them here, and asserting it would test
/// the harness. The defect was a COMPILE-TIME panic, and "it compiles" is the
/// invariant that regressed.
///
/// Two arities rather than one so the test is not silently vacuous on either
/// platform: 4 args is over the limit on Win64 only, 6 is over it on BOTH, so
/// the second row still asks a real question on a six-register host.
#[test]
fn selfrec_more_args_than_entry_regs_still_compiles() {
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    // ON, because the guard being tested lives on the enabled path: with the
    // direct route off there is nothing to run off the end of.
    cratonvm_jit::__set_selfrec_direct_override(Some(true));

    let helpers = dummy_helpers();

    // static int fN(int a, int b, ...) { return a > 0 ? fN(a-1, b, ...) + 1 : b; }
    // Built for an arbitrary arity so the two rows share one shape.
    for arity in [4usize, 6usize] {
        let mut code: Vec<u8> = vec![
            0x1a, // 0  iload_0
            0x9d, 0x00, 0x05, // 1  ifgt -> 6
            0x1b, // 4  iload_1
            0xac, // 5  ireturn
            0x1a, // 6  iload_0
            0x04, // 7  iconst_1
            0x64, // 8  isub
        ];
        // The remaining arguments, forwarded unchanged: locals 1..arity.
        for local in 1..arity {
            // iload <n> — iload_1/2/3 have short forms, the rest take `iload n`.
            match local {
                1 => code.push(0x1b),
                2 => code.push(0x1c),
                3 => code.push(0x1d),
                n => {
                    code.push(0x15);
                    code.push(n as u8);
                }
            }
        }
        code.extend_from_slice(&[0xb8, 0x00, 0x02]); // invokestatic #2
        code.extend_from_slice(&[0x04, 0x60, 0xac]); // iconst_1; iadd; ireturn

        let name = format!("f{arity}");
        let descriptor = format!("({})I", "I".repeat(arity));
        let cm = cached(&name, &descriptor, code, arity as u16, arity as u16);
        let (rname, rdesc) = (name.clone(), descriptor.clone());
        let resolver = move |cp: u16| -> Option<(String, String, String)> {
            if cp == 2 {
                Some(("Corpus".into(), rname.clone(), rdesc.clone()))
            } else {
                None
            }
        };
        compile_with_dispatch(&cm, &helpers, &resolver).unwrap_or_else(|| {
            panic!("{arity}-arg self-recursive method must compile by whichever route fits")
        });
    }
}

#[test]
fn selfrec_long_direct_call_executes_correctly() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // fib44-fix follow-up: with CRATONVM_JIT_IR_SELFREC_DIRECT on, a self-recursive
    // long method stays on the IR pipeline but its recursive call is a DIRECT call
    // to its own entry (not jit_invoke_dispatch). Verify it (a) takes the IR
    // backend and (b) recurses correctly end-to-end (the real codegen arbiter).
    //
    // Reset guard: the override is thread-local and the test harness reuses
    // threads, so a leak (even on panic) would flip the flag for a later test on
    // this thread (e.g. `selfrec_long_return_bails_to_singlepass`). Drop restores.
    struct ResetOverride;
    impl Drop for ResetOverride {
        fn drop(&mut self) {
            cratonvm_jit::__set_selfrec_direct_override(None);
        }
    }
    let _guard = ResetOverride;
    cratonvm_jit::__set_selfrec_direct_override(Some(true));

    let helpers = dummy_helpers();
    // static long fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
    let code = vec![
        0x1a, 0x04, 0xa3, 0x00, 0x06, // iload_0; iconst_1; if_icmpgt 8
        0x1a, 0x85, 0xad, // iload_0; i2l; lreturn
        0x1a, 0x04, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_1; isub; invokestatic #2 (self)
        0x1a, 0x05, 0x64, 0xb8, 0x00, 0x02, // iload_0; iconst_2; isub; invokestatic #2 (self)
        0x61, 0xad, // ladd; lreturn
    ];
    let cm = cached("fib", "(I)J", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "fib".into(), "(I)J".into()))
        } else {
            None
        }
    };
    let compiled = compile_with_dispatch(&cm, &helpers, &resolver)
        .expect("self-recursive long compiles on the IR direct-call path");
    assert!(
        compiled.used_ir_backend,
        "with the flag on, a self-recursive long method stays on the IR backend (direct self-call)"
    );

    // `vm_ptr` is only forwarded to recursive calls, never dereferenced by fib —
    // a dummy buffer pointer suffices.
    let dummy_vm = [0u8; 64];
    for (n, expect) in [
        (0i64, 0i64),
        (1, 1),
        (2, 1),
        (5, 5),
        (10, 55),
        (20, 6765),
        (30, 832040),
    ] {
        let r = unsafe { compiled.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("call fib({n}): {e:?}"));
        assert_eq!(r, expect, "fib({n}) via IR direct self-call");
    }
}

// ---------------------------------------------------------------------------
// IR direct-call lowering (perf(jit): direct cross-method calls in the IR)
//
// The IR path historically routed EVERY invoke through `jit_invoke_dispatch`,
// which is why `ir_compatible` capped `invoke_ops` at 5: an "optimizing"
// recompile of a call-heavy method was a net regression versus the single-pass
// body, which has always bound a statically-resolved callee with a raw `CALL`.
// The lowering below closes that gap for `invokestatic` / non-`<init>`
// `invokespecial`. These tests supply a `callee_compiler` whose returned "entry"
// is a real `extern "C"` stub, so the generated code actually calls it — proving
// the entry ABI (vm_ptr placement, argument register order, result in RAX) and
// the exception sentinel end to end.
//
// `invoke_dispatch` is deliberately wired to a DIFFERENT observable function in
// each test: if the site fell back to helper dispatch the result would differ, so
// each assertion is also a proof that the direct edge was taken.
// ---------------------------------------------------------------------------

/// A "compiled callee" that takes the hidden VM context pointer (needs_context
/// == true): `(vm_ptr, a, b) -> a * 1000 + b`.
unsafe extern "C" fn direct_callee_ctx(_vm: i64, a: i64, b: i64) -> i64 {
    (a as i32 as i64) * 1000 + (b as i32 as i64)
}

/// A "compiled callee" that does NOT take a context pointer (needs_context ==
/// false), so its FIRST register argument is the first Java argument:
/// `(a, b) -> a * 1000 + b + 7`.
unsafe extern "C" fn direct_callee_noctx(a: i64, b: i64) -> i64 {
    (a as i32 as i64) * 1000 + (b as i32 as i64) + 7
}

/// A "compiled callee" that threw: returns the `i64::MIN` deopt sentinel.
unsafe extern "C" fn direct_callee_throws(_vm: i64, _a: i64, _b: i64) -> i64 {
    i64::MIN
}

/// `compile_with_dispatch` plus a `callee_compiler` — the resolver the IR
/// eligibility loop consults to eagerly compile a statically-bound callee and
/// obtain `(entry, needs_context)` for the direct `CALL`.
fn compile_with_direct_callee(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
    callee_compiler: &dyn Fn(&str, &str, &str) -> Option<(usize, bool)>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        Some(callee_compiler),
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        true,  // optimize (C2 / IR pipeline)
        true,  // ir_emit_calls
        true,  // ir_emit_special_calls
        true,  // ir_emit_long
        true,  // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,
    )
}

#[test]
fn ir_direct_call_static_with_context_executes_correctly() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // static int f(int a, int b) { return g(a, b) * 2; }
    //   iload_0; iload_1; invokestatic #2; iconst_2; imul; ireturn
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0x05, 0x68, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_ctx as *const () as usize;
    let callee_compiler = |c: &str, m: &str, d: &str| -> Option<(usize, bool)> {
        if (c, m, d) == ("pkg/Helper", "g", "(II)I") {
            Some((entry, true))
        } else {
            None
        }
    };
    let ir = compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler)
        .expect("IR compile with a direct static call");
    assert!(ir.used_ir_backend, "must stay on the IR backend");
    assert!(
        ir._direct_callee_entries.contains(&entry),
        "the baked callee entry must be recorded for keep-alive + invalidation"
    );
    let dummy_vm = [0u8; 64];
    for (a, b) in [(3i64, 4i64), (0, 0), (-1, 5), (12, -7)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call f({a},{b}): {e:?}"));
        let expected = ((a as i32).wrapping_mul(1000).wrapping_add(b as i32)).wrapping_mul(2);
        // Compared as `i32`, matching the `check` helper: the IR keeps an int
        // result in its frame slot without guaranteeing upper-32 sign extension
        // (`Op::Mul` Int is a 32-bit `IMUL EAX, ECX`), and the VM narrows an `I`
        // return to i32. Pre-existing convention, unrelated to call lowering.
        assert_eq!(
            r as i32, expected,
            "f({a},{b}) must go through the DIRECT callee (helper dispatch would              have produced the positional_dispatch value instead)"
        );
    }
}

#[test]
fn ir_direct_call_static_without_context_executes_correctly() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // Same shape, but the callee reports `needs_context == false`, so the first
    // Java argument occupies abi[0] rather than abi[1]. A mis-shifted marshalling
    // would pass the VM pointer as `a`.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_noctx as *const () as usize;
    let callee_compiler = |c: &str, m: &str, d: &str| -> Option<(usize, bool)> {
        if (c, m, d) == ("pkg/Helper", "g", "(II)I") {
            Some((entry, false))
        } else {
            None
        }
    };
    let ir = compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler)
        .expect("IR compile with a context-free direct callee");
    let dummy_vm = [0u8; 64];
    for (a, b) in [(3i64, 4i64), (-2, 9), (100, 200)] {
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[a, b]) }
            .unwrap_or_else(|e| panic!("call f({a},{b}): {e:?}"));
        let expected = (a as i32)
            .wrapping_mul(1000)
            .wrapping_add(b as i32)
            .wrapping_add(7);
        assert_eq!(
            r as i32, expected,
            "context-free direct call for f({a},{b})"
        );
    }
}

#[test]
fn ir_direct_call_exception_sentinel_bails() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    // A directly-called callee that threw returns `i64::MIN`. The caller must
    // detect the sentinel and bail (returning it unchanged so the VM takes the
    // pending exception) instead of using it as the call's result — exactly the
    // protocol the helper-dispatch path follows.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return g(a, b) * 2; } — the `* 2` would be
    // visible in the result if the bail did not happen.
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0x05, 0x68, 0xac];
    let cm = cached("f", "(II)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_throws as *const () as usize;
    let callee_compiler =
        |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { Some((entry, true)) };
    let ir =
        compile_with_direct_callee(&cm, &helpers, &resolver, &callee_compiler).expect("compile");
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[1, 2]) }.expect("call");
    assert_eq!(
        r,
        i64::MIN,
        "a throwing direct callee must bail and propagate the sentinel"
    );
}

#[test]
fn ir_direct_call_declines_self_recursion_and_unknown_callees() {
    // Two negative controls for the eligibility gate:
    //  1. A SELF-recursive static call must NOT become a cross-method direct call
    //     (it has its own `invoke_kind == 4` path, whose stack guard is what keeps
    //     runaway recursion a catchable StackOverflowError).
    //  2. A callee the `callee_compiler` cannot compile must keep helper dispatch.
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = positional_dispatch as *const () as usize;
    // static int f(int a, int b) { return f(a, b); }  (self-recursive)
    let code = vec![0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let cm = cached("f", "(II)I", code.clone(), 2, 2);
    let self_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("Corpus".into(), "f".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let entry = direct_callee_ctx as *const () as usize;
    let any_callee =
        |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { Some((entry, true)) };
    let ir = compile_with_direct_callee(&cm, &helpers, &self_resolver, &any_callee)
        .expect("self-recursive compile");
    assert!(
        !ir._direct_callee_entries.contains(&entry),
        "a self-recursive site must never be bound as a CROSS-method direct call"
    );

    // (2) unknown callee → no direct entry recorded, dispatch retained.
    let cm2 = cached("f", "(II)I", code, 2, 2);
    let cross_resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Helper".into(), "g".into(), "(II)I".into()))
        } else {
            None
        }
    };
    let no_callee = |_c: &str, _m: &str, _d: &str| -> Option<(usize, bool)> { None };
    let ir2 = compile_with_direct_callee(&cm2, &helpers, &cross_resolver, &no_callee)
        .expect("compile with an uncompilable callee");
    assert!(
        ir2._direct_callee_entries.is_empty(),
        "no direct entry may be recorded when the callee cannot be compiled"
    );
    let dummy_vm = [0u8; 64];
    let r = unsafe { ir2.try_call_with_context(dummy_vm.as_ptr() as i64, &[3, 4]) }.expect("call");
    assert_eq!(
        r as i32,
        host_positional(&[3, 4]) as i32,
        "an uncompilable callee must still reach the dispatch helper"
    );
}

// ── Exception tables on the optimizing tier (STUB-S8 fix) ────────────────
//
// The optimizing pipeline used to refuse EVERY method declaring a `try`/`catch`
// (`cached.exception_table.is_empty()` in `try_compile_inner`), because the IR
// builder walked handler bytecode with stale abstract state. The builder now
// skips handler bodies — they are unreachable in a compiled frame — so the
// table alone no longer disqualifies a method. What still does is
// `precise_exception_frames` (RBC.6): a handler that reads a NON-parameter
// local needs the reason-9 precise-frame handoff, which only the single-pass
// backend emits.
//
// These two tests pin both sides of that line. They are the regression guard
// for the ~7x that the blanket exclusion cost every try/catch method.

/// `cached()` with a one-entry exception table covering `start..end` with its
/// handler at `handler`.
fn cached_with_handler(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    start: u16,
    end: u16,
    handler: u16,
) -> CachedBytecodeMethod {
    use cratonvm_reader::attribute::ExceptionTableEntry;
    let mut cm = cached(name, descriptor, code, max_locals, num_params);
    cm.exception_table = Arc::from(
        vec![ExceptionTableEntry {
            start_pc: start,
            end_pc: end,
            handler_pc: handler,
            catch_type: 0,
        }]
        .as_slice(),
    );
    cm
}

/// `static int f(int n) { int r = n + 1; try { r = r * 2; } catch (X e) { r =
/// -n; } return r + 3; }` — the handler reads only the parameter `n`, so RBC.6
/// does not fire and the method belongs on the optimizing tier.
fn try_catch_code(handler_reads: u8) -> Vec<u8> {
    vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x60, // 2: iadd
        0x3c, // 3: istore_1        r = n + 1
        0x1b, // 4: iload_1     ┐ protected range [4, 8)
        0x05, // 5: iconst_2    │
        0x68, // 6: imul        │
        0x3c, // 7: istore_1    ┘  r = r * 2
        0xa7,
        0x00,
        0x08,          // 8: goto +8 -> 16
        0x4d,          // 11: astore_2       HANDLER: store the exception
        handler_reads, // 12: iload_0 (param) or iload_1 (non-param local)
        0x74,          // 13: ineg
        0x3c,          // 14: istore_1
        0x00,          // 15: nop
        0x1b,          // 16: iload_1        join
        0x06,          // 17: iconst_3
        0x60,          // 18: iadd
        0xac,          // 19: ireturn        return r + 3
    ]
}

#[test]
fn try_catch_param_only_handler_uses_ir() {
    // This test exercises the optimizing IR pipeline, which is gated off
    // whenever the young generation can relocate. Pin the policy so the
    // test covers IR lowering regardless of DEFAULT_MOVING_YOUNG.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let helpers = dummy_helpers();
    // 0x1a = iload_0: the handler reads only the incoming parameter.
    let cm = cached_with_handler("f", "(I)I", try_catch_code(0x1a), 3, 1, 4, 8, 11);
    let ir = compile_opt(&cm, &helpers, true)
        .expect("a try/catch method must still compile on the optimizing tier");
    assert!(
        ir.used_ir_backend,
        "a method whose handler reads only parameters must reach the optimizing \
         IR backend — the blanket `exception_table.is_empty()` exclusion cost \
         ~7x on every try/catch method in every workload"
    );

    // …and it must compute the same thing the single-pass backend does. The
    // handler is unreachable here (nothing in the range throws), which is
    // precisely the point: the builder skips it, and the reachable code either
    // side of it must be unaffected.
    let sp = compile_opt(&cm, &helpers, false).expect("single-pass compiles");
    for n in [0i64, 1, 7, -3, 1000] {
        let r_ir = unsafe { ir.try_call(&[n]) }.expect("IR call");
        let r_sp = unsafe { sp.try_call(&[n]) }.expect("single-pass call");
        let expected = ((n as i32) + 1) * 2 + 3;
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "IR vs single-pass diverge for n={n}"
        );
        assert_eq!(r_ir as i32, expected, "wrong result for n={n}");
    }
}

#[test]
fn try_catch_nonparam_handler_local_stays_single_pass() {
    let helpers = dummy_helpers();
    // 0x1b = iload_1: the handler reads a local that is NOT a parameter, so
    // RBC.6 fires and the compile needs precise exceptional frames.
    let cm = cached_with_handler("g", "(I)I", try_catch_code(0x1b), 3, 1, 4, 8, 11);
    // Whatever else happens, it must not reach the optimizing backend. Which of
    // the two permitted outcomes occurs depends on
    // `CRATONVM_JIT_PRECISE_HANDLER_FRAMES` (opt-in, default OFF): with the flag
    // off RBC.6 refuses the compile outright, and with it on the method is
    // compiled by the single-pass backend with reason-9 frames. Asserting only
    // the shared invariant keeps the test honest under both settings.
    match compile_opt(&cm, &helpers, true) {
        None => { /* RBC.6 refused it — the default, flag-off behaviour. */ }
        Some(compiled) => assert!(
            !compiled.used_ir_backend,
            "a handler reading a non-parameter local requires the reason-9 \
             precise frame the IR lowerer cannot publish; it must stay on the \
             single-pass backend or the handler would observe null/0"
        ),
    }
}

// ── COV-02: integral / reference array element access, arraylength, dup_x1 ──
//
// `docs/known-issues/c2/archive/cov-02-array-element-access.md`. Before this lane
// `IrBuilder::build` had arms for `faload`/`daload`/`fastore`/`dastore` and for
// no integral or reference array access at all — the arms an FP kernel needs,
// not the arms Java uses. These cases are the differential the doc asks for:
// same method, both backends, same answer, plus the host anchor.
//
// The corpus is deliberately built from SYNTHETIC array buffers (the same trick
// `make_f32_array` uses for the FP arms) rather than real heap objects: the
// element layout the two backends emit is `HEADER_SIZE + index * width` with
// the length at `ARRAY_LENGTH_OFFSET`, and that is all either one reads.

/// Build a synthetic primitive array: `length` at `ARRAY_LENGTH_OFFSET`, then
/// `elems.len()` elements of `width` bytes packed from `HEADER_SIZE`, each
/// element's low `width` bytes taken from the corresponding `i64` (little
/// endian — the same truncation the store opcodes perform).
fn make_prim_array(width: usize, elems: &[i64]) -> Vec<u8> {
    let mut buf = vec![0u8; HEADER_SIZE + elems.len() * width];
    buf[ARRAY_LENGTH_OFFSET..ARRAY_LENGTH_OFFSET + 4]
        .copy_from_slice(&(elems.len() as u32).to_le_bytes());
    for (i, &v) in elems.iter().enumerate() {
        let off = HEADER_SIZE + i * width;
        buf[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
    buf
}

/// Compile `code` both ways and assert IR == single-pass == `want` for each
/// `(args, want)`, with the IR side actually taking the IR backend.
///
/// The `used_ir_backend` assertion is the anti-vacuity check: without it a
/// builder arm that silently refuses would compare single-pass against
/// single-pass and pass. That is exactly how the getfield corpus in this file
/// was vacuous before the compact-layout refusal was moved.
fn check_array(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    cases: &[(Vec<i64>, i64)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    assert!(
        ir.used_ir_backend,
        "{name}: the IR pipeline fell back to single-pass, so this case would \
         compare single-pass with itself"
    );
    let sp = compile_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for (args, want) in cases {
        // SAFETY: both bodies are JIT-compiled array access over a live,
        // correctly laid-out synthetic array whose address is arg0, with an
        // in-bounds index. No runtime helper is reachable on the non-faulting
        // path.
        let r_sp = unsafe { sp.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: single-pass call {args:?}: {e:?}"));
        let r_ir = unsafe { ir.try_call(args) }
            .unwrap_or_else(|e| panic!("{name}: IR call {args:?}: {e:?}"));
        assert_eq!(
            r_ir, r_sp,
            "{name}: IR vs single-pass DIVERGE for {args:?}: IR={r_ir}, single-pass={r_sp}"
        );
        assert_eq!(
            r_ir, *want,
            "{name}: both backends agree but disagree with host for {args:?}"
        );
    }
}

#[test]
fn ir_vs_singlepass_iaload() {
    // static int get(int[] a, int i) { return a[i]; }
    //   aload_0; iload_1; iaload; ireturn
    let elems: Vec<i64> = vec![7, -1, i32::MAX as i64, i32::MIN as i64, 0];
    let arr = make_prim_array(4, &elems);
    let base = arr.as_ptr() as i64;
    let cases: Vec<(Vec<i64>, i64)> = elems
        .iter()
        .enumerate()
        .map(|(i, &v)| (vec![base, i as i64], v))
        .collect();
    check_array(
        "iaload",
        "([II)I",
        vec![0x2a, 0x1b, 0x2e, 0xac],
        2,
        2,
        &cases,
    );
}

#[test]
fn ir_vs_singlepass_baload_sign_extends() {
    // static int get(byte[] a, int i) { return a[i]; }  — JVMS: SIGN-extends.
    // 0xFF must read back as -1, not 255. This is the one place an
    // "int[] and byte[] are the same shape" shortcut produces wrong code.
    let raw: Vec<i64> = vec![0x00, 0x01, 0x7f, 0x80, 0xff];
    let want: Vec<i64> = vec![0, 1, 127, -128, -1];
    let arr = make_prim_array(1, &raw);
    let base = arr.as_ptr() as i64;
    let cases: Vec<(Vec<i64>, i64)> = want
        .iter()
        .enumerate()
        .map(|(i, &v)| (vec![base, i as i64], v))
        .collect();
    check_array(
        "baload",
        "([BI)I",
        vec![0x2a, 0x1b, 0x33, 0xac],
        2,
        2,
        &cases,
    );
}

#[test]
fn ir_vs_singlepass_caload_zero_extends() {
    // static int get(char[] a, int i) { return a[i]; }  — JVMS: ZERO-extends.
    // 0xFFFF must read back as 65535, the mirror image of `baload` above.
    let raw: Vec<i64> = vec![0x0000, 0x0041, 0x7fff, 0x8000, 0xffff];
    let want: Vec<i64> = vec![0, 65, 32767, 32768, 65535];
    let arr = make_prim_array(2, &raw);
    let base = arr.as_ptr() as i64;
    let cases: Vec<(Vec<i64>, i64)> = want
        .iter()
        .enumerate()
        .map(|(i, &v)| (vec![base, i as i64], v))
        .collect();
    check_array(
        "caload",
        "([CI)I",
        vec![0x2a, 0x1b, 0x34, 0xac],
        2,
        2,
        &cases,
    );
}

#[test]
fn ir_vs_singlepass_saload_sign_extends() {
    // static int get(short[] a, int i) { return a[i]; }  — JVMS: SIGN-extends,
    // so 0xFFFF is -1 here and 65535 for `caload` above. Same width, opposite
    // rule: the pair is what makes a width-only lowering visibly wrong.
    let raw: Vec<i64> = vec![0x0000, 0x0041, 0x7fff, 0x8000, 0xffff];
    let want: Vec<i64> = vec![0, 65, 32767, -32768, -1];
    let arr = make_prim_array(2, &raw);
    let base = arr.as_ptr() as i64;
    let cases: Vec<(Vec<i64>, i64)> = want
        .iter()
        .enumerate()
        .map(|(i, &v)| (vec![base, i as i64], v))
        .collect();
    check_array(
        "saload",
        "([SI)I",
        vec![0x2a, 0x1b, 0x35, 0xac],
        2,
        2,
        &cases,
    );
}

#[test]
fn ir_vs_singlepass_aaload() {
    // static Object get(Object[] a, int i) { return a[i]; }
    //   aload_0; iload_1; aaload; areturn
    //
    // The elements are plausible 8-aligned pointer-shaped words; both backends
    // return the raw pointer. The IR node is `IrType::Ref`, which is what makes
    // `emit_safepoint_map` publish the result slot as a relocatable root — the
    // moving-GC half of that claim is an E2E fixture, not something a unit test
    // over a synthetic buffer can observe.
    let elems: Vec<i64> = vec![0, 0x1000, 0x7fff_fff8, 0x2000_0000, 0x8];
    let arr = make_prim_array(8, &elems);
    let base = arr.as_ptr() as i64;
    let cases: Vec<(Vec<i64>, i64)> = elems
        .iter()
        .enumerate()
        .map(|(i, &v)| (vec![base, i as i64], v))
        .collect();
    check_array(
        "aaload",
        "([Ljava/lang/Object;I)Ljava/lang/Object;",
        vec![0x2a, 0x1b, 0x32, 0xb0],
        2,
        2,
        &cases,
    );
}

#[test]
fn ir_vs_singlepass_arraylength() {
    // static int len(int[] a) { return a.length; }
    //   aload_0; arraylength; ireturn
    //
    // 43 of this lane's 77 measured events, and the cheapest: one 32-bit load
    // at a fixed header offset behind a null check.
    let a0 = make_prim_array(4, &[]);
    let a3 = make_prim_array(4, &[1, 2, 3]);
    let a9 = make_prim_array(4, &[0; 9]);
    check_array(
        "arraylength",
        "([I)I",
        vec![0x2a, 0xbe, 0xac],
        1,
        1,
        &[
            (vec![a0.as_ptr() as i64], 0),
            (vec![a3.as_ptr() as i64], 3),
            (vec![a9.as_ptr() as i64], 9),
        ],
    );
}

#[test]
fn ir_vs_singlepass_arraylength_of_a_loaded_element() {
    // static int f(int[] a, int i) { return a[i] + a.length; }
    //   aload_0; iload_1; iaload; aload_0; arraylength; iadd; ireturn
    // Two array ops in one method: the element read is in the memory-token
    // chain and the length read is not, so this is where a scheduler that
    // reordered them would show up.
    let arr = make_prim_array(4, &[10, 20, 30, 40]);
    let base = arr.as_ptr() as i64;
    check_array(
        "elem_plus_len",
        "([II)I",
        vec![0x2a, 0x1b, 0x2e, 0x2a, 0xbe, 0x60, 0xac],
        2,
        2,
        &[
            (vec![base, 0], 14),
            (vec![base, 1], 24),
            (vec![base, 3], 44),
        ],
    );
}

#[test]
fn ir_vs_singlepass_dup_x1() {
    // static int f(int a, int b) { … }
    //   iload_0; iload_1; dup_x1; isub; isub; ireturn
    //   stack after dup_x1 (bottom→top): b, a, b
    //   isub → b, (a - b)
    //   isub → b - (a - b)
    let code = vec![0x1a, 0x1b, 0x5a, 0x64, 0x64, 0xac];
    // Anti-vacuity. `check` alone would pass with the arm MISSING: a builder
    // refusal falls back to single-pass, and the differential would then be
    // comparing single-pass with itself. The edit that trips this assertion is
    // deleting the `0x5a` arm from `IrBuilder::build`.
    assert!(
        compile_opt(
            &cached("dup_x1", "(II)I", code.clone(), 2, 2),
            &dummy_helpers(),
            true,
        )
        .expect("dup_x1 must compile through the IR pipeline")
        .used_ir_backend,
        "dup_x1 must reach the IR backend, or this differential is vacuous"
    );
    check(
        "dup_x1",
        "(II)I",
        code,
        2,
        2,
        &[
            (vec![10, 3], 3 - (10 - 3)),
            (vec![0, 0], 0),
            (vec![-5, 7], 7 - (-5 - 7)),
            (vec![100, 1], 1 - (100 - 1)),
        ],
    );
}

/// Compile an array-STORE method both ways, run each backend against its own
/// fresh array, and compare the resulting element bytes.
fn check_array_store(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    width: usize,
    len: usize,
    cases: &[(usize, i64)],
) {
    let helpers = dummy_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    assert!(
        ir.used_ir_backend,
        "{name}: the IR pipeline fell back to single-pass — vacuous comparison"
    );
    let sp = compile_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));
    for &(i, v) in cases {
        let read_back = |m: &CompiledMethod| -> Vec<u8> {
            let mut arr = make_prim_array(width, &vec![0i64; len]);
            let args = [arr.as_mut_ptr() as i64, i as i64, v];
            // SAFETY: JIT-compiled array store over a live synthetic array,
            // in-bounds index, value in a GPR. No helper reached.
            unsafe { m.try_call(&args) }.unwrap();
            let off = HEADER_SIZE + i * width;
            arr[off..off + width].to_vec()
        };
        let got_sp = read_back(&sp);
        let got_ir = read_back(&ir);
        assert_eq!(got_ir, got_sp, "{name}: IR vs single-pass DIVERGE at {i}");
        // Host anchor: the element must hold the value's low `width` bytes.
        assert_eq!(
            got_ir,
            v.to_le_bytes()[..width].to_vec(),
            "{name}: both backends agree but disagree with host at {i}"
        );
    }
}

#[test]
fn ir_vs_singlepass_iastore() {
    // static void set(int[] a, int i, int v) { a[i] = v; }
    //   aload_0; iload_1; iload_2; iastore; return
    check_array_store(
        "iastore",
        "([III)V",
        vec![0x2a, 0x1b, 0x1c, 0x4f, 0xb1],
        3,
        3,
        4,
        5,
        &[(0, 9), (2, -1), (4, i32::MIN as i64)],
    );
}

#[test]
fn ir_vs_singlepass_bastore_truncates() {
    // static void set(byte[] a, int i, int v) { a[i] = (byte)v; }
    // The store TRUNCATES to the element width — 0x1FF must land as 0xFF.
    check_array_store(
        "bastore",
        "([BII)V",
        vec![0x2a, 0x1b, 0x1c, 0x54, 0xb1],
        3,
        3,
        1,
        5,
        &[(0, 0x7f), (2, 0x1ff), (4, -1)],
    );
}

#[test]
fn ir_vs_singlepass_castore_and_sastore_truncate() {
    // `castore` and `sastore` share one 16-bit store in both backends.
    for (name, op) in [("castore", 0x55u8), ("sastore", 0x56u8)] {
        check_array_store(
            name,
            "([CII)V",
            vec![0x2a, 0x1b, 0x1c, op, 0xb1],
            3,
            3,
            2,
            5,
            &[(0, 0x41), (2, 0x1_ffff), (4, -1)],
        );
    }
}

// ── The fault paths: null array, negative index, index == length ────────────
//
// The doc's third requirement, and the one that cannot be answered by comparing
// return values alone: the two backends signal a fault through DIFFERENT
// channels, so "the same exception with the same bci" has to be read out of
// each channel separately.
//
//  * single-pass, out of bounds: an out-of-line stub calls
//    `helpers.throw_aioobe(index, length, array_ptr, bci)` and returns the
//    `i64::MIN` sentinel through the epilogue. The bci is an ARGUMENT.
//  * single-pass, null array: an out-of-line stub calls
//    `helpers.jit_npe_with_action(action)` and returns the same sentinel. It
//    passes an element-type action code and **no bci at all**.
//  * IR, either fault: an inline guard deopts. `ir_deopt_entry` stashes a
//    `ReconstructedFrame` (whose `bci` is the trapping bytecode) in a
//    thread-local and returns the sentinel; the interpreter then re-executes
//    the opcode and throws the real exception with the method's own handler
//    semantics.
//
// So `bci` is directly comparable for AIOOBE and is **not published by the
// single-pass NPE path** — a premise delta against the doc, recorded here in
// the test rather than in prose. What both paths do share, and what these
// cases assert, is: the sentinel return, the fault CLASS, and (for AIOOBE) the
// bci and the reported index/length.

use std::cell::Cell;

thread_local! {
    /// `(index, length, bci)` from the last single-pass AIOOBE stub call.
    static SP_AIOOBE: Cell<Option<(i64, i64, i64)>> = const { Cell::new(None) };
    /// The action code from the last single-pass NPE stub call.
    static SP_NPE: Cell<Option<i64>> = const { Cell::new(None) };
}

/// # Safety
/// Called only from JIT-compiled code through the helper table; reads no
/// pointer arguments.
unsafe extern "C" fn rec_throw_aioobe(index: i64, length: i64, _arr: i64, bci: i64) -> i64 {
    SP_AIOOBE.with(|c| c.set(Some((index, length, bci))));
    i64::MIN
}

/// # Safety
/// Same contract as [`rec_throw_aioobe`].
unsafe extern "C" fn rec_npe_with_action(action: i64) -> i64 {
    SP_NPE.with(|c| c.set(Some(action)));
    i64::MIN
}

/// `dummy_helpers()` with the two fault stubs replaced by recorders instead of
/// the panicking "unwired helper" stub, so the fault paths are reachable.
fn fault_helpers() -> JitRuntimeHelpers {
    let mut h = dummy_helpers();
    h.throw_aioobe = rec_throw_aioobe as *const () as usize;
    h.jit_npe_with_action = rec_npe_with_action as *const () as usize;
    h
}

/// Both backends must take the fault path for `args`: the call returns the
/// `i64::MIN` deopt/exception sentinel, single-pass reports the fault through
/// the recorder named by `aioobe`, and the IR side stashes a reconstructed
/// frame at `bci`.
fn check_fault(
    name: &str,
    descriptor: &str,
    code: Vec<u8>,
    max_locals: u16,
    num_params: u16,
    args: &[i64],
    bci: u32,
    aioobe: Option<(i64, i64)>,
) {
    let helpers = fault_helpers();
    let cm = cached(name, descriptor, code, max_locals, num_params);
    let ir = compile_opt(&cm, &helpers, true)
        .unwrap_or_else(|| panic!("{name}: optimize=true (IR pipeline) failed to compile"));
    assert!(ir.used_ir_backend, "{name}: IR fell back to single-pass");
    let sp = compile_opt(&cm, &helpers, false)
        .unwrap_or_else(|| panic!("{name}: optimize=false (single-pass) failed to compile"));

    SP_AIOOBE.with(|c| c.set(None));
    SP_NPE.with(|c| c.set(None));
    let _ = cratonvm_jit::deopt::take_last_deopt();

    // SAFETY: a JIT-compiled array access whose guard is about to fire. The
    // guard runs BEFORE the element address is formed, so the null or
    // out-of-range argument is never dereferenced.
    let r_sp = unsafe { sp.try_call(args) }.unwrap();
    assert_eq!(
        r_sp,
        i64::MIN,
        "{name}: single-pass must return the sentinel on the fault path"
    );
    match aioobe {
        Some((want_index, want_len)) => {
            let got = SP_AIOOBE.with(|c| c.get());
            assert_eq!(
                got,
                Some((want_index, want_len, bci as i64)),
                "{name}: single-pass AIOOBE (index, length, bci)"
            );
            assert!(
                SP_NPE.with(|c| c.get()).is_none(),
                "{name}: single-pass must not also report an NPE"
            );
        }
        None => {
            assert!(
                SP_NPE.with(|c| c.get()).is_some(),
                "{name}: single-pass must report the NPE through jit_npe_with_action"
            );
            assert!(
                SP_AIOOBE.with(|c| c.get()).is_none(),
                "{name}: single-pass must not also report an AIOOBE"
            );
        }
    }

    // SAFETY: as above, for the IR body.
    let r_ir = unsafe { ir.try_call(args) }.unwrap();
    assert_eq!(
        r_ir,
        i64::MIN,
        "{name}: IR must return the deopt sentinel on the fault path"
    );
    let frame = cratonvm_jit::deopt::take_last_deopt()
        .unwrap_or_else(|| panic!("{name}: the IR guard did not deopt"));
    assert_eq!(
        frame.bci, bci,
        "{name}: the IR deopt must name the trapping bytecode, so the \
         interpreter re-executes THIS opcode and throws with this method's \
         handler semantics"
    );
}

#[test]
fn ir_vs_singlepass_iaload_faults() {
    // aload_0; iload_1; iaload(bci 2); ireturn
    let code = vec![0x2a, 0x1b, 0x2e, 0xac];
    let arr = make_prim_array(4, &[1, 2, 3, 4]);
    let base = arr.as_ptr() as i64;
    // index == length: the classic off-by-one.
    check_fault(
        "iaload_at_length",
        "([II)I",
        code.clone(),
        2,
        2,
        &[base, 4],
        2,
        Some((4, 4)),
    );
    // A negative index. Both backends reject it with the SAME unsigned compare
    // (a negative index has a huge unsigned value), which is why one CMP
    // covers both ends of the range.
    check_fault(
        "iaload_negative",
        "([II)I",
        code.clone(),
        2,
        2,
        &[base, -1],
        2,
        Some((-1, 4)),
    );
    // A null array.
    check_fault("iaload_null", "([II)I", code, 2, 2, &[0, 0], 2, None);
}

#[test]
fn ir_vs_singlepass_iastore_faults() {
    // aload_0; iload_1; iload_2; iastore(bci 3); return
    let code = vec![0x2a, 0x1b, 0x1c, 0x4f, 0xb1];
    let arr = make_prim_array(4, &[0; 3]);
    let base = arr.as_ptr() as i64;
    check_fault(
        "iastore_at_length",
        "([III)V",
        code.clone(),
        3,
        3,
        &[base, 3, 42],
        3,
        Some((3, 3)),
    );
    check_fault(
        "iastore_negative",
        "([III)V",
        code.clone(),
        3,
        3,
        &[base, -7, 42],
        3,
        Some((-7, 3)),
    );
    check_fault("iastore_null", "([III)V", code, 3, 3, &[0, 0, 42], 3, None);
}

#[test]
fn ir_vs_singlepass_arraylength_null_faults() {
    // aload_0; arraylength(bci 1); ireturn
    //
    // `arraylength` has no index and therefore no bounds check — a null
    // receiver is its ONLY fault, and it is the one this VM shipped without:
    // the raw `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` dereferenced low memory
    // and SIGSEGV'd (the crash handler re-raises rather than throwing).
    check_fault(
        "arraylength_null",
        "([I)I",
        vec![0x2a, 0xbe, 0xac],
        1,
        1,
        &[0],
        1,
        None,
    );
}

// ── cov-01 increments 2-4: the constant-pool constants that are calls ────
//
// `ldc <String>`, `ldc <Class>` and `getstatic` are constants in the bytecode's
// sense and runtime calls in the machine's: each names a constant-pool SITE
// whose value is materialised on every execution, because an `ObjectRef` baked
// at compile time is stale the moment a relocating collector moves it, and
// because `<clinit>` is a side effect the constant owes on first touch.
//
// `getstatic` alone was 92 of the 273 opcode-gap events measured on 2026-08-03
// (`ir-coverage-survey-20260803.md`) — the largest single
// opcode in the survey.

/// `jit_ldc_string_cp(vm, holder_class_id, cp_idx)` stand-in. Returns a value
/// derived from BOTH baked immediates, so a site that baked the wrong class id
/// or the wrong index is distinguishable from one that baked the right ones —
/// a stub returning a constant would pass either way.
///
/// The predecessor took `(bytes, len)` and derived its answer from the
/// literal's first byte and length. Both shapes test the same property: that
/// the two backends bake the same operands and call the same helper with them.
unsafe extern "C" fn fake_ldc_string_cp(_vm: i64, holder: i64, cp_idx: i64) -> i64 {
    holder * 100_000 + cp_idx + 1
}

/// `jit_ldc_class_cp(vm, holder_class_id, cp_idx)` stand-in, likewise derived
/// from both baked immediates. Never 0 for the sites these tests use, so the
/// arm's zero-means-pending-exception path is not taken.
unsafe extern "C" fn fake_ldc_class_cp(_vm: i64, holder: i64, cp_idx: i64) -> i64 {
    holder * 1000 + cp_idx
}

/// `jit_getstatic(vm, class_id, field_index)` stand-in. The value is one no
/// direct load of the test statics block could produce, so which of the two
/// `getstatic` routes ran is observable from the RESULT rather than from a
/// disassembly.
unsafe extern "C" fn marker_getstatic(_vm: i64, class_id: i64, field_index: i64) -> i64 {
    424_242 + class_id + field_index
}

/// Call a body with a zeroed buffer as the hidden VM context. Every helper above
/// ignores it; the artifacts are `needs_context` because their helpers take it
/// as arg0.
fn call_with_dummy_context(m: &CompiledMethod, args: &[i64]) -> i64 {
    let dummy_vm = [0u8; 64];
    // SAFETY: the body was JIT-compiled from valid bytecode into executable
    // memory; every helper it can reach is one of the stubs above, none of which
    // dereferences the context pointer.
    unsafe {
        if m.needs_context() {
            m.try_call_with_context(dummy_vm.as_ptr() as i64, args)
        } else {
            m.try_call(args)
        }
    }
    .expect("jit call")
}

#[test]
fn ir_vs_singlepass_string_ldc() {
    // cov-01 inc 2. Object f() { return "hello"; }  —  ldc #1 ; areturn
    let code = vec![0x12, 0x01, 0xb0];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::String {
                holder_class_id: 0,
                cp_idx: 1,
            }),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.ldc_string_cp = fake_ldc_string_cp as *const () as usize;
    let cm = cached("sldcv", "()Ljava/lang/Object;", code, 1, 0);
    let ir = compile_ldc(&cm, &helpers, true, false, &ldc).expect("IR String ldc");
    let sp = compile_ldc(&cm, &helpers, false, false, &ldc).expect("single-pass String ldc");
    assert!(
        ir.used_ir_backend,
        "cov-01: a String `ldc` must reach the optimizing backend"
    );
    // holder 0, cp#1. Both backends must have baked the same (class id, CP
    // index) pair and called the same helper with it. The `+ 1` in the stub
    // keeps the answer non-zero: a `0` return is the site's
    // pending-exception convention and would take the exception path instead
    // of being pushed.
    let expected = 0 * 100_000 + 1 + 1;
    assert_eq!(call_with_dummy_context(&ir, &[]), expected, "IR String ldc");
    assert_eq!(
        call_with_dummy_context(&sp, &[]),
        expected,
        "single-pass String ldc"
    );
}

#[test]
fn ir_vs_singlepass_class_ldc() {
    // cov-01 inc 3. Object f() { return Foo.class; }  —  ldc #1 ; areturn
    let code = vec![0x12, 0x01, 0xb0];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::ClassMirror {
                holder_class_id: 7,
                cp_idx: 1,
            }),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.ldc_class_cp = fake_ldc_class_cp as *const () as usize;
    let cm = cached("cldcv", "()Ljava/lang/Object;", code, 1, 0);
    let ir = compile_ldc(&cm, &helpers, true, false, &ldc).expect("IR Class ldc");
    let sp = compile_ldc(&cm, &helpers, false, false, &ldc).expect("single-pass Class ldc");
    assert!(
        ir.used_ir_backend,
        "cov-01: a Class `ldc` must reach the optimizing backend"
    );
    let expected = 7 * 1000 + 1;
    assert_eq!(call_with_dummy_context(&ir, &[]), expected, "IR Class ldc");
    assert_eq!(
        call_with_dummy_context(&sp, &[]),
        expected,
        "single-pass Class ldc"
    );
}

#[test]
fn string_ldc_with_the_helper_unwired_stays_on_single_pass() {
    // The fail-closed rung for `Op::ConstString`. `helpers.ldc_string_cp` is 0
    // only in a synthetic table like this one, but `lower_inner` must refuse
    // rather than emit `CALL 0` — the same guard, and the same history, as the
    // monitor helper's.
    //
    // The edit that trips it: delete the `ldc_string_cp` row from
    // `lower_inner`'s constant-pool helper loop, or the matching guard in the
    // single-pass planner.
    //
    // The ASYMMETRY this test used to record is GONE. It was written against
    // `helpers.ldc_string`, a RequiredPtr the single-pass backend treated as
    // always wired — so the fallback body would itself have called address 0,
    // and the result could not be invoked. Since 2026-08-20 the site is served
    // by the OptionalPtr `ldc_string_cp` and the single-pass planner bails the
    // whole compile on the same condition (`ldc_string_helper_unwired`), so
    // `compile_ldc` may legitimately answer `None` here.
    let code = vec![0x12, 0x01, 0xb0];
    let ldc = |cp: u16| -> Option<cratonvm_jit::JitLdcConstant> {
        match cp {
            1 => Some(cratonvm_jit::JitLdcConstant::String {
                holder_class_id: 0,
                cp_idx: 1,
            }),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.ldc_string_cp = 0;
    let cm = cached("sldcoff", "()Ljava/lang/Object;", code, 1, 0);
    if let Some(compiled) = compile_ldc(&cm, &helpers, true, false, &ldc) {
        assert!(
            !compiled.used_ir_backend,
            "cov-01: an `ldc <String>` with `ldc_string_cp` unwired must refuse \
             the IR graph rather than emit a CALL through address zero"
        );
    }
}

/// [`compile_opt`] plus a `cp_static_field_resolver`, so the IR builder can
/// lower `getstatic`.
fn compile_getstatic(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    statics: &dyn Fn(u16) -> Option<(u32, usize, u8, bool)>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        Some(statics),
        None,
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        false,
        false,
        false,
        None,
    )
}

#[test]
fn ir_vs_singlepass_getstatic_through_the_helper() {
    // cov-01 inc 4, the HELPER route — the one every site takes when the
    // declaring class is not already initialised at compile time, which is what
    // `resolve_static_base` reports with no VM registered (this binary has
    // none, except for the single pair the direct-route test below registers).
    //
    //   int f() { return Holder.VALUE; }  —  getstatic #1 ; ireturn
    let code = vec![0xb2, 0x00, 0x01, 0xac];
    let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
        match cp {
            1 => Some((0x11u32, 3usize, b'I', false)),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.getstatic = marker_getstatic as *const () as usize;
    let cm = cached("gs", "()I", code, 1, 0);
    let ir = compile_getstatic(&cm, &helpers, true, &statics).expect("IR getstatic");
    let sp = compile_getstatic(&cm, &helpers, false, &statics).expect("single-pass getstatic");
    assert!(
        ir.used_ir_backend,
        "cov-01: a getstatic must reach the optimizing backend — it was the \
         largest single opcode gap in the survey"
    );
    let expected = 424_242 + 0x11 + 3;
    assert_eq!(
        call_with_dummy_context(&ir, &[]) as i32,
        expected,
        "IR getstatic through the helper"
    );
    assert_eq!(
        call_with_dummy_context(&sp, &[]) as i32,
        expected,
        "single-pass getstatic through the helper"
    );
    // RBC.5: compiled code reads static storage directly, so the declaring
    // class must be ensure-initialized before the body first runs. The
    // single-pass artifact has recorded this since RBC.5; an IR artifact that
    // lowers `getstatic` owes exactly the same list, and without it the DIRECT
    // route would read a block whose `<clinit>` has not run.
    assert_eq!(
        ir.static_init_classes,
        vec![0x11u32],
        "cov-01: an IR body with a getstatic must record its declaring class \
         for the compiled-entry ensure-init walk"
    );
    assert_eq!(ir.static_init_classes, sp.static_init_classes);
    assert!(
        ir.has_dispatch,
        "jit_getstatic resolves `&mut JvmThread` through the JIT_THREAD TLS to \
         run <clinit>, and the !has_dispatch fast entry never sets it"
    );
}

#[test]
fn ir_vs_singlepass_getstatic_direct_load() {
    // cov-01 inc 4, the DIRECT route — the two dependent loads that replace the
    // helper call when `resolve_static_base` accepts the site. This is the only
    // hand-written instruction encoding this lane adds, so it is driven rather
    // than inspected: the marker helper returns a value no direct load of the
    // block below could produce, which makes the ROUTE observable from the
    // result.
    //
    // Both payload widths are covered, at the two
    // `FIELD_CELL_PAYLOAD{32,64}_OFFSET` biases the two encodings use.
    //
    // The resolver answers for exactly one class id and declines everything
    // else, so it stays inert for every other test in this binary — the same
    // discipline `x64::tests::test_getstatic_inline_direct_load_and_fallback`
    // uses, and necessary for the same reason: `set_static_base_resolver`
    // latches its context for the life of the process.
    use cratonvm_types::Value;
    use std::sync::atomic::AtomicPtr;

    /// Stands in for `jit_resolve_static_base`; `ctx` IS the base-pointer cell.
    ///
    /// **One registration for this whole binary.** A second
    /// `set_static_base_resolver` with a different context does not replace this
    /// one — it POISONS the resolver, and every direct site silently reverts to
    /// the helper. (Learned the expensive way: a separate wide-static test with
    /// its own block made this one's rung 1 return the marker value.) So the
    /// wide and floating-point widths are rungs of THIS test, sharing this
    /// block, rather than a sibling with a block of its own.
    unsafe extern "C" fn test_resolver(ctx: i64, class_id: i64, field_index: i64) -> i64 {
        if class_id == 0x1CE && (1..=5).contains(&field_index) {
            ctx
        } else {
            0
        }
    }

    static CELL_ADDR: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cell_addr = *CELL_ADDR.get_or_init(|| {
        // The same two-level shape `StaticsIndex` publishes: a leaked block and
        // an `AtomicPtr` cell naming it. Slot 1 is the int-category case; slot 2
        // is read back through the 64-bit payload offset the reference encoding
        // uses, so a bias mixed up between the two arms shows up as a wrong
        // number rather than as nothing. Slots 3-5 are the wide and
        // floating-point widths.
        let block: &'static mut [Value] = Box::leak(
            vec![
                Value::Int(0),
                Value::Int(-7),
                Value::Long(0x0BAD_F00D_1234_5678),
                Value::Long(0x0123_4567_89AB_CDEF),
                Value::Double(-2.5f64),
                Value::Float(-2.5f32),
            ]
            .into_boxed_slice(),
        );
        let cell: &'static AtomicPtr<Value> =
            Box::leak(Box::new(AtomicPtr::new(block.as_mut_ptr())));
        cell as *const AtomicPtr<Value> as usize
    });
    cratonvm_jit::x64::set_static_base_resolver(test_resolver as *const () as usize, cell_addr);

    let mut helpers = dummy_helpers();
    helpers.getstatic = marker_getstatic as *const () as usize;

    // 1. int-category, slot 1 → MOVSXD of the 32-bit payload.
    {
        let code = vec![0xb2, 0x00, 0x01, 0xac];
        let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x1CEu32, 1usize, b'I', false)),
                _ => None,
            }
        };
        let cm = cached("gsdi", "()I", code, 1, 0);
        let ir = compile_getstatic(&cm, &helpers, true, &statics).expect("IR direct getstatic");
        let sp = compile_getstatic(&cm, &helpers, false, &statics).expect("single-pass");
        assert!(
            ir.used_ir_backend,
            "cov-01: direct getstatic on the IR tier"
        );
        assert_eq!(
            call_with_dummy_context(&ir, &[]) as i32,
            -7,
            "a resolved getstatic must read the block DIRECTLY (MOVSXD of the \
             Int payload); the marker value means it took the helper instead"
        );
        assert_eq!(call_with_dummy_context(&sp, &[]) as i32, -7);
    }

    // 2. A site the resolver DECLINES keeps the helper, on both backends — the
    //    control that proves rung 1 is measuring the ROUTE and not merely that
    //    getstatic works at all.
    {
        let code = vec![0xb2, 0x00, 0x01, 0xac];
        // Field 0, which `test_resolver` declines. This control used to use
        // field 5; when rung 4 widened the resolver's accepted range to `1..=5`
        // it silently became a RESOLVED site and started reading slot 5 (a
        // float) as an int. Index 0 is both outside the accepted range and a
        // real slot, so a resolver that wrongly accepted it would read a valid
        // word rather than off the end of the block.
        let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x1CEu32, 0usize, b'I', false)),
                _ => None,
            }
        };
        let cm = cached("gsdf", "()I", code, 1, 0);
        let ir = compile_getstatic(&cm, &helpers, true, &statics).expect("IR fallback getstatic");
        let sp = compile_getstatic(&cm, &helpers, false, &statics).expect("single-pass");
        let expected = 424_242 + 0x1CE;
        assert_eq!(call_with_dummy_context(&ir, &[]) as i32, expected);
        assert_eq!(call_with_dummy_context(&sp, &[]) as i32, expected);
    }

    // 3. Reference-typed, slot 2 → the 64-bit payload, no sign extension. The
    //    value is a `Value::Long`'s payload rather than a real oop: what is
    //    under test is the DISPLACEMENT and the width, and a genuine object
    //    reference would need a heap this harness does not have.
    {
        let code = vec![0xb2, 0x00, 0x01, 0xb0];
        let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x1CEu32, 2usize, b'L', false)),
                _ => None,
            }
        };
        let cm = cached("gsdr", "()Ljava/lang/Object;", code, 1, 0);
        let ir = compile_getstatic(&cm, &helpers, true, &statics).expect("IR ref getstatic");
        let sp = compile_getstatic(&cm, &helpers, false, &statics).expect("single-pass");
        assert!(
            ir.used_ir_backend,
            "cov-01: a reference static on the IR tier"
        );
        assert_eq!(
            call_with_dummy_context(&ir, &[]),
            0x0BAD_F00D_1234_5678u64 as i64,
            "a reference static must be read at the 64-bit payload offset"
        );
        assert_eq!(
            call_with_dummy_context(&sp, &[]),
            0x0BAD_F00D_1234_5678u64 as i64
        );
    }

    // 4. The wide and floating-point widths, on the same block and the same one
    //    registration. This is where the payload OFFSET and the load WIDTH are
    //    both tag-dependent: `J`/`D` read the 64-bit payload, `F` the 32-bit
    //    one, and `F` must be ZERO-extended — `MOVSXD` on a NEGATIVE float's bit
    //    pattern (bit 31 set, which is why the value is -2.5 and not 2.5) fills
    //    the high half of the home word with garbage, and the home word is what
    //    every deopt frame and `publish_fp_from_slot` read.
    for (slot_idx, tag, desc, ret, want) in [
        (3usize, b'J', "()J", 0xadu8, 0x0123_4567_89AB_CDEFi64),
        (4, b'D', "()D", 0xaf, (-2.5f64).to_bits() as i64),
        (5, b'F', "()F", 0xae, (-2.5f32).to_bits() as u32 as i64),
    ] {
        let code = vec![0xb2, 0x00, 0x01, ret];
        let statics = move |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x1CEu32, slot_idx, tag, false)),
                _ => None,
            }
        };
        let cm = cached("gsdw", desc, code, 2, 0);
        let ir = compile_wide_getstatic(&cm, &helpers, true, &statics)
            .unwrap_or_else(|| panic!("IR direct `{}` static", tag as char));
        let sp = compile_wide_getstatic(&cm, &helpers, false, &statics)
            .unwrap_or_else(|| panic!("single-pass direct `{}` static", tag as char));
        assert!(
            ir.used_ir_backend,
            "cov-01: a `{}` static must reach the optimizing backend under its \
             value-tier gate",
            tag as char
        );
        let r_ir = call_with_dummy_context(&ir, &[]);
        let r_sp = call_with_dummy_context(&sp, &[]);
        // The two backends are compared on the FULL 64-bit word, deliberately.
        // A `float` return only defines the low 32 bits, so the high half is
        // ABI-undefined — but it is not backend-undefined: the single-pass arm
        // zero-extends (`MOV r32`) and this is the assertion that catches the
        // IR arm sign-extending instead. Masking here would discard exactly the
        // bits the `MOVSXD`-vs-`MOV EAX` choice decides, which is what the first
        // draft of this rung did, and it passed against a deliberately wrong
        // width.
        assert_eq!(
            r_ir, r_sp,
            "`{}` direct static: IR vs single-pass diverge",
            tag as char
        );
        // Against the host, only the bits the ABI defines.
        let masked = if tag == b'F' {
            r_ir as u32 as i64
        } else {
            r_ir
        };
        assert_eq!(
            masked, want,
            "`{}` direct static: wrong bits — the marker value would mean it \
             took the helper instead",
            tag as char
        );
    }
}

#[test]
fn a_volatile_static_agrees_on_both_backends() {
    // A volatile static read is a JMM acquire and both backends emit MFENCE
    // after the load. The fence itself is not observable from a single-threaded
    // call; what this pins is that the volatile flag does not change the VALUE,
    // because the failure mode a mis-placed fence insertion produces in a
    // hand-written encoder is a desynced instruction stream — a wrong result or
    // a crash, not a quietly missing barrier.
    let code = vec![0xb2, 0x00, 0x01, 0xac];
    let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
        match cp {
            1 => Some((0x33u32, 2usize, b'I', true)),
            _ => None,
        }
    };
    let mut helpers = dummy_helpers();
    helpers.getstatic = marker_getstatic as *const () as usize;
    let cm = cached("gsv", "()I", code, 1, 0);
    let ir = compile_getstatic(&cm, &helpers, true, &statics).expect("IR volatile getstatic");
    let sp = compile_getstatic(&cm, &helpers, false, &statics).expect("single-pass");
    let expected = 424_242 + 0x33 + 2;
    assert_eq!(call_with_dummy_context(&ir, &[]) as i32, expected);
    assert_eq!(call_with_dummy_context(&sp, &[]) as i32, expected);
}

// ── cov-01 residual: `J` / `D` / `F` statics ─────────────────────────────
//
// When `getstatic` first lowered, these three tags were refused in the BUILDER
// on the grounds that admitting one would put a `Long`/`Double`/`Float` node in
// a graph whose admission clause may have been the int one. The hazard was
// real; the refusal was in the wrong place. `getstatic` is polymorphic and
// appears in neither `is_category2_opcode` nor `is_float_opcode`, so the builder
// genuinely cannot see the width — but `try_compile` resolved the type tag in
// order to build the table at all, so the gate belongs there, keyed on
// `ir_emit_long` / `ir_emit_fp` exactly as the float `ldc` and `ldc2_w` feeds
// already are.

/// `jit_dispatch_threw` stand-in that reports **no** pending signal.
///
/// Reached only on the cold `RAX == i64::MIN` branch of a wide static's helper
/// route. Returning 0 is the "that was a real value, keep it" answer, which is
/// the case `long_min_value_static_is_a_value_not_a_sentinel` exists to drive.
unsafe extern "C" fn no_signal_pending() -> i64 {
    0
}

/// `jit_dispatch_threw` stand-in that reports a pending signal — the other half
/// of the same branch, used to prove the peek is consulted rather than ignored.
unsafe extern "C" fn signal_pending() -> i64 {
    1
}

/// `jit_getstatic` stand-in whose value is chosen by `field_index`.
///
/// The first draft of these tests shared one `AtomicI64` that each test stored
/// into before calling. They run in parallel threads, so they raced and three of
/// the four failed — while passing under `--test-threads=1`. Keying on an
/// ARGUMENT removes the shared mutable state rather than serialising around it,
/// which is the same fix `proxy_lambda_dispatch_preserves_diagnostic_counters`
/// needed for the same reason.
unsafe extern "C" fn wide_value_getstatic(_vm: i64, _class_id: i64, field_index: i64) -> i64 {
    match field_index {
        0 => 0x0123_4567_89AB_CDEF,
        1 => (-2.5f64).to_bits() as i64,
        2 => (-2.5f32).to_bits() as i64,
        _ => 7,
    }
}

/// `jit_getstatic` stand-in that always returns the value bit-identical to the
/// failed-`<clinit>` sentinel. Its own function, so nothing else can perturb it.
unsafe extern "C" fn long_min_getstatic(_vm: i64, _class_id: i64, _field_index: i64) -> i64 {
    i64::MIN
}

/// [`compile_getstatic`] with the long and FP value tiers on, which is what a
/// `J` / `D` / `F` static needs to be fed to the builder at all.
fn compile_wide_getstatic(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    statics: &dyn Fn(u16) -> Option<(u32, usize, u8, bool)>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        Some(statics),
        None,
        None,
        None,
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        true, // ir_emit_long
        false,
        true, // ir_emit_fp
        None,
    )
}

#[test]
fn ir_vs_singlepass_wide_and_fp_statics_through_the_helper() {
    // `long f() { return H.J; }`, `double f() { return H.D; }`,
    // `float f() { return H.F; }` — getstatic #1 ; {lreturn,dreturn,freturn}.
    //
    // The helper route (no VM registered for this class id, so
    // `resolve_static_base` declines). Both backends must return the same bits.
    let mut helpers = dummy_helpers();
    helpers.getstatic = wide_value_getstatic as *const () as usize;
    helpers.dispatch_threw = no_signal_pending as *const () as usize;

    // `field_index` selects the value `wide_value_getstatic` returns, so no two
    // parallel tests share a cell.
    for (slot_idx, tag, desc, ret, bits) in [
        (0usize, b'J', "()J", 0xadu8, 0x0123_4567_89AB_CDEFu64 as i64),
        (1, b'D', "()D", 0xafu8, (-2.5f64).to_bits() as i64),
        // A NEGATIVE float, so bit 31 is set: the inline arm's `MOV EAX` vs
        // `MOVSXD` distinction is invisible for a positive one.
        (2, b'F', "()F", 0xaeu8, (-2.5f32).to_bits() as i64),
    ] {
        let code = vec![0xb2, 0x00, 0x01, ret];
        let statics = move |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x44u32, slot_idx, tag, false)),
                _ => None,
            }
        };
        let cm = cached("gsw", desc, code, 2, 0);
        let ir = compile_wide_getstatic(&cm, &helpers, true, &statics)
            .unwrap_or_else(|| panic!("IR `{}` static", tag as char));
        let sp = compile_wide_getstatic(&cm, &helpers, false, &statics)
            .unwrap_or_else(|| panic!("single-pass `{}` static", tag as char));
        assert!(
            ir.used_ir_backend,
            "cov-01: a `{}` static must reach the optimizing backend under its \
             value-tier gate",
            tag as char
        );
        let r_ir = call_with_dummy_context(&ir, &[]);
        let r_sp = call_with_dummy_context(&sp, &[]);
        // Full 64 bits between the two backends — see the direct-load rung for
        // why masking here would hide a width bug — and the ABI-defined bits
        // against the host.
        assert_eq!(
            r_ir, r_sp,
            "`{}` static: IR vs single-pass diverge",
            tag as char
        );
        let (masked, want) = if tag == b'F' {
            (r_ir as u32 as i64, bits as u32 as i64)
        } else {
            (r_ir, bits)
        };
        assert_eq!(masked, want, "`{}` static: wrong bits", tag as char);
    }
}

#[test]
fn long_min_value_static_is_a_value_not_a_sentinel() {
    // The case wide statics exist to get wrong. `jit_getstatic` reports a failed
    // `<clinit>` by returning `i64::MIN`, and `Long.MIN_VALUE` is bit-identical
    // to it — so a plain compare-and-bail would turn
    //
    //     static final long L = Long.MIN_VALUE;
    //
    // into a spurious exception on every read. The helper route peeks
    // `jit_dispatch_threw` on that branch to tell the two apart.
    //
    // The edit that trips it: replace `emit_call_return_check(slot, node.ty)` in
    // the `Op::LoadStatic` arm with the unconditional `CMP RAX, i64::MIN ; JE
    // bail` it used to have. **Verified to fail.**
    //
    // The body is `return H.L + 1;`, not `return H.L;`, and that is the whole
    // reason this test can fail at all. On the bail path the shared epilogue
    // returns `i64::MIN` unchanged — so a method that merely returned the static
    // would produce `i64::MIN` whether the value was KEPT or the sentinel was
    // PROPAGATED, and the assertion would hold against a backend that got it
    // exactly wrong. Adding one makes the two answers different bit patterns:
    // `MIN + 1` if the value survived, `MIN` if it was mistaken for a signal.
    // (The first draft of this test omitted the `+ 1` and passed against a
    // deliberately broken lowering.)
    let mut helpers = dummy_helpers();
    helpers.getstatic = long_min_getstatic as *const () as usize;
    helpers.dispatch_threw = no_signal_pending as *const () as usize;

    let code = vec![
        0xb2, 0x00, 0x01, // getstatic #1   (Long.MIN_VALUE)
        0x0a, // lconst_1
        0x61, // ladd
        0xad, // lreturn
    ];
    let statics = |cp: u16| -> Option<(u32, usize, u8, bool)> {
        match cp {
            1 => Some((0x55u32, 0usize, b'J', false)),
            _ => None,
        }
    };
    let cm = cached("gsmin", "()J", code, 4, 0);
    let ir = compile_wide_getstatic(&cm, &helpers, true, &statics).expect("IR Long.MIN static");
    let sp = compile_wide_getstatic(&cm, &helpers, false, &statics).expect("single-pass");
    assert!(ir.used_ir_backend, "must be the optimizing body");
    let kept = i64::MIN.wrapping_add(1);
    assert_eq!(
        call_with_dummy_context(&ir, &[]),
        kept,
        "a static holding Long.MIN_VALUE must READ BACK as Long.MIN_VALUE, not \
         be mistaken for the failed-<clinit> sentinel (getting i64::MIN here \
         means the read bailed to the exception epilogue)"
    );
    assert_eq!(call_with_dummy_context(&sp, &[]), kept);

    // The other half of the same branch: with a signal genuinely pending, the
    // identical bits must propagate the sentinel instead — and now that is
    // observable, because the `+ 1` never runs. Without this rung the assertion
    // above passes just as well against a lowering that never checks at all.
    helpers.dispatch_threw = signal_pending as *const () as usize;
    let ir2 = compile_wide_getstatic(&cm, &helpers, true, &statics).expect("IR");
    assert_eq!(
        call_with_dummy_context(&ir2, &[]),
        i64::MIN,
        "with a signal pending the same bits must propagate the sentinel and \
         skip the rest of the method"
    );
}

#[test]
fn wide_statics_stay_on_single_pass_with_their_value_tier_off() {
    // The fail-closed rung, and the reason the gate lives in `try_compile`
    // rather than in the builder.
    //
    // The body is `sink(H.WIDE);` — `getstatic #1 ; invokestatic #2 ; return`.
    // That is the ONLY shape in which a wide or floating-point static appears
    // in a method containing no category-2 or FP OPCODE, and it is also what
    // ordinary Java looks like. Every other consumer of a `long` is itself
    // category-2 (`lstore`, `lreturn`, `ladd`, `l2i`, `lcmp`), which would make
    // the admission gate refuse the method and the test pass vacuously; the one
    // non-category-2 discard, `pop2`, is implemented by neither backend.
    //
    // So `method_uses_category2("()V")` is false, `fp_in_body` is false, and
    // the method is ADMITTED to the optimizing pipeline through the int clause
    // — at which point only the feed knows that `H.WIDE` is 64 bits wide.
    //
    // The edit that trips it: delete the `admitted_by_value_tier` match in
    // `try_compile`'s cov-01 static-field feed.
    let mut helpers = dummy_helpers();
    // `field_index` 3 ⇒ the value 7; the site below uses it.
    helpers.getstatic = wide_value_getstatic as *const () as usize;
    helpers.dispatch_threw = no_signal_pending as *const () as usize;

    for (tag, sink_desc, long_gate, fp_gate) in [
        (b'J', "(J)V", true, false),
        (b'D', "(D)V", false, true),
        (b'F', "(F)V", false, true),
    ] {
        let code = vec![
            0xb2, 0x00, 0x01, // getstatic #1  (the wide static)
            0xb8, 0x00, 0x02, // invokestatic #2  sink(<tag>)V
            0xb1, // return
        ];
        let statics = move |cp: u16| -> Option<(u32, usize, u8, bool)> {
            match cp {
                1 => Some((0x66u32, 3usize, tag, false)),
                _ => None,
            }
        };
        let invokes = move |cp: u16| -> Option<(String, String, String)> {
            match cp {
                2 => Some((
                    "Corpus".to_string(),
                    "sink".to_string(),
                    sink_desc.to_string(),
                )),
                _ => None,
            }
        };
        // Never CALLED — `invoke_dispatch` is the harness's panicking stub. This
        // rung is about which BACKEND produced the body; the value paths are
        // driven by the two tests above.
        let compile = |cm: &CachedBytecodeMethod, long: bool, fp: bool| {
            try_compile(
                cm,
                None,
                None,
                Some(&statics),
                Some(&invokes),
                None,
                None,
                None,
                None,
                None,
                &helpers,
                None,
                None,
                None,
                None,
                true,  // optimize
                true,  // ir_emit_calls — the invokestatic must lower either way,
                false, //   so the value-tier gate is the only difference
                long,
                false,
                fp,
                None,
            )
        };
        let cm = cached("gsoff", "()V", code, 2, 0);

        // Gate OFF for this tag ⇒ the builder must bail to single-pass.
        let off = compile(&cm, false, false)
            .expect("the method still compiles — on the single-pass backend");
        assert!(
            !off.used_ir_backend,
            "cov-01: a `{}` static with its value tier off must bail the \
             optimizing builder",
            tag as char
        );

        // Gate ON ⇒ the same body IS lowered. Without this control, "the gate
        // refused it" and "the builder cannot lower this shape at all" are
        // indistinguishable and the assertion above proves nothing.
        let on = compile(&cm, long_gate, fp_gate).expect("IR body under the value-tier gate");
        assert!(
            on.used_ir_backend,
            "cov-01: the same `{}` body must reach the optimizing backend with \
             its value tier on",
            tag as char
        );
    }
}

// ---------------------------------------------------------------------------
// cov-04 — the invoke arms: a real `<init>` CALL, and the elision that outranks it
//
// `cov-04-the-invoke-arms-RETIRED-20260803.md`. The census on the three
// Spring Boot workloads found that EVERY refusal in the `0xb7` arm was an
// `<init>` — 29 a `super(...)`/`this(...)` chain call in a compiled
// constructor, 23 a `new X(args)` site — and none was a non-`<init>`
// `invokespecial` with no lowering. One test per shape, plus the guard on the
// transform the lane must not lose.
// ---------------------------------------------------------------------------

/// Stands in for the VM heap so a live (escaping) `Op::New` has something to
/// return. `jit_new_object`'s ABI: `(vm_ptr, class_id, num_fields) -> oop`,
/// `0` on failure. Zero-initialised, 8-aligned and above the
/// `TEST_REGION_BOUNDS` floor exactly like [`make_object`]'s buffers, and
/// deliberately leaked — JIT-emitted code holds the raw address.
///
/// Each test that counts allocations owns its **own** counter and its own
/// `extern "C"` wrapper around this, declared inside the test body. A single
/// shared counter is wrong here and was wrong once: `cargo test` runs these
/// tests on concurrent threads, so a global would have the two allocating tests
/// incrementing each other's expected values. That is a flaky test, which this
/// directory's rule 5 rates worse than no test — and it cost a real diagnosis,
/// because the interference looked exactly like "escape analysis failed to
/// scalar-replace" until `CRATONVM_DBG_SCALAR_NEW=1` said `1/1`.
fn leak_zeroed_object(num_fields: i64) -> i64 {
    let words = (HEADER_SIZE + num_fields as usize * SLOT_SIZE)
        .div_ceil(8)
        .max(1);
    let buf: Vec<u64> = vec![0u64; words];
    let ptr = buf.as_ptr() as i64;
    std::mem::forget(buf);
    ptr
}

/// [`compile_with_dispatch`] plus the two resolvers the `new` / `<init>` path
/// needs: `cp_new_resolver` (allocation layout) and `cp_elidable_init_resolver`
/// (which `<init>()V` calls may be elided rather than emitted).
fn compile_with_dispatch_and_new(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
    new_resolver: &dyn Fn(u16) -> Option<cratonvm_jit::JitNewSite>,
    elidable_init_resolver: &dyn Fn(u16) -> bool,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        None,
        None,
        None,
        Some(invoke_resolver),
        None,
        Some(new_resolver),
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        Some(elidable_init_resolver),
        true,  // optimize (C2 / IR pipeline)
        true,  // ir_emit_calls
        true,  // ir_emit_special_calls
        true,  // ir_emit_long
        true,  // ir_emit_virtual_calls
        false, // ir_emit_fp
        None,
    )
}

#[test]
fn ir_invokespecial_super_constructor_chain_is_called() {
    // cov-04 group A — 29 of the 68 events measured at `48fba3a31`, and the
    // whole of the 35 compiles the `is_special && mn == "<init>"` term used to
    // discard. (Counts are baseline-relative; the shape is not. See the
    // closeout.) A compiled CONSTRUCTOR's `super(...)` / `this(...)` chain
    // call: the receiver is `this` — a parameter, never a fresh `Op::New` — so
    // the elision path is structurally inapplicable and the only correct
    // lowering is a real call.
    //
    //   int <init>(Corpus this, int n) { super(n); return this.f0; }
    //   aload_0; iload_1; invokespecial #2 <init>(I)V; aload_0; getfield #4; ireturn
    //
    // The dispatch stub WRITES `n * 3` into the receiver's field 0, which the
    // method reads back and returns. Exact edits that trip this test: restore
    // the `is_special && mn == "<init>"` bulk-disable (compile returns None),
    // or drop the call emission (returns 0), or swap receiver and argument
    // (writes to the wrong address).
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    unsafe extern "C" fn ctor_dispatch(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 2, "<init>(I)V takes (receiver, int)");
        let p = args_ptr as *const i64;
        let recv = *p;
        let n = *p.add(1) as i32;
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        std::ptr::write_unaligned((recv as *mut u8).add(off) as *mut i32, n * 3);
        0 // `<init>` returns void
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = ctor_dispatch as *const () as usize;
    helpers.getfield = legacy_getfield as *const () as usize;
    let code = vec![
        0x2a, 0x1b, 0xb7, 0x00, 0x02, // aload_0; iload_1; invokespecial #2
        0x2a, 0xb4, 0x00, 0x04, 0xac, // aload_0; getfield #4; ireturn
    ];
    let cm = cached("<init>", "(Lpkg/Corpus;I)I", code, 2, 2);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Super".into(), "<init>".into(), "(I)V".into()))
        } else {
            None
        }
    };
    let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        if cp == 4 {
            Some((0, b'I', None))
        } else {
            None
        }
    };
    let ir = try_compile(
        &cm,
        None,
        Some(&field_resolver),
        None,
        Some(&resolver),
        None,
        None,
        None,
        None,
        None,
        &helpers,
        None,
        None,
        None,
        None,
        true,
        true,
        true,
        true,
        true,
        false,
        None,
    )
    .expect(
        "a constructor's super(...) chain call must lower through the optimizing \
         tier — cov-04 group A",
    );
    assert!(
        ir.used_ir_backend,
        "the point of this test is the IR arm; a single-pass fallback proves nothing",
    );
    let dummy_vm = [0u8; 64];
    for n in [7i64, -3, 0, 5] {
        let mut obj = make_object(&[0]);
        let args = [obj.as_mut_ptr() as i64, n];
        // SAFETY: `obj` is a live 8-aligned synthetic object with one int field;
        // the stub writes only that field and `getfield #4` reads only it.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &args) }
            .unwrap_or_else(|e| panic!("super-ctor chain call n={n}: {e:?}"));
        assert_eq!(
            r,
            n * 3,
            "the super constructor must actually RUN: 0 means the call was never \
             emitted, anything else means the receiver/arg pair is wrong"
        );
        drop(obj);
    }
}

#[test]
fn ir_elidable_trivial_init_on_fresh_new_is_still_elided() {
    // The transform cov-04 must not lose. Once a `<init>` can take an
    // `invoke_info` entry, an elidable `<init>()V` on a fresh `Op::New` has TWO
    // available lowerings, and the builder must keep choosing ELISION — a call
    // here arg-escapes the allocation and defeats scalar replacement.
    //
    //   int f(int n) { Corpus c = new Corpus(); c.f0 = n; return c.f0; }
    //   new #3; dup; invokespecial #2 <init>()V; astore_1;
    //   aload_1; iload_0; putfield #4; aload_1; getfield #4; ireturn
    //
    // The SAME bytecode is compiled twice — once with the site elidable, once
    // not — so the assertion is a comparison rather than a claim about one run:
    //
    //   elidable  → the dispatch helper must NOT be reached (elision chosen)
    //   !elidable → the dispatch helper MUST be reached exactly once per call
    //
    // Exact edit that trips this test: put the `invoke_info` branch of the
    // `0xb7` arm ahead of the elision branch. The elidable arm then dispatches
    // and `must_not_dispatch` panics.
    //
    // The allocation count is asserted too, but it is NOT the property this
    // lane owns and the two arms agree on it: the object is really allocated in
    // both. That is worth pinning precisely because it is surprising —
    // `CRATONVM_DBG_SCALAR_NEW=1` reports `scalar-replaced 1/1` for the elidable
    // arm, so escape analysis OFFERS the replacement and the emitted body
    // allocates anyway. The offer and the emitted code disagree; see the
    // residual in `cov-04-the-invoke-arms-RETIRED-20260803.md`.
    //
    // If this assertion ever fails because arm 1's count went to ZERO, that is
    // an improvement, not a regression: change it to 0 and delete that residual.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    unsafe extern "C" fn must_not_dispatch(_vm: i64, _i: i64, _a: i64, _n: i64) -> i64 {
        // Deliberately a hard failure rather than a plausible return value: a
        // stub that quietly returned 0 here would let an un-elided <init> pass.
        panic!("an elidable <init>()V on a fresh `new` was DISPATCHED, not elided");
    }
    // Test-local, never shared: see `leak_zeroed_object`.
    static CTOR_DISPATCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static ALLOCS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn counting_dispatch(_vm: i64, _i: i64, _a: i64, num_args: i64) -> i64 {
        assert_eq!(num_args, 1, "<init>()V takes the receiver and nothing else");
        CTOR_DISPATCHES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        0
    }
    unsafe extern "C" fn counting_alloc(_vm: i64, _class_id: i64, num_fields: i64) -> i64 {
        ALLOCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        leak_zeroed_object(num_fields)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = must_not_dispatch as *const () as usize;
    helpers.new_object = counting_alloc as *const () as usize;
    helpers.getfield = legacy_getfield as *const () as usize;
    helpers.putfield_int = legacy_putfield_int as *const () as usize;
    let code = vec![
        0xbb, 0x00, 0x03, // new #3
        0x59, // dup
        0xb7, 0x00, 0x02, // invokespecial #2  <init>()V
        0x4c, // astore_1
        0x2b, 0x1a, 0xb5, 0x00, 0x04, // aload_1; iload_0; putfield #4
        0x2b, 0xb4, 0x00, 0x04, // aload_1; getfield #4
        0xac, // ireturn
    ];
    let cm = cached("f", "(I)I", code, 2, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        if cp == 2 {
            Some(("pkg/Corpus".into(), "<init>".into(), "()V".into()))
        } else {
            None
        }
    };
    let field_resolver = |cp: u16| -> Option<(usize, u8, Option<(u32, bool)>)> {
        if cp == 4 {
            Some((0, b'I', None))
        } else {
            None
        }
    };
    let new_resolver = |cp: u16| -> Option<cratonvm_jit::JitNewSite> {
        if cp == 3 {
            Some(cratonvm_jit::JitNewSite::Resolved {
                class_id: 7,
                num_fields: 1,
                has_prim_init: false,
                has_finalizer: false,
            })
        } else {
            None
        }
    };
    let compile = |elidable: &dyn Fn(u16) -> bool, helpers: &JitRuntimeHelpers| {
        try_compile(
            &cm,
            None,
            Some(&field_resolver),
            None,
            Some(&resolver),
            None,
            Some(&new_resolver),
            None,
            None,
            None,
            helpers,
            None,
            None,
            None,
            Some(elidable),
            true,
            true,
            true,
            true,
            true,
            false,
            None,
        )
    };
    let dummy_vm = [0u8; 64];

    // Arm 1 — the site IS elidable: elision must win over the (now available)
    // call, so `must_not_dispatch` must never run.
    let elidable = |cp: u16| -> bool { cp == 2 };
    let ir = compile(&elidable, &helpers)
        .expect("an elidable trivial-<init> allocation must still compile");
    assert!(ir.used_ir_backend, "this test is about the IR arm");
    for (i, n) in [11i64, 0, -4].into_iter().enumerate() {
        // SAFETY: every pointer the emitted code touches comes from
        // `counting_alloc`, which hands out live leaked 8-aligned buffers.
        // Reaching `must_not_dispatch` is the failure this arm is here for.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("elidable-init method n={n}: {e:?}"));
        assert_eq!(r, n, "the field written is the field read back");
        // The improvement this assertion's predecessor invited, taken.
        //
        // It used to require `i + 1` and say "0 here would be an improvement".
        // 0 is what `CRATONVM_SCALAR_DEOPT` produces: escape analysis had
        // always OFFERED this `new` as scalar-replaceable, and that flag is
        // what lets the offer be acted on — so the allocation is really gone
        // and `counting_alloc` never runs. Without the flag the offer is
        // refused at `plan_scalar_replacement`'s descriptor gate and the object
        // is really allocated, exactly as before.
        //
        // Asserted BOTH ways rather than relaxed to "0 or i+1": this count is
        // the only evidence in the test that the elision happened at all, and
        // an assertion accepting either answer would pass on a build where the
        // flag had silently stopped working.
        let elided = cratonvm_jit::scalar_deopt_enabled() && cratonvm_jit::deopt_real_enabled();
        assert_eq!(
            ALLOCS.load(std::sync::atomic::Ordering::SeqCst),
            if elided { 0 } else { i + 1 },
            "with the deopt descriptor available the allocation is elided (0); \
             without it the offer is refused and the object is really allocated"
        );
    }
    let allocs_after_arm1 = ALLOCS.load(std::sync::atomic::Ordering::SeqCst);

    // Arm 2 — the control. Identical bytecode, but the elision analysis
    // DECLINES the site, so the constructor must be really called. Without this
    // arm the test above would also pass on a builder that silently dropped
    // every `<init>`.
    let mut call_helpers = helpers;
    call_helpers.invoke_dispatch = counting_dispatch as *const () as usize;
    let not_elidable = |_cp: u16| -> bool { false };
    let ir2 = compile(&not_elidable, &call_helpers)
        .expect("a non-elidable <init>()V on a fresh `new` must still compile — cov-04");
    assert!(ir2.used_ir_backend, "this test is about the IR arm");
    for (i, n) in [11i64, 0, -4].into_iter().enumerate() {
        // SAFETY: as above.
        let r = unsafe { ir2.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("non-elidable-init method n={n}: {e:?}"));
        assert_eq!(r, n, "the field written is the field read back");
        assert_eq!(
            CTOR_DISPATCHES.load(std::sync::atomic::Ordering::SeqCst),
            i + 1,
            "a constructor the elision analysis declines must be CALLED, once \
             per invocation"
        );
        assert_eq!(
            ALLOCS.load(std::sync::atomic::Ordering::SeqCst),
            allocs_after_arm1 + i + 1,
            "and its receiver arg-escapes into that call, so the allocation must \
             survive — once per invocation, never scalar-replaced"
        );
    }
}

#[test]
fn ir_new_with_non_elidable_constructor_allocates_and_calls_init() {
    // cov-04 group B — 39 of the 68 events measured at `48fba3a31`: a method
    // containing a
    // `new` whose constructor the elision analysis declines. Two things had to
    // change for this to compile — the `<init>` call itself, and
    // `call_eligible`, which discarded `invoke_info` for the WHOLE method
    // whenever `scan.new_ops` was non-empty, on the premise that the lowerer
    // had no allocation path. It has one: `ir_lower`'s `Op::New` arm, through
    // the shared TLAB-aware stub.
    //
    //   int f(int n) { return sink(new Corpus(n)); }
    //   new #3; dup; iload_0; invokespecial #2 <init>(I)V; invokestatic #5; ireturn
    //
    // `<init>` writes `n * 5` into field 0; `sink` reads field 0 back. The
    // assertion therefore covers the allocation, the constructor call, the
    // argument marshalling, and the identity of the object handed on after it.
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    unsafe extern "C" fn ctor_or_sink(_vm: i64, _info: i64, args_ptr: i64, num_args: i64) -> i64 {
        let p = args_ptr as *const i64;
        let off = HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET;
        match num_args {
            // `<init>(I)V` — (receiver, int)
            2 => {
                let recv = *p;
                let n = *p.add(1) as i32;
                std::ptr::write_unaligned((recv as *mut u8).add(off) as *mut i32, n * 5);
                0
            }
            // `sink(LCorpus;)I` — (object)
            1 => {
                let obj = *p;
                std::ptr::read_unaligned((obj as *const u8).add(off) as *const i32) as i64
            }
            n => panic!("unexpected arg count {n}"),
        }
    }
    // Test-local, never shared: see `leak_zeroed_object`.
    static ALLOCS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    unsafe extern "C" fn counting_alloc(_vm: i64, _class_id: i64, num_fields: i64) -> i64 {
        ALLOCS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        leak_zeroed_object(num_fields)
    }
    let mut helpers = dummy_helpers();
    helpers.invoke_dispatch = ctor_or_sink as *const () as usize;
    helpers.new_object = counting_alloc as *const () as usize;
    let code = vec![
        0xbb, 0x00, 0x03, // new #3
        0x59, // dup
        0x1a, // iload_0
        0xb7, 0x00, 0x02, // invokespecial #2  <init>(I)V
        0xb8, 0x00, 0x05, // invokestatic #5   sink(LCorpus;)I
        0xac, // ireturn
    ];
    let cm = cached("f", "(I)I", code, 1, 1);
    let resolver = |cp: u16| -> Option<(String, String, String)> {
        match cp {
            2 => Some(("pkg/Corpus".into(), "<init>".into(), "(I)V".into())),
            5 => Some(("pkg/Helper".into(), "sink".into(), "(Lpkg/Corpus;)I".into())),
            _ => None,
        }
    };
    let new_resolver = |cp: u16| -> Option<cratonvm_jit::JitNewSite> {
        if cp == 3 {
            Some(cratonvm_jit::JitNewSite::Resolved {
                class_id: 7,
                num_fields: 1,
                has_prim_init: false,
                has_finalizer: false,
            })
        } else {
            None
        }
    };
    // Nothing is elidable here: this is the case the elision analysis DECLINES.
    let elidable = |_cp: u16| -> bool { false };
    let ir = compile_with_dispatch_and_new(&cm, &helpers, &resolver, &new_resolver, &elidable)
        .expect(
            "a `new X(n)` with a non-elidable constructor must lower through the \
             optimizing tier — cov-04 group B",
        );
    assert!(ir.used_ir_backend, "this test is about the IR arm");
    let dummy_vm = [0u8; 64];
    for (i, n) in [3i64, -2, 0, 9].into_iter().enumerate() {
        // SAFETY: every pointer the emitted code touches comes from
        // `counting_alloc`, which hands out live leaked 8-aligned buffers.
        let r = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[n]) }
            .unwrap_or_else(|e| panic!("new + non-elidable ctor n={n}: {e:?}"));
        assert_eq!(
            r,
            n * 5,
            "constructor side effect must be visible to `sink`"
        );
        assert_eq!(
            ALLOCS.load(std::sync::atomic::Ordering::SeqCst),
            i + 1,
            "an object passed to a call arg-escapes: it must be really allocated, \
             once per invocation, not scalar-replaced",
        );
    }
}

// ---------------------------------------------------------------------------
// cov-06: `newarray` (0xbc) IR-vs-single-pass differential.
// ---------------------------------------------------------------------------

/// Backs `helpers.newarray` with a REAL (leaked, test-only) buffer laid out
/// exactly the way `Op::ArrayLoad`/`Op::ArrayStore`'s inline codegen expects:
/// the length at `ARRAY_LENGTH_OFFSET` and int elements packed at
/// `HEADER_SIZE + index*4` — see `ir_lower::emit_array_null_bounds_guards`
/// and `emit_gpr_array_elem_load`/`_store`. This is what makes "allocate,
/// store, read back, return" a REAL round trip through both backends' array
/// element codegen, not just a check that the allocator was called.
///
/// Negative-length handling is intentionally OUT of scope here: `jit_newarray`
/// routes a negative length through the pending-Java-exception channel, which
/// this synthetic-helper harness has no VM/interpreter to drain — see
/// `vm/tests/jit_cov06_array_allocation.rs` for the real (heap + exception +
/// GC) end-to-end coverage the doc's "How to verify" section also asks for.
///
/// # Safety
/// `atype` is always `10` (`T_INT`) in this file's corpus; `length` is
/// non-negative in every case exercised. The leaked buffer outlives the test
/// process, which is acceptable for a one-shot test binary.
unsafe extern "C" fn synthetic_newarray(_vm: i64, _atype: i64, length: i64) -> i64 {
    let len = length as usize;
    let total = HEADER_SIZE + len * 4;
    let buf = vec![0u8; total].into_boxed_slice();
    let ptr = Box::into_raw(buf) as *mut u8;
    std::ptr::write_unaligned(ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32, length as i32);
    ptr as i64
}

/// `static int f(int n, int idx, int val) { int[] a = new int[n]; a[idx] =
/// val; return a[idx]; }` — allocate, store, read back, return, through BOTH
/// backends, with the real `Op::ArrayLoad`/`Op::ArrayStore` element codegen
/// (cov-02) reading the exact buffer `synthetic_newarray` laid out above.
#[test]
fn ir_vs_singlepass_newarray_allocate_store_load() {
    // iload_0 (n); newarray T_INT; astore_3 (a);
    // aload_3; iload_1 (idx); iload_2 (val); iastore;
    // aload_3; iload_1 (idx); iaload; ireturn
    let code = vec![
        0x1a, 0xbc, 0x0a, 0x4e, 0x2d, 0x1b, 0x1c, 0x4f, 0x2d, 0x1b, 0x2e, 0xac,
    ];
    let mut helpers = dummy_helpers();
    helpers.newarray = synthetic_newarray as *const () as usize;
    let cm = cached("newarrayRoundTrip", "(III)I", code, 4, 3);
    let ir = compile_opt(&cm, &helpers, true)
        .expect("newarrayRoundTrip: optimize=true (IR pipeline) failed to compile");
    let sp = compile_opt(&cm, &helpers, false)
        .expect("newarrayRoundTrip: optimize=false (single-pass) failed to compile");
    assert!(
        ir.used_ir_backend,
        "this test is about the IR arm — a silent single-pass fallback would make it vacuous"
    );

    let dummy_vm = [0u8; 64];
    for (n, idx, val) in [(5i64, 2i64, 42i64), (1, 0, -7), (8, 7, i32::MAX as i64)] {
        // SAFETY: both bodies were produced by the JIT from valid bytecode;
        // `synthetic_newarray` hands back a real, correctly-laid-out buffer
        // for the allocation each call performs (a fresh one per call, so
        // the two backends never share or race over one buffer).
        let r_sp = unsafe { sp.try_call_with_context(dummy_vm.as_ptr() as i64, &[n, idx, val]) }
            .unwrap_or_else(|e| panic!("newarrayRoundTrip: single-pass ({n},{idx},{val}): {e:?}"));
        let r_ir = unsafe { ir.try_call_with_context(dummy_vm.as_ptr() as i64, &[n, idx, val]) }
            .unwrap_or_else(|e| panic!("newarrayRoundTrip: IR ({n},{idx},{val}): {e:?}"));
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "newarrayRoundTrip: IR vs single-pass DIVERGE for ({n},{idx},{val}): IR={}, \
             single-pass={}",
            r_ir as i32, r_sp as i32,
        );
        assert_eq!(
            r_ir as i32, val as i32,
            "newarrayRoundTrip: both backends agree but disagree with the expected \
             allocate/store/load round trip for ({n},{idx},{val}): got {}",
            r_ir as i32,
        );
    }
}

// ---------------------------------------------------------------------------
// cov-05 increment 1 — `instanceof` (0xc1) against an already-loaded target
//
// `cov-05-checkcast-and-instanceof-RETIRED-20260804.md`. The largest
// single whole-method refusal in the survey (306 events, more than every
// opcode gap combined). This lane's whole premise is "give the IR tier the
// test the other backend already performs" — `Op::InstanceOf` lowers to a
// CALL to the SAME `jit_instanceof` helper the single-pass backend's 0xc1 arm
// calls, so this differential harness exists to prove the LOWERING (register
// ABI, safepoint handling, result plumbing) is faithful, not to re-verify
// `jit_instanceof`'s own subtype logic — that has its own unit tests in
// `vm/src/jit/helpers.rs` (loader-dup fallback, the primitive-array vs
// `Object[]` fix, etc.), which this IR path inherits for free by calling the
// identical real helper in production.
// ---------------------------------------------------------------------------

/// `jit_instanceof` stand-in for this harness's synthetic receivers.
/// `make_object`'s field 0 carries a small "runtime type" tag distinguishing
/// three synthetic classes: 0 = `pkg/Base`, 1 = `pkg/Sub` (extends `Base`,
/// implements `pkg/Iface`), 2 = `pkg/Other` (unrelated). This tiny fixed
/// table is enough to exercise the exact-class / subclass / interface / miss
/// shapes `cov-05`'s verification list asks for; a real hierarchy walk is
/// `jit_typecheck_resolve`'s job, already covered elsewhere. Null → 0, per
/// JVMS §6.5 (`instanceof` on `null` is always `false`, never a fault).
///
/// # Safety
/// `obj` is either 0 or one of [`make_object`]'s live buffers; `name_ptr` /
/// `name_len` name one of the three literals below.
unsafe extern "C" fn instanceof_stub(
    _vm: i64,
    obj: i64,
    name_ptr: *const u8,
    name_len: i64,
) -> i64 {
    if obj == 0 {
        return 0;
    }
    // SAFETY: the caller's contract above.
    let name = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len as usize))
    };
    let at = (obj as *const u8).add(HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET);
    // SAFETY: `obj` is a `make_object` buffer with a valid field-0 int cell.
    let tag = unsafe { std::ptr::read_unaligned(at as *const i32) };
    let is = matches!(
        (tag, name),
        (0, "pkg/Base") | (1, "pkg/Base" | "pkg/Sub" | "pkg/Iface") | (2, "pkg/Other")
    );
    is as i64
}

/// [`try_compile`] with only the two resolvers `instanceof`'s
/// `instanceof_info` construction (`lib.rs`) needs: `cp_new_resolver`
/// (answers "is the target loaded") and `cp_class_name_resolver` (names it).
fn compile_instanceof(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    new_resolver: &dyn Fn(u16) -> Option<cratonvm_jit::JitNewSite>,
    name_resolver: &dyn Fn(u16) -> Option<String>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    try_compile(
        cm,
        Some(name_resolver),
        None,
        None,
        None,
        None,
        Some(new_resolver),
        None,
        None,
        None,
        helpers,
        None,
        None,
        None,
        None,
        optimize,
        false,
        false,
        false,
        false,
        false,
        None,
    )
}

/// `boolean f(Object o) { return o instanceof <name at cp 1>; }` —
/// `aload_0; instanceof #1; ireturn`.
fn instanceof_method() -> CachedBytecodeMethod {
    let code = vec![0x2a, 0xc1, 0x00, 0x01, 0xac];
    cached("f", "(Ljava/lang/Object;)Z", code, 1, 1)
}

/// A `cp_new_resolver` reporting cp 1's target as already loaded — the
/// admission gate `IrBuilder::build`'s 0xc1 arm requires.
fn loaded_resolver(cp: u16) -> Option<cratonvm_jit::JitNewSite> {
    (cp == 1).then_some(cratonvm_jit::JitNewSite::Resolved {
        class_id: 0x99,
        num_fields: 0,
        has_prim_init: false,
        has_finalizer: false,
    })
}

fn name_resolver_for(name: &'static str) -> impl Fn(u16) -> Option<String> {
    move |cp: u16| (cp == 1).then(|| name.to_string())
}

/// One (tag, target name, expected) case run through BOTH backends and
/// asserted equal to each other and to `expected`.
fn assert_instanceof_case(tag: i32, target: &'static str, expected: bool, label: &str) {
    let mut helpers = dummy_helpers();
    helpers.instanceof_check = instanceof_stub as *const () as usize;
    let cm = instanceof_method();
    let name_resolver = name_resolver_for(target);
    let ir = compile_instanceof(&cm, &helpers, true, &loaded_resolver, &name_resolver)
        .expect("IR instanceof (target reported loaded)");
    let sp = compile_instanceof(&cm, &helpers, false, &loaded_resolver, &name_resolver)
        .expect("single-pass instanceof");
    assert!(
        ir.used_ir_backend,
        "cov-05: an instanceof-only method against an already-loaded target \
         must reach the optimizing backend ({label})"
    );
    let buf = make_object(&[tag]);
    let obj = buf.as_ptr() as i64;
    let ir_r = call_with_dummy_context(&ir, &[obj]);
    let sp_r = call_with_dummy_context(&sp, &[obj]);
    assert_eq!(ir_r, sp_r, "IR and single-pass must agree ({label})");
    assert_eq!(ir_r, expected as i64, "wrong instanceof answer ({label})");
}

#[test]
fn ir_vs_singlepass_instanceof_exact_class_hit() {
    assert_instanceof_case(1, "pkg/Sub", true, "exact class");
}

#[test]
fn ir_vs_singlepass_instanceof_subclass_hit() {
    assert_instanceof_case(1, "pkg/Base", true, "subclass");
}

#[test]
fn ir_vs_singlepass_instanceof_interface_hit() {
    assert_instanceof_case(1, "pkg/Iface", true, "interface");
}

#[test]
fn ir_vs_singlepass_instanceof_miss() {
    assert_instanceof_case(2, "pkg/Sub", false, "miss (unrelated class)");
}

#[test]
fn ir_vs_singlepass_instanceof_null() {
    let mut helpers = dummy_helpers();
    helpers.instanceof_check = instanceof_stub as *const () as usize;
    let cm = instanceof_method();
    let name_resolver = name_resolver_for("pkg/Sub");
    let ir = compile_instanceof(&cm, &helpers, true, &loaded_resolver, &name_resolver)
        .expect("IR instanceof");
    let sp = compile_instanceof(&cm, &helpers, false, &loaded_resolver, &name_resolver)
        .expect("single-pass instanceof");
    assert!(ir.used_ir_backend);
    let ir_r = call_with_dummy_context(&ir, &[0]);
    let sp_r = call_with_dummy_context(&sp, &[0]);
    assert_eq!(ir_r, 0, "instanceof on null is always false (JVMS 6.5)");
    assert_eq!(ir_r, sp_r);
}

#[test]
fn ir_vs_singlepass_instanceof_not_yet_loaded_refuses_ir() {
    // cov-05's first-increment gate: a target the resolver reports `Deferred`
    // (not yet loaded) must NOT reach `Op::InstanceOf` — the not-yet-loaded
    // resolution path can run a user classloader, arbitrary Java this tier
    // does not host inside a helper call. The method must fall back to
    // single-pass, which resolves lazily and still answers correctly (it
    // always calls the helper, loaded or not).
    let mut helpers = dummy_helpers();
    helpers.instanceof_check = instanceof_stub as *const () as usize;
    let cm = instanceof_method();
    let deferred_resolver = |cp: u16| -> Option<cratonvm_jit::JitNewSite> {
        (cp == 1).then_some(cratonvm_jit::JitNewSite::Deferred {
            holder_class_id: 0x77,
            cp_idx: cp,
        })
    };
    let name_resolver = name_resolver_for("pkg/Sub");
    let sp = compile_instanceof(&cm, &helpers, true, &deferred_resolver, &name_resolver)
        .expect("not-yet-loaded target must still compile, via single-pass fallback");
    // 2026-09-06: this assertion was INVERTED, deliberately. A `instanceof`
    // whose target class is not loaded is now replaced by an uncommon trap and
    // the rest of the method compiles -- the class has never been loaded, so no
    // path that has ever executed reached this site, which is a proof of
    // coldness rather than a guess. `CRATONVM_JIT_IR_SITE_TRAP=0` restores the
    // refusal, and that is what this asserts against rather than a constant.
    // The unresolved-class trap is OPT-IN and off by default, and THIS TEST is
    // why: it executes the very path the trap would sit on, so with the trap
    // planted the compiled body returns the deopt sentinel instead of the
    // object -- forever, on every call. See
    // `ir::ir_unresolved_class_trap_enabled`.
    assert_eq!(
        sp.used_ir_backend,
        cratonvm_jit::ir::ir_unresolved_class_trap_enabled(),
        "by default a not-yet-loaded instanceof still refuses IR admission for the          whole method; the trap is reachable only under the opt-in"
    );
    let buf = make_object(&[1]);
    let obj = buf.as_ptr() as i64;
    assert_eq!(
        call_with_dummy_context(&sp, &[obj]),
        1,
        "single-pass fallback must still answer correctly"
    );
}

// ---------------------------------------------------------------------------
// cov-05 — `checkcast` (0xc0), `instanceof`'s throwing sibling
//
// A definitive refusal throws `ClassCastException` through the SAME
// sentinel-drain protocol `Op::ConstClass`'s resolution failure already uses
// (`Lowerer::emit_call_return_check`) — not athrow's bci-baked exception-table
// machinery. `ir::ir_compatible`'s own unit tests cover the admission-gate
// side of that claim; these differential tests cover the CODEGEN side: the
// right registers, the right sentinel check, the right result type (`Ref`,
// not `Int`).
// ---------------------------------------------------------------------------

/// `jit_checkcast` stand-in, same tiny fixed hierarchy as [`instanceof_stub`]
/// (`make_object`'s field 0: 0 = `pkg/Base`, 1 = `pkg/Sub`, 2 = `pkg/Other`).
/// Returns `obj` unchanged on a successful cast (lenient — SBR-03's
/// `Object[]`→`T[]` carve-out is not modelled; this harness has no arrays),
/// `0` for a null receiver (always a valid cast, JVMS §6.5.checkcast), and
/// the `i64::MIN` sentinel on a definitive refusal — the real helper stashes
/// a `ClassCastException` there via `jit_thread_mut()`; this stand-in has no
/// VM to stash one in, so it only proves the LOWERING detects and propagates
/// the sentinel, which is the thing this differential harness exists to
/// check (the helper's own exception construction has its own unit tests in
/// `vm/src/jit/helpers.rs`).
///
/// # Safety
/// `obj` is either 0 or one of [`make_object`]'s live buffers; `name_ptr` /
/// `name_len` name one of the three literals below.
unsafe extern "C" fn checkcast_stub(_vm: i64, obj: i64, name_ptr: *const u8, name_len: i64) -> i64 {
    if obj == 0 {
        return 0;
    }
    // SAFETY: the caller's contract above.
    let name = unsafe {
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(name_ptr, name_len as usize))
    };
    let at = (obj as *const u8).add(HEADER_SIZE + FIELD_CELL_PAYLOAD32_OFFSET);
    // SAFETY: `obj` is a `make_object` buffer with a valid field-0 int cell.
    let tag = unsafe { std::ptr::read_unaligned(at as *const i32) };
    let is = matches!(
        (tag, name),
        (0, "pkg/Base") | (1, "pkg/Base" | "pkg/Sub" | "pkg/Iface") | (2, "pkg/Other")
    );
    if is {
        obj
    } else {
        i64::MIN
    }
}

/// `Object f(Object o) { return (<name at cp 1>) o; }` —
/// `aload_0; checkcast #1; areturn`.
fn checkcast_method() -> CachedBytecodeMethod {
    let code = vec![0x2a, 0xc0, 0x00, 0x01, 0xb0];
    cached("f", "(Ljava/lang/Object;)Ljava/lang/Object;", code, 1, 1)
}

/// [`try_compile`] with the same two resolvers [`compile_instanceof`] uses —
/// `checkcast`'s `checkcast_info` construction (`lib.rs`) needs the same
/// pair.
fn compile_checkcast(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    new_resolver: &dyn Fn(u16) -> Option<cratonvm_jit::JitNewSite>,
    name_resolver: &dyn Fn(u16) -> Option<String>,
) -> Option<CompiledMethod> {
    routing_not_policy();
    compile_instanceof(cm, helpers, optimize, new_resolver, name_resolver)
}

fn assert_checkcast_case(tag: i32, target: &'static str, succeeds: bool, label: &str) {
    let mut helpers = dummy_helpers();
    helpers.checkcast = checkcast_stub as *const () as usize;
    let cm = checkcast_method();
    let name_resolver = name_resolver_for(target);
    let ir = compile_checkcast(&cm, &helpers, true, &loaded_resolver, &name_resolver)
        .expect("IR checkcast (target reported loaded)");
    let sp = compile_checkcast(&cm, &helpers, false, &loaded_resolver, &name_resolver)
        .expect("single-pass checkcast");
    assert!(
        ir.used_ir_backend,
        "cov-05: a checkcast-only method against an already-loaded target \
         must reach the optimizing backend ({label})"
    );
    let buf = make_object(&[tag]);
    let obj = buf.as_ptr() as i64;
    let ir_r = call_with_dummy_context(&ir, &[obj]);
    let sp_r = call_with_dummy_context(&sp, &[obj]);
    assert_eq!(ir_r, sp_r, "IR and single-pass must agree ({label})");
    let expected = if succeeds { obj } else { i64::MIN };
    assert_eq!(ir_r, expected, "wrong checkcast result ({label})");
}

#[test]
fn ir_vs_singlepass_checkcast_exact_class_succeeds() {
    assert_checkcast_case(1, "pkg/Sub", true, "exact class");
}

#[test]
fn ir_vs_singlepass_checkcast_subclass_succeeds() {
    assert_checkcast_case(1, "pkg/Base", true, "subclass");
}

#[test]
fn ir_vs_singlepass_checkcast_interface_succeeds() {
    assert_checkcast_case(1, "pkg/Iface", true, "interface");
}

#[test]
fn ir_vs_singlepass_checkcast_definitive_refusal_returns_sentinel() {
    // The result IR/single-pass must agree on is the raw i64::MIN sentinel —
    // full VM-level exception dispatch (pending-exception drain via
    // `has_dispatch` + `catch_unwind`) is outside this codegen-focused
    // harness, exactly as `ir_vs_singlepass_invokestatic_exception_sentinel`
    // above only checks the sentinel propagates, not the full unwind.
    assert_checkcast_case(2, "pkg/Sub", false, "miss (unrelated class)");
}

#[test]
fn ir_vs_singlepass_checkcast_null_always_succeeds() {
    // JVMS 6.5: a null reference is always a valid checkcast target.
    let mut helpers = dummy_helpers();
    helpers.checkcast = checkcast_stub as *const () as usize;
    let cm = checkcast_method();
    let name_resolver = name_resolver_for("pkg/Sub");
    let ir = compile_checkcast(&cm, &helpers, true, &loaded_resolver, &name_resolver)
        .expect("IR checkcast");
    let sp = compile_checkcast(&cm, &helpers, false, &loaded_resolver, &name_resolver)
        .expect("single-pass checkcast");
    assert!(ir.used_ir_backend);
    assert_eq!(call_with_dummy_context(&ir, &[0]), 0);
    assert_eq!(call_with_dummy_context(&sp, &[0]), 0);
}

#[test]
fn ir_vs_singlepass_checkcast_not_yet_loaded_refuses_ir() {
    let mut helpers = dummy_helpers();
    helpers.checkcast = checkcast_stub as *const () as usize;
    let cm = checkcast_method();
    let deferred_resolver = |cp: u16| -> Option<cratonvm_jit::JitNewSite> {
        (cp == 1).then_some(cratonvm_jit::JitNewSite::Deferred {
            holder_class_id: 0x77,
            cp_idx: cp,
        })
    };
    let name_resolver = name_resolver_for("pkg/Sub");
    let sp = compile_checkcast(&cm, &helpers, true, &deferred_resolver, &name_resolver)
        .expect("not-yet-loaded target must still compile, via single-pass fallback");
    // 2026-09-06: this assertion was INVERTED, deliberately. A `checkcast`
    // whose target class is not loaded is now replaced by an uncommon trap and
    // the rest of the method compiles -- the class has never been loaded, so no
    // path that has ever executed reached this site, which is a proof of
    // coldness rather than a guess. `CRATONVM_JIT_IR_SITE_TRAP=0` restores the
    // refusal, and that is what this asserts against rather than a constant.
    // The unresolved-class trap is OPT-IN and off by default, and THIS TEST is
    // why: it executes the very path the trap would sit on, so with the trap
    // planted the compiled body returns the deopt sentinel instead of the
    // object -- forever, on every call. See
    // `ir::ir_unresolved_class_trap_enabled`.
    assert_eq!(
        sp.used_ir_backend,
        cratonvm_jit::ir::ir_unresolved_class_trap_enabled(),
        "by default a not-yet-loaded checkcast still refuses IR admission for the          whole method; the trap is reachable only under the opt-in"
    );
    let buf = make_object(&[1]);
    let obj = buf.as_ptr() as i64;
    assert_eq!(call_with_dummy_context(&sp, &[obj]), obj);
}

#[test]
fn ir_vs_singlepass_mixed_checkcast_and_instanceof_in_one_method() {
    // The shape cov-05 newly allows: BOTH opcodes in one method, at two
    // different pcs referencing the SAME cp index — proving `lib.rs`'s split
    // of `scan.typecheck_ops` into `checkcast_info`/`instanceof_info` is
    // keyed by PC, not by cp_idx, and that admitting `checkcast` no longer
    // refuses a method containing `instanceof` too (the pre-cov-05-checkcast
    // shape, where ANY checkcast refused the WHOLE method).
    //
    //   int f(Object a, Object b) {
    //     int t = a instanceof <name>;
    //     <name> unused = (<name>) b;   // discarded; only its side effect
    //                                    // (throw-or-not) matters
    //     return t;
    //   }
    let code = vec![
        0x2a, // aload_0 (a)
        0xc1, 0x00, 0x01, // instanceof #1
        0x3d, // istore_2 (t)
        0x2b, // aload_1 (b)
        0xc0, 0x00, 0x01, // checkcast #1
        0x57, // pop
        0x1c, // iload_2
        0xac, // ireturn
    ];
    let cm = cached("f", "(Ljava/lang/Object;Ljava/lang/Object;)I", code, 3, 2);
    let mut helpers = dummy_helpers();
    helpers.instanceof_check = instanceof_stub as *const () as usize;
    helpers.checkcast = checkcast_stub as *const () as usize;
    let name_resolver = name_resolver_for("pkg/Sub");
    let ir = compile_instanceof(&cm, &helpers, true, &loaded_resolver, &name_resolver)
        .expect("IR: a method mixing checkcast and instanceof must still compile");
    let sp = compile_instanceof(&cm, &helpers, false, &loaded_resolver, &name_resolver)
        .expect("single-pass: mixed method");
    assert!(
        ir.used_ir_backend,
        "cov-05: a method with BOTH checkcast and instanceof must reach the \
         optimizing backend — checkcast no longer refuses the whole method"
    );
    // a=Sub (tag 1): instanceof pkg/Sub -> true (1). b=Sub (tag 1): checkcast
    // pkg/Sub succeeds (pop'd, no effect on the result). Expected: 1.
    let a = make_object(&[1]);
    let b_ok = make_object(&[1]);
    let (a_ptr, b_ok_ptr) = (a.as_ptr() as i64, b_ok.as_ptr() as i64);
    let ir_r = call_with_dummy_context(&ir, &[a_ptr, b_ok_ptr]);
    let sp_r = call_with_dummy_context(&sp, &[a_ptr, b_ok_ptr]);
    assert_eq!(ir_r, sp_r);
    assert_eq!(ir_r, 1, "instanceof true, checkcast succeeds");
    // Same `a`, but b=Other (tag 2): checkcast pkg/Sub on `b` fails AFTER the
    // instanceof/istore already ran — the sentinel must override the normal
    // `iload_2; ireturn` result.
    let b_bad = make_object(&[2]);
    let b_bad_ptr = b_bad.as_ptr() as i64;
    let ir_r2 = call_with_dummy_context(&ir, &[a_ptr, b_bad_ptr]);
    let sp_r2 = call_with_dummy_context(&sp, &[a_ptr, b_bad_ptr]);
    assert_eq!(ir_r2, sp_r2);
    assert_eq!(
        ir_r2,
        i64::MIN,
        "the checkcast failure after the instanceof must still bail the method"
    );
}

// ── cov-07: athrow ────────────────────────────────────────────────────────
//
// `cov-07-athrow-RETIRED-20260804.md`. Before this lane, `scan.has_athrow`
// refused every method containing an `athrow` (0xbf) from the optimizing
// pipeline outright — the blanket exclusion this lane removes. `Op::Throw`
// reuses the exact `jit_throw_exception(exc_ptr, bci) -> i64::MIN` call and
// sentinel-drain protocol the single-pass backend's own `0xbf` arm already
// uses (see `Op::Throw`'s doc comment in `ir.rs`), so the correctness case
// that matters HERE is: does the IR-compiled method produce the IDENTICAL raw
// sentinel a single-pass compile of the same bytecode does. End-to-end
// handler-dispatch correctness (does the interpreter's
// `route_jit_exception_through_method` actually run the right `catch`/
// `finally`) is a VM-level question this jit-crate-only harness cannot
// exercise — see `vm/tests/jit_local_exception_handler_tests.rs`, whose
// existing `JitLocalHandler.java`/`AthrowCountBisect.java` golden-checksum
// suite is real Java containing real `throw` statements and now exercises
// this lane's lowering directly once a method tiers up to C2 (or is forced
// there with `CRATONVM_JIT_FORCE_C2=1`).

/// `static void f() { throw null; }` — `aconst_null; athrow`. No exception
/// table at all: the simplest possible admission case, and it pins that a
/// throw-only method (no `Op::Return` anywhere in the graph) does not trip
/// `ir_verify`'s reachability lane, which used to require a live `Op::Return`
/// unconditionally.
fn unconditional_athrow_code() -> Vec<u8> {
    vec![
        0x01, // 0: aconst_null
        0xbf, // 1: athrow
    ]
}

#[test]
fn athrow_no_longer_refuses_ir_admission() {
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let mut helpers = dummy_helpers();
    // A real `jit_throw_exception` observable-behaviour stand-in: always
    // returns the `i64::MIN` deopt sentinel, exactly like the production
    // helper (`vm/src/jit/helpers.rs`) does on every path. `dummy_helpers`'s
    // default `throw_exception: s` is a panicking stub — swapped out here
    // because this test, unlike the admission-only try/catch tests above,
    // actually EXECUTES the throw.
    unsafe extern "C" fn always_sentinel(_exc_ptr: i64, _bci: i64) -> i64 {
        i64::MIN
    }
    helpers.throw_exception = always_sentinel as *const () as usize;
    let cm = cached("f", "()V", unconditional_athrow_code(), 0, 0);
    let ir = compile_opt(&cm, &helpers, true)
        .expect("a throw-only method must still compile on the optimizing tier");
    assert!(
        ir.used_ir_backend,
        "cov-07: `scan.has_athrow` must no longer refuse the whole method — \
         `Op::Throw` gives the IR builder a real lowering"
    );
    let sp = compile_opt(&cm, &helpers, false).expect("single-pass compiles");
    // SAFETY: both bodies were produced by the JIT from valid bytecode into
    // executable memory; `always_sentinel` never dereferences its arguments.
    let r_ir = unsafe { ir.try_call(&[]) }.expect("IR call");
    let r_sp = unsafe { sp.try_call(&[]) }.expect("single-pass call");
    assert_eq!(
        r_ir,
        i64::MIN,
        "an IR-compiled unconditional throw must propagate the deopt/exception \
         sentinel, exactly like a plain return would propagate a value"
    );
    assert_eq!(
        r_ir, r_sp,
        "IR vs single-pass must agree on the raw sentinel for an identical throw"
    );
}

/// `static int f(int n) { if (n > 0) return n; throw null; }` — the throw sits
/// on a branch nothing in this test's cases takes, exactly the "handler /
/// throw is unreachable, only the reachable code is asserted" shape the
/// try/catch RBC.6 tests above use. Pins that ADMITTING an athrow does not
/// disturb the surrounding method's ordinary control flow, DCE, scheduler or
/// escape analysis — `Op::Throw` must be seeded as a DCE root, classified as
/// a control node by the scheduler, and excluded from
/// `program_order_proves_dominance`'s dominance stand-in exactly as
/// `Op::Return` is (see the plumbing this lane touched).
fn athrow_never_taken_code() -> Vec<u8> {
    vec![
        0x1a, // 0: iload_0
        0x9d, 0x00, 0x05, // 1: ifgt +5 -> 6
        0x01, // 4: aconst_null
        0xbf, // 5: athrow
        0x1a, // 6: iload_0
        0xac, // 7: ireturn
    ]
}

#[test]
fn athrow_never_taken_branch_matches_single_pass() {
    cratonvm_jit::x64::set_moving_young_override(Some(false));
    let mut helpers = dummy_helpers();
    unsafe extern "C" fn always_sentinel(_exc_ptr: i64, _bci: i64) -> i64 {
        i64::MIN
    }
    helpers.throw_exception = always_sentinel as *const () as usize;
    let cm = cached("f", "(I)I", athrow_never_taken_code(), 1, 1);
    let ir = compile_opt(&cm, &helpers, true)
        .expect("a method with a never-taken athrow must compile on the optimizing tier");
    assert!(
        ir.used_ir_backend,
        "cov-07: athrow no longer refuses IR admission"
    );
    let sp = compile_opt(&cm, &helpers, false).expect("single-pass compiles");
    for n in [1i64, 7, 1000] {
        // SAFETY: pure-int bytecode; the athrow branch is never taken by these
        // inputs, so `always_sentinel` is never reached.
        let r_ir = unsafe { ir.try_call(&[n]) }.expect("IR call");
        let r_sp = unsafe { sp.try_call(&[n]) }.expect("single-pass call");
        assert_eq!(
            r_ir as i32, r_sp as i32,
            "IR vs single-pass diverge for n={n}"
        );
        assert_eq!(r_ir as i32, n as i32, "wrong result for n={n}");
    }
}

// ── IR-tier inlining: the arm the designated gate could not see ─────────────
//
// The 2026-08-28 IR-inline gauntlet soak lists this as blocker 2 for flipping
// `CRATONVM_JIT_IR_INLINE` on: a differential harness that runs the inliner
// zero times cannot gate it, and it is the named gate. It ran zero times for a
// structural reason — every `try_compile`
// above passes `None` for `inline_resolver`, so the splicer is never handed a
// callee body and has nothing to inline, whatever the flag says.
//
// Supplying a resolver is the whole fix. What matters as much is that these
// tests FAIL if inlining stops happening: a differential that silently reverts
// to comparing two un-inlined bodies is exactly the vacuity being repaired, and
// it would look green forever.

/// A static `int callee(int a, int b) { return a * 3 + b; }` as raw bytecode.
///
/// `iload_0; iconst_3; imul; iload_1; iadd; ireturn`
fn inline_callee_site() -> InlineSite {
    let code = vec![0x1a, 0x06, 0x68, 0x1b, 0x60, 0xac, 0x00, 0x00];
    InlineSite {
        callee_code_len: code.len() - 2,
        callee_code: code,
        callee_max_locals: 2,
        callee_num_args: 2,
        callee_is_static: true,
        return_type: b'I',
        class_name: "GateCallee".to_string(),
        // `append_ir_inline_site` re-derives the argument slots from this, so an
        // empty descriptor (what `..Default::default()` leaves) makes it refuse
        // the site and plan zero — silently, which is how the first draft of
        // this test "passed" while nothing was ever spliced.
        descriptor: "(II)I".to_string(),
        ..InlineSite::default()
    }
}

/// Compile with a real `inline_resolver`, and with `CRATONVM_JIT_IR_INLINE`
/// forced to `on`/`off` for this thread only.
///
/// `ir_inline_enabled()` reads through `flags::runtime_var` and is not cached,
/// which is what makes a per-thread override reach it — the property the flag
/// inventory exists to guarantee, used here rather than described.
fn compile_inline_arm(
    cm: &CachedBytecodeMethod,
    helpers: &JitRuntimeHelpers,
    optimize: bool,
    inline_on: bool,
) -> Option<CompiledMethod> {
    let site = inline_callee_site();
    let ir_inline_resolver =
        |_c: &str, _m: &str, _d: &str| -> Option<InlineSite> { Some(site.clone()) };
    let invoke_resolver = |_idx: u16| -> Option<(String, String, String)> {
        Some((
            "GateCallee".to_string(),
            "callee".to_string(),
            "(II)I".to_string(),
        ))
    };
    // `try_compile_with_invokespecial_resolver`, not `try_compile`: the narrow
    // wrapper every other test here uses has NO `ir_inline_resolver` parameter,
    // so the IR splicer can never be handed a callee body through it. That —
    // and not the flag's default — is why this harness "runs the inliner zero
    // times" and could not gate `CRATONVM_JIT_IR_INLINE`.
    //
    // Note the two resolvers are different questions: `inline_resolver` feeds
    // the SINGLE-PASS inliner, `ir_inline_resolver` the IR splicer. Supplying
    // the first and expecting the second to splice is the mistake this comment
    // exists to stop repeating — it produces a body that differs (a plan was
    // built) while nothing was ever spliced.
    cratonvm_types::flags::with_thread_overrides(
        &[(
            "CRATONVM_JIT_IR_INLINE",
            Some(if inline_on { "1" } else { "0" }),
        )],
        || {
            cratonvm_jit::try_compile_with_invokespecial_resolver(
                cm,                        // 0 cached
                None,                      // 1 cp_class_name_resolver
                None,                      // 2 cp_field_resolver
                None,                      // 3 cp_static_field_resolver
                Some(&invoke_resolver),    // 4 cp_invoke_resolver
                None,                      // 5 cp_invokespecial_owner_resolver
                None,                      // 6 callee_compiler
                None,                      // 7 cp_new_resolver
                None,                      // 8 cp_ldc_resolver
                None,                      // 9 cp_ldc2w_resolver
                None,                      // 10 profile
                helpers,                   // 11 helpers
                None,                      // 12 inline_resolver (single-pass)
                None,                      // 13 string_layout_resolver
                None,                      // 14 cp_invoke_class_id_resolver
                None,                      // 15 cp_elidable_init_resolver
                optimize,                  // 16 optimize
                true,                      // 17 ir_emit_calls
                true,                      // 18 ir_emit_special_calls
                false,                     // 19 ir_emit_long
                false,                     // 20 ir_emit_virtual_calls
                false,                     // 21 ir_emit_fp
                None,                      // 22 cp_invokedynamic_descriptor_resolver
                None,                      // 23 class_id_name_resolver
                None,                      // 24 receiver_inline_resolver
                Some(&ir_inline_resolver), // 25 ir_inline_resolver  <- the point
                false,                     // 26 jdk_only
                None,                      // 27 intrinsic_resolver
                None,                      // 28 despec
                None,                      // 29 cp_invoke_declaring_class_resolver
            )
        },
    )
}

/// `int caller(int x) { return callee(x, 7) + 1; }`
///
/// `iload_0; bipush 7; invokestatic #1; iconst_1; iadd; ireturn`
fn inline_caller_code() -> Vec<u8> {
    vec![
        0x1a, 0x10, 0x07, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac, 0x00, 0x00,
    ]
}

/// The gate can now SEE the flag: with a callee body available, turning
/// `CRATONVM_JIT_IR_INLINE` on changes the emitted body.
///
/// A cheap canary, and NOT on its own proof that anything was spliced: while
/// this test was being written it passed twice with zero splices, because
/// building an inline PLAN perturbs the emitted code even when every site is
/// then refused. Both times the compiler's own `[ir] inline-plan` line was the
/// thing that said so. The real engagement proof is
/// `ir_inline_agrees_with_the_host` below, which cannot run at all unless the
/// call was spliced away.
#[test]
fn ir_inline_flag_changes_the_emitted_body() {
    let helpers = dummy_helpers();
    let cm = cached("caller", "(I)I", inline_caller_code(), 1, 1);
    let on = compile_inline_arm(&cm, &helpers, true, true)
        .expect("IR pipeline must compile the caller with inlining on");
    let off = compile_inline_arm(&cm, &helpers, true, false)
        .expect("IR pipeline must compile the caller with inlining off");
    assert_ne!(
        on.code_bytes(),
        off.code_bytes(),
        "CRATONVM_JIT_IR_INLINE changed nothing about the emitted code, so this          harness is not exercising the inliner and cannot gate it — which is          exactly the vacuity ir-inline-gauntlet-soak-20260828 recorded",
    );
}

/// And the correctness half: the spliced body must compute what the source says.
///
/// Only the INLINED arm is executed, and that is not a shortcut — it is the
/// same fact that kept this harness from gating the flag. With inlining off the
/// caller emits a real `invokestatic`, and this file wires no runtime helpers
/// (`dummy_helpers` panics on any call), so the un-inlined arm cannot be run
/// here at all. Splicing is what makes the body self-contained.
///
/// That also makes this test its own engagement check, which is the property
/// the soak asked for: if the splice stops happening, the body keeps its
/// `invokestatic`, `dummy_helpers` panics on the call, and the test aborts. It
/// is not possible for this to pass while the inliner is inert. (Observed:
/// with `InlineSite::descriptor` left empty, `append_ir_inline_site` refuses
/// every site and this test aborts on exactly that panic.)
///
/// So the anchor is the host value rather than the other backend:
/// `callee(x, 7) + 1` = `x * 3 + 7 + 1`. A splicer that drops an argument,
/// mis-maps a callee local onto a caller slot, or returns the wrong stack entry
/// fails this — those are the shapes the 2026-08-28 `InternalError` regression
/// came from.
#[test]
fn ir_inline_agrees_with_the_host() {
    let helpers = dummy_helpers();
    let cm = cached("caller", "(I)I", inline_caller_code(), 1, 1);
    let on = compile_inline_arm(&cm, &helpers, true, true).expect("inlining on must compile");
    for x in [0i64, 1, -1, 7, -13, 1000, -100000] {
        let want = (x as i32).wrapping_mul(3).wrapping_add(8);
        // SAFETY: as `check` above — a JIT-produced body, System V i64 entry
        // ABI, and after splicing no runtime helper is reachable for this shape.
        let got = unsafe { on.try_call(&[x]) }.expect("inlined call") as i32;
        assert_eq!(got, want, "spliced body disagrees with the host for x={x}");
    }
}
