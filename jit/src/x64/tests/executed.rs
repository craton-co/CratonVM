#[allow(unused_imports)]
use super::*;

/// r21 barriers (BARRIERS21-1): the single-pass G1 gated `aastore`'s barrier
/// bytes, EXECUTED in the order the arm emits them. First the SATB gate
/// (`emit_sp_aastore_satb_gate_skip`), whose fall-through is the old-element
/// load and the `satb_pre_write_barrier` call. Then, after the store, the F-08
/// filter (`emit_g1_barrier_filter`), whose fall-through is the lean
/// `jit_g1_post_write_barrier` call.
///
/// ```text
///   <gate skip>  ; armed: mov eax, 2 ; ret        "the SATB call runs"
///   mov rax, arg0 ; mov rdx, arg1
///   <filter>     ; mov eax, 1 ; ret               "the post call runs"
/// nothing:
///   xor eax, eax ; ret                            "no barrier"
/// ```
///
/// An armed gate answers 2 for every store, null and same-region ones
/// included: the old element is logged whatever the new value is. A clear gate
/// gives 0 for a null or same-region value and 1 for a cross-region one, on an
/// arena that is not region-aligned.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::x64::tests) fn r21_barriers_sp_g1_aastore_gate_then_filter_when_executed() {
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    const REGION_SIZE: usize = 0x10_0000;
    const ARENA_BASE: usize = 0x4000_0000 + 0x30;
    const ARENA_LEN: usize = 8 * REGION_SIZE;
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static GATE: AtomicU8 = AtomicU8::new(0);
    TABLE[0].store(ARENA_BASE, Ordering::Release);
    TABLE[2].store(!(REGION_SIZE - 1), Ordering::Release);
    TABLE[1].store(ARENA_LEN, Ordering::Release);
    GATE.store(0, Ordering::Release);

    let mut helpers = test_helpers();
    helpers.g1_barrier_addr = TABLE.as_ptr() as usize; // Cast: the table's address
    let gate_addr = &GATE as *const AtomicU8 as usize; // Cast: the gate's address

    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "r21-barriers-sp-g1-aastore".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        helpers,
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );
    // A leaf entered after the prologue `Compiler::new` emitted (see the F-08
    // filter test above).
    let entry_off = compiler.buf.pos();
    let skip = compiler.emit_sp_aastore_satb_gate_skip(gate_addr);
    compiler.emit_mov_imm64(RAX, 2);
    compiler.emit_ret();
    compiler.patch_rel32_to_here(skip);
    compiler.emit_mov_r64_r64(RAX, ARG_REGS[0]);
    compiler.emit_mov_r64_r64(RDX, ARG_REGS[1]);
    let nothing = compiler.emit_g1_barrier_filter(RAX, RDX, RCX);
    compiler.emit_mov_imm64(RAX, 1);
    compiler.emit_ret();
    for patch in nothing {
        compiler.patch_rel32_to_here(patch);
    }
    compiler.emit_xor_reg_self(RAX);
    compiler.emit_ret();
    assert!(!compiler.buf.overflowed(), "the test buffer must hold the snippet");
    crate::platform::make_executable(compiler.buf.as_ptr() as *mut u8, compiler.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { compiler.buf.as_ptr().add(entry_off) };
    // SAFETY: the snippet takes two i64 arguments, reads only GATE and TABLE,
    // clobbers RAX/RCX/RDX/R11 (caller-saved on both ABIs) and returns in RAX;
    // `compiler` (and so the buffer) outlives every call below.
    let f: extern "C" fn(i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };

    let r0 = ARENA_BASE + 8;
    let r1 = ARENA_BASE + REGION_SIZE + 8;
    let same_lo = ARENA_BASE + REGION_SIZE - 0x38;
    let same_hi = same_lo + 0x10;
    let last_of_r0 = ARENA_BASE + REGION_SIZE - 8;
    let first_of_r1 = ARENA_BASE + REGION_SIZE + 8;
    let cases: [(usize, usize, i64, &str); 5] = [
        (r0, 0, 0, "a null store records nothing"),
        (r0, r0 + 64, 0, "a same-region store records nothing"),
        (r0, r1, 1, "a cross-region store reaches the lean barrier"),
        (same_lo, same_hi, 0, "one region across an aligned block boundary"),
        (last_of_r0, first_of_r1, 1, "two regions inside one aligned block"),
    ];
    for (array, value, want, why) in cases {
        assert_eq!(f(array as i64, value as i64), want, "gate clear: {why}"); // Cast: test addresses
    }
    GATE.store(1, Ordering::Release);
    for (array, value, _, why) in cases {
        assert_eq!(
            f(array as i64, value as i64), // Cast: test addresses
            2,
            "gate armed: the SATB call runs first ({why})"
        );
    }
    GATE.store(0, Ordering::Release);
    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

/// r21 barriers (BARRIERS21-1 / -2): the G1 gated-`aastore` plan is G1's
/// alone. It needs the pre gate, the live G1 table and the lean barrier. A
/// helper table that publishes any post-barrier word (a post gate, a floor, a
/// mask: the Generational and ZGC plans) or a card view gets no plan, whatever
/// the process-wide G1 table says.
#[test]
pub(in crate::x64::tests) fn r21_barriers_the_g1_aastore_plan_is_g1_s_alone() {
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
    use super::objects::{g1_aastore_plan_of, G1AastorePlan};
    static TABLE: [AtomicUsize; 5] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    static PRE: AtomicU8 = AtomicU8::new(0);
    static POST: AtomicU8 = AtomicU8::new(1);
    let table = TABLE.as_ptr() as usize; // Cast: the table's address
    let pre = &PRE as *const AtomicU8 as usize; // Cast: the gate's address
    let post = &POST as *const AtomicU8 as usize; // Cast: the gate's address
    let lean = 0x1234_5678usize;

    let mut h = test_helpers();
    h.ref_store_pre_gate = pre;
    h.ref_store_post_gate = 0;
    h.ref_store_post_young_floor = 0;
    h.ref_store_post_skip_mask = 0;
    h.jit_card_table_addr = 0;
    h.g1_barrier_addr = table;
    h.g1_post_write_barrier = lean;
    assert_eq!(g1_aastore_plan_of(&h), None, "an unpublished G1 table is not live");

    TABLE[0].store(0x4000_0030, Ordering::Release);
    TABLE[2].store(!(0x10_0000usize - 1), Ordering::Release);
    TABLE[1].store(0x80_0000, Ordering::Release);
    assert_eq!(
        g1_aastore_plan_of(&h),
        Some(G1AastorePlan { pre, table, post: lean }),
        "G1's helper table: the pre gate, no plan, no view"
    );

    let mut gen_h = h;
    gen_h.ref_store_post_gate = post;
    gen_h.ref_store_post_skip_mask = usize::from(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(g1_aastore_plan_of(&gen_h), None, "a Generational plan is not G1");
    let mut zgc = h;
    zgc.ref_store_post_gate = post;
    zgc.ref_store_post_young_floor = post;
    assert_eq!(g1_aastore_plan_of(&zgc), None, "a ZGC plan is not G1");
    let mut half = h;
    half.ref_store_post_gate = post;
    assert_eq!(g1_aastore_plan_of(&half), None, "any post word declines");
    let mut no_pre = h;
    no_pre.ref_store_pre_gate = 0;
    assert_eq!(g1_aastore_plan_of(&no_pre), None, "no pre gate: no SATB proof");
    let mut no_lean = h;
    no_lean.g1_post_write_barrier = 0;
    assert_eq!(g1_aastore_plan_of(&no_lean), None, "no lean barrier: nothing to call");

    for w in TABLE.iter() {
        w.store(0, Ordering::Release);
    }
}

// -----------------------------------------------------------------------
// JIT r22 INLINE22-3 (`jit-r10-splice-proposals.md` section 4): a REAL
// point published inside a live splice, and the rollback it must cause
// -----------------------------------------------------------------------

/// `keyed_int_method` with inline sites: the splice postcondition's
/// enclosing-owner clause needs a non-empty root key.
pub(in crate::x64::tests) fn keyed_int_method_with_inlines(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    method_key: &str,
    inline_sites: HashMap<usize, crate::InlineSite>,
) -> Option<CompiledMethod> {
    let param_jvm_slots: Vec<usize> = (0..num_params).collect();
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code_len,
        num_params,
        num_params,
        false,
        Vec::new(),         // multianewarray_info
        Vec::new(),         // field_info
        Vec::new(),         // typecheck_info
        Vec::new(),         // static_field_info
        Vec::new(),         // new_info
        Vec::new(),         // new_deferred_info
        Vec::new(),         // anewarray_info
        Vec::new(),         // anewarray_deferred_info
        Vec::new(),         // invoke_info
        Vec::new(),         // direct_calls
        Vec::new(),         // mic_slots
        Vec::new(),         // pic_slots
        Vec::new(),         // ldc_info
        Vec::new(),         // ldc_string_info
        Vec::new(),         // ldc_class_info
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &param_jvm_slots,
        num_params,
        0, // param_oop_mask: int parameters only
        Vec::new(),
        method_key,
        None, // despec: no VM, so no despeculation verdicts
        Vec::new(),
        None, // elidable_init_pcs: no constant pool, so nothing is proven empty,
        crate::x64::BackendRequest::default(),
    )
}

// -----------------------------------------------------------------------
// `CRATONVM_JIT_SP_SPLICE_TYPECHECK`: a spliced body's checkcast / instanceof
// -----------------------------------------------------------------------

/// `int f(Object o) { return g(o); }` (or `Object`, for `checkcast`) with
/// `g(o)`, `return o instanceof <name>` / `return (<name>) o`, spliced at pc 1
/// through the trusted-oop door the top-level fixtures use. The callee's row
/// is what the resolver hands the single-pass splice: the target resolved
/// through the callee's constant pool (`ir_typecheck_info`).
pub(in crate::x64::tests) fn spliced_typecheck_fixture(
    helpers: &JitRuntimeHelpers,
    name: &str,
    target_class_id: u32,
    checkcast: bool,
) -> Option<CompiledMethod> {
    let ret = if checkcast { 0xb0 } else { 0xac }; // areturn / ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb8, 0x00, 0x02, // 1: invokestatic #2
        ret,  // 4
        0, 0,
    ];
    let desc: &'static str = if checkcast {
        "(Ljava/lang/Object;)Ljava/lang/Object;"
    } else {
        "(Ljava/lang/Object;)I"
    };
    let ret_tag = if checkcast { b'L' } else { b'I' };
    let callee = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "g",
        descriptor: desc,
        num_jit_args: 1,
        return_type: ret_tag,
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let op = if checkcast { 0xc0 } else { 0xc1 };
    let mut site = make_inline_site(&[0x2a, op, 0x00, 0x01, ret], 1, 1, true, ret_tag);
    site.descriptor = desc.to_string();
    site.needs_heap = true;
    site.ir_typecheck_info = vec![(1, target_class_id, name.to_string(), checkcast)];
    let mut inline_sites = HashMap::new();
    inline_sites.insert(1usize, site);
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,
        1,
        true,       // needs_heap
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info: the caller has none of its own
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(1usize, callee as *const JitInvokeInfo)], // invoke_info: the unspliced call
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc_string_info
        Vec::new(), // ldc_class_info
        Vec::new(), // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        std::collections::HashSet::new(),
        inline_sites,
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &[],
        0,
        0b1, // param_oop_mask: the parameter is a reference
        Vec::new(),
        if checkcast {
            "T.f:(Ljava/lang/Object;)Ljava/lang/Object;"
        } else {
            "T.f:(Ljava/lang/Object;)I"
        },
        None,
        Vec::new(),
        None,
        crate::x64::BackendRequest::default(),
    )
}

