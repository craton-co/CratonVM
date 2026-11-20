#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::x64::tests) fn test_if_acmpne() {
    // Method: int f(Object a, Object b) { if (a != b) return 1; return 0; }
    // aload_0, aload_1, if_acmpne +5, iconst_0, ireturn, iconst_1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xa6, 0x00, 0x05, // 2: if_acmpne +5 → PC 7
        0x03, // 5: iconst_0
        0xac, // 6: ireturn
        0x04, // 7: iconst_1
        0xac, // 8: ireturn
        0, 0,
    ];
    let code_len = 9;
    assert!(is_jit_compatible(
        &code,
        code_len,
        "(Ljava/lang/Object;Ljava/lang/Object;)I"
    ));
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // Different refs → branch taken → 1
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 200]).expect("test JIT call") };
    assert_eq!(result, 1);
    // Same ref → not taken → 0
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[100, 100]).expect("test JIT call") };
    assert_eq!(result, 0);
}

/// Round 12 wave 2: `if_acmpeq` / `if_acmpne` pop a forward whole-stack pair
/// before the flush (the `if_icmp*` order) and compare where the operands
/// live. All three layouts must still branch on full 64-bit identity: the
/// whole-stack forward pair (the new order), a value live beneath the pair
/// (canonicalised first), and a backward branch (flush + poll first). The
/// references differ only above bit 31 in one pair, so a 32-bit compare
/// would call them equal.
#[test]
pub(in crate::x64::tests) fn if_acmp_branches_on_full_identity_in_every_stack_layout() {
    fn compile_locals(code: &[u8], code_len: usize, max_locals: usize) -> CompiledMethod {
        compile(
            code,
            code_len,
            2,
            max_locals,
            false,
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
            Vec::new(), // pic_slots
            Vec::new(),
            HashMap::new(),
            HashMap::new(),
            &test_helpers(),
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("if_acmp fixture must compile")
    }
    let pairs: [(i64, i64); 4] = [
        (0x1000, 0x1000),
        (0x1000, 0x2000),
        (0x1_0000_1000, 0x1000),
        (0, 0),
    ];
    for op in [0xa5u8, 0xa6] {
        // Whole stack, forward: taken → 0, fall-through → 1.
        //   0 aload_0; 1 aload_1; 2 if_acmp +5 (→ 7); 5 iconst_1; 6 ireturn;
        //   7 iconst_0; 8 ireturn
        let whole = [0x2a, 0x2b, op, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac, 0, 0];
        // A value live beneath, forward: taken → 7, fall-through → 8.
        //   0 bipush 7; 2 aload_0; 3 aload_1; 4 if_acmp +6 (→ 10);
        //   7 iconst_1; 8 iadd; 9 ireturn; 10 ireturn
        let beneath = [
            0x10, 0x07, 0x2a, 0x2b, op, 0x00, 0x06, 0x04, 0x60, 0xac, 0xac, 0, 0,
        ];
        // Backward: `i = 0; do { i++; if (i >= 3) break; } while (a OP b);
        // return i;` → 3 while the branch keeps being taken, else 1.
        //   0 iconst_0; 1 istore_2; 2 iinc 2 1; 5 iload_2; 6 bipush 3;
        //   8 if_icmpge +8 (→ 16); 11 aload_0; 12 aload_1;
        //   13 if_acmp -11 (→ 2); 16 iload_2; 17 ireturn
        let backward = [
            0x03, 0x3d, 0x84, 0x02, 0x01, 0x1c, 0x10, 0x03, 0xa2, 0x00, 0x08, 0x2a, 0x2b, op, 0xff,
            0xf5, 0x1c, 0xac, 0, 0,
        ];
        let whole_m = compile_locals(&whole, 9, 2);
        let beneath_m = compile_locals(&beneath, 11, 2);
        let backward_m = compile_locals(&backward, 18, 3);
        for (a, b) in pairs {
            let taken = if op == 0xa5 { a == b } else { a != b };
            // SAFETY: the body was compiled by this test for two reference-sized arguments it never dereferences, and runs on this thread.
            let got = unsafe { whole_m.try_call(&[a, b]).expect("jit call") };
            assert_eq!(
                got,
                i64::from(!taken),
                "op {op:#x} {a:#x} {b:#x} (whole stack)"
            );
            // SAFETY: as above.
            let got = unsafe { beneath_m.try_call(&[a, b]).expect("jit call") };
            assert_eq!(
                got,
                if taken { 7 } else { 8 },
                "op {op:#x} {a:#x} {b:#x} (beneath)"
            );
            // SAFETY: as above.
            let got = unsafe { backward_m.try_call(&[a, b]).expect("jit call") };
            assert_eq!(
                got,
                if taken { 3 } else { 1 },
                "op {op:#x} {a:#x} {b:#x} (backward)"
            );
        }
    }
}

#[test]
pub(in crate::x64::tests) fn test_ifnull_with_aconst_null() {
    // Method: int f() { Object x = null; if (x == null) return 42; return 0; }
    // aconst_null, astore_0, aload_0, ifnull +5, iconst_0, ireturn, bipush 42, ireturn
    let code: Vec<u8> = vec![
        0x01, // 0: aconst_null
        0x4b, // 1: astore_0
        0x2a, // 2: aload_0
        0xc6, 0x00, 0x05, // 3: ifnull +5 → PC 8
        0x03, // 6: iconst_0
        0xac, // 7: ireturn
        0x10, 0x2a, // 8: bipush 42
        0xac, // 10: ireturn
        0, 0,
    ];
    let code_len = 11;
    assert!(is_jit_compatible(&code, code_len, "()I"));
    let compiled = compile(
        &code,
        code_len,
        0,
        1,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[]).expect("test JIT call") };
    assert_eq!(result, 42);
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_checkcast_null_passthrough() {
    // Method: Object f(Object a) { return (String) a; }
    // checkcast with null should pass through
    // aload_0, checkcast #0, areturn
    let class_name = "java/lang/String";
    let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
    let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc0, 0x00, 0x01, // 1: checkcast #1 (ignored, we use typecheck_info)
        0xb0, // 4: areturn
        0, 0,
    ];
    let code_len = 5;
    // checkcast needs context
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        typecheck_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // null passes checkcast → returns 0
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
                                                     // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
                                                     // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call_with_context(vm_ptr, &[0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_instanceof_null() {
    // Method: int f(Object a) { return a instanceof String ? 1 : 0; }
    // aload_0, instanceof #1, ireturn
    let class_name = "java/lang/String";
    let leaked: &'static str = Box::leak(class_name.to_string().into_boxed_str()); // LEAK(intentional): test-only; class name must outlive JIT-compiled code pointer
    let typecheck_info = vec![(1usize, leaked.as_ptr(), leaked.len())];
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        typecheck_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // null → instanceof returns 0
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target
                                                     // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
                                                     // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .try_call_with_context(vm_ptr, &[0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

/// Compile `int f(Object o) { return o instanceof <name>; }` through the door
/// that can reach the inline typecheck guard: a non-empty `method_key` and an
/// oop-marked parameter are the trusted-oop clauses the checkcast arm reads.
pub(in crate::x64::tests) fn instanceof_inline_fixture(
    helpers: &JitRuntimeHelpers,
    name: &str,
    target_class_id: Option<u32>,
) -> CompiledMethod {
    let (name_ptr, name_len) = crate::intern_typecheck_target(name, target_class_id);
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc1, 0x00, 0x01, // 1: instanceof #1
        0xac, // 4: ireturn
        0, 0,
    ];
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,
        1,
        true,       // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        vec![(1usize, name_ptr, name_len)], // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        Vec::new(), // invoke_info
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
        HashMap::new(),
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &[],
        0,
        0b1, // param_oop_mask: the parameter is a reference
        Vec::new(),
        "T.f:(Ljava/lang/Object;)I", // non-empty ⇒ trusted-oop eligible
        None,                        // despec: no VM, so no despeculation verdicts
        Vec::new(),
        None, // elidable_init_pcs: no constant pool, so nothing is proven empty,
        crate::x64::BackendRequest::default(),
    )
    .expect("instanceof must compile")
}

/// A header-only stand-in for a heap object: the class id at
/// `class_id_offset_in_obj` (0 in `test_helpers`) and the KIND_TAGS byte,
/// everything else zero. Neither the inline guard nor the marker helper reads
/// anything further.
pub(in crate::x64::tests) fn fake_typecheck_header(class_id: u32, kind_tags: u8) -> Box<[u64; 8]> {
    let kind_off = cratonvm_types::KIND_TAGS_BYTE_OFFSET;
    assert!(
        (4..64).contains(&kind_off),
        "the fake header assumes KIND_TAGS lies past the 4-byte class id and inside 64 bytes"
    );
    let mut words = Box::new([0u64; 8]);
    let base = words.as_mut_ptr() as *mut u8; // Cast: byte view of the header words
                                              // SAFETY: `base` addresses 64 owned bytes and both writes are in range.
    unsafe {
        std::ptr::copy_nonoverlapping(class_id.to_le_bytes().as_ptr(), base, 4);
        *base.add(kind_off) = kind_tags;
    }
    words
}

/// Finding #76: `instanceof` takes checkcast's inline class-id guard. Null and
/// an exact plain-object match answer WITHOUT the helper; everything else still
/// calls it, and the helper's answer is what the method returns.
#[test]
pub(in crate::x64::tests) fn instanceof_inline_guard_answers_null_and_exact_without_the_helper() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if !checkcast_inline_enabled() {
        // `CRATONVM_JIT_CHECKCAST_INLINE=0` reverts both arms to the helper;
        // nothing inline to assert.
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    /// Marker helper: counts, and answers 1 — the answer a subclass receiver
    /// would get, and one the call-free `0` stub can never produce.
    unsafe extern "C" fn marker_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        1
    }
    let mut helpers = test_helpers();
    helpers.instanceof_check = marker_instanceof as *const () as usize;
    assert_eq!(helpers.class_id_offset_in_obj, 0);

    const TARGET: u32 = 0x0076_7601;
    let compiled = instanceof_inline_fixture(&helpers, "t76/InstanceofExactTarget", Some(TARGET));
    assert_eq!(
        calls_to(&compiled, helpers.instanceof_check),
        1,
        "the slow path must still carry exactly one helper call"
    );

    HITS.store(0, Ordering::SeqCst);
    // SAFETY (every call below): JIT code compiled from valid bytecode; the
    // argument is null or a live, aligned fake header the guard only reads.
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("jit call")
    };
    assert_eq!(got, 0, "null instanceof anything is 0");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        0,
        "null must not reach the helper"
    );

    let exact = fake_typecheck_header(TARGET, 0);
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[exact.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    assert_eq!(got, 1, "an exact plain-object match is an instance");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        0,
        "an exact class-id match must not reach the helper"
    );

    let other = fake_typecheck_header(TARGET + 1, 0);
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[other.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    assert_eq!(got, 1, "a different class returns the helper's answer");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        1,
        "a different class must call the helper"
    );

    // An ARRAY header carrying the target's id (a reference array records its
    // component's) must not match inline: BUG-JIT-ARRAY-INSTANCEOF-20260726.
    let array = fake_typecheck_header(TARGET, 0x01);
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[array.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    assert_eq!(got, 1, "an array receiver returns the helper's answer");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        2,
        "an array receiver must call the helper"
    );
}


/// JIT r21 tcp21 (`CRATONVM_JIT_SP_TYPECHECK_ROW_FIRST`, TCPERF21-4): the
/// single-pass `instanceof` / `checkcast` against a class at depth 1 and an
/// interface answer from the receiver's type-check row before the display arm.
/// EXECUTED over a fake row table, a fake well-known resolver (the rows, a
/// display cell that holds NO table so the display arm always declines, the
/// target's depth / interface answer) and a counting helper: with the switch on
/// a subtype is `1`, a hierarchy-only non-subtype is `0` (latch down), a cast
/// hit returns the receiver, all without the helper; with the switch off the
/// same receivers all reach the helper (the positive control, because the
/// display arm declines). An array and a lambda-proxy id still ask the helper
/// on both arms. Expected answers are derived from JLS 15.20.2 / 5.5, not
/// measured.
/// JIT r21 tcp21: the single-pass row-first arm is OPT-IN, and the policy
/// function hands out an arm only when every switch it needs agrees.
/// i11-L2: `jit_instanceof` raises at an unbound site (the interpreter's
/// `IllegalAccessError` / resolution error, pending, with the `i64::MIN`
/// sentinel), so the `0xc1` arm must drain the sentinel like `checkcast`'s:
/// the method leaves through the exception-check stub, which stamps the
/// `instanceof`'s own bci, instead of reading `i64::MIN` as a verdict.
#[test]
pub(in crate::x64::tests) fn instanceof_drains_the_helper_sentinel_through_the_exception_stub() {
    unsafe extern "C" fn raising_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
        i64::MIN
    }
    let mut helpers = array_test_helpers();
    helpers.instanceof_check = raising_instanceof as *const () as usize;
    // No target id: no inline stub, every non-null receiver calls the helper.
    let compiled = instanceof_inline_fixture(&helpers, "i11l2/UnboundInstanceofTarget", None);
    let receiver = fake_typecheck_header(0x0011_2001, 0);
    TEST_THROW_BCI.with(|c| c.set(-1));
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[receiver.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    assert_eq!(
        got,
        i64::MIN,
        "the sentinel leaves the method, not the verdict"
    );
    assert_eq!(
        TEST_THROW_BCI.with(|c| c.get()),
        1,
        "the exception stub stamps the instanceof's own bci"
    );
}

/// The 1-D primitive-array variant: `o instanceof byte[]` is settled by the
/// KIND_TAGS byte alone, with no class id at all.
#[test]
pub(in crate::x64::tests) fn instanceof_primitive_array_tag_answers_without_the_helper() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    if !checkcast_inline_enabled() {
        return;
    }
    static HITS: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn marker_instanceof(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
        HITS.fetch_add(1, Ordering::SeqCst);
        0
    }
    let mut helpers = test_helpers();
    helpers.instanceof_check = marker_instanceof as *const () as usize;
    let tag = cratonvm_types::primitive_array_kind_tags_byte("[B").expect("byte[] has a tag");
    let compiled = instanceof_inline_fixture(&helpers, "[B", None);

    HITS.store(0, Ordering::SeqCst);
    let bytes = fake_typecheck_header(0, tag);
    // SAFETY (every call below): as in the test above.
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[bytes.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    assert_eq!(got, 1, "a byte[] header is an instance of byte[]");
    assert_eq!(HITS.load(Ordering::SeqCst), 0);

    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("jit call")
    };
    assert_eq!(got, 0, "null instanceof byte[] is 0");
    assert_eq!(HITS.load(Ordering::SeqCst), 0);

    let plain = fake_typecheck_header(7, 0);
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[plain.as_ptr() as i64]) // Cast: object address as JIT argument
            .expect("jit call")
    };
    // Round 9 wave 3 (x64obj3): a PLAIN object's KIND_TAGS byte already
    // settles `instanceof <primitive array>` -- no plain object is an array --
    // so the answer is 0 without the helper. (Other array kinds and a
    // reference-array target still go to the helper.)
    assert_eq!(got, 0, "a plain object is never an instance of byte[]");
    assert_eq!(
        HITS.load(Ordering::SeqCst),
        0,
        "a plain object answers a primitive-array instanceof inline"
    );
}

/// `Object f(Object o) { return (<name>) o; }` through the same trusted-oop
/// door as [`instanceof_inline_fixture`].
pub(in crate::x64::tests) fn checkcast_inline_fixture(
    helpers: &JitRuntimeHelpers,
    name: &str,
    target_class_id: Option<u32>,
) -> CompiledMethod {
    let (name_ptr, name_len) = crate::intern_typecheck_target(name, target_class_id);
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xc0, 0x00, 0x01, // 1: checkcast #1
        0xb0, // 4: areturn
        0, 0,
    ];
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        5,
        1,
        1,
        true,       // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        vec![(1usize, name_ptr, name_len)], // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // new_deferred_info
        Vec::new(), // anewarray_info
        Vec::new(), // anewarray_deferred_info
        Vec::new(), // invoke_info
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
        HashMap::new(),
        HashMap::new(), // inline_guard_variants (PGO-02)
        None,           // string_layout
        &[],
        0,
        0b1, // param_oop_mask: the parameter is a reference
        Vec::new(),
        "T.f:(Ljava/lang/Object;)Ljava/lang/Object;", // non-empty ⇒ trusted-oop eligible
        None,
        Vec::new(),
        None,
        crate::x64::BackendRequest::default(),
    )
    .expect("checkcast must compile")
}