/// A spliced `instanceof` is the top-level arm's: null and an exact
/// plain-object match answer inline, everything else asks the helper and
/// returns its answer, and the call it replaced is gone.
#[test]
pub(in crate::x64::tests) fn a_spliced_instanceof_answers_like_the_top_level_arm() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if !checkcast_inline_enabled() {
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn marker_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        1
    }
    let mut helpers = test_helpers();
    helpers.instanceof_check = marker_instanceof as *const () as usize;
    const TARGET: u32 = 0x0027_1001;
    let compiled = spliced_typecheck_fixture(&helpers, "sp27/SplicedInstanceofTarget", TARGET, false)
        .expect("a body whose instanceof target resolved must compile spliced");
    assert_eq!(
        calls_to(&compiled, helpers.invoke_dispatch),
        0,
        "the call to the callee must be spliced away"
    );
    assert_eq!(calls_to(&compiled, helpers.instanceof_check), 1);

    HITS.store(0, Ordering::SeqCst);
    let exact = fake_typecheck_header(TARGET, 0);
    let other = fake_typecheck_header(TARGET + 1, 0);
    for (arg, want, hits, what) in [
        (0i64, 0i64, 0usize, "null"),
        (exact.as_ptr() as i64, 1, 0, "an exact match"), // Cast: object address as JIT argument
        (other.as_ptr() as i64, 1, 1, "another class"), // Cast: object address as JIT argument
    ] {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let got = unsafe { compiled.try_call_with_context(test_vm_ctx(), &[arg]).expect("jit call") };
        assert_eq!(got, want, "{what}");
        assert_eq!(HITS.load(Ordering::SeqCst), hits, "{what}: helper calls so far");
    }
}

/// A spliced `checkcast`: the exact match passes the object through inline,
/// another class takes the helper and returns what it returns.
#[test]
pub(in crate::x64::tests) fn a_spliced_checkcast_answers_like_the_top_level_arm() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if !checkcast_inline_enabled() {
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn marker_checkcast(_vm: i64, obj: i64, _name: i64, _len: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        obj
    }
    let mut helpers = test_helpers();
    helpers.checkcast = marker_checkcast as *const () as usize;
    const TARGET: u32 = 0x0027_1002;
    let compiled = spliced_typecheck_fixture(&helpers, "sp27/SplicedCheckcastTarget", TARGET, true)
        .expect("a body whose checkcast target resolved must compile spliced");
    assert_eq!(calls_to(&compiled, helpers.invoke_dispatch), 0);
    assert_eq!(calls_to(&compiled, helpers.checkcast), 1);

    HITS.store(0, Ordering::SeqCst);
    let exact = fake_typecheck_header(TARGET, 0);
    let other = fake_typecheck_header(TARGET + 1, 0);
    for (arg, hits, what) in [
        (0i64, 0usize, "null"),
        (exact.as_ptr() as i64, 0, "an exact match"), // Cast: object address as JIT argument
        (other.as_ptr() as i64, 1, "another class"), // Cast: object address as JIT argument
    ] {
        // SAFETY: as above.
        let got = unsafe { compiled.try_call_with_context(test_vm_ctx(), &[arg]).expect("jit call") };
        assert_eq!(got, arg, "{what} passes through");
        assert_eq!(HITS.load(Ordering::SeqCst), hits, "{what}: helper calls so far");
    }
}