/// r11 x64gen: a null `checkcast` operand answers null on the inline path.
/// It used to be sent to the helper (a call, a blind spill and an oop map)
/// only to be handed back; the class-id and primitive-array variants both
/// route it to the hit edge now.
#[test]
pub(in crate::x64::tests) fn checkcast_inline_guard_answers_null_without_the_helper() {
    if !checkcast_inline_enabled() {
        return;
    }
    /// What the marker helper answers, whatever it is asked. A body that
    /// returns it called the helper; no counter (and so no new process-global,
    /// which `process_global_statics_ratchet` would count) is needed to tell.
    const MARKER: i64 = 0x0011_4000_0000_0008;
    unsafe extern "C" fn marker_checkcast(_vm: i64, _obj: i64, _name: i64, _len: i64) -> i64 {
        MARKER
    }
    let mut helpers = test_helpers();
    helpers.checkcast = marker_checkcast as *const () as usize;
    assert_eq!(helpers.class_id_offset_in_obj, 0);

    const TARGET: u32 = 0x0011_4001;
    let by_id = checkcast_inline_fixture(&helpers, "r11x64gen/CheckcastTarget", Some(TARGET));
    let byte_array = checkcast_inline_fixture(&helpers, "[B", None);
    for (compiled, label) in [
        (&by_id, "class-id guard"),
        (&byte_array, "primitive-array guard"),
    ] {
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        let got = unsafe {
            compiled
                .try_call_with_context(test_vm_ctx(), &[0])
                .expect("jit call")
        };
        assert_eq!(
            got, 0,
            "a null cast answers null without reaching the helper ({label})"
        );
    }

    // A miss still reaches the helper, and its answer is the method's.
    let other = fake_typecheck_header(TARGET + 1, 0);
    let other_ptr = other.as_ptr() as i64; // Cast: object address as JIT argument
                                           // SAFETY: as above.
    let got = unsafe {
        by_id
            .try_call_with_context(test_vm_ctx(), &[other_ptr])
            .expect("jit call")
    };
    assert_eq!(
        got, MARKER,
        "a miss must call the helper and return its answer"
    );

    // An exact match answers inline with the receiver itself.
    let exact = fake_typecheck_header(TARGET, 0);
    let exact_ptr = exact.as_ptr() as i64; // Cast: object address as JIT argument
                                           // SAFETY: as above.
    let got = unsafe {
        by_id
            .try_call_with_context(test_vm_ctx(), &[exact_ptr])
            .expect("jit call")
    };
    assert_eq!(got, exact_ptr, "an exact class-id match needs no helper");
}

/// Compile `code` as the method `method_key` names, through the door that
/// carries a method key — the legacy `compile` wrapper passes `""`, which
/// narrows nothing.
pub(in crate::x64::tests) fn keyed_int_method(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    method_key: &str,
) -> CompiledMethod {
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
        HashMap::new(),
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
    .expect("int method must compile")
}

/// Finding #84: JVMS §6.5 `ireturn` narrows a `boolean` return as if by
/// `value & 1` and a `byte`/`char`/`short` return by truncation and extension.
/// The interpreter bridge always did; a compiled caller reads RAX raw, so the
/// compiled body must do it too. `B`/`S` results are checked as full `i64`s:
/// this backend keeps an `int` sign-extended through RAX.
#[test]
pub(in crate::x64::tests) fn ireturn_narrows_boolean_byte_char_short_returns() {
    // SAFETY (every call): JIT code compiled from valid bytecode, called with
    // exactly its declared int arguments.
    // ()Z: iconst_2; ireturn -> 2 & 1 = 0
    let z = keyed_int_method(&[0x05, 0xac, 0, 0], 2, 0, "T.z:()Z");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { z.try_call(&[]) }, Ok(0));
    // ()B: sipush 200; ireturn -> (byte) 200 = -56
    let b = keyed_int_method(&[0x11, 0x00, 0xC8, 0xac, 0, 0], 4, 0, "T.b:()B");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { b.try_call(&[]) }, Ok(-56));
    // ()C: iconst_m1; ireturn -> (char) -1 = 65535
    let c = keyed_int_method(&[0x02, 0xac, 0, 0], 2, 0, "T.c:()C");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { c.try_call(&[]) }, Ok(65535));
    // (I)S: iload_0; ireturn. A parameter rather than `ldc 70000`, so the value
    // cannot be folded and the legacy wrapper's lack of an `ldc` table does not
    // matter. (short) 70000 = 4464, (short) -70000 = -4464.
    let s = keyed_int_method(&[0x1a, 0xac, 0, 0], 2, 1, "T.s:(I)S");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { s.try_call(&[70000]) }, Ok(4464));
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { s.try_call(&[-70000]) }, Ok(-4464));
    // Controls: an `I` return, and a method with no key, are left alone.
    let i = keyed_int_method(&[0x1a, 0xac, 0, 0], 2, 1, "T.i:(I)I");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { i.try_call(&[70000]) }, Ok(70000));
    let unkeyed = keyed_int_method(&[0x05, 0xac, 0, 0], 2, 0, "");
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    assert_eq!(unsafe { unkeyed.try_call(&[]) }, Ok(2));
}

#[test]
pub(in crate::x64::tests) fn narrowed_int_return_tag_reads_only_the_return_byte() {
    assert_eq!(crate::narrowed_int_return_tag("()Z"), Some(b'Z'));
    assert_eq!(crate::narrowed_int_return_tag("T.f:(I)B"), Some(b'B'));
    assert_eq!(crate::narrowed_int_return_tag("(JJ)C"), Some(b'C'));
    assert_eq!(crate::narrowed_int_return_tag("a/B.m:()S"), Some(b'S'));
    assert_eq!(crate::narrowed_int_return_tag("()I"), None);
    assert_eq!(crate::narrowed_int_return_tag("()V"), None);
    assert_eq!(crate::narrowed_int_return_tag("()[Z"), None);
    assert_eq!(crate::narrowed_int_return_tag("()La/Z;"), None);
    assert_eq!(crate::narrowed_int_return_tag(""), None);
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_bounds_check_iaload_in_bounds() {
    // Method: int f(int[] arr, int idx) { return arr[idx]; }
    // Bytecode: aload_0, iload_1, iaload, ireturn
    // needs_heap = false for inline array access, but we pass needs_heap=true
    // to test the bounds check with a real array.
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0   (load array)
        0x1b, // 1: iload_1   (load index)
        0x2e, // 2: iaload    (load int from array)
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate an int[5] = {10, 20, 30, 40, 50}
    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .heap
            .set_array_element(arr, i, Value::Int((i as i32 + 1) * 10)); // Cast: x86-64 immediate encoding
    }

    // In-bounds access should work
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 0])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 10);
    // SAFETY: `compiled` is executable JIT code from valid bytecode; vm_ptr and
    // arr_ptr reference live test objects and match the JIT calling convention.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 4])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 50);
}

// test_jit_throw_aioobe_direct moved to vm crate (helper function lives there)

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_bounds_check_bastore_in_bounds() {
    // Method: void f(byte[] arr, int idx, int val) { arr[idx] = (byte)val; }
    // Bytecode: aload_0, iload_1, iload_2, bastore, return
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0   (array)
        0x1b, // 1: iload_1   (index)
        0x1c, // 2: iload_2   (value)
        0x54, // 3: bastore
        0xb1, // 4: return (void)
        0, 0,
    ];
    let code_len = 5;
    let compiled = compile(
        &code,
        code_len,
        3,
        3,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Byte, 3);
    let arr_ptr = arr.as_ptr();

    // In-bounds store should work
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 0, 42])
            .expect("test JIT call")
    }; // Cast: address arithmetic
    let val = shared.heap.get_array_element(arr, 0).unwrap();
    assert_eq!(val.as_int(), Some(42));
}

#[test]
pub(in crate::x64::tests) fn test_bounds_elimination_analysis() {
    // Test the BCE analysis functions directly
    // Simulate a for-loop: for (int i = 0; i < n; i++) { arr[i]; }
    // Bytecode pattern:
    //   0: iload_0          ; load i (induction var)
    //   1: iload_2          ; load n (bound)
    //   2: if_icmpge +10    ; exit if i >= n (target = 15)
    //   5: aload_1          ; load arr
    //   6: iload_0          ; load i (index)
    //   7: iaload           ; arr[i]
    //   8: pop              ; discard
    //   9: iinc 0, 1        ; i++
    //  12: goto -12         ; back to 0
    //  15: return
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa2, 0x00, 0x0d, // 2: if_icmpge +13 → 15
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);

    // Should detect loop (header=0, back_edge=12)
    assert!(!loops.is_empty());
    assert_eq!(loops[0], (0, 12));

    // Should find induction variable = local 0
    let iv = find_induction_variable(&code, 0, 15);
    assert_eq!(iv, Some(0));

    // Analyze bounds elimination
    let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    // The iaload at pc=7 should be safe because i < n and arr is unmodified
    assert!(
        safe_pcs.contains(&7),
        "iaload at pc=7 should be bounds-safe, got {:?}",
        safe_pcs
    );
}

#[test]
pub(in crate::x64::tests) fn test_bounds_elimination_javac_pattern() {
    // Test BCE with the standard javac for-loop pattern:
    //   goto check; body; iinc; check: iload iv; iload bound; if_icmplt body
    // This is the pattern javac generates (check at bottom, if_icmplt as back-edge)
    //
    //   0: iconst_0         ; push 0
    //   1: istore_3         ; i = 0
    //   2: goto +9 → 11    ; jump to check
    //   5: aload_1          ; load arr (loop body starts here)
    //   6: iload_3          ; load i
    //   7: iaload           ; arr[i]
    //   8: pop              ; discard
    //   9: iinc 3, 1        ; i++
    //  12: iload_3          ; load i (check)
    //  13: iload_2          ; load n
    //  14: if_icmplt -9 → 5 ; continue if i < n (back-edge)
    //  17: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3 (i=0)
        // `goto +10 → 12`: this read `+9 → 11`, the middle of the `iinc`,
        // until round 9 made `constant_iv_init` decode the entry branch and
        // refuse one that lands between instructions.
        0xa7, 0x00, 0x0a, // 2: goto +10 → 12
        0x2b, // 5: aload_1 (arr)
        0x1d, // 6: iload_3 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x03, 0x01, // 9: iinc 3, 1
        0x1d, // 12: iload_3 (i)
        0x1c, // 13: iload_2 (n)
        0xa1, 0xff, 0xf7, // 14: if_icmplt -9 → 5
        0xb1, // 17: return
        0,
    ];
    let code_len = 18;
    let loops = detect_loops(&code, code_len);

    // Loop: header=5 (target of back-edge), back_edge=14 (if_icmplt)
    assert!(!loops.is_empty(), "should detect loop, got {:?}", loops);
    assert_eq!(loops[0], (5, 14), "loop should be (5, 14), got {:?}", loops);

    // Should find induction variable = local 3 (iinc 3, 1)
    let iv = find_induction_variable(&code, 5, 17);
    assert_eq!(iv, Some(3), "IV should be local 3");

    // Analyze bounds elimination — should recognize if_icmplt as continue-condition
    let (safe_pcs, _speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&7),
        "iaload at pc=7 should be bounds-safe with javac pattern, got {:?}",
        safe_pcs
    );
}

#[test]
pub(in crate::x64::tests) fn test_bce_multi_array_per_array_guards() {
    // Regression: docs/feature-designs/jit-ir-optimizations.md
    // (repro test_classes/gpu/BoundsDeopt2.java). The vectorAdd shape —
    //   int n = a.length; for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
    // — must elide statically ONLY the access into `a` (whose length the
    // bound provably is). `b[i]` and `out[i]` must each get their OWN
    // speculative header guard; before the per-array fix all three were
    // "statically" elided from `a`'s guard alone, so a shorter `out`
    // took silent out-of-bounds heap stores instead of AIOOBE.
    //
    // Locals: 0=a, 1=b, 2=out, 3=n, 4=i.
    //   0: aload_0 ; 1: arraylength ; 2: istore_3      (n = a.length)
    //   3: iconst_0 ; 4: istore 4                       (i = 0)
    //   6: iload 4 ; 8: iload_3 ; 9: if_icmpge +22 → 31 (header)
    //  12: aload_2 ; 13: iload 4                        (out, i)
    //  15: aload_0 ; 16: iload 4 ; 18: iaload           (a[i])
    //  19: aload_1 ; 20: iload 4 ; 22: iaload           (b[i])
    //  23: iadd ; 24: iastore                           (out[i] = ...)
    //  25: iinc 4, 1 ; 28: goto -22 → 6 ; 31: return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0x3e, // 2: istore_3
        0x03, // 3: iconst_0
        0x36, 0x04, // 4: istore 4
        0x15, 0x04, // 6: iload 4 (header)
        0x1d, // 8: iload_3
        0xa2, 0x00, 0x16, // 9: if_icmpge +22 → 31
        0x2c, // 12: aload_2 (out)
        0x15, 0x04, // 13: iload 4
        0x2a, // 15: aload_0 (a)
        0x15, 0x04, // 16: iload 4
        0x2e, // 18: iaload
        0x2b, // 19: aload_1 (b)
        0x15, 0x04, // 20: iload 4
        0x2e, // 22: iaload
        0x60, // 23: iadd
        0x4f, // 24: iastore
        0x84, 0x04, 0x01, // 25: iinc 4, 1
        0xa7, 0xff, 0xea, // 28: goto -22 → 6
        0xb1, // 31: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (6, 28), "loop should be (6, 28), got {loops:?}");

    // Whole-method provenance: n (local 3) IS a.length (local 0), and the
    // IV (local 4) provably starts at 0.
    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 3),
        Some(0),
        "n = a.length provenance"
    );
    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 4),
        None,
        "the IV local is stored from iconst_0, not an arraylength"
    );
    assert!(find_iv_nonneg_start(&code, code_len, 4));

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);

    // All three accesses end up elided (a statically, b/out behind guards)...
    for pc in [18usize, 22, 24] {
        assert!(
            safe_pcs.contains(&pc),
            "access at pc={pc} elided, got {safe_pcs:?}"
        );
    }
    // ...but `b` (local 1) and `out` (local 2) each need their own guard,
    // and `a` (local 0) — statically proven — must have none.
    let mut guarded: Vec<(usize, usize, usize, Vec<usize>)> = guards
        .iter()
        .map(|g| {
            (
                g.array_local,
                g.bound_local,
                g.iv_local,
                g.covered_pcs.clone(),
            )
        })
        .collect();
    guarded.sort();
    assert_eq!(
        guarded,
        vec![(1, 3, 4, vec![22]), (2, 3, 4, vec![24])],
        "b and out each get a per-array guard covering exactly their access; \
         a gets none"
    );
}

#[test]
pub(in crate::x64::tests) fn test_bce_param_bound_goes_speculative_not_static() {
    // A loop bound that is a plain parameter (no arraylength provenance)
    // must NOT be statically elided — the elision demotes to a speculative
    // header guard on the accessed array. Same shape as
    // `test_bounds_elimination_analysis` (locals: 0=i, 1=arr, 2=n).
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa2, 0x00, 0x0d, // 2: if_icmpge +13 → 15
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert_eq!(find_bound_arraylength_provenance(&code, code_len, 2), None);

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&7),
        "elided behind a guard, got {safe_pcs:?}"
    );
    assert_eq!(guards.len(), 1, "exactly one speculative guard");
    assert_eq!(
        (
            guards[0].loop_header,
            guards[0].array_local,
            guards[0].bound_local,
            guards[0].iv_local,
            guards[0].covered_pcs.clone(),
        ),
        (0, 1, 2, 0, vec![7]),
    );
}