/// A row without a class id (or none at all) has nothing to intern: the
/// splice bails and the call stays.
#[test]
pub(in crate::x64::tests) fn a_spliced_typecheck_without_a_resolved_target_keeps_the_call() {
    let mut helpers = test_helpers();
    helpers.instanceof_check = 0x1234_5678;
    let compiled = spliced_typecheck_fixture(&helpers, "sp27/Unresolved", 0, false)
        .expect("the spliced call compiles");
    assert_eq!(calls_to(&compiled, helpers.instanceof_check), 1);
    assert_eq!(calls_to(&compiled, helpers.invoke_dispatch), 0);
}

/// j27 SPREVIEW: the helper call of a spliced `checkcast` / `instanceof` files
/// an inline-frame row, so a walk taken in the helper (the
/// `ClassCastException`'s trace) names the spliced callee at the type check's
/// bci, under the caller's invoke bci (pc 1) -- not the caller alone.
#[test]
pub(in crate::x64::tests) fn j27_a_spliced_typecheck_helper_call_keeps_the_callee_frame() {
    if !crate::x64::inlining::inline_frame_map_enabled() {
        return;
    }
    let mut helpers = test_helpers();
    helpers.instanceof_check = 0x1234_5678;
    for (checkcast, name, target) in [
        (true, "sp27/RowCheckcastTarget", 0x0027_1003u32),
        (false, "sp27/RowInstanceofTarget", 0x0027_1004u32),
    ] {
        let compiled = spliced_typecheck_fixture(&helpers, name, target, checkcast)
            .expect("a body whose target resolved must compile spliced");
        assert_eq!(calls_to(&compiled, helpers.invoke_dispatch), 0, "{name}: spliced");
        let chain = compiled
            .inline_frame_map
            .chain_for_safepoint_bci(1)
            .unwrap_or_else(|| panic!("{name}: the helper call files no inline-frame row"));
        assert_eq!(chain.len(), 1, "{name}: one inlined level");
        assert!(
            chain[0].label.starts_with("Test.inlined:"),
            "{name}: innermost level names the callee, got {}",
            chain[0].label
        );
        assert_eq!(chain[0].bci, 1, "{name}: at the type check's callee bci");
    }
}