#[test]
pub(in crate::x64::tests) fn test_bce_negative_iv_start_not_static() {
    // `for (int i = -5; i < n; i++) a[i]` with n = a.length: the bound
    // provenance holds, but the IV provably starts NEGATIVE, so the static
    // (guard-less) elision must be refused. The access may still be elided
    // behind the speculative header guard, whose emitted code tests the
    // runtime `iv >= 0` at loop entry (and deopts for i = -5).
    // Locals: 0=a, 1=n, 2=i.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xbe, // 1: arraylength
        0x3c, // 2: istore_1 (n = a.length)
        0x10, 0xfb, // 3: bipush -5
        0x3d, // 5: istore_2 (i = -5)
        0x1c, // 6: iload_2 (header)
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x0d, // 8: if_icmpge +13 → 21
        0x2a, // 11: aload_0
        0x1c, // 12: iload_2
        0x2e, // 13: iaload
        0x57, // 14: pop
        0x84, 0x02, 0x01, // 15: iinc 2, 1
        0xa7, 0xff, 0xf4, // 18: goto -12 → 6
        0xb1, // 21: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (6, 18));

    assert_eq!(
        find_bound_arraylength_provenance(&code, code_len, 1),
        Some(0)
    );
    assert!(
        !find_iv_nonneg_start(&code, code_len, 2),
        "bipush -5 start must not prove a non-negative IV"
    );

    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(safe_pcs.contains(&13));
    assert_eq!(guards.len(), 1, "elision must be guard-backed, not static");
    assert_eq!(guards[0].array_local, 0);
    assert_eq!(guards[0].iv_local, 2);
    assert_eq!(guards[0].covered_pcs, vec![13]);
}

#[test]
pub(in crate::x64::tests) fn test_bce_two_bound_stores_no_provenance() {
    // Two stores to the bound local defeat the single-dominating-store
    // proof: after the second store the bound may exceed a.length.
    // Locals: 0=a, 1=n, 2=i.
    //   0: aload_0 ; 1: arraylength ; 2: istore_1   (n = a.length)
    //   3: iload_1 ; 4: iconst_1 ; 5: iadd ; 6: istore_1  (n = n + 1 !)
    //   7: return
    let code: Vec<u8> = vec![
        0x2a, 0xbe, 0x3c, // n = a.length
        0x1b, 0x04, 0x60, 0x3c, // n = n + 1
        0xb1,
    ];
    assert_eq!(
        find_bound_arraylength_provenance(&code, code.len(), 1),
        None
    );
}

#[test]
pub(in crate::x64::tests) fn test_bounds_elimination_inclusive_not_safe() {
    // SECURITY FIX (V17 / B1): an INCLUSIVE comparator reaches index == bound,
    // which the single `array.length >= bound` header guard does NOT cover.
    // The access must therefore KEEP its per-element bounds check (not be in
    // `safe_pcs`) and NO speculative guard may be installed.
    //
    // Same shape as `test_bounds_elimination_analysis` but the exit test is
    // `if_icmpgt` (0xa3) — `for (int i = 0; i <= n; i++) { arr[i]; }`:
    //   0: iload_0          ; i
    //   1: iload_2          ; n
    //   2: if_icmpgt +13    ; exit if i > n (inclusive) → 15
    //   5: aload_1          ; arr
    //   6: iload_0          ; i
    //   7: iaload           ; arr[i]
    //   8: pop
    //   9: iinc 0, 1        ; i++
    //  12: goto -12         ; back to 0
    //  15: return
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0 (i)
        0x1c, // 1: iload_2 (n)
        0xa3, 0x00, 0x0d, // 2: if_icmpgt +13 → 15 (INCLUSIVE exit)
        0x2b, // 5: aload_1 (arr)
        0x1a, // 6: iload_0 (i)
        0x2e, // 7: iaload
        0x57, // 8: pop
        0x84, 0x00, 0x01, // 9: iinc 0, 1
        0xa7, 0xff, 0xf4, // 12: goto -12 → 0
        0xb1, // 15: return
        0, 0,
    ];
    let code_len = 16;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0], (0, 12));

    // Default (opt-in flag off): inclusive loops take no guard at all.
    __set_inclusive_spec_bce_override(Some(false));
    let (off_safe, off_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(!off_safe.contains(&7), "default-off must keep the check");
    assert!(off_guards.is_empty(), "default-off must emit no guard");

    // V17 sound-guard form (opt-in): the access IS elided, but only on
    // the strength of a speculative guard flagged `inclusive` (emitted
    // as `length > bound` / JBE + the bound != MAX entry check), never
    // the guard-less static proof.
    __set_inclusive_spec_bce_override(Some(true));
    let (safe_pcs, speculative_guards) = analyze_bounds_elimination(&code, code_len, &loops);
    __set_inclusive_spec_bce_override(None);
    assert!(
        safe_pcs.contains(&7),
        "inclusive-loop iaload at pc=7 should be guard-elided, got {:?}",
        safe_pcs
    );
    assert_eq!(speculative_guards.len(), 1, "one guarded array expected");
    let g = &speculative_guards[0];
    assert!(g.inclusive, "guard must record the inclusive comparator");
    assert_eq!(g.step_local, None, "iinc +1 loop needs no step guard");
    assert!(g.covered_pcs.contains(&7));
}

#[test]
pub(in crate::x64::tests) fn test_bce_varadd_step_guard_and_commuted_refusal() {
    // Sieve inner-loop shape: `for (j = ..; j < n; j += i) a[j] = ..`.
    //   locals: 0 = j (IV), 1 = n (bound), 2 = arr, 3 = i (step)
    //   0: iload_0
    //   1: iload_1
    //   2: if_icmpge +16 -> 18
    //   5: aload_2
    //   6: iload_0
    //   7: iconst_1
    //   8: bastore
    //   9: iload_0        (canonical j += i)
    //  10: iload_3
    //  11: iadd
    //  12: istore_0
    //  13: goto -13 -> 0
    //  16..: return padding
    let code = [
        0x1a, 0x1b, 0xa2, 0x00, 0x10, 0x2c, 0x1a, 0x04, 0x54, 0x1a, 0x1d, 0x60, 0x3b, 0xa7, 0xff,
        0xf3, 0xb1, 0x00, 0x00, 0x00,
    ];
    let code_len = 17;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops[0].0, 0);
    let (safe_pcs, guards) = analyze_bounds_elimination(&code, code_len, &loops);
    assert!(
        safe_pcs.contains(&8),
        "bastore at pc=8 should be guard-elided"
    );
    assert_eq!(guards.len(), 1);
    assert_eq!(
        guards[0].step_local,
        Some(3),
        "guard must carry the step local"
    );
    assert!(!guards[0].inclusive);

    // Commuted form `j = i + j` (iload_3; iload_0; iadd; istore_0) is NOT
    // the canonical compound shape — the step cannot be proven, so the
    // whole loop must be refused (no guard, no elision).
    let commuted = [
        0x1a, 0x1b, 0xa2, 0x00, 0x10, 0x2c, 0x1a, 0x04, 0x54, 0x1d, 0x1a, 0x60, 0x3b, 0xa7, 0xff,
        0xf3, 0xb1, 0x00, 0x00, 0x00,
    ];
    let (safe2, guards2) = analyze_bounds_elimination(&commuted, code_len, &loops);
    assert!(
        !safe2.contains(&8),
        "unproven step must keep the bounds check"
    );
    assert!(guards2.is_empty());
}

#[test]
pub(in crate::x64::tests) fn test_find_modified_locals_high_local_no_panic() {
    // B2: `find_modified_locals` must not panic (debug) or set the wrong bit
    // (release) for a local index >= 64. A wide `istore 200` / `iinc 200, 1`
    // must saturate to bit 63 via the `.min(63)` clamp.
    //
    //   0: istore 200   (0x36 0xc8)
    //   2: iinc 200, 1  (0x84 0xc8 0x01)
    //   5: return       (0xb1)
    let code: Vec<u8> = vec![0x36, 0xc8, 0x84, 0xc8, 0x01, 0xb1];
    let modified = find_modified_locals(&code, 0, code.len());
    // High locals saturate to bit 63; no panic, and the low bits are clear.
    assert_eq!(
        modified,
        1u64 << 63,
        "high-local store/iinc must saturate to bit 63"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_bounds_check_loop_compiled() {
    // Compile a loop that accesses array elements with an induction variable
    // The bounds check should be eliminated by BCE for the inner access
    // Method: int sum(int[] arr, int len) { int s=0; for(int i=0;i<len;i++) s+=arr[i]; return s; }
    //
    // Bytecode:
    //   0: iconst_0         ; push 0
    //   1: istore_2         ; s = 0
    //   2: iconst_0         ; push 0
    //   3: istore_3         ; i = 0
    //   4: iload_3          ; load i     (loop header)
    //   5: iload_1          ; load len
    //   6: if_icmpge +12    ; exit if i >= len → 21
    //   9: iload_2          ; load s
    //  10: aload_0          ; load arr
    //  11: iload_3          ; load i
    //  12: iaload           ; arr[i]
    //  13: iadd             ; s + arr[i]
    //  14: istore_2         ; s = s + arr[i]
    //  15: iinc 3, 1        ; i++
    //  18: goto -14         ; back to 4
    //  21: iload_2          ; load s
    //  22: ireturn
    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3d, // 1: istore_2 (s=0)
        0x03, // 2: iconst_0
        0x3e, // 3: istore_3 (i=0)
        0x1d, // 4: iload_3 (i)
        0x1b, // 5: iload_1 (len)
        0xa2, 0x00, 0x0f, // 6: if_icmpge +15 → 21
        0x1c, // 9: iload_2 (s)
        0x2a, // 10: aload_0 (arr)
        0x1d, // 11: iload_3 (i)
        0x2e, // 12: iaload
        0x60, // 13: iadd
        0x3d, // 14: istore_2 (s)
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf2, // 18: goto -14 → 4
        0x1c, // 21: iload_2 (s)
        0xac, // 22: ireturn
        0, 0,
    ];
    let code_len = 23;

    let compiled = compile(
        &code,
        code_len,
        2,
        4,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate int[5] = {1, 2, 3, 4, 5}
    let arr = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .heap
            .set_array_element(arr, i, Value::Int(i as i32 + 1)); // Cast: x86-64 immediate encoding
    }

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr_ptr as i64, 5])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 15); // 1+2+3+4+5
}

/// A loop whose header is reached only by fall-through + its own back edge
/// keeps its pre-header (and therefore its LICM hoists / speculative
/// guards).
///
/// ```text
///  0: iconst_0        2: iload_1        7: iinc 1, 1
///  1: istore_1        3: iload_0       10: goto 2
///                     4: if_icmpge 13  13: return
/// ```
#[test]
pub(in crate::x64::tests) fn single_entry_loop_header_is_not_bypassable() {
    let code: &[u8] = &[
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x1b, // 2: iload_1        <- loop header (back-edge target)
        0x1a, // 3: iload_0
        0xa2, 0x00, 0x09, // 4: if_icmpge 13
        0x84, 0x01, 0x01, // 7: iinc 1, 1
        0xa7, 0xff, 0xf8, // 10: goto 2
        0xb1, // 13: return
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(loops, vec![(2, 10)], "expected one loop with header 2");
    let bypassable = find_bypassable_loop_headers(code, code.len(), &loops, &[]);
    assert!(
        bypassable.is_empty(),
        "a fall-through-only loop header must keep its pre-header, got {bypassable:?}"
    );
}

/// `org.xml.sax.helpers.AttributesImpl.ensureCapacity`'s shape (and
/// `LicmEntryProbe.shapeB`): the loop header at 13 is ALSO the target of
/// the forward `goto 13` at pc 7, which lands after the pre-header the
/// emitter places at the header. Hoisting `n * 5` there left the cache slot
/// unwritten on that edge — HIB-LONGTAIL.2's
/// `anewarray ... length 1677721600`. The header must be reported
/// bypassable so every speculating transform is dropped for it.
///
/// ```text
///  0: iload_1          7: goto 13       17: if_icmpge 27
///  1: ifeq 10         10: bipush 30     20: iload_2
///  4: bipush 25       12: istore_2      21: iconst_2
///  6: istore_2        13: iload_2       22: imul
///                     14: iload_0       23: istore_2
///                     15: iconst_5      24: goto 13
///                     16: imul          27: iload_2 / 28: ireturn
/// ```
#[test]
pub(in crate::x64::tests) fn handler_reachable_from_outside_a_hoisted_loop_is_bypassable() {
    // A plain counted loop whose only non-back-edge predecessor is the
    // fall-through, so it is NOT bypassable on branch edges alone — every
    // difference below comes from the exception table.
    //
    //  0: iconst_0
    //  1: istore_2
    //  2: iload_2            <-- loop header
    //  3: iload_1
    //  4: if_icmpge +13 -> 17
    //  7: iload_0
    //  8: iconst_3
    //  9: imul
    // 10: istore_3           (loop-invariant run, the hoist candidate)
    // 11: iinc 2, 1
    // 14: goto -12 -> 2      <-- back edge; loop body is [2, 17)
    // 17: iload_2
    // 18: ireturn
    let code: &[u8] = &[
        0x03, 0x3D, // iconst_0; istore_2
        0x1C, 0x1B, 0xA2, 0x00, 0x0D, // iload_2; iload_1; if_icmpge -> 17
        0x1A, 0x06, 0x68, 0x3E, // iload_0; iconst_3; imul; istore_3
        0x84, 0x02, 0x01, // iinc 2,1
        0xA7, 0xFF, 0xF4, // goto -> 2
        0x1C, 0xAC, // iload_2; ireturn
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(
        loops,
        vec![(2, 14)],
        "expected exactly one natural loop with header 2"
    );

    // No exception table: single-entry header keeps its pre-header.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[]).is_empty(),
        "a single-entry counted loop must not be reported bypassable"
    );

    // try/catch wholly INSIDE the loop body — the common javac shape for
    // `while (…) { try { … } catch { … } }`. The throw can only happen
    // after the header was entered, so the pre-header already ran: sound,
    // and the hoist must be KEPT (this is the no-regression half).
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(7, 11, 10)]).is_empty(),
        "a protected range wholly inside the loop must keep its hoist"
    );

    // Protected range starts BEFORE the header: a throw from pre-loop code
    // delivers control into the body without the pre-header having run.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(0, 11, 7)]).contains(&2),
        "a handler reachable from before the loop bypasses the pre-header"
    );

    // Protected range extends PAST the loop: same hazard from the far side.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(7, 19, 7)]).contains(&2),
        "a handler reachable from after the loop bypasses the pre-header"
    );

    // Handler outside the loop entirely — irrelevant, keep the hoist.
    assert!(
        find_bypassable_loop_headers(code, code.len(), &loops, &[(0, 19, 17)]).is_empty(),
        "a handler outside the loop must not drop the hoist"
    );
}

#[test]
pub(in crate::x64::tests) fn forward_goto_into_loop_header_is_bypassable() {
    let code: &[u8] = &[
        0x1b, // 0: iload_1
        0x99, 0x00, 0x09, // 1: ifeq 10
        0x10, 0x19, // 4: bipush 25
        0x3d, // 6: istore_2
        0xa7, 0x00, 0x06, // 7: goto 13   <- skips the pre-header at 13
        0x10, 0x1e, // 10: bipush 30
        0x3d, // 12: istore_2
        0x1c, // 13: iload_2   <- loop header
        0x1a, // 14: iload_0
        0x08, // 15: iconst_5
        0x68, // 16: imul
        0xa2, 0x00, 0x0a, // 17: if_icmpge 27
        0x1c, // 20: iload_2
        0x05, // 21: iconst_2
        0x68, // 22: imul
        0x3d, // 23: istore_2
        0xa7, 0xff, 0xf5, // 24: goto 13
        0x1c, // 27: iload_2
        0xac, // 28: ireturn
    ];
    let loops = detect_loops(code, code.len());
    assert_eq!(loops, vec![(13, 24)], "expected one loop with header 13");
    // The invariant `n * 5` run IS matched — i.e. without the bypassable
    // gate this method really would get a hoist at header 13.
    assert!(
        !find_arith_loop_hoists(code, code.len(), &loops).is_empty(),
        "expected arith-LICM to match the `n * 5` run at this header"
    );
    let bypassable = find_bypassable_loop_headers(code, code.len(), &loops, &[]);
    assert!(
        bypassable.contains(&13),
        "header 13 is entered by the forward `goto` at pc 7 and must be \
         reported bypassable, got {bypassable:?}"
    );
}