/// A spliced body `callee_code` (static, no arguments, one static-field row at
/// `field_pc`) called from a caller `invokestatic #2; <ret>` at pc 0.
pub(in crate::x64::tests) fn spliced_static_field_fixture(
    helpers: &JitRuntimeHelpers,
    callee_code: &[u8],
    field_pc: usize,
    ret_tag: u8,
) -> Option<CompiledMethod> {
    let ret = if ret_tag == b'V' { 0xb1 } else { 0xac }; // return / ireturn
    let code: Vec<u8> = vec![0xb8, 0x00, 0x02, ret, 0, 0];
    let desc: &'static str = if ret_tag == b'V' { "()V" } else { "()I" };
    let callee = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "g",
        descriptor: desc,
        num_jit_args: 0,
        return_type: ret_tag,
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut site = make_inline_site(callee_code, 0, 0, true, ret_tag);
    site.descriptor = desc.to_string();
    site.needs_heap = true;
    site.static_field_info = vec![(field_pc, 0x0027_2001u32, 0, b'I', false)];
    let mut inline_sites = HashMap::new();
    inline_sites.insert(0usize, site);
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        4,
        0,
        1,
        true,       // needs_heap
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info: the caller has none of its own
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        vec![(0usize, callee as *const JitInvokeInfo)], // invoke_info: the unspliced call
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc_string_info
        Vec::new(), // ldc_class_info
        Vec::new(), // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        helpers,
        std::collections::HashSet::new(),
        inline_sites,
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &[],
        0,
        0, // param_oop_mask: no parameters
        Vec::new(),
        if ret_tag == b'V' { "T.f:()V" } else { "T.f:()I" },
        None,
        Vec::new(),
        None,
        crate::x64::BackendRequest::default(),
    )
}

/// j27 NATMISC (page `j27-spliced-typecheck-helper-call-drops-the-callee-frame`,
/// field-helper half): the `jit_getstatic` / `jit_putstatic_*` call of a
/// spliced static access can run `<clinit>` and throw (EIIE / NCDFE), so it
/// files an inline-frame row naming the spliced callee at the access's bci.
#[test]
pub(in crate::x64::tests) fn j27_a_spliced_static_field_helper_call_keeps_the_callee_frame() {
    if !crate::x64::inlining::inline_frame_map_enabled() {
        return;
    }
    unsafe extern "C" fn marker_get(_vm: i64, _cid: i64, _idx: i64) -> i64 {
        0
    }
    unsafe extern "C" fn marker_put(_vm: i64, _cid: i64, _idx: i64, _v: i64) -> i64 {
        0
    }
    let mut helpers = test_helpers();
    helpers.getstatic = marker_get as *const () as usize;
    helpers.putstatic_int = marker_put as *const () as usize;
    // getstatic #1 ; ireturn   /   iconst_1 ; putstatic #1 ; return
    for (what, callee_code, field_pc, ret_tag, helper) in [
        ("getstatic", &[0xb2u8, 0x00, 0x01, 0xac][..], 0usize, b'I', helpers.getstatic),
        ("putstatic", &[0x04u8, 0xb3, 0x00, 0x01, 0xb1][..], 1, b'V', helpers.putstatic_int),
    ] {
        let compiled = spliced_static_field_fixture(&helpers, callee_code, field_pc, ret_tag)
            .unwrap_or_else(|| panic!("{what}: the body must compile"));
        assert_eq!(calls_to(&compiled, helpers.invoke_dispatch), 0, "{what}: spliced");
        assert_eq!(calls_to(&compiled, helper), 1, "{what}: through the helper");
        let chain = compiled
            .inline_frame_map
            .chain_for_safepoint_bci(0)
            .unwrap_or_else(|| panic!("{what}: the helper call files no inline-frame row"));
        assert_eq!(chain.len(), 1, "{what}: one inlined level");
        assert!(
            chain[0].label.starts_with("Test.inlined:"),
            "{what}: innermost level names the callee, got {}",
            chain[0].label
        );
        assert_eq!(chain[0].bci, field_pc as u32, "{what}: at the access's callee bci");
    }
}