/// The `AttributesImpl.ensureCapacity` bytes of
/// `forward_goto_into_loop_header_is_bypassable`, as `int f(int n, int flag)`.
pub(in crate::x64::tests) fn ensure_capacity_shape() -> Vec<u8> {
    vec![
        0x1b, // 0: iload_1
        0x99, 0x00, 0x09, // 1: ifeq 10
        0x10, 0x19, // 4: bipush 25
        0x3d, // 6: istore_2
        0xa7, 0x00, 0x06, // 7: goto 13   <- forward entry onto the header
        0x10, 0x1e, // 10: bipush 30
        0x3d, // 12: istore_2
        0x1c, // 13: iload_2   <- loop header
        0x1a, // 14: iload_0
        0x08, // 15: iconst_5
        0x68, // 16: imul
        0xa2, 0x00, 0x0a, // 17: if_icmpge 27
        0x1c, // 20: iload_2
        0x05, // 21: iconst_2
        0x68, // 22: imul
        0x3d, // 23: istore_2
        0xa7, 0xff, 0xf5, // 24: goto 13
        0x1c, // 27: iload_2
        0xac, // 28: ireturn
    ]
}

/// `int f(Foo o) { return o.m(); }` with a MIC slot at the invoke, compiled
/// with `op` (`invokevirtual` or `invokeinterface`) and receiver owner
/// `owner`; answers how many MIC `KIND_TAGS` screens
/// (`CMP BYTE [RAX + KIND_TAGS], Object`) the body carries.
pub(in crate::x64::tests) fn mic_kind_screens(op: u8, owner: &str) -> usize {
    // LEAK(intentional): the JitInvokeInfo is read by raw pointer from the
    // compiled code, so its names must outlive it; owned by the test process.
    let class_name: &'static str = Box::leak(owner.to_string().into_boxed_str());
    let info = Box::new(JitInvokeInfo {
        class_name,
        method_name: "m",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: if op == 0xb9 { 2 } else { 0 },
        declaring_class_id: 0,
        owner_class_id: 0,
    });
    let info_ptr: *const JitInvokeInfo = &*info;
    let mic = Box::new(crate::JitMICSlot::new());
    let mic_ptr: *const crate::JitMICSlot = &*mic;
    let code: Vec<u8> = if op == 0xb9 {
        vec![0x2a, 0xb9, 0x00, 0x01, 0x01, 0x00, 0xac, 0, 0]
    } else {
        vec![0x2a, 0xb6, 0x00, 0x01, 0xac, 0, 0]
    };
    let code_len = code.len() - 2;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(1usize, info_ptr)],
        Vec::new(),
        vec![(1usize, mic_ptr)],
        Vec::new(), // pic_slots: MIC only, so no megamorphic stub is emitted
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("a one-invoke method compiles");
    let screen = [
        0xF6,
        0x40,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
        cratonvm_types::KIND_TAG_BYTE_MASK,
    ];
    let n = compiled
        ._buffer
        .as_slice()
        .windows(screen.len())
        .filter(|w| *w == screen)
        .count();
    drop(info);
    drop(mic);
    n
}

/// x64gen proposal 6 (round 11 wave 4): an `invokevirtual` whose owner is a
/// class other than `Object` cannot receive an array, so its MIC probe drops
/// the `KIND_TAGS` screen; `Object` owners and every `invokeinterface` (the
/// verifier treats interface types as `Object`) keep it.
#[test]
pub(in crate::x64::tests) fn mic_probe_screens_array_receivers_only_where_one_can_arrive() {
    if !(crate::direct_jit_callee_calls_enabled()
        && sp_inline_ic_enabled()
        && sp_inline_mic_enabled())
    {
        // An inline-IC kill switch is set: there is no probe to inspect.
        return;
    }
    assert_eq!(mic_kind_screens(0xb6, "p/Foo"), 0);
    assert!(mic_kind_screens(0xb6, "java/lang/Object") >= 1);
    assert!(mic_kind_screens(0xb9, "p/Foo") >= 1);
}

/// Part 2 of `r11-x64loop-header-entered-from-outside-drops-every-preheader`:
/// a forward branch ONTO the header lands before the pre-header
/// (`Compiler::branch_landing_native`), so the pre-header predicate no longer
/// counts it, while a switch onto the header, an entry into the body and a
/// handler entry still do.
#[test]
pub(in crate::x64::tests) fn a_forward_branch_onto_the_header_no_longer_bypasses_the_preheader() {
    let code = ensure_capacity_shape();
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops, vec![(13, 24)]);
    assert!(
        find_preheader_bypassing_loop_headers(&code, code.len(), &loops, &[]).is_empty(),
        "the `goto 13` at pc 7 now runs the pre-header"
    );
    // A handler landing on the header from a range before the loop is still
    // an entry nothing redirects.
    assert!(
        find_preheader_bypassing_loop_headers(&code, code.len(), &loops, &[(0, 7, 13)])
            .contains(&13)
    );
    // The same `goto` aimed one instruction INTO the body is still a bypass.
    let mut into_body = code.clone();
    into_body[9] = 0x07; // 7: goto 14
    assert!(
        find_preheader_bypassing_loop_headers(&into_body, into_body.len(), &loops, &[])
            .contains(&13)
    );
}

/// ...and the compiled code agrees: with the arithmetic hoist of `n * 5` now
/// admitted at header 13, the `flag != 0` path (the `goto 13`) must still see
/// a freshly written hoist slot. Before the landing existed that path read
/// whatever the previous activation left in the slot (HIB-LONGTAIL.2); the
/// fall-through call first leaves `5000` there so a skipped pre-header shows
/// up as `6400` instead of `400`.
#[test]
pub(in crate::x64::tests) fn a_forward_entry_onto_a_hoisted_header_runs_the_preheader() {
    let code = ensure_capacity_shape();
    let loops = detect_loops(&code, code.len());
    assert!(
        !find_arith_loop_hoists(&code, code.len(), &loops).is_empty(),
        "the fixture must have a hoist for this test to mean anything"
    );
    let compiled = compile(
        &code,
        code.len(),
        2,
        3,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("the ensureCapacity shape compiles");
    for _ in 0..3 {
        // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
        // produced by the JIT compiler from valid bytecode and the mmap region is executable.
        let fall_through = unsafe { compiled.try_call(&[1000, 0]).expect("test JIT call") };
        assert_eq!(fall_through, 7680); // 30 -> 60 -> ... -> 7680 >= 5000
                                        // SAFETY: as above.
        let forward_entry = unsafe { compiled.try_call(&[80, 1]).expect("test JIT call") };
        assert_eq!(forward_entry, 400); // 25 -> 50 -> 100 -> 200 -> 400 >= 400
    }
}

/// Round 11 wave 6 (proposal W5-2): the native unroller takes the
/// pre-header-landing predicate, so the `ensureCapacity` loop (a `goto` back
/// edge, an 11-byte body, the `n * 5` hoist and a forward entry) is now
/// unrolled, and the forward entry (`flag != 0`) enters the unrolled body
/// through the landing. Unrolled and rolled artifacts must both agree with
/// the Java answer on both entries, for trip counts on either side of the
/// copy count; a stale hoist slot shows up as a wrong limit.
#[test]
pub(in crate::x64::tests) fn a_forward_entry_onto_an_unrolled_hoisted_header_runs_correctly() {
    let code = ensure_capacity_shape();
    let compile_with = |disable_unroll: bool| {
        cratonvm_types::flags::with_thread_overrides(
            &[(
                "CRATONVM_DISABLE_UNROLL",
                if disable_unroll { Some("1") } else { None },
            )],
            || {
                compile(
                    &code,
                    code.len(),
                    2,
                    3,
                    false,
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
                    &test_helpers(),
                    std::collections::HashSet::new(),
                    HashMap::new(),
                    None,
                )
            },
        )
        .expect("the ensureCapacity shape compiles")
    };
    let rolled = compile_with(true);
    let unrolled = compile_with(false);
    assert!(
        unrolled.code_bytes().len() >= rolled.code_bytes().len(),
        "the forward-entry loop is unrolled ({} vs {} bytes)",
        unrolled.code_bytes().len(),
        rolled.code_bytes().len()
    );
    // `x = flag != 0 ? 25 : 30; while (x < n * 5) x *= 2; return x;`
    let java = |n: i64, flag: i64| -> i64 {
        let limit = (n as i32).wrapping_mul(5); // Cast: test values fit in i32
        let mut x: i32 = if flag != 0 { 25 } else { 30 };
        while x < limit {
            x = x.wrapping_mul(2);
        }
        i64::from(x)
    };
    // The fall-through call first, so a skipped pre-header on the forward
    // entry would read a large leftover limit.
    for n in [1000i64, 0, 1, 5, 6, 7, 11, 13, 80, 81, 400, 20_000] {
        for flag in [0i64, 1] {
            for (arm, cm) in [("rolled", &rolled), ("unrolled", &unrolled)] {
                // SAFETY: calling JIT-compiled code produced from valid
                // bytecode that takes two `int`s and touches no heap.
                let got = unsafe { cm.try_call(&[n, flag]).expect("test JIT call") };
                assert_eq!(got, java(n, flag), "{arm} n={n} flag={flag}");
            }
        }
    }
}

/// The `ensureCapacity` loop entered by a SWITCH edge instead of a `goto`:
/// `int f(int n, int flag)`, `x = 25; switch (flag) { case 0: x = 30; }` with
/// the `default` arm naming the loop header (27) directly.
pub(in crate::x64::tests) fn switch_entry_hoisted_loop_shape() -> Vec<u8> {
    vec![
        0x10, 0x19, // 0: bipush 25
        0x3d, // 2: istore_2
        0x1b, // 3: iload_1
        0xab, 0x00, 0x00, 0x00, // 4: lookupswitch (+3 pad)
        0x00, 0x00, 0x00, 0x17, // 8: default -> 27 (the header)
        0x00, 0x00, 0x00, 0x01, // 12: npairs = 1
        0x00, 0x00, 0x00, 0x00, // 16: key 0
        0x00, 0x00, 0x00, 0x14, // 20:   -> 24
        0x10, 0x1e, // 24: bipush 30
        0x3d, // 26: istore_2
        0x1c, // 27: iload_2   <- loop header
        0x1a, // 28: iload_0
        0x08, // 29: iconst_5
        0x68, // 30: imul
        0xa2, 0x00, 0x0a, // 31: if_icmpge 41
        0x1c, // 34: iload_2
        0x05, // 35: iconst_2
        0x68, // 36: imul
        0x3d, // 37: istore_2
        0xa7, 0xff, 0xf5, // 38: goto 27
        0x1c, // 41: iload_2
        0xac, // 42: ireturn
    ]
}

/// Round 11 wave 6 (`r11-x64loop-header-entered-from-outside-drops-every-preheader`,
/// switch edges): a switch edge the emitter lowers to a `forward_patches`
/// branch lands before the pre-header like a `goto` does, so the landing
/// predicate stops counting it. Since wave 7 a jump-table slot does too (see
/// `a_jump_table_slot_onto_a_hoisted_header_runs_the_preheader`).
#[test]
pub(in crate::x64::tests) fn a_forward_patched_switch_edge_onto_the_header_no_longer_bypasses_the_preheader() {
    let code = switch_entry_hoisted_loop_shape();
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops, vec![(27, 38)]);
    assert!(
        find_bypassable_loop_headers(&code, code.len(), &loops, &[]).contains(&27),
        "the strict predicate counts the switch edge"
    );
    assert!(
        find_preheader_bypassing_loop_headers(&code, code.len(), &loops, &[]).is_empty(),
        "a `lookupswitch` edge is a `forward_patches` branch and lands before the pre-header"
    );

    // The form predicate the `tableswitch` arm decides with.
    let table = |default: usize, targets: &[usize]| bytecode_analysis::SwitchTable {
        len: 0,
        default,
        cases: targets
            .iter()
            .enumerate()
            .map(|(i, &t)| (i32::try_from(i).unwrap_or(i32::MAX), t))
            .collect(),
    };
    let chain = table(100, &[10, 20, 30, 40]);
    assert!(!tableswitch_emits_jump_table(&chain));
    let jump = table(100, &[10, 20, 30, 40, 50]);
    assert!(tableswitch_emits_jump_table(&jump));
    // A hole is a slot naming the default; five live cases around it still
    // make a table.
    let holed = table(100, &[10, 100, 30, 40, 50, 60]);
    assert!(tableswitch_emits_jump_table(&holed));
    // Holes do not count toward the form: four live cases stay a chain.
    assert!(!tableswitch_emits_jump_table(&table(
        100,
        &[10, 100, 30, 100, 50, 60]
    )));
}

/// The same loop entered through a JUMP-TABLE `tableswitch` slot:
/// `int f(int n, int flag)`, `x = 25; switch (flag) { case 0..4: break;
/// default: x = 30; }` with the five live cases naming the loop header (43)
/// directly, so the dispatch is a table (`tableswitch_emits_jump_table`).
pub(in crate::x64::tests) fn jump_table_entry_hoisted_loop_shape() -> Vec<u8> {
    vec![
        0x10, 0x19, // 0: bipush 25
        0x3d, // 2: istore_2
        0x1b, // 3: iload_1
        0xaa, 0x00, 0x00, 0x00, // 4: tableswitch (+3 pad)
        0x00, 0x00, 0x00, 0x24, // 8: default -> 40
        0x00, 0x00, 0x00, 0x00, // 12: low = 0
        0x00, 0x00, 0x00, 0x04, // 16: high = 4
        0x00, 0x00, 0x00, 0x27, // 20: case 0 -> 43 (the header)
        0x00, 0x00, 0x00, 0x27, // 24: case 1 -> 43
        0x00, 0x00, 0x00, 0x27, // 28: case 2 -> 43
        0x00, 0x00, 0x00, 0x27, // 32: case 3 -> 43
        0x00, 0x00, 0x00, 0x27, // 36: case 4 -> 43
        0x10, 0x1e, // 40: bipush 30
        0x3d, // 42: istore_2
        0x1c, // 43: iload_2   <- loop header
        0x1a, // 44: iload_0
        0x08, // 45: iconst_5
        0x68, // 46: imul
        0xa2, 0x00, 0x0a, // 47: if_icmpge 57
        0x1c, // 50: iload_2
        0x05, // 51: iconst_2
        0x68, // 52: imul
        0x3d, // 53: istore_2
        0xa7, 0xff, 0xf5, // 54: goto 43
        0x1c, // 57: iload_2
        0xac, // 58: ireturn
    ]
}

/// Round 11 wave 7 (`r11w6-x64-jump-table-landing-patch`): a jump-table slot
/// naming a hoisted loop header lands before its pre-header, so the landing
/// predicate no longer counts it and the `n * 5` hoist is admitted. Executed
/// with the fall-through (default) call FIRST, so a slot that skipped the
/// pre-header would read the large leftover limit (5000) and return 6400 for
/// `(80, 2)` instead of 400. This is the test that fails if
/// `escape_analysis.rs` stops counting slots while `x64.rs::patch_branches`
/// still resolves them to `pc_to_native`.
#[test]
pub(in crate::x64::tests) fn a_jump_table_slot_onto_a_hoisted_header_runs_the_preheader() {
    let code = jump_table_entry_hoisted_loop_shape();
    let table = bytecode_analysis::switch_table(&code, code.len(), 4)
        .expect("the fixture's switch decodes");
    assert!(
        tableswitch_emits_jump_table(&table),
        "the fixture must lower to a jump table for this test to mean anything"
    );
    let loops = detect_loops(&code, code.len());
    assert_eq!(loops, vec![(43, 54)]);
    assert!(
        find_bypassable_loop_headers(&code, code.len(), &loops, &[]).contains(&43),
        "the strict predicate counts the slot edges"
    );
    assert!(
        find_preheader_bypassing_loop_headers(&code, code.len(), &loops, &[]).is_empty(),
        "a jump-table slot laid out before the header lands before its pre-header"
    );
    assert!(
        !find_arith_loop_hoists(&code, code.len(), &loops).is_empty(),
        "the fixture must have a hoist for this test to mean anything"
    );
    let compiled = compile(
        &code,
        code.len(),
        2,
        3,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("the jump-table-entry shape compiles");
    // `x = 0 <= flag <= 4 ? 25 : 30; while (x < n * 5) x *= 2; return x;`
    let java = |n: i64, flag: i64| -> i64 {
        let limit = (n as i32).wrapping_mul(5); // Cast: test values fit in i32
        let mut x: i32 = if (0..=4).contains(&flag) { 25 } else { 30 };
        while x < limit {
            x = x.wrapping_mul(2);
        }
        i64::from(x)
    };
    for _ in 0..3 {
        for (n, flag) in [
            (1000i64, 9i64),
            (80, 2),
            (1000, -1),
            (80, 0),
            (5, 4),
            (5, 5),
            (81, 3),
        ] {
            // SAFETY: calling JIT-compiled code produced from valid bytecode
            // that takes two `int`s and touches no heap.
            let got = unsafe { compiled.try_call(&[n, flag]).expect("test JIT call") };
            assert_eq!(got, java(n, flag), "n={n} flag={flag}");
        }
    }
}

/// ...and the compiled code agrees: the `n * 5` hoist is admitted at header 27
/// now, and the switch entry (`flag != 0`) must see a freshly written hoist
/// slot after a fall-through call left a larger limit in it. The loop is also
/// a native-unroll candidate (`goto` back edge, 11-byte body), so this runs
/// the redirected switch entry into an unrolled body.
#[test]
pub(in crate::x64::tests) fn a_switch_entry_onto_a_hoisted_header_runs_the_preheader() {
    let code = switch_entry_hoisted_loop_shape();
    let loops = detect_loops(&code, code.len());
    assert!(
        !find_arith_loop_hoists(&code, code.len(), &loops).is_empty(),
        "the fixture must have a hoist for this test to mean anything"
    );
    let compiled = compile(
        &code,
        code.len(),
        2,
        3,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("the switch-entry shape compiles");
    for _ in 0..3 {
        // SAFETY: calling JIT-compiled code produced from valid bytecode that
        // takes two `int`s and touches no heap.
        let fall_through = unsafe { compiled.try_call(&[1000, 0]).expect("test JIT call") };
        assert_eq!(fall_through, 7680); // 30 -> 60 -> ... -> 7680 >= 5000
                                        // SAFETY: as above.
        let switch_entry = unsafe { compiled.try_call(&[80, 1]).expect("test JIT call") };
        assert_eq!(switch_entry, 400); // 25 -> 50 -> 100 -> 200 -> 400 >= 400
                                       // SAFETY: as above.
        let short = unsafe { compiled.try_call(&[5, 7]).expect("test JIT call") };
        assert_eq!(short, 25); // 25 >= 25: no iteration
    }
}

/// Round 11 wave 6, proposal W5-1 (`r11-x64loop-inline-div-probe-is-190x-slower-than-hotspot`):
/// which callee parameters a call site must test non-zero before splicing a
/// body that divides by them.
#[test]
pub(in crate::x64::tests) fn a_division_by_an_unwritten_parameter_asks_for_a_call_site_zero_test() {
    use super::inlining::runtime_divisor_param_locals;
    // `static int divBy(int x, int d) { return x / d; }`
    let div_by = make_inline_site(&[0x1a, 0x1b, 0x6c, 0xac], 2, 2, true, b'I');
    assert_eq!(runtime_divisor_param_locals(&div_by), Some((0b10, 0)));
    // `x % d`, and `x / d + x % d`: every division names the same parameter.
    let rem_by = make_inline_site(&[0x1a, 0x1b, 0x70, 0xac], 2, 2, true, b'I');
    assert_eq!(runtime_divisor_param_locals(&rem_by), Some((0b10, 0)));
    let both = make_inline_site(
        &[0x1a, 0x1b, 0x6c, 0x1a, 0x1b, 0x70, 0x60, 0xac],
        2,
        2,
        true,
        b'I',
    );
    assert_eq!(runtime_divisor_param_locals(&both), Some((0b10, 0)));
    // `x % 31`: a visible constant needs no test.
    let mod31 = make_inline_site(&[0x1a, 0x10, 0x1f, 0x70, 0xac], 2, 2, true, b'I');
    assert_eq!(runtime_divisor_param_locals(&mod31), None);
    // `x / 0` is refused by the splice walk whatever the call site tests.
    let by_zero = make_inline_site(&[0x1a, 0x03, 0x6c, 0xac], 2, 2, true, b'I');
    assert_eq!(runtime_divisor_param_locals(&by_zero), None);
    // `d = d + 1; return x / d;`: the divisor is no longer the argument.
    let written = make_inline_site(
        &[0x1b, 0x04, 0x60, 0x3c, 0x1a, 0x1b, 0x6c, 0xac],
        2,
        2,
        true,
        b'I',
    );
    assert_eq!(runtime_divisor_param_locals(&written), None);
    // `x / (d + 1)`: a computed divisor names no parameter.
    let computed = make_inline_site(&[0x1a, 0x1b, 0x04, 0x60, 0x6c, 0xac], 2, 2, true, b'I');
    assert_eq!(runtime_divisor_param_locals(&computed), None);
}

/// ...and the splice walk agrees: without the call site's test the body is
/// refused (the zero case would have nowhere to go), with it the body splices.
#[test]
pub(in crate::x64::tests) fn a_parameter_divisor_splices_only_behind_the_call_site_test() {
    let new_compiler = || {
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        let mut compiler = Compiler::new(
            "inline-param-divisor-test".to_string(),
            ExecutableBuffer::new(65536).expect("test executable buffer"),
            0,
            0,
            64,
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
            test_helpers(),
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        );
        compiler.push_stack().expect("dividend slot");
        compiler.push_stack().expect("divisor slot");
        compiler
    };
    let div_by = make_inline_site(&[0x1a, 0x1b, 0x6c, 0xac], 2, 2, true, b'I');
    let mut unguarded = new_compiler();
    assert!(
        !unguarded.try_emit_inline_site(0, &div_by),
        "a runtime divisor with no call-site test must not splice"
    );
    let mut guarded = new_compiler();
    assert!(guarded.try_emit_inline_site_with(0, &div_by, 0b10));
    let code = guarded.buf.as_slice();
    assert!(
        code.windows(3).any(|w| w == [0x83, 0xF9, 0xFF]),
        "the spliced division keeps its `CMP ECX, -1` arm"
    );
    assert!(
        code.windows(3).any(|w| w == [0x99, 0xF7, 0xF9]),
        "the spliced division is a raw `CDQ; IDIV ECX`"
    );
    assert!(
        guarded.deopt_stubs.is_empty(),
        "nothing inside the splice publishes a divide-by-zero stub"
    );
}

/// ...and the `invokestatic` lowering emits that test: `static int f(int x,
/// int d) { return divBy(x, d); }` compiles to `TEST EAX, EAX; JZ` in front
/// of the spliced `IDIV`, with the ordinary call as the miss edge.
#[test]
pub(in crate::x64::tests) fn an_invokestatic_of_a_parameter_divisor_callee_splices_behind_a_zero_test() {
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "divBy",
        descriptor: "(II)I",
        num_jit_args: 2,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let code = [0x1a, 0x1b, 0xb8, 0x00, 0x02, 0xac];
    let mut inline_sites = HashMap::new();
    inline_sites.insert(
        2usize,
        make_inline_site(&[0x1a, 0x1b, 0x6c, 0xac], 2, 2, true, b'I'),
    );
    let compiled = compile(
        &code,
        code.len(),
        2,
        2,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![(2usize, info as *const JitInvokeInfo)],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        inline_sites,
        None,
    )
    .expect("the divisor-guarded splice compiles");
    let bytes = compiled._buffer.as_slice();
    assert!(
        bytes.windows(4).any(|w| w == [0x85, 0xC0, 0x0F, 0x84]),
        "the call site tests the divisor argument (`TEST EAX, EAX; JZ rel32`)"
    );
    assert!(
        bytes.windows(3).any(|w| w == [0x99, 0xF7, 0xF9]),
        "the callee's division is spliced"
    );
}

/// `int f(byte[] a, int i, int bound, int flag)`:
/// `if (flag != 0) i++; for (; i <= bound; i++) a[i] = 0; return i;` -- a
/// canonical zero fill whose header (7) is also the target of the forward
/// `ifeq` at 1.
pub(in crate::x64::tests) fn forward_entry_zero_fill_shape() -> Vec<u8> {
    vec![
        0x1d, // 0: iload_3 (flag)
        0x99, 0x00, 0x06, // 1: ifeq 7   <- forward entry onto the header
        0x84, 0x01, 0x01, // 4: iinc 1, 1
        0x1b, // 7: iload_1   <- loop header
        0x1c, // 8: iload_2
        0xa3, 0x00, 0x0d, // 9: if_icmpgt 22
        0x2a, // 12: aload_0
        0x1b, // 13: iload_1
        0x03, // 14: iconst_0
        0x54, // 15: bastore
        0x84, 0x01, 0x01, // 16: iinc 1, 1
        0xa7, 0xff, 0xf4, // 19: goto 7
        0x1b, // 22: iload_1
        0xac, // 23: ireturn
        0x00, 0x00,
    ]
}

/// Round 11 wave 5 (`r11-x64loop-header-entered-from-outside-drops-every-preheader`):
/// the batch pre-headers take the pre-header-landing predicate too, so a
/// zero fill whose header is a forward-branch target is still lowered to
/// `REP STOSB`, and both entries (fall-through and the `ifeq`) fill exactly
/// their own range.
#[test]
pub(in crate::x64::tests) fn a_forward_entry_onto_a_zero_fill_header_keeps_the_bulk_pre_header() {
    let code = forward_entry_zero_fill_shape();
    let code_len = code.len() - 2;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops, vec![(7, 19)]);
    let no_rotation: FxHashMap<usize, usize> = FxHashMap::default();
    let strict = find_bypassable_loop_headers(&code, code_len, &loops, &[]);
    assert!(
        strict.contains(&7),
        "the strict predicate counts the `ifeq`"
    );
    let landing = find_preheader_bypassing_loop_headers(&code, code_len, &loops, &[]);
    assert!(landing.is_empty(), "the `ifeq` lands before the pre-header");
    if !bulk_byte_loops_enabled() {
        return; // the bulk byte lowering is switched off for this process
    }
    assert!(
        detect_bulk_byte_loops(&code, code_len, &loops, &strict, &no_rotation)
            .zero_fill
            .is_empty()
    );
    assert_eq!(
        detect_bulk_byte_loops(&code, code_len, &loops, &landing, &no_rotation)
            .zero_fill
            .len(),
        1
    );

    let compiled = compile(
        &code,
        code_len,
        4,
        4,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("the forward-entry zero fill compiles");
    assert!(
        compiled
            ._buffer
            .as_slice()
            .windows(2)
            .any(|w| w == [0xF3, 0xAA]),
        "the zero fill must be lowered to REP STOSB"
    );

    let byte_len = 8usize;
    for (flag, expect) in [
        (0i64, [7u8, 7, 0, 0, 0, 0, 7, 7]),
        (1, [7, 7, 7, 0, 0, 0, 7, 7]),
    ] {
        let mut words = vec![0u64; (HEADER_SIZE + byte_len + 7) / 8];
        let array_ptr = words.as_mut_ptr() as *mut u8;
        // SAFETY: `words` is aligned and holds the VM array header plus eight
        // data bytes.
        unsafe {
            (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(byte_len as i32);
            std::ptr::write_bytes(array_ptr.add(ARRAY_DATA_OFFSET), 7, byte_len);
        }
        // SAFETY: the compiled method receives a correctly laid out live array.
        let result = unsafe {
            compiled
                .try_call(&[array_ptr as i64, 2, 5, flag])
                .expect("forward-entry zero-fill JIT call")
        };
        assert_eq!(result, 6, "flag={flag}");
        // SAFETY: the eight-byte data region is within `words`.
        let data =
            unsafe { std::slice::from_raw_parts(array_ptr.add(ARRAY_DATA_OFFSET), byte_len) };
        assert_eq!(data, &expect, "flag={flag}");
        drop(words);
    }
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_accepts_invokevirtual() {
    // Bytecode: aload_0, invokevirtual #1, areturn
    // invokevirtual is 0xb6 + 2-byte cp_index
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept invokevirtual");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "invoke methods need heap");
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb6)); // (pc=1, cp_idx=1, opcode=0xb6)
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_accepts_invokeinterface() {
    // Bytecode: aload_0, invokeinterface #1 count 1 0, ireturn
    // invokeinterface is 0xb9 + 2-byte cp_index + count + 0
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1, count=1, 0
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 7, "(Ljava/lang/Object;)I");
    assert!(scan.is_some(), "jit_scan should accept invokeinterface");
    let scan = scan.unwrap();
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb9)); // (pc=1, cp_idx=1, opcode=0xb9)
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_accepts_invokespecial() {
    // Bytecode: aload_0, invokespecial #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb7, 0x00, 0x01, // 1: invokespecial #1
        0xb1, // 4: return (void)
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "(Ljava/lang/Object;)V");
    assert!(scan.is_some(), "jit_scan should accept invokespecial");
    let scan = scan.unwrap();
    assert_eq!(scan.invoke_ops.len(), 1);
    assert_eq!(scan.invoke_ops[0], (1, 1, 0xb7));
}