/// zt t9/ca (Q4 of `zgc-zs5bt2-proposal-store-gate-next-steps-20261015.md`):
/// single-pass `aastore` on ZGC's age-floor plan, with a mark running
/// (`pre_active` set), skips calling `satb_pre_write_barrier` when the old
/// element is null, and calls it when the old element is non-null. The
/// generational mask plan keeps calling on a null old element.
#[test]
pub(in crate::x64::tests) fn zt9ca_single_pass_aastore_skips_the_satb_call_on_a_null_old_element() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SATB: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn type_check(_vm: i64, _array: i64, _val: i64) -> i64 {
        0
    }
    extern "C" fn marker_satb(_vm: i64, _old: i64) {
        SATB.fetch_add(1, Ordering::SeqCst);
    }
    extern "C" fn marker_write_barrier(_vm: i64, _obj: i64, _val: i64) {}
    if !gated_ref_store_enabled() || cratonvm_types::narrow_oop::ref_element_size() != 8 {
        return;
    }
    // pre = 1 (a mark runs), post = 0.
    let gates: *mut u8 = Box::leak(Box::new([1u8, 0u8])).as_mut_ptr();
    let floor: *mut u8 = Box::leak(Box::new(0x10u8));
    // static void f(Object[] a, Object v) { a[0] = v; }
    let code: Vec<u8> = vec![0x2a, 0x03, 0x2b, 0x53, 0xb1, 0, 0, 0];
    let helpers_of = |mask_plan: bool| {
        let mut helpers = test_helpers();
        helpers.aastore_type_check = type_check as *const () as usize;
        helpers.satb_pre_write_barrier = marker_satb as *const () as usize;
        helpers.write_barrier = marker_write_barrier as *const () as usize;
        helpers.ref_store_pre_gate = gates as usize;
        helpers.ref_store_post_gate = gates as usize + 1;
        if mask_plan {
            helpers.ref_store_post_young_floor = 0;
            helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;
        } else {
            helpers.ref_store_post_young_floor = floor as usize;
            helpers.ref_store_post_skip_mask = 0;
        }
        helpers
    };
    let (floor_helpers, mask_helpers) = (helpers_of(false), helpers_of(true));
    assert!(super::objects::zgc_sp_aastore_pre_arm_of(&floor_helpers));
    assert!(!super::objects::zgc_sp_aastore_pre_arm_of(&mask_helpers));
    assert!(!super::objects::zgc_sp_aastore_pre_arm_of(&test_helpers()));
    let compile = |mask_plan: bool| {
        let helpers = helpers_of(mask_plan);
        compile_with_request(
            BackendRequest::default(),
            Vec::new(),
            &code,
            5,
            2,
            2,
            true,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None,
        )
        .expect("aastore must compile")
    };
    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize;
    let slot = cratonvm_types::ARRAY_DATA_OFFSET / 8;
    for mask_plan in [false, true] {
        let compiled = compile(mask_plan);
        for old in [0u64, 0x5150] {
            for v in [0usize, val_addr] {
                let mut arr = Box::new([0x0000_0004_0000_0004u64; 8]);
                for w in arr.iter_mut().skip(2) {
                    *w = 0;
                }
                // SAFETY: the flags byte inside the boxed header.
                unsafe {
                    *(arr.as_mut_ptr() as *mut u8).add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = 0;
                }
                arr[slot] = old;
                SATB.store(0, Ordering::SeqCst);
                let args = [arr.as_ptr() as i64, v as i64];
                // SAFETY: compiled leaf method operating on the boxed array.
                unsafe {
                    compiled.call_with_heap(0, &args);
                }
                let skipped = !mask_plan && old == 0;
                assert_eq!(
                    SATB.load(Ordering::SeqCst),
                    usize::from(!skipped),
                    "mask_plan={mask_plan} old={old:#x} null_value={}",
                    v == 0
                );
                assert_eq!(
                    arr[slot] as usize,
                    v,
                    "mask_plan={mask_plan}: the element is stored"
                );
            }
        }
    }
    // SAFETY: reset leaked gate bytes.
    unsafe { gates.write_volatile(0) };
    drop(val);
}