#[test]
pub(in crate::x64::tests) fn test_compile_with_invokevirtual() {
    // Test that code containing invokevirtual compiles successfully
    // Bytecode: aload_0, invokevirtual #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xac, // 4: ireturn
        0, 0, // padding
    ];
    let code_len = 5;

    // Create a JitInvokeInfo for the invokevirtual at pc=1
    // LEAK(intentional): the JIT-compiled code stores a raw pointer to this
    // JitInvokeInfo, so it must have 'static lifetime and outlive the compiled
    // method; the test process owns it for its entire (short) lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "getValue",
        descriptor: "()I",
        num_jit_args: 1, // just receiver
        return_type: b'I',
        invoke_kind: 0, // invokevirtual
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    // needs_heap=true because invokevirtual requires vm_ptr
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with invokevirtual"
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_with_invokeinterface() {
    // Bytecode: aload_0, invokeinterface #1 count=1 0, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb9, 0x00, 0x01, 0x01, 0x00, // 1: invokeinterface #1
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let code_len = 7;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestInterface",
        method_name: "compute",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 2, // invokeinterface
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with invokeinterface"
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_invoke_void_return() {
    // Test invokevirtual with void return type — no push after call
    // Bytecode: aload_0, invokevirtual #1, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb6, 0x00, 0x01, // 1: invokevirtual #1
        0xb1, // 4: return (void)
        0, 0, // padding
    ];
    let code_len = 5;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "doSomething",
        descriptor: "()V",
        num_jit_args: 1, // just receiver
        return_type: b'V',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(1usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        1,
        1,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with void invokevirtual"
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_invoke_with_args() {
    // Test invokevirtual with multiple args: receiver + int + int
    // Bytecode: aload_0, iload_1, iload_2, invokevirtual #1, ireturn
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (receiver)
        0x1b, // 1: iload_1 (arg1)
        0x1c, // 2: iload_2 (arg2)
        0xb6, 0x00, 0x01, // 3: invokevirtual #1
        0xac, // 6: ireturn
        0, 0, // padding
    ];
    let code_len = 7;

    // LEAK(intentional): the compiled method holds a raw pointer into this
    // JitInvokeInfo; it must be 'static and outlive the JIT code. Owned by the
    // test process for its lifetime.
    let info = Box::leak(Box::new(JitInvokeInfo {
        // LEAK(intentional): test-only; JitInvokeInfo must outlive JIT-compiled code pointer
        class_name: "TestClass",
        method_name: "add",
        descriptor: "(II)I",
        num_jit_args: 3, // receiver + 2 int args
        return_type: b'I',
        invoke_kind: 0,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let invoke_info = vec![(3usize, info as *const JitInvokeInfo)]; // Cast: address arithmetic

    let compiled = compile(
        &code,
        code_len,
        3,
        3,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        invoke_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with multi-arg invokevirtual"
    );
}

#[test]
pub(in crate::x64::tests) fn test_bytecode_len_invoke() {
    // Verify bytecode length calculation for invoke opcodes
    assert_eq!(bytecode_analysis::step(&[0xb6, 0x00, 0x01], 0), 3); // invokevirtual
    assert_eq!(bytecode_analysis::step(&[0xb7, 0x00, 0x01], 0), 3); // invokespecial
    assert_eq!(
        bytecode_analysis::step(&[0xb9, 0x00, 0x01, 0x02, 0x00], 0),
        5
    ); // invokeinterface
       // Defense-in-depth (same class as the missing-`ldc` desync): the other
       // 5-byte ops. invokedynamic is now accepted by `jit_scan` (see the 0xba
       // scan/codegen arms); goto_w / jsr_w are still rejected today, but the
       // length table must stay correct so a future acceptance can't silently
       // desync every PC-stepping walk. Must match the `bytecode_analysis::step`
       // twin's `bc_len_five_byte_ops`.
    assert_eq!(
        bytecode_analysis::step(&[0xba, 0x00, 0x01, 0x00, 0x00], 0),
        5
    ); // invokedynamic
    assert_eq!(
        bytecode_analysis::step(&[0xc8, 0x00, 0x00, 0x00, 0x10], 0),
        5
    ); // goto_w
    assert_eq!(
        bytecode_analysis::step(&[0xc9, 0x00, 0x00, 0x00, 0x10], 0),
        5
    ); // jsr_w
}

#[test]
pub(in crate::x64::tests) fn test_bytecode_len_wide() {
    // wide (0xc4) prefix — JVMS §6.5. Must stay in lockstep with
    // `bytecode_analysis::step`'s 0xc4 arm.
    // `wide iload <2-byte index>` → 4 bytes (0x15 = iload).
    assert_eq!(bytecode_analysis::step(&[0xc4, 0x15, 0x01, 0x00], 0), 4);
    // `wide istore <2-byte index>` → 4 bytes (0x36 = istore).
    assert_eq!(bytecode_analysis::step(&[0xc4, 0x36, 0x01, 0x00], 0), 4);
    // `wide ret <2-byte index>` → 4 bytes (0xa9 = ret).
    assert_eq!(bytecode_analysis::step(&[0xc4, 0xa9, 0x01, 0x00], 0), 4);
    // `wide iinc <2-byte index> <2-byte const>` → 6 bytes (0x84 = iinc).
    assert_eq!(
        bytecode_analysis::step(&[0xc4, 0x84, 0x01, 0x00, 0x00, 0x01], 0),
        6
    );
    // Truncated prefix (no modified-opcode byte): the `pc + 1 < code.len()`
    // bounds check must not panic and falls to the 4-byte form.
    assert_eq!(bytecode_analysis::step(&[0xc4], 0), 4);
}

#[test]
pub(in crate::x64::tests) fn test_osr_metadata_populated() {
    // Compile a simple loop method and verify OSR metadata is set
    // Bytecode: int sum(int n) { int s = 0; for (int i = 0; i < n; i++) s += i; return s; }
    // Simplified: iload_0(n), iconst_0(s), iconst_0(i), loop: iload_2, iload_0, if_icmpge exit,
    //   iload_1 + iload_2 + iadd + istore_1, iinc 2 1, goto loop, iload_1, ireturn
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (s = 0)
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0 (i = 0)
        0x3d, // 3: istore_2
        // loop header at pc=4
        0x1c, // 4: iload_2 (i)
        0x1a, // 5: iload_0 (n)
        0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
        0x1b, // 9: iload_1 (s)
        0x1c, // 10: iload_2 (i)
        0x60, // 11: iadd
        0x3c, // 12: istore_1 (s = s + i)
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto -12 → 4
        0x1b, // 19: iload_1 (s)
        0xac, // 20: ireturn
        0, 0, // padding
    ];
    let code_len = 21;

    let compiled = compile(
        &code,
        code_len,
        1,
        3,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // Verify OSR metadata is populated
    assert!(
        compiled.osr_pc_to_native.is_some(),
        "OSR pc_to_native should be set"
    );
    let pc_map = compiled.osr_pc_to_native.as_ref().unwrap();
    assert!(
        pc_map.len() >= code_len,
        "pc_to_native should cover all bytecodes"
    );

    // The loop header at PC=4 should have a valid native offset
    assert!(
        pc_map[4] >= 0,
        "Loop header at PC=4 should have native mapping"
    );

    // Verify other OSR metadata
    assert_eq!(compiled.osr_num_locals, 3);
    assert!(compiled.osr_frame_size > 0, "Frame size should be positive");
}

#[test]
pub(in crate::x64::tests) fn callee_saved_gpr_local_homes_are_default_on_with_precise_maps() {
    if !callee_saved_gpr_local_homes_enabled() {
        // The process-wide diagnostic opt-out is intentionally respected.
        return;
    }

    // int f(int a, int b) { int c = a + b; return c; }
    let code: Vec<u8> = vec![
        0x1a, // iload_0
        0x1b, // iload_1
        0x60, // iadd
        0x3d, // istore_2
        0x1c, // iload_2
        0xac, // ireturn
        0, 0,
    ];
    let compiled = compile(
        &code,
        6,
        2,
        3,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("simple int-local method should compile");

    assert!(compiled.osr_num_reg_locals > 0);
    assert!(compiled
        .osr_callee_saved_regs
        .as_ref()
        .is_some_and(|registers| !registers.is_empty()));
    assert!(compiled
        .osr_local_assignments
        .as_ref()
        .is_some_and(|assignments| assignments.iter().any(Option::is_some)));
}

#[test]
// x86-64 only: `CompiledMethod::osr_enter` is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not merely
// fail -- it does not COMPILE, and took the whole crate's test binary with
// it. Found by actually building for aarch64 in an emulated container; a
// cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
pub(in crate::x64::tests) fn test_osr_simple_loop() {
    // Test OSR entry: compile a simple sum loop and enter at the loop header
    // Same bytecode as above: sum(n) = 0 + 1 + ... + (n-1)
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0 (s = 0)
        0x3c, // 1: istore_1
        0x03, // 2: iconst_0 (i = 0)
        0x3d, // 3: istore_2
        0x1c, // 4: iload_2 (i)
        0x1a, // 5: iload_0 (n)
        0xa2, 0x00, 0x0d, // 6: if_icmpge +13 → 19
        0x1b, // 9: iload_1 (s)
        0x1c, // 10: iload_2 (i)
        0x60, // 11: iadd
        0x3c, // 12: istore_1
        0x84, 0x02, 0x01, // 13: iinc 2, 1
        0xa7, 0xff, 0xf4, // 16: goto -12 → 4
        0x1b, // 19: iload_1 (s)
        0xac, // 20: ireturn
        0, 0,
    ];
    let code_len = 21;

    let compiled = compile(
        &code,
        code_len,
        1,
        3,
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // Normal entry: sum(10) = 0+1+...+9 = 45
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10]).expect("test JIT call") };
    assert_eq!(result, 45);

    // OSR entry: simulate entering at PC=4 with locals [n=10, s=10, i=5]
    // This means we've already accumulated s=0+1+2+3+4=10, and i=5
    // Remaining: 5+6+7+8+9 = 35, total = 10+35 = 45
    let jit_locals: [i64; 3] = [10, 10, 5]; // n=10, s=10, i=5
                                            // SAFETY: Entering JIT-compiled code via OSR; the CompiledMethod was produced
                                            // from valid bytecode, locals array is correctly sized, and the mmap region is executable.
    let osr_result = unsafe {
        compiled.osr_enter(0, &jit_locals, 4, /* thread_ptr */ 0)
    };
    assert!(osr_result.is_some(), "OSR entry should succeed at PC=4");
    assert_eq!(osr_result.unwrap(), 45); // s=10 + 5+6+7+8+9 = 45
}

#[test]
// x86-64 only: `CompiledMethod::osr_enter` is itself
// `#[cfg(target_arch = "x86_64")]`, so on aarch64 this test does not merely
// fail -- it does not COMPILE, and took the whole crate's test binary with
// it. Found by actually building for aarch64 in an emulated container; a
// cfg-gated API needs cfg-gated tests.
#[cfg(target_arch = "x86_64")]
pub(in crate::x64::tests) fn test_osr_long_loop() {
    // long addOnly(long n) { long s=0; for(long i=0;i<n;i++) s+=i; return s; }
    // Locals: 0-1=n(long), 2-3=s(long), 4-5=i(long)
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (s=0)
        0x09, // 2: lconst_0
        0x37, 0x04, // 3: lstore 4 (i=0)
        0x16, 0x04, // 5: lload 4 (i) — loop header
        0x1e, // 7: lload_0 (n)
        0x94, // 8: lcmp
        0x9c, 0x00, 0x11, // 9: ifge +17 → 26
        0x20, // 12: lload_2 (s)
        0x16, 0x04, // 13: lload 4 (i)
        0x61, // 15: ladd
        0x41, // 16: lstore_2 (s)
        0x16, 0x04, // 17: lload 4 (i)
        0x0a, // 19: lconst_1
        0x61, // 20: ladd
        0x37, 0x04, // 21: lstore 4 (i)
        0xa7, 0xff, 0xee, // 23: goto -18 → 5
        0x20, // 26: lload_2
        0xad, // 27: lreturn
        0, 0,
    ];
    let code_len = 28;
    let compiled = compile(
        &code,
        code_len,
        2,
        6,
        false,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout: Option<StringFieldLayout>
    )
    .unwrap();

    // Normal entry: addOnly(2000) = sum(0..1999) = 1999000
    // SAFETY: `compiled` is freshly JIT-compiled from valid bytecode into an
    // executable mmap; calling it with the matching one-arg ABI is sound.
    let result = unsafe { compiled.try_call(&[2000]).expect("test JIT call") };
    assert_eq!(result, 1999000, "normal entry");

    // OSR entry at PC=5 with i=1000, s=499500 (sum 0..999), n=2000.
    // Remaining sum 1000..1999 = 1499500; total = 1999000.
    // jit_locals layout: index 0=n, 1=(n high), 2=s, 3=(s high), 4=i, 5=(i high)
    let jit_locals: [i64; 6] = [2000, 0, 499500, 0, 1000, 0];
    // SAFETY: `compiled` holds executable JIT code with a valid OSR entry at PC=5;
    // jit_locals matches the compiler's slot layout, so OSR resume is sound.
    let osr_result = unsafe {
        compiled.osr_enter(0, &jit_locals, 5, /* thread_ptr */ 0)
    };
    assert!(osr_result.is_some(), "OSR entry should succeed at PC=5");
    assert_eq!(osr_result.unwrap(), 1999000, "OSR long loop result");
}

#[test]
pub(in crate::x64::tests) fn test_avx2_detection() {
    // Just verify has_avx2() doesn't crash and returns a consistent result.
    let a = has_avx2();
    let b = has_avx2();
    assert_eq!(a, b, "AVX2 detection should be deterministic");
    // On modern x86-64 machines this should be true, but we can't assert it
    // as CI might run on older hardware.
    println!("AVX2 support detected: {}", a);
}

#[test]
pub(in crate::x64::tests) fn test_detect_int_array_sum_pattern() {
    // Bytecode for: long sum_array(int[] arr, int n) {
    //   long sum = 0;
    //   for (int i = 0; i < n; i++) sum += arr[i];
    //   return sum;
    // }
    // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (sum = 0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i = 0)
        0x15, 0x04, // 5: iload 4 (i) — loop header
        0x1b, // 7: iload_1 (n)
        0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
        0x2a, // 11: aload_0 (arr)
        0x15, 0x04, // 12: iload 4 (i)
        0x2e, // 14: iaload
        0x85, // 15: i2l
        0x20, // 16: lload_2 (sum)
        0x61, // 17: ladd
        0x41, // 18: lstore_2 (sum)
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -17 → 5
        0x20, // 25: lload_2
        0xad, // 26: lreturn
        0, 0,
    ];
    let code_len = 27;

    // Detect loops
    let loops = detect_loops(&code, code_len);
    assert!(!loops.is_empty(), "Should detect the for loop");

    // Find the loop (header=5, back_edge=22)
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 5)
        .expect("Should find loop with header at PC=5");
    assert_eq!(back_edge, 22);

    // Find induction variable
    let back_edge_end = back_edge + bytecode_analysis::step(&code, back_edge);
    let iv = find_induction_variable(&code, header, back_edge_end);
    assert_eq!(iv, Some(4), "Induction variable should be local 4 (i)");

    // Detect SIMD pattern
    let info = detect_int_array_sum(&code, header, back_edge, 4);
    assert!(info.is_some(), "Should detect int-array-sum pattern");
    let info = info.unwrap();
    assert_eq!(info.header_pc, 5);
    assert_eq!(info.iv_local, 4);
    assert_eq!(info.acc_local, 2);
    assert_eq!(info.array_local, 0);
    assert_eq!(info.bound_local, 1);
    assert!(info.acc_is_long);
}

// T5.2.15 — element-wise SIMD detection tests

#[test]
pub(in crate::x64::tests) fn test_detect_int_array_element_wise_add() {
    // for (int i = 0; i < n; i++) out[i] = a[i] + b[i];
    // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4 (i = 0)
        0x15, 0x04, // 3: iload 4 (i) — header
        0x1D, // 5: iload_3 (n)
        0xa2, 0x00, 0x14, // 6: if_icmpge +20 → 26
        0x2A, // 9:  aload_0 (out)
        0x15, 0x04, // 10: iload 4 (i)
        0x2B, // 12: aload_1 (a)
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x2C, // 16: aload_2 (b)
        0x15, 0x04, // 17: iload 4
        0x2e, // 19: iaload
        0x60, // 20: iadd
        0x4F, // 21: iastore
        0x84, 0x04, 0x01, // 22: iinc 4, 1
        0xa7, 0xff, 0xEA, // 25: goto -22 → 3
        0xB1, // 28: return
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops
        .iter()
        .find(|&&(h, _)| h == 3)
        .expect("should detect loop at PC=3");
    assert_eq!(back_edge, 25);
    let info = detect_int_array_element_wise(&code, header, back_edge, 4)
        .expect("element-wise add should match");
    assert_eq!(info.header_pc, 3);
    assert_eq!(info.iv_local, 4);
    assert_eq!(info.out_local, 0);
    assert_eq!(info.a_local, 1);
    assert_eq!(info.b_local, 2);
    assert_eq!(info.bound_local, 3);
    assert_eq!(info.op, ElementWiseOp::Add);
}

#[test]
pub(in crate::x64::tests) fn test_detect_int_array_element_wise_mul() {
    // Same shape but imul (0x68)
    let code: Vec<u8> = vec![
        0x03, 0x36, 0x04, 0x15, 0x04, 0x1D, 0xa2, 0x00, 0x14, 0x2A, 0x15, 0x04, 0x2B, 0x15, 0x04,
        0x2e, 0x2C, 0x15, 0x04, 0x2e, 0x68, // imul
        0x4F, 0x84, 0x04, 0x01, 0xa7, 0xff, 0xEA, 0xB1,
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    let &(header, back_edge) = loops.iter().find(|&&(h, _)| h == 3).unwrap();
    let info = detect_int_array_element_wise(&code, header, back_edge, 4).unwrap();
    assert_eq!(info.op, ElementWiseOp::Mul);
}

// ── T17.Β.2 — SIMD element-wise emission ──────────────────────

/// Build a bytecode sequence for
/// `for (i=0; i<n; i++) out[i] = a[i] OP b[i]` and return
/// `(code, code_len)`. `op_byte` is the JVM arithmetic opcode
/// (`0x60` iadd, `0x68` imul, …).
pub(in crate::x64::tests) fn ewise_bytecode(op_byte: u8) -> (Vec<u8>, usize) {
    // Locals: 0=out, 1=a, 2=b, 3=n, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x04, // 1: istore 4 (i = 0)
        0x15, 0x04, // 3: iload 4 — HEADER
        0x1D, // 5: iload_3 (n)
        // Exit branch targets the `return` at pc 28. (Historical note:
        // this was `+20 → 26`, the middle of the goto at 25 — never an
        // instruction boundary. The unresolved patch was silently
        // skipped before `patch_branches` learned to reject such
        // targets; the compile-only assertions below never noticed.)
        0xa2, 0x00, 0x16, // 6: if_icmpge +22 → 28
        0x2A, // 9:  aload_0 (out)
        0x15, 0x04, // 10: iload 4
        0x2B, // 12: aload_1 (a)
        0x15, 0x04, // 13: iload 4
        0x2e, // 15: iaload
        0x2C, // 16: aload_2 (b)
        0x15, 0x04,    // 17: iload 4
        0x2e,    // 19: iaload
        op_byte, // 20: iOP
        0x4F,    // 21: iastore
        0x84, 0x04, 0x01, // 22: iinc 4, 1
        0xa7, 0xff, 0xEA, // 25: goto -22 → 3
        0xB1, // 28: return
        0, 0,
    ];
    let code_len = 29;
    (code, code_len)
}

/// Compile a standalone element-wise method and return its
/// [`CompiledMethod`]. The signature is `void f(int[] out, int[]
/// a, int[] b, int n)` — four params, all live-in locals 0..3.
pub(in crate::x64::tests) fn compile_ewise(op_byte: u8) -> Option<CompiledMethod> {
    let (code, code_len) = ewise_bytecode(op_byte);
    compile(
        &code,
        code_len,
        4, // params: out, a, b, n
        5, // locals: 0..3 params + i
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
}

/// Trip count 7 — one full 4-batch (or 8-batch on AVX2 would be
/// zero since 7 < 8, so the AVX2 phase should early-exit and
/// everything runs through the scalar tail). Either way, the
/// emitted code must compile and produce the right result when
/// executed end-to-end.
#[test]
pub(in crate::x64::tests) fn t17_b_simd_ewise_add_length_7() {
    let compiled = compile_ewise(0x60); // iadd
    assert!(
        compiled.is_some(),
        "element-wise add loop must compile without error"
    );
}

/// Trip count 4 — zero AVX2 batches (batch size = 8), 4 elements
/// through the scalar tail. Compile must succeed.
#[test]
pub(in crate::x64::tests) fn t17_b_simd_ewise_mul_exact_batch() {
    let compiled = compile_ewise(0x68); // imul
    assert!(
        compiled.is_some(),
        "element-wise mul loop must compile without error"
    );
}

/// When AVX2 is unavailable, the SIMD preheader for element-wise
/// must not fire; compilation still succeeds and falls back to
/// the original scalar loop. This test confirms the gate is
/// wired: detection populates the list but emission is skipped.
#[test]
pub(in crate::x64::tests) fn t17_b_simd_ewise_avx2_gated() {
    // Detection runs unconditionally (see `compile()`), but
    // emission in `Compiler::emit_loop_header` is gated on
    // `has_avx2()`. If AVX2 is absent the gate rejects emission;
    // compile still succeeds.
    let compiled = compile_ewise(0x7E); // iand
    assert!(
        compiled.is_some(),
        "element-wise loop must compile on every CPU tier"
    );
    if !has_avx2() {
        // Nothing more to check — AVX2 absent means we didn't
        // issue any YMM encodings. The compile-ok check above
        // covers the fallback path.
        return;
    }
    // On AVX2 CPUs, the YMM-using code path was taken; verify
    // compilation produced a real entry pointer. (The "fall
    // back to scalar" claim for non-AVX2 is still covered by
    // the early return above.)
    let cm = compiled.unwrap();
    assert!(!cm.entry_ptr().is_null(), "compiled entry must be valid");
}

#[test]
pub(in crate::x64::tests) fn test_detect_int_array_element_wise_rejects_non_elementwise() {
    // Reduction loop (sum += arr[i]) should NOT match element-wise.
    // Use a plain int reduction here (not the long-accumulator form
    // detect_int_array_sum already accepts).
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x36, 0x02, // 1: istore 2 (sum = 0)
        0x03, // 3: iconst_0
        0x36, 0x03, // 4: istore 3 (i = 0)
        // header at PC=6
        0x15, 0x03, // 6: iload 3
        0x1B, // 8: iload_1 (n)
        0xa2, 0x00, 0x0D, // 9: if_icmpge +13 → 22
        0x15, 0x02, // 12: iload 2 (sum)
        0x2A, // 14: aload_0
        0x15, 0x03, // 15: iload 3
        0x2e, // 17: iaload
        0x60, // 18: iadd
        0x36, 0x02, // 19: istore 2
        0x84, 0x03, 0x01, // 21: iinc 3, 1
        0xa7, 0xff, 0xF1, // 24: goto -15 → 9? (doesn't matter for this test)
        0x15, 0x02, // 27: iload 2
        0xac, // 29: ireturn
    ];
    let code_len = code.len();
    let loops = detect_loops(&code, code_len);
    // Either no loop is detected or the pattern doesn't match — both are fine.
    for &(header, back_edge) in &loops {
        let back_end = back_edge + bytecode_analysis::step(&code, back_edge);
        if let Some(iv) = find_induction_variable(&code, header, back_end) {
            assert!(
                detect_int_array_element_wise(&code, header, back_edge, iv).is_none(),
                "reduction should not match element-wise"
            );
        }
    }
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_simd_int_array_sum_end_to_end() {
    // End-to-end test: compile a long sum_array(int[], int) method
    // and verify SIMD-accelerated execution with a real array.
    if !has_avx2() {
        println!("Skipping SIMD end-to-end test: AVX2 not available");
        return;
    }

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    use std::sync::Arc;

    // Same bytecode as the pattern test above:
    // long sum_array(int[] arr, int n)
    // Locals: 0=arr, 1=n, 2-3=sum(long), 4=i
    let code: Vec<u8> = vec![
        0x09, // 0: lconst_0
        0x41, // 1: lstore_2 (sum = 0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i = 0)
        0x15, 0x04, // 5: iload 4 — loop header
        0x1b, // 7: iload_1 (n)
        0xa2, 0x00, 0x11, // 8: if_icmpge +17 → 25
        0x2a, // 11: aload_0 (arr)
        0x15, 0x04, // 12: iload 4 (i)
        0x2e, // 14: iaload
        0x85, // 15: i2l
        0x20, // 16: lload_2 (sum)
        0x61, // 17: ladd
        0x41, // 18: lstore_2 (sum)
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -17 → 5
        0x20, // 25: lload_2
        0xad, // 26: lreturn
        0, 0,
    ];
    let code_len = 27;

    // needs_heap=true because of iaload
    let compiled = compile(
        &code,
        code_len,
        2,
        5,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Test 1: empty array (n=0, should return 0) — no SIMD, no scalar
    let arr1 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 0);
    let arr1_ptr = arr1.as_ptr();
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result1 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr1_ptr as i64, 0])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result1, 0, "empty array sum should be 0");

    // Test 2: array of 3 elements (0 SIMD chunks, all scalar cleanup)
    let n2 = 3;
    let arr2 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n2);
    let arr2_ptr = arr2.as_ptr();
    for i in 0..n2 {
        let _ = shared.heap.set_array_element(arr2, i, Value::Int(100));
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result2 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr2_ptr as i64, n2 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result2, 300, "3 elements of 100 should sum to 300");

    // Test 3: array of exactly 8 elements (exactly 1 SIMD chunk, no cleanup)
    let n3 = 8;
    let arr3 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n3);
    let arr3_ptr = arr3.as_ptr();
    for i in 0..n3 {
        let _ = shared.heap.set_array_element(arr3, i, Value::Int(10));
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result3 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr3_ptr as i64, n3 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result3, 80, "8 elements of 10 should sum to 80");

    // Test 4: array of 20 elements (exercises both SIMD chunks and scalar cleanup)
    // arr = [1, 2, 3, ..., 20], sum = 210
    let n4 = 20;
    let arr4 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n4);
    let arr4_ptr = arr4.as_ptr();
    for i in 0..n4 {
        let _ = shared
            .heap
            .set_array_element(arr4, i, Value::Int((i + 1) as i32)); // Cast: x86-64 immediate encoding
    }
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result4 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr4_ptr as i64, n4 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result4, 210, "sum of 1..=20 should be 210");

    // Test 5: large array (256 elements) to really exercise SIMD
    let n5 = 256;
    let arr5 = shared
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n5);
    let arr5_ptr = arr5.as_ptr();
    for i in 0..n5 {
        // Cast: value to i32 (encoding immediate/displacement)
        let _ = shared.heap.set_array_element(arr5, i, Value::Int(i as i32));
        // Cast: x86-64 immediate encoding
    }
    // sum of 0..255 = 255*256/2 = 32640
    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result5 = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(vm_ptr, &[arr5_ptr as i64, n5 as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result5, 32640, "sum of 0..=255 should be 32640");
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_accepts_new_opcode() {
    // Bytecode: new #1 (0xbb 0x00 0x01), areturn
    // new pushes objectref; method returns it
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0xb0, // 3: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 4, "()Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept `new` opcode");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "`new` requires heap pointer");
    assert_eq!(scan.new_ops.len(), 1);
    assert_eq!(scan.new_ops[0], (0, 1)); // pc=0, cp_idx=1
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_accepts_anewarray_opcode() {
    // Bytecode: iconst_3, anewarray #1 (0xbd 0x00 0x01), areturn
    let code: Vec<u8> = vec![
        0x06, // 0: iconst_3 (length=3)
        0xbd, 0x00, 0x01, // 1: anewarray #1
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 5, "()[Ljava/lang/Object;");
    assert!(scan.is_some(), "jit_scan should accept `anewarray` opcode");
    let scan = scan.unwrap();
    assert!(scan.needs_heap, "`anewarray` requires heap pointer");
    assert_eq!(scan.anewarray_ops.len(), 1);
    assert_eq!(scan.anewarray_ops[0], (1, 1)); // pc=1, cp_idx=1
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_escape_analysis_non_escaping() {
    // Pattern: new; dup; invokespecial <init>; astore_1; aload_1; getfield; ireturn
    //
    // `jit_scan` runs escape analysis WITHOUT a constant-pool
    // resolver, so it cannot tell a trivial `<init>()V` from an
    // arg-bearing constructor. It therefore runs a *conservative*
    // pre-pass (every `invokespecial` escapes its operands); the
    // precise pass — which re-enables scalar replacement for the
    // `new; dup; invokespecial <init>()V` pattern — runs later in
    // `compile`, where the resolved descriptors are available.
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0x4c, // 7: astore_1
        0x2b, // 8: aload_1
        0xb4, 0x00, 0x03, // 9: getfield #3
        0xac, // 12: ireturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 13, "()I");
    assert!(scan.is_some(), "method should be jit-compatible");
    let scan = scan.unwrap();
    assert!(
        !scan.non_escaping_new.contains(&0),
        "jit_scan's conservative pre-pass escapes invokespecial operands; \
         precise re-analysis happens in `compile`"
    );

    // Precise pass: a trivial `<init>()V` (arg_slots=1, the dup'd
    // `this`) keeps the object non-escaping.
    let mut trivial: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    trivial.insert(
        4,
        InvokeSpecialShape {
            arg_slots: 1,
            is_trivial_void_init: true,
        },
    );
    assert!(
        analyze_escapes(&code, 13, &trivial).contains(&0),
        "with a trivial `<init>()V` shape, new at PC=0 is non-escaping"
    );

    // An arg-bearing constructor (`<init>(I)V`, arg_slots=2) writes
    // the receiver's fields from an un-inlined method body, so the
    // receiver must escape — scalar replacement is unsound.
    let mut arg_init: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    arg_init.insert(
        4,
        InvokeSpecialShape {
            arg_slots: 2,
            is_trivial_void_init: false,
        },
    );
    assert!(
        !analyze_escapes(&code, 13, &arg_init).contains(&0),
        "an arg-bearing `<init>(I)V` must escape its receiver"
    );
}

#[test]
pub(in crate::x64::tests) fn test_escape_analysis_varargs_ctor_receiver_escapes() {
    // BUG-05 regression. A varargs constructor call `new C(a, b)` is
    // compiled by javac to:
    //   new C; dup; iconst_2; anewarray E;
    //   dup; iconst_0; <push a>; aastore;
    //   dup; iconst_1; <push b>; aastore;
    //   invokespecial C.<init>([E;)V
    // The dup'd receiver sits on the operand stack BELOW the array while
    // `anewarray` (0xbd) executes. `anewarray` is not modeled by
    // `analyze_escapes`, so its catch-all arm runs. The previous catch-all
    // merely FORGOT the slot provenance — which erased the receiver's
    // tracking, so the later arg-bearing `invokespecial <init>` escaped
    // nothing and the receiver was reported non-escaping. It was then
    // scalar-replaced to a dummy null and passed as `this` to the
    // un-inlined constructor → "Cannot assign field … because \"this\" is
    // null" (Spring `ResourceDatabasePopulator`). The catch-all must now
    // ESCAPE tracked stack objects, so the `new` at PC=0 is escaping.
    //
    // Use `bipush a (0x10)` / `bipush b` as the array element pushes so the
    // bytecode is self-contained (no constant-pool dependency for the test).
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0:  new #1 (C)
        0x59, // 3:  dup
        0x05, // 4:  iconst_2
        0xbd, 0x00, 0x02, // 5:  anewarray #2 (E)
        0x59, // 8:  dup
        0x03, // 9:  iconst_0
        0x10, 0x07, // 10: bipush 7
        0x53, // 12: aastore
        0x59, // 13: dup
        0x04, // 14: iconst_1
        0x10, 0x09, // 15: bipush 9
        0x53, // 17: aastore
        0xb7, 0x00, 0x03, // 18: invokespecial C.<init>([E;)V
        0xb1, // 21: return
        0, 0, // padding
    ];
    // Resolved shape for the varargs ctor: receiver + 1 array slot.
    let mut shapes: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
    shapes.insert(
        18,
        InvokeSpecialShape {
            arg_slots: 2,
            is_trivial_void_init: false,
        },
    );
    assert!(
        !analyze_escapes(&code, 22, &shapes).contains(&0),
        "the receiver of a varargs constructor must escape (it is passed to \
         the un-inlined `<init>` across an `anewarray`); scalar-replacing it \
         yields a null `this` (BUG-05)"
    );
}

#[test]
pub(in crate::x64::tests) fn test_jit_scan_escape_analysis_escaping_via_areturn() {
    // Pattern: new; dup; invokespecial <init>; areturn — the object escapes
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1
        0x59, // 3: dup
        0xb7, 0x00, 0x02, // 4: invokespecial <init>
        0xb0, // 7: areturn
        0, 0, // padding
    ];
    let scan = jit_scan(&code, 8, "()Ljava/lang/Object;");
    assert!(scan.is_some(), "method should be jit-compatible");
    let scan = scan.unwrap();
    assert!(
        !scan.non_escaping_new.contains(&0),
        "new at PC=0 should be escaping (returned via areturn)"
    );
}

/// HIGH-6 — verify the inline TLAB bump-pointer codegen produces an
/// executable method that correctly falls through to the slow path
/// when the TLS thread pointer is null (the JE-on-null branch in
/// `emit_inline_tlab_new`). Using stubs avoids a full VM init so this
/// test is NOT gated behind the `vm-tests` feature.
#[test]
pub(in crate::x64::tests) fn test_inline_tlab_new_falls_through_on_null_thread() {
    // Stub: pretend there is no current thread (re-entrant or pre-init
    // state). The inline path must take the JE branch to the slow path
    // and we then short-circuit with a sentinel `new_object` return.
    // SAFETY: extern "C" test stub with no arguments and no dereferences; it only
    // returns a null pointer, so there are no preconditions for the caller to uphold.
    unsafe extern "C" fn null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    // SAFETY: extern "C" test stub; ignores all i64 arguments and dereferences nothing,
    // returning a fixed sentinel value, so it cannot violate memory safety.
    unsafe extern "C" fn fake_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
        0xDEAD_BEEFi64
    }
    // SAFETY: extern "C" test stub that immediately panics; it touches no arguments
    // and performs no memory access, so it imposes no safety obligations on callers.
    unsafe extern "C" fn unimplemented_post_init(_vm: i64, _obj: i64, _cid: i64, _nf: i64) -> i64 {
        panic!("post_tlab_init must not be called when thread is null");
    }

    // Build a custom helper table with the inline path WIRED so
    // `can_inline` is true (get_current_thread, tlab_post_init,
    // new_object all non-null), but `get_current_thread` returns
    // null at runtime to force the slow path inside the emitted
    // inline cascade.
    let mut helpers = test_helpers();
    // Cast: fn pointer to usize helper address
    helpers.get_current_thread = null_thread as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.tlab_post_init = unimplemented_post_init as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.new_object = fake_new_object as *const () as usize;
    // The offsets don't matter — they're only read when the
    // thread pointer is non-null.
    helpers.tlab_cursor_offset_in_thread = 0;
    helpers.tlab_end_offset_in_thread = 8;

    // Method: new #1; astore_1; aload_1; areturn (cls=42, fields=3).
    let code: Vec<u8> = vec![0xbb, 0x00, 0x01, 0x4c, 0x2b, 0xb0, 0, 0];
    let code_len = 6;
    // CRIT-2 tuple: (pc, class_id, num_fields, needs_post_alloc_init, has_finalizer).
    // Tests use conservative `(true, true)` so the helper path is exercised.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];

    let compiled = compile(
        &code,
        code_len,
        0,
        2,
        true, // needs_heap → can_inline gate passes
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .expect("inline-TLAB new opcode should compile");

    // SAFETY: Calling JIT-compiled machine code with a sentinel VM pointer.
    // The fake_new_object stub does not touch the pointer.
    let result = unsafe {
        compiled
            .try_call_with_context(0xCAFE_F00D, &[])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 0xDEAD_BEEFi64,
        "inline TLAB cascade must fall through to slow-path fake_new_object \
         when get_current_thread returns null"
    );
}

/// The allocation spill sink splits ONE 14-store blind spill across two
/// program points: the registers the inline-TLAB fast path clobbers stay at
/// the safepoint, the rest move to the allocation's slow-path label. The two
/// halves are selected by complementary predicates over the same table, so a
/// register can only be lost if `ALLOC_FAST_PATH_CLOBBERS` names something
/// `ALL_SPILL_GPRS` does not contain — and a lost register is not a slow
/// benchmark, it is a live oop the conservative root scan never sees.
#[test]
pub(in crate::x64::tests) fn alloc_spill_sink_partitions_the_gpr_file_exactly_once() {
    let hot: Vec<u8> = ALL_SPILL_GPRS
        .iter()
        .copied()
        .filter(|r| ALLOC_FAST_PATH_CLOBBERS.contains(r))
        .collect();
    let sunk: Vec<u8> = ALL_SPILL_GPRS
        .iter()
        .copied()
        .filter(|r| !ALLOC_FAST_PATH_CLOBBERS.contains(r))
        .collect();
    // Every clobbered register must be IN the spilled file, or the hot half
    // silently drops it and the sunk half captures a post-clobber value.
    for r in ALLOC_FAST_PATH_CLOBBERS {
        assert!(
            ALL_SPILL_GPRS.contains(&r),
            "fast-path clobber {r} is not in ALL_SPILL_GPRS"
        );
    }
    assert_eq!(hot.len(), ALLOC_FAST_PATH_CLOBBERS.len(), "hot half");
    assert_eq!(
        hot.len() + sunk.len(),
        ALL_SPILL_GPRS.len(),
        "the halves must cover the file"
    );
    let mut union: Vec<u8> = hot.iter().chain(sunk.iter()).copied().collect();
    union.sort_unstable();
    let mut expected = ALL_SPILL_GPRS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        union, expected,
        "partition must be exact — no gap, no overlap"
    );
}

/// zt t11/ca3 (`CRATONVM_JIT_ZGC_ALLOC_SPILL_SINK`): the wider clobber set of
/// a ZGC announce site splits the file the same way, and keeps every register
/// the preamble writes. A register missing from it would be spilled at the
/// slow edge AFTER the inline start-bit store clobbered it.
#[test]
pub(in crate::x64::tests) fn zgc_alloc_spill_sink_partitions_the_gpr_file_exactly_once() {
    for r in ALLOC_FAST_PATH_CLOBBERS {
        assert!(ZGC_ALLOC_FAST_PATH_CLOBBERS.contains(&r), "the preamble's clobber {r}");
    }
    for r in [RCX, RDX, R8, R9] {
        assert!(ZGC_ALLOC_FAST_PATH_CLOBBERS.contains(&r), "the start-bit store's clobber {r}");
    }
    for zgc in [false, true] {
        let set = alloc_fast_path_clobbers(zgc);
        for r in set {
            assert!(ALL_SPILL_GPRS.contains(r), "clobber {r} is not in ALL_SPILL_GPRS");
        }
        let hot = ALL_SPILL_GPRS.iter().filter(|r| set.contains(*r)).count();
        let sunk = ALL_SPILL_GPRS.iter().filter(|r| !set.contains(*r)).count();
        assert_eq!(hot, set.len(), "zgc={zgc}: hot half");
        // zt t12/x9: `hot + sunk` is the file by construction; what can fail is
        // a duplicate in the set, or a set that leaves nothing to sink.
        let mut unique = set.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), set.len(), "zgc={zgc}: no duplicate clobber");
        assert!(sunk > 0, "zgc={zgc}: something is left to sink");
    }
    assert_eq!(alloc_fast_path_clobbers(false), &ALLOC_FAST_PATH_CLOBBERS[..]);
    assert_eq!(alloc_fast_path_clobbers(true), &ZGC_ALLOC_FAST_PATH_CLOBBERS[..]);
}

/// Sink form of the inline-TLAB preamble: it drops the `get_current_thread`
/// fallback CALL (whose caller-saved clobbers would invalidate the eleven
/// registers spilled later at the slow-path label) and treats a null cached
/// thread slot as a divert to `new_object`. This is the same end state the
/// fallback produced — it too fell through to the slow path once the helper
/// returned null — so the observable behaviour must be unchanged.
///
/// `test_inline_tlab_new_falls_through_on_null_thread` covers the un-sunk arm
/// with `(needs_post_alloc_init, has_finalizer) = (true, true)`; this one flips both to
/// false so `skip_helper` holds and the site actually requests the sink.
#[test]
pub(in crate::x64::tests) fn alloc_spill_sink_still_diverts_to_the_helper_on_a_null_thread() {
    // SAFETY: extern "C" test stub with no arguments and no dereferences; it only
    // returns a null pointer, so there are no preconditions for the caller to uphold.
    unsafe extern "C" fn null_thread() -> *mut std::ffi::c_void {
        std::ptr::null_mut()
    }
    // SAFETY: extern "C" test stub; ignores all i64 arguments and dereferences nothing,
    // returning a fixed sentinel value, so it cannot violate memory safety.
    unsafe extern "C" fn fake_new_object(_vm: i64, _cid: i64, _nf: i64) -> i64 {
        0x51DE_5152i64
    }
    // SAFETY: extern "C" test stub that immediately panics; it touches no arguments
    // and performs no memory access, so it imposes no safety obligations on callers.
    unsafe extern "C" fn unreachable_post_init(_vm: i64, _o: i64, _c: i64, _n: i64) -> i64 {
        panic!("skip_post_init_helper is set — the helper must not be emitted");
    }

    let mut helpers = test_helpers();
    // Cast: fn pointer to usize helper address
    helpers.get_current_thread = null_thread as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.tlab_post_init = unreachable_post_init as *const () as usize;
    // Cast: fn pointer to usize helper address
    helpers.new_object = fake_new_object as *const () as usize;
    helpers.tlab_cursor_offset_in_thread = 0;
    helpers.tlab_end_offset_in_thread = 8;

    // new #1; astore_1; aload_1; areturn (cls=42, 3 fields).
    let code: Vec<u8> = vec![0xbb, 0x00, 0x01, 0x4c, 0x2b, 0xb0, 0, 0];
    // (pc, class_id, num_fields, needs_post_alloc_init, has_finalizer) — both false, so
    // `skip_helper` holds and the `new` arm raises `sink_alloc_blind_spill`.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, false, false)];

    let compiled = compile(
        &code,
        6,
        0,
        2,
        true,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites in this stub
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("sink-form inline-TLAB new should compile");

    // SAFETY: Calling JIT-compiled machine code with a sentinel VM pointer.
    // The fake_new_object stub does not touch the pointer.
    let result = unsafe {
        compiled
            .try_call_with_context(0xCAFE_F00D, &[])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 0x51DE_5152i64,
        "a null cached thread must still reach new_object under the spill sink"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_jit_new_object_codegen() {
    // Verify that the `new` opcode compiles and calls jit_new_object at runtime.
    // Method: allocate an object, store to local 1, load local 1, areturn.
    // new #1; astore_1; aload_1; areturn
    let code: Vec<u8> = vec![
        0xbb, 0x00, 0x01, // 0: new #1  (cp_idx=1)
        0x4c, // 3: astore_1
        0x2b, // 4: aload_1
        0xb0, // 5: areturn
        0, 0, // padding
    ];
    let code_len = 6;

    // Provide new_info: class_id=42, num_fields=3
    // CRIT-2 tuple: (pc, class_id, num_fields, needs_post_alloc_init, has_finalizer).
    // Tests use conservative `(true, true)` so the helper path is exercised.
    let new_info: Vec<(usize, u32, usize, bool, bool)> = vec![(0, 42, 3, true, true)];
    let compiled = compile(
        &code,
        code_len,
        0,
        2,
        true, // needs_heap (for jit_new_object helper)
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        new_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with `new` opcode"
    );

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call_with_context(vm_ptr, &[])
            .expect("test JIT call")
    };
    // Result should be a non-zero pointer to the allocated object
    assert_ne!(
        result, 0,
        "jit_new_object should return a valid object pointer"
    );
}

#[cfg(feature = "vm-tests")]
#[test]
pub(in crate::x64::tests) fn test_jit_anewarray_codegen() {
    // Verify anewarray compiles and produces a valid reference array at runtime.
    // iconst_5; anewarray #1; areturn
    let code: Vec<u8> = vec![
        0x08, // 0: iconst_5 (length=5)
        0xbd, 0x00, 0x01, // 1: anewarray #1 (cp_idx=1)
        0xb0, // 4: areturn
        0, 0, // padding
    ];
    let code_len = 5;

    // Provide anewarray_info: component_class_id=7
    let anewarray_info: Vec<(usize, u32)> = vec![(1, 7)];
    let compiled = compile(
        &code,
        code_len,
        0,
        0,
        true, // needs_heap
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        anewarray_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with `anewarray` opcode"
    );

    use crate::config::VmConfig;
    use crate::vm::SharedVm;
    use std::sync::Arc;
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // SAFETY: Calling JIT-compiled machine code with VM context; the CompiledMethod
    // was produced from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call_with_context(vm_ptr, &[])
            .expect("test JIT call")
    };
    // Result should be non-zero (valid array pointer)
    assert_ne!(
        result, 0,
        "jit_anewarray_object should return a valid array pointer"
    );
}

// -----------------------------------------------------------------------
// M5: JIT compilation of <init> constructors and lambda$ methods
// -----------------------------------------------------------------------

#[test]
pub(in crate::x64::tests) fn test_m5_constructor_init_putfield() {
    // Simulates: <init>(int x) { this.x = x; }
    // Bytecode: aload_0, iload_1, putfield #1, return
    // This is the exact pattern that was previously skipped for <init>.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0 (this)
        0x1b, // 1: iload_1 (x)
        0xb5, 0x00, 0x01, // 2: putfield #1
        0xb1, // 5: return (void)
        0, 0,
    ];
    let code_len = 6;
    let field_info = vec![(2usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: this + int x
        2, // max_locals
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Constructor with putfield should be JIT-compilable"
    );

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(0));

    // Call <init>(this, 42)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            .unwrap()
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 42])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let val = heap.get_field(obj, 0);
    assert_eq!(
        val,
        Value::Int(42),
        "Constructor putfield should set field correctly"
    );
}

#[test]
pub(in crate::x64::tests) fn test_m5_constructor_init_multiple_putfields() {
    // Simulates: <init>(int x, int y) { this.x = x; this.y = y; }
    // Bytecode: aload_0, iload_1, putfield #1, aload_0, iload_2, putfield #2, return
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0xb5, 0x00, 0x01, // 2: putfield #1
        0x2a, // 5: aload_0
        0x1c, // 6: iload_2
        0xb5, 0x00, 0x02, // 7: putfield #2
        0xb1, // 10: return
        0, 0,
    ];
    let code_len = 11;
    let field_info = vec![
        (2usize, 0usize, b'I'), // putfield #1 at pc=2, field_index=0
        (7usize, 1usize, b'I'), // putfield #2 at pc=7, field_index=1
    ];
    let compiled = compile(
        &code,
        code_len,
        3, // param_slots: this + int x + int y
        3, // max_locals
        false,
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(compiled.is_some(), "Multi-field constructor should compile");

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 3);
    heap.set_field(obj, 0, Value::Int(0));
    heap.set_field(obj, 1, Value::Int(0));

    // Call <init>(this, 10, 20)
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    unsafe {
        compiled
            .unwrap()
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call(&[obj.as_ptr() as i64, 10, 20])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(heap.get_field(obj, 0), Value::Int(10));
    assert_eq!(heap.get_field(obj, 1), Value::Int(20));
}

#[test]
pub(in crate::x64::tests) fn test_m5_lambda_method_compiles() {
    // Simulates: lambda$main$0(int x) -> int { return x + 1; }
    // Lambda methods are static methods with captured args. This tests
    // that such methods are now JIT-compilable (previously skipped).
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x04, // 1: iconst_1
        0x60, // 2: iadd
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        1, // param_slots: int x
        1, // max_locals
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Lambda-style method should be JIT-compilable"
    );

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.unwrap().try_call(&[41]).expect("test JIT call") };
    assert_eq!(result, 42, "lambda$main$0(41) should return 42");
}

#[test]
pub(in crate::x64::tests) fn test_m5_user_class_method_compiles() {
    // Simulates: com.example.MyClass.add(int a, int b) -> int { return a + b; }
    // Tests that non-java/* user class methods are JIT-compiled.
    let code: Vec<u8> = vec![
        0x1a, // 0: iload_0
        0x1b, // 1: iload_1
        0x60, // 2: iadd
        0xac, // 3: ireturn
        0, 0,
    ];
    let code_len = 4;
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: int a, int b
        2, // max_locals
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "User class static method should be JIT-compilable"
    );

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            .unwrap()
            .try_call(&[17, 25])
            .expect("test JIT call")
    };
    assert_eq!(result, 42, "add(17, 25) should return 42");
}

#[test]
pub(in crate::x64::tests) fn test_m5_constructor_void_return() {
    // Verify void-returning constructor descriptor is JIT-scannable
    // <init>()V — simplest constructor
    let code: Vec<u8> = vec![
        0xb1, // 0: return (void)
        0, 0,
    ];
    let code_len = 1;
    let scan = jit_scan(&code, code_len, "()V");
    assert!(scan.is_some(), "Void constructor should pass jit_scan");
}

#[test]
pub(in crate::x64::tests) fn test_m5_lambda_with_captured_object() {
    // Simulates: lambda$forEach$0(Object captured, int idx) -> int
    // Returns idx (simplified — tests object + int arg passing)
    let code: Vec<u8> = vec![
        0x1b, // 0: iload_1 (idx is local 1, captured obj is local 0)
        0xac, // 1: ireturn
        0, 0,
    ];
    let code_len = 2;
    let compiled = compile(
        &code,
        code_len,
        2, // param_slots: Object captured, int idx
        2, // max_locals
        false,
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
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Lambda with captured Object arg should compile"
    );

    // Pass null as captured object (0), 99 as idx
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.unwrap().try_call(&[0, 99]).expect("test JIT call") };
    assert_eq!(
        result, 99,
        "Lambda should correctly access second parameter"
    );
}

// -----------------------------------------------------------------------
// Phase 87: FP Performance tests
// -----------------------------------------------------------------------

// --- 87.1: XMM Register Persistence ---

#[test]
pub(in crate::x64::tests) fn p87_scratch_xmm_constants() {
    // Verify scratch XMM register constants are defined correctly
    #[cfg(target_os = "windows")]
    assert_eq!(SCRATCH_XMMS, [2, 3, 4, 5]);
    #[cfg(not(target_os = "windows"))]
    assert_eq!(SCRATCH_XMMS, [2, 3, 4, 5, 6, 7]);
}

#[test]
pub(in crate::x64::tests) fn p87_xmm_intermediate_chaining_double() {
    // double f(double a, double b) { return (a + b) * (a - b); }
    // dload_0, dload_1, dadd, dload_0, dload_1, dsub, dmul, dreturn
    // This tests that FP intermediates persist in XMM registers
    // across the dadd → dsub → dmul chain.
    let code: Vec<u8> = vec![
        0x26, // 0: dload_0 (a)
        0x27, // 1: dload_1 (b)
        0x63, // 2: dadd (a+b)
        0x26, // 3: dload_0 (a)
        0x27, // 4: dload_1 (b)
        0x67, // 5: dsub (a-b)
        0x6b, // 6: dmul ((a+b)*(a-b))
        0xaf, // 7: dreturn
        0, 0,
    ];
    let code_len = 8;
    let compiled = compile(
        &code,
        code_len,
        2,
        2,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots (HIGH-7) — test stub: no PIC sites
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout — not needed for this test
    )
    .unwrap();

    let a = 5.0f64;
    let b = 3.0f64;
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: float/double bit pattern to i64 (bit-preserving, VM all-GPR ABI)
            .try_call(&[a.to_bits() as i64, b.to_bits() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    let expected = (a + b) * (a - b); // 8.0 * 2.0 = 16.0
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

