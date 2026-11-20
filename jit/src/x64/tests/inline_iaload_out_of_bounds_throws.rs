#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::x64::tests) fn test_inline_iaload_out_of_bounds_throws_aioobe() {
    // int f(int[] arr, int i) { return arr[i]; }
    // The bounds check (`CMP ECX, R10D; JAE stub`) treats the index
    // as unsigned, so both `idx >= len` and negative indices land
    // in the AIOOBE stub which calls `helpers.throw_aioobe`.
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x1b, // 1: iload_1
        0x2e, // 2: iaload
        0xac, // 3: ireturn
        0, 0,
    ];
    let compiled = compile_array_test(&code, 4, 2, 2);

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let arr = heap.alloc_array(ClassId::new(0), ArrayElementType::Int, 3);

    // Index == length (just past the end).
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, 3])
            .expect("test JIT call")
    };
    assert_eq!(result, i64::MIN, "OOB iaload must deopt with sentinel");
    assert_eq!(
        TEST_AIOOBE_HIT.with(|c| c.get()),
        Some((3, 3, 2)),
        "OOB iaload must report index, length, and originating bci"
    );

    // Negative index — unsigned compare catches it as huge.
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, -1])
            .expect("test JIT call")
    };
    assert_eq!(result, i64::MIN, "negative-index iaload must deopt");
    assert!(
        TEST_AIOOBE_HIT.with(|c| c.get()).is_some(),
        "negative-index iaload must take the AIOOBE stub"
    );

    // In-bounds index must NOT trip the stub.
    heap.set_array_element(arr, 2, Value::Int(99)).unwrap();
    TEST_AIOOBE_HIT.with(|c| c.set(None));
    // SAFETY: JIT-compiled code from valid bytecode; mmap region executable.
    let result = unsafe {
        compiled
            .try_call(&[arr.as_ptr() as i64, 2])
            .expect("test JIT call")
    };
    assert_eq!(result, 99);
    assert_eq!(
        TEST_AIOOBE_HIT.with(|c| c.get()),
        None,
        "in-bounds iaload must not call throw_aioobe"
    );
}

// ============================================================
// Task #60 — Unroll robustness tests (helper rel32 re-patching
// + per-clone MIC/PIC slots). These bodies were previously
// outside the byte-copy unroller's allow-list because they
// contained helper CALLs (E8 rel32) and IC sites (MOV R10,
// imm64) that would land on the wrong target after a shift. The
// unroller now re-resolves helper rel32 per copy and mints a
// fresh MIC/PIC slot per IC site per copy.
// ============================================================

/// Unroll a small loop body that calls a helper (`getfield` →
/// `stub_getfield` via `emit_call_absolute`). Verifies the
/// helper rel32 re-patching path: every duplicated copy of the
/// body must dispatch to the same `stub_getfield` address, not
/// to `stub_getfield + shift` (which would SIGSEGV).
///
/// Method shape (Java pseudocode):
/// ```
/// int sum(Foo obj, int n) {
///     int s = 0;
///     int i = 0;
///     while (i < n) {           // <- header
///         s += obj.x;           // getfield → helper call inside loop body
///         i++;
///         // implicit goto header (back-edge that triggers unroll)
///     }
///     return s;
/// }
/// ```
/// The natural Java bytecode lowering uses if_icmpge to exit
/// (forward conditional) and a goto back-edge — the exact shape
/// the unroller recognizes. The loop body contains a `getfield`,
/// which lowers to `emit_call_absolute(helpers.getfield)` — the
/// E8 rel32 site the old allow-list bailed on.
#[test]
pub(in crate::x64::tests) fn test_unroll_with_getfield_helper_call() {
    // Bytecode (offsets in comments):
    //   0: iconst_0          ; push 0
    //   1: istore_2          ; s = 0
    //   2: iconst_0          ; push 0
    //   3: istore_3          ; i = 0
    //   4: iload_3           ; loop header — load i
    //   5: iload_1           ; load n
    //   6: if_icmpge +16→22  ; exit if i >= n
    //   9: iload_2           ; load s
    //  10: aload_0           ; load obj
    //  11: getfield #1       ; obj.x  (lowers to helper call)
    //  14: iadd              ; s + obj.x
    //  15: istore_2          ; s = s + obj.x
    //  16: iinc 3, 1         ; i++
    //  19: goto -15 → 4      ; back-edge (triggers unroll)
    //  22: iload_2
    //  23: ireturn
    let code: Vec<u8> = vec![
        0x03, // 0
        0x3d, // 1
        0x03, // 2
        0x3e, // 3
        0x1d, // 4
        0x1b, // 5
        0xa2, 0x00, 0x10, // 6: if_icmpge +16 → 22
        0x1c, // 9
        0x2a, // 10
        0xb4, 0x00, 0x01, // 11: getfield #1
        0x60, // 14: iadd
        0x3d, // 15: istore_2
        0x84, 0x03, 0x01, // 16: iinc 3 1
        0xa7, 0xff, 0xf1, // 19: goto -15 → 4
        0x1c, // 22
        0xac, // 23
        0, 0,
    ];
    let code_len = 24;

    // Deliberately leave field_info empty so the JIT emits the
    // *helper-call* path for getfield (E8 rel32 → helpers.getfield)
    // instead of the inline MOV. The inline path is shift-safe
    // anyway (no helper call inside the body), so we'd be testing
    // the wrong code path with field_info populated. Test stub
    // `stub_getfield` falls through cleanly for field_index=0 on
    // a 2-slot object.
    let field_info: Vec<(usize, usize, u8)> = Vec::new();

    let compiled = compile(
        &code,
        code_len,
        2,
        4,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
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
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .unwrap();

    // Build a heap object whose field 0 holds the loop's per-
    // iteration addend. The static heuristic unrolls 4x for a
    // body of this size (≤20 bytes between back-edge and header),
    // so the helper rel32 is patched into 3 duplicated copies in
    // addition to the original. If any copy dispatched to the
    // wrong address the call would SIGSEGV before returning.
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(7));

    // n = 8 → loop trips 8 times. With a 4x unroll the body runs
    // a mix of original + copy bodies; correct dispatch from every
    // copy is required to land on stub_getfield → return 7.
    // Expected sum: 7 * 8 = 56.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64, 8])
            .expect("test JIT call")
    };
    assert_eq!(
        result, 56,
        "getfield-in-loop unroll must dispatch correctly"
    );

    // Smaller trip count: 7 * 3 = 21.
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64, 3])
            .expect("test JIT call")
    };
    assert_eq!(result, 21);

    // n = 0 → loop body never executes. Still must compile + run
    // (verifying the unrolled copies don't fault on cold entry).
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64, 0])
            .expect("test JIT call")
    };
    assert_eq!(result, 0);
}

/// Unroll the previously-broken N-Body pattern: getfield-double +
/// dmul + dastore in the same loop body. This is the Body.x
/// SIGSEGV shape from CHANGELOG that the old allow-list specifically
/// targeted. The shape doesn't matter beyond "helper calls + FP
/// math + back-edge" — what matters is that every unrolled copy
/// reaches the correct helper.
///
/// We can't easily run a real N-body kernel from a unit test
/// (it needs allocator + dispatch infrastructure that the test
/// helpers stub out with panic-sentinels). Instead we exercise
/// the same DUPLICATOR code path with a minimal double-getfield
/// loop and verify both compile and result, which is what would
/// have crashed before this task.
///
/// ```
/// double sum(Foo obj, int n) {
///   double s = 0;
///   int i = 0;
///   while (i < n) { s += obj.d; i++; }
///   return s;
/// }
/// ```
#[test]
pub(in crate::x64::tests) fn test_unroll_nbody_pattern_getfield_double() {
    // Locals: 0=obj, 1=n, 2..3=s (double, wide), 4=i
    // Bytecode:
    //   0: dconst_0          ; push 0.0
    //   1: dstore_2          ; s = 0
    //   2: iconst_0
    //   3: istore 4          ; i = 0  (wide local index — uses bipush'd istore)
    //   5: iload 4           ; loop header
    //   7: iload_1           ; n
    //   8: if_icmpge +16→24  ; exit
    //  11: dload_2           ; load s
    //  12: aload_0           ; obj
    //  13: getfield #1       ; obj.d → double
    //  16: dadd
    //  17: dstore_2
    //  18: iinc 4, 1
    //  21: goto -16 → 5
    //  24: dload_2
    //  25: dreturn
    //
    // Use simple short-form opcodes where possible. We avoid the
    // wide local 4 by storing i in a non-wide slot — restructure:
    // 0=obj, 1=n, 2..3=s, 4=i (istore 4 = istore + index 4 → 0x36 0x04).
    let code: Vec<u8> = vec![
        0x0e, // 0: dconst_0
        0x49, // 1: dstore_2  (s = 0.0; takes slots 2 and 3)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4
        0x15, 0x04, // 5: iload 4
        0x1b, // 7: iload_1
        0xa2, 0x00, 0x10, // 8: if_icmpge +16 → 24
        0x28, // 11: dload_2
        0x2a, // 12: aload_0
        0xb4, 0x00, 0x01, // 13: getfield #1 (double)
        0x63, // 16: dadd
        0x49, // 17: dstore_2
        0x84, 0x04, 0x01, // 18: iinc 4 1
        0xa7, 0xff, 0xf0, // 21: goto -16 → 5
        0x28, // 24: dload_2
        0xaf, // 25: dreturn
        0, 0,
    ];
    let code_len = 26;

    // getfield at pc=13, field_index=0, type='D'.
    let field_info = vec![(13usize, 0usize, b'D')];

    let compiled = compile(
        &code,
        code_len,
        2,
        6,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Double(2.5));

    // n=8, addend=2.5 → 20.0. Return is a double-bits-as-i64.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64, 8])
            .expect("test JIT call")
    };
    let result_f = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert!(
        (result_f - 20.0).abs() < 1e-9,
        "expected ~20.0, got {} (raw={:#x}) — N-Body unroll pattern broke",
        result_f,
        result,
    );

    // n=0 → 0.0 (cold loop body, copies never executed but must
    // still be valid code).
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64, 0])
            .expect("test JIT call")
    };
    let result_f = f64::from_bits(result as u64); // Cast: JIT ABI convention
    assert_eq!(result_f, 0.0);
}


/// Larger unroll trip: 4x with 8 trips means the body executes
/// twice — once via the original and once via a copy — on the
/// first iteration. Stresses both per-clone helper-call patching
/// AND oop-map shift for the same body.
///
/// Method: int sum_two_fields(Foo a, Foo b, int n) {
///   int s = 0;
///   int i = 0;
///   while (i < n) { s += a.x; s += b.x; i++; }
///   return s;
/// }
///
/// Two helper calls per body. With 4x unroll, that's 8 helper
/// calls in the emitted block — each rel32 must independently
/// land on the same `stub_getfield` address. A single off-by-N
/// in the rel32 reconstruction would corrupt one of them and
/// crash on the affected copy.
#[test]
pub(in crate::x64::tests) fn test_unroll_with_two_getfields_per_body() {
    // Locals: 0=a, 1=b, 2=n, 3=s, 4=i
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3   (s=0)
        0x03, // 2: iconst_0
        0x36, 0x04, // 3: istore 4 (i=0)
        0x15, 0x04, // 5: iload 4 (header)
        0x1c, // 7: iload_2 (n)
        0xa2, 0x00, 0x15, // 8: if_icmpge +21 → 29
        0x1d, // 11: iload_3 (s)
        0x2a, // 12: aload_0 (a)
        0xb4, 0x00, 0x01, // 13: getfield #1
        0x60, // 16: iadd
        0x2b, // 17: aload_1 (b)
        0xb4, 0x00, 0x01, // 18: getfield #1
        0x60, // 21: iadd
        0x3e, // 22: istore_3
        0x84, 0x04, 0x01, // 23: iinc 4, 1
        0xa7, 0xff, 0xeb, // 26: goto -21 → 5
        0x1d, // 29: iload_3
        0xac, // 30: ireturn
        0, 0,
    ];
    let code_len = 31;

    let field_info = vec![(13usize, 0usize, b'I'), (18usize, 0usize, b'I')];

    let compiled = compile(
        &code,
        code_len,
        3,
        5,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let a = heap.alloc_object(ClassId::new(0), 2);
    let b = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(a, 0, Value::Int(3));
    heap.set_field(b, 0, Value::Int(5));

    // n=10 → (3 + 5) * 10 = 80.
    // SAFETY: JIT-compiled code from valid bytecode.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[a.as_ptr() as i64, b.as_ptr() as i64, 10])
            .expect("test JIT call")
    };
    assert_eq!(result, 80, "two-helper-calls-per-body unroll failed");

    // n=4 (matches the 4x unroll factor exactly): one full
    // unrolled block, zero spillover. Exercises the case where
    // every copy + the original execute exactly once.
    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[a.as_ptr() as i64, b.as_ptr() as i64, 4])
            .expect("test JIT call")
    };
    assert_eq!(result, 32);
}

/// Helper: drive `compile` over a raw `(I)I` method body. Returns whether
/// compilation succeeded (`Some`) or bailed to the interpreter (`None`).
/// Used by the switch-decode DoS regression tests below: a bail (`None`)
/// is the *expected* safe outcome for crafted/unverified switch bytecode.
pub(in crate::x64::tests) fn try_compile_int_body(code: &[u8], code_len: usize) -> bool {
    compile(
        code,
        code_len,
        1,
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .is_some()
}

/// M2a regression: a `tableswitch` whose `high - low + 1` count overflows
/// i32 (here low = i32::MIN, high = i32::MAX) must not wrap to a bogus
/// positive count and drive a ~16 GB allocation / out-of-bounds read.
/// Compilation must bail (return `None`) without panicking.
#[test]
pub(in crate::x64::tests) fn test_tableswitch_count_overflow_bails() {
    // iconst_0; tableswitch @ pc=1 (padding to pc=4):
    //   default=+0, low=i32::MIN, high=i32::MAX
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00]; // iconst_0 + pad to 4
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&i32::MIN.to_be_bytes()); // low
    code.extend_from_slice(&i32::MAX.to_be_bytes()); // high
                                                     // (no jump-table entries follow — the count guard must reject first)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with overflowing count must bail, not compile"
    );
}

/// M2a regression: a `tableswitch` whose declared table extends past the
/// end of `code` must bail rather than index out of bounds.
#[test]
pub(in crate::x64::tests) fn test_tableswitch_table_past_end_bails() {
    // iconst_0; tableswitch: default=+0, low=0, high=999 (1000 entries)
    // but no entry bytes follow → remaining-bytes guard must reject.
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&0i32.to_be_bytes()); // low
    code.extend_from_slice(&999i32.to_be_bytes()); // high → 1000 entries
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with table past end must bail, not compile"
    );
}

/// M2a regression: a `tableswitch` header truncated by `code_len` must bail
/// before reading the 12-byte default/low/high header.
#[test]
pub(in crate::x64::tests) fn test_tableswitch_truncated_header_bails() {
    // iconst_0; tableswitch + only a few header bytes (header needs 12).
    let mut code: Vec<u8> = vec![0x03, 0xaa, 0x00, 0x00];
    code.extend_from_slice(&[0x00, 0x00, 0x00]); // truncated header
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "tableswitch with truncated header must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` with a negative `npairs` must not be
/// reinterpreted as a huge usize and drive an OOM allocation / OOB read.
#[test]
pub(in crate::x64::tests) fn test_lookupswitch_negative_npairs_bails() {
    // iconst_0; lookupswitch @ pc=1 (padding to pc=4): default=+0, npairs=-1
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&(-1i32).to_be_bytes()); // npairs (negative)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with negative npairs must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` whose declared pair table extends past
/// the end of `code` must bail rather than index out of bounds.
#[test]
pub(in crate::x64::tests) fn test_lookupswitch_pairs_past_end_bails() {
    // iconst_0; lookupswitch: default=+0, npairs=1000 but no pair bytes.
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&0i32.to_be_bytes()); // default
    code.extend_from_slice(&1000i32.to_be_bytes()); // npairs
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with pair table past end must bail, not compile"
    );
}

/// M2b regression: a `lookupswitch` header truncated by `code_len` must
/// bail before reading the 8-byte default/npairs header.
#[test]
pub(in crate::x64::tests) fn test_lookupswitch_truncated_header_bails() {
    let mut code: Vec<u8> = vec![0x03, 0xab, 0x00, 0x00];
    code.extend_from_slice(&[0x00, 0x00, 0x00]); // truncated header (<8)
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "lookupswitch with truncated header must bail, not compile"
    );
}

/// A branch whose target lands INSIDE another instruction (here: the
/// middle of a `goto`'s operand bytes) is never emitted as an
/// instruction boundary, so `pc_to_native[target]` stays -1.
/// `patch_branches` must reject the method (bail to the interpreter)
/// instead of leaving the rel32 placeholder 0 in executable code —
/// a zero rel32 silently falls through, and when the branch is the
/// last emitted instruction execution runs off the body into the
/// out-of-line stubs (observed as STATUS_ACCESS_VIOLATION from a
/// hand-written test with an off-by-one target, 2026-06-09). javac
/// output is verified and cannot contain this; unverified/synthetic
/// bytecode can.
#[test]
pub(in crate::x64::tests) fn test_branch_target_mid_instruction_bails() {
    //  0: iconst_3            [3]
    //  1: iconst_0            [3, 0]
    //  2: iconst_0            [3, 0, 0]
    //  3: if_icmpeq +4 → 7    [3]   (7 = middle of the goto at 6..=8)
    //  6: goto +4 → 10        [3]
    //  9: iconst_0            (dead filler, never a target)
    // 10: ireturn
    let code: Vec<u8> = vec![
        0x06, 0x03, 0x03, 0x9f, 0x00, 0x04, 0xa7, 0x00, 0x04, 0x03, 0xac,
    ];
    let code_len = code.len();
    assert!(
        !try_compile_int_body(&code, code_len),
        "branch into the middle of an instruction must bail, not compile"
    );
}

// ---------------------------------------------------------------------------
// `dup2_x2` (0x5E) — all four JVMS forms, executed.
//
// This opcode was admitted by `jit_scan` and lowered by neither x64 backend
// from the first day both existed, so every method containing one reached the
// dispatch loop's `_ =>` catch-all and stayed interpreted for the life of the
// process — the refusal attributed to an arm that names nothing. See
// dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md.
//
// Each case below RUNS the compiled body and checks a value that a
// wrong-width shuffle cannot produce, because the failure mode this opcode
// invites is not a crash: it is duplicating an unrelated slot. `.expect(...)`
// alone would pass against a shuffle that compiles and computes nonsense.
//
// Local numbering follows this harness's convention (see
// `test_compile_math_min_max_long_intrinsic`): every parameter, `long`
// included, is ONE local slot.
// ---------------------------------------------------------------------------

/// FORM 4 — `[v2, v1] -> [v1, v2, v1]`, both operands category-2. Two entries
/// in this backend's one-entry-per-value model, structurally `dup_x1`.
///
/// `long f(long a, long b) { ... }` computing `a + 2b`: any shuffle that
/// duplicated two entries instead of one, or inserted the copy at the wrong
/// depth, gives a different sum.
#[test]
pub(in crate::x64::tests) fn dup2_x2_form4_two_category_2_operands() {
    // 0: lload_0     [a]
    // 1: lload_1     [a, b]
    // 2: dup2_x2     [b, a, b]
    // 3: ladd        [b, a+b]
    // 4: ladd        [a+2b]
    // 5: lreturn
    let code = [0x1e, 0x1f, 0x5e, 0x61, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 2, 2)
        .expect("FORM-4 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[3, 5]).expect("test JIT call") };
    assert_eq!(r, 3 + 2 * 5, "a + 2b for a=3 b=5");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-7, 11]).expect("test JIT call") };
    assert_eq!(r, -7 + 2 * 11, "a + 2b for a=-7 b=11");
}

/// FORM 2 — `[v3, v2, v1] -> [v1, v3, v2, v1]`, a category-2 top over two
/// category-1 values. Three entries; structurally `dup_x2`.
///
/// This is the form javac actually emits: `longArr[i] = otherArr[j] = v`
/// leaves `[arrayref, index, longvalue]` and has to slide the value under the
/// two category-1 slots.
#[test]
pub(in crate::x64::tests) fn dup2_x2_form2_category_2_over_two_category_1() {
    // 0: iload_1     [i]
    // 1: iload_2     [i, j]
    // 2: lload_0     [i, j, a]
    // 3: dup2_x2     [a, i, j, a]
    // 4: l2i         [a, i, j, (int)a]
    // 5: iadd        [a, i, j+(int)a]
    // 6: iadd        [a, i+j+(int)a]
    // 7: i2l         [a, (long)(i+j+(int)a)]
    // 8: ladd        [2a+i+j]
    // 9: lreturn
    let code = [0x1b, 0x1c, 0x1e, 0x5e, 0x88, 0x60, 0x60, 0x85, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 3, 3)
        .expect("FORM-2 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 2 * 10 + 3 + 4, "2a + i + j for a=10 i=3 j=4");
}

/// FORM 3 — `[v3, v2, v1] -> [v2, v1, v3, v2, v1]`, two category-1 values
/// duplicated over one category-2. Three entries; the copy goes three deep,
/// not four, because those four JVM slots are a single entry here.
#[test]
pub(in crate::x64::tests) fn dup2_x2_form3_two_category_1_over_a_category_2() {
    //  0: lload_0    [a]
    //  1: iload_1    [a, i]
    //  2: iload_2    [a, i, j]
    //  3: dup2_x2    [i, j, a, i, j]
    //  4: iadd       [i, j, a, i+j]
    //  5: i2l        [i, j, a, (long)(i+j)]
    //  6: ladd       [i, j, a+i+j]
    //  7: lstore_3   [i, j]
    //  8: iadd       [i+j]
    //  9: i2l        [(long)(i+j)]
    // 10: lload_3    [(long)(i+j), a+i+j]
    // 11: ladd       [a+2i+2j]
    // 12: lreturn
    let code = [
        0x1e, 0x1b, 0x1c, 0x5e, 0x60, 0x85, 0x61, 0x42, 0x60, 0x85, 0x21, 0x61, 0xad,
    ];
    let compiled = compile_probe_method(&code, 3, 4)
        .expect("FORM-3 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 10 + 2 * 3 + 2 * 4, "a + 2i + 2j for a=10 i=3 j=4");
}

/// FORM 1 — `[v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]`, all four
/// category-1. The only form aarch64's unconditional four-pop arm ever
/// handled correctly, and the deepest of the four.
#[test]
pub(in crate::x64::tests) fn dup2_x2_form1_four_category_1_operands() {
    // 0: iload_0   [a]
    // 1: iload_1   [a,b]
    // 2: iload_2   [a,b,c]
    // 3: iload_3   [a,b,c,d]
    // 4: dup2_x2   [c,d,a,b,c,d]
    // 5: iadd      [c,d,a,b,c+d]
    // 6: iadd      [c,d,a,b+c+d]
    // 7: iadd      [c,d,a+b+c+d]
    // 8: iadd      [c,a+b+c+2d]
    // 9: iadd      [a+b+2c+2d]
    // 10: ireturn
    let code = [
        0x1a, 0x1b, 0x1c, 0x1d, 0x5e, 0x60, 0x60, 0x60, 0x60, 0x60, 0xac,
    ];
    let compiled = compile_probe_method(&code, 4, 4)
        .expect("FORM-1 dup2_x2 must JIT-compile, not reach the catch-all");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3, 4]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 + 2 * 3 + 2 * 4, "a + b + 2c + 2d for 1,2,3,4");
}

/// The conservative half of the contract: when the width analysis cannot type
/// the entries under the dup, the method stays interpreted and says so under
/// its own name — not under whatever opcode the walk reached afterwards.
///
/// `dup2_x2` at pc 0 has no operands at all, so no oracle can answer.
#[test]
pub(in crate::x64::tests) fn dup2_x2_without_a_provable_form_bails_under_its_own_name() {
    let code = [0x5e, 0xac]; // dup2_x2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup2_x2-unprovable-form");
}

// ---------------------------------------------------------------------------
// dup_x2 (0x5B) and dup2_x1 (0x5D) — the forms the second-entry width oracle
// admitted on 2026-08-27.
//
// Before it, `dup_x2` was lowered ONLY when the next opcode was a category-1
// array store (the `++z[i]` peephole) and `dup2_x1` ONLY when its top was
// category-2. Everything else reached `self.fail(...)` and stayed interpreted
// for the life of the process — including javac's POST-increment
// `arr[n[0]++] = v`, which is what kept `HibfixComposeProbe2.chain`
// `ineligible-by-policy` with `dup_x2-unprovable-form(pc=15,op=0x5b)`.
//
// Each case RUNS the compiled body and checks a value a wrong-depth insert
// cannot produce: `.expect(...)` alone would pass against a shuffle that
// compiles and computes nonsense. Local numbering follows this harness's
// convention — every parameter, `long` included, is ONE local slot.
// ---------------------------------------------------------------------------

/// `dup_x2` FORM-1 — `[c, b, a] -> [a, c, b, a]`, all three category-1, and
/// NOT followed by an array store.
///
/// This is the case the old peephole could not prove. The body computes
/// `a + b + 2c`, so a copy inserted two deep instead of three (i.e. FORM-2's
/// depth) gives a different sum.
#[test]
pub(in crate::x64::tests) fn dup_x2_form1_admitted_without_the_array_store_peephole() {
    // 0: iload_0  [a]
    // 1: iload_1  [a, b]
    // 2: iload_2  [a, b, c]
    // 3: dup_x2   [c, a, b, c]
    // 4: iadd     [c, a, b+c]
    // 5: iadd     [c, a+b+c]
    // 6: iadd     [a+b+2c]
    // 7: ireturn
    let code = [0x1a, 0x1b, 0x1c, 0x5b, 0x60, 0x60, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 3, 3)
        .expect("FORM-1 dup_x2 must JIT-compile without the astore peephole");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 + 2 * 3, "a + b + 2c for a=1 b=2 c=3");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-5, 7, 11]).expect("test JIT call") };
    assert_eq!(r, -5 + 7 + 2 * 11, "a + b + 2c for a=-5 b=7 c=11");
}

/// `dup_x2` FORM-2 — `[w, a] -> [a, w, a]`, where `w` is a category-2
/// long/double and therefore ONE entry in this backend's model.
///
/// The whole point of the second-entry oracle: the top is category-1 in both
/// forms, so only the width of the entry BELOW it decides whether the copy
/// goes two entries deep or three. Inserting at FORM-1's depth here would
/// index past the bottom of a two-entry stack.
#[test]
pub(in crate::x64::tests) fn dup_x2_form2_category_2_below_the_top() {
    // 0: lload_0  [w]
    // 1: iload_1  [w, i]
    // 2: dup_x2   [i, w, i]
    // 3: pop      [i, w]
    // 4: l2i      [i, (int)w]
    // 5: iadd     [i + (int)w]
    // 6: ireturn
    let code = [0x1e, 0x1b, 0x5b, 0x57, 0x88, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 2, 2).expect("FORM-2 dup_x2 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") };
    assert_eq!(r, 13, "i + (int)w for w=10 i=3");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[-4, 9]).expect("test JIT call") };
    assert_eq!(r, 5, "i + (int)w for w=-4 i=9");
}

/// `dup2_x1` FORM-1 — `[c, b, a] -> [b, a, c, b, a]`, all three category-1.
///
/// TWO entries are duplicated here, not one, so this is the form the old
/// top-width-only peephole could never prove: `dup2_top_cat2` answering
/// `false` says the top is category-1 and says nothing about how deep the pair
/// must be inserted. The body computes `a + 2b + 2c`.
#[test]
pub(in crate::x64::tests) fn dup2_x1_form1_all_category_1() {
    // 0: iload_0  [a]
    // 1: iload_1  [a, b]
    // 2: iload_2  [a, b, c]
    // 3: dup2_x1  [b, c, a, b, c]
    // 4: iadd     [b, c, a, b+c]
    // 5: iadd     [b, c, a+b+c]
    // 6: iadd     [b, a+b+2c]
    // 7: iadd     [a+2b+2c]
    // 8: ireturn
    let code = [0x1a, 0x1b, 0x1c, 0x5d, 0x60, 0x60, 0x60, 0x60, 0xac];
    let compiled = compile_probe_method(&code, 3, 3).expect("FORM-1 dup2_x1 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[1, 2, 3]).expect("test JIT call") };
    assert_eq!(r, 1 + 2 * 2 + 2 * 3, "a + 2b + 2c for a=1 b=2 c=3");
}

/// `dup2_x1` FORM-2 — `[b, w] -> [w, b, w]`, a category-2 top over one
/// category-1. The shape the peephole alone already proved; kept so a future
/// change to the oracle cannot silently drop it. Computes `2w + (long)b`.
#[test]
pub(in crate::x64::tests) fn dup2_x1_form2_category_2_top() {
    // 0: iload_1  [b]
    // 1: lload_0  [b, w]
    // 2: dup2_x1  [w, b, w]
    // 3: lstore_2 [w, b]
    // 4: i2l      [w, (long)b]
    // 5: ladd     [w + (long)b]
    // 6: lload_2  [w + b, w]
    // 7: ladd     [2w + b]
    // 8: lreturn
    let code = [0x1b, 0x1e, 0x5d, 0x41, 0x85, 0x61, 0x20, 0x61, 0xad];
    let compiled = compile_probe_method(&code, 2, 3).expect("FORM-2 dup2_x1 must JIT-compile");
    // SAFETY: JIT-compiled machine code produced from valid bytecode in-test.
    let r = unsafe { compiled.try_call(&[10, 3]).expect("test JIT call") };
    assert_eq!(r, 2 * 10 + 3, "2w + b for w=10 b=3");
}

/// The conservative half of the contract, for `dup_x2`: with no operands at
/// all under the dup, neither the width analysis nor the peephole can answer,
/// and the method stays interpreted under its OWN name.
#[test]
pub(in crate::x64::tests) fn dup_x2_without_a_provable_form_bails_under_its_own_name() {
    let code = [0x5b, 0xac]; // dup_x2; ireturn
    let _ = crate::take_jit_bail_site(); // clear anything a prior test left
    assert!(compile_probe_method(&code, 0, 1).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(site, "singlepass-codegen/dup_x2-unprovable-form");
}

/// The catch-all is no longer anonymous. `frem` (0x72) has no arm in this
/// walk — `jit_scan` admits it on purpose so the optimizing IR backend can
/// see the method, which is why it is on
/// [`SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM`] — so driving the single-pass
/// walk onto it lands on the catch-all and pins the reason string it now
/// records. Before, every opcode that landed here reported the bare
/// `singlepass-codegen` site, naming nothing.
///
/// The exemplar used to be `wide` (0xC4), which stopped being unlowered on
/// 2026-08-28 and turned this test red for a reason that had nothing to do
/// with what it asserts. So the exemplar is now checked before it is used:
/// a hard-coded opcode in a test about UNLOWERED opcodes is a fact with a
/// shelf life, and the failure should say which fact expired.
#[test]
pub(in crate::x64::tests) fn the_unlowered_opcode_catch_all_names_itself() {
    // The reason string must survive even while no admitted opcode can reach
    // it: the guard below keeps the exemption list empty, and the catch-all is
    // what a future admission without an arm would land on.
    assert!(
        include_str!("../bytecode_walk.rs")
            .contains("self.fail(\"singlepass-codegen/opcode-scan-admitted-but-unlowered\")"),
        "the catch-all's named reason is gone"
    );
    let Some(&(exemplar, _)) = SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM.first() else {
        return;
    };
    #[allow(non_snake_case)]
    let EXEMPLAR = exemplar;
    assert!(
        !single_pass_dispatch_arms().contains(&EXEMPLAR),
        "0x{EXEMPLAR:02x} is lowered now, so it can no longer drive the walk \
         onto its catch-all. Pick another opcode with no dispatch arm — \
         `SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM` lists the deliberate ones."
    );
    // fconst_0; fconst_0; frem; freturn — the two operands keep the walk's
    // stack model honest right up to the opcode under test.
    let code = [0x0b, 0x0b, EXEMPLAR, 0xae];
    let _ = crate::take_jit_bail_site();
    assert!(compile_probe_method(&code, 0, 2).is_none());
    let (site, _, _) = crate::take_jit_bail_site().expect("a refusal records a site");
    assert_eq!(
        site, "singlepass-codegen/opcode-scan-admitted-but-unlowered",
        "the catch-all must name itself rather than claim it cannot happen"
    );
}

// ---------------------------------------------------------------------------
// The opcode-coverage guard.
//
// `jit_scan` (admission) and the single-pass dispatch loop (codegen) are two
// hand-maintained match statements over the same 202 opcode values, and until
// this test nothing forced them to agree. An opcode the scanner advances past
// but the walk has no arm for does not fail loudly: it falls into the walk's
// catch-all and the method is bail-listed for the life of the process, with
// the refusal attributed to an arm that names nothing.
//
// That gap has now cost three opcodes — `pop2` (0x58) and `dup2_x1` (0x5D),
// found by the commons-math throughput work, and `dup2_x2` (0x5E), found by
// hand-enumerating the arms of all four walkers. Each had been asserted away
// for years by the comment "should not happen — jit_scan should have caught
// this". A "should not happen" arm is a CLAIM, and claims about opcode
// coverage are checkable.
// ---------------------------------------------------------------------------

/// Opcodes `jit_scan` admits on purpose despite the single-pass walk having
/// no arm for them, each with the reason it is not the `dup2_x2` shape.
///
/// The bar for an entry here is a SECOND HOME: some other backend must lower
/// the opcode, so admitting it buys a compilation the scanner would otherwise
/// refuse. "Nobody lowers it anywhere" is the `dup2_x2` shape and belongs in
/// an arm, not on this list.
///
/// Empty since 2026-09-12, when `frem`/`drem` gained a single-pass arm.
pub(in crate::x64::tests) const SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM: &[(u8, &str)] = &[];

/// Every opcode value the top-level dispatch `match op` in `bytecode_walk.rs`
/// has an arm for.
///
/// Parsed from the source at COMPILE time (`include_str!`), because the set is
/// a property of that match statement and nothing else — there is no runtime
/// handle on it. Only arms at the match's own brace depth count, so the
/// nested `match op` statements inside the branch arms (which re-dispatch on
/// the same variable to pick a condition code) cannot forge coverage.
pub(in crate::x64::tests) fn single_pass_dispatch_arms() -> std::collections::BTreeSet<u8> {
    let src = include_str!("../bytecode_walk.rs");
    // The dispatch loop's own `match op {`. Anchored on the two lines that
    // immediately precede it so a nested `match op {` cannot be picked up.
    let anchor = "self.dbg_last_op = op;\n            let family = match op {\n";
    let start = src
        .find(anchor)
        .expect("the single-pass dispatch `match op` must be findable")
        + anchor.len();

    let mut arms = std::collections::BTreeSet::new();
    let mut depth = 0i32; // brace depth relative to the match body
    for line in src[start..].lines() {
        if depth == 0 {
            if let Some(set) = parse_opcode_arm(line) {
                arms.extend(set);
            }
            // The catch-all closes the enumeration.
            if line.trim_start().starts_with("_ => {") {
                break;
            }
        }
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        if depth < 0 {
            break; // the match's own closing brace
        }
    }
    arms
}

/// `0x5e => {`, `0xC2 | 0xC3 => {`, `0x1a..=0x1d => {` — and nothing else.
/// Returns `None` for any line that is not an opcode match arm, including
/// comments and emitted byte literals.
pub(in crate::x64::tests) fn parse_opcode_arm(line: &str) -> Option<Vec<u8>> {
    let t = line.trim();
    if t.starts_with("//") {
        return None;
    }
    let head = t.split("=>").next()?;
    if head == t {
        return None; // no `=>` on this line
    }
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for alt in head.split('|') {
        let alt = alt.trim();
        let bytes: Vec<u8> = if let Some((lo, hi)) = alt.split_once("..=") {
            let lo = parse_hex_byte(lo.trim())?;
            let hi = parse_hex_byte(hi.trim())?;
            (lo..=hi).collect()
        } else {
            vec![parse_hex_byte(alt)?]
        };
        out.extend(bytes);
    }
    Some(out)
}

pub(in crate::x64::tests) fn parse_hex_byte(tok: &str) -> Option<u8> {
    let hex = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X"))?;
    if hex.len() != 2 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    u8::from_str_radix(hex, 16).ok()
}

/// The parser must actually find the dispatch arms — a `single_pass_dispatch_arms`
/// that silently returned an empty set would make the guard below vacuously
/// green, which is precisely how the `dup2_x2` gap survived so long.
#[test]
pub(in crate::x64::tests) fn the_dispatch_arm_parser_reads_the_real_match() {
    let arms = single_pass_dispatch_arms();
    assert!(
        arms.len() > 150,
        "the single-pass walk lowers most of the opcode space; parsed only {}",
        arms.len()
    );
    // Spot checks across the shapes the parser has to handle.
    for (op, what) in [
        (0x00u8, "nop, a bare single arm"),
        (0x5eu8, "dup2_x2, the arm this guard was written for"),
        (0x1bu8, "iload_1, inside a `..=` range arm"),
        (0xc2u8, "monitorenter, inside an alternation arm"),
        (0xacu8, "ireturn"),
        // 2026-08-28: `wide` moved from the negative list below to this
        // one when the prefix was lowered (netty's `FastLz.compress` is
        // 1617 bytes carrying three `iinc_w`, and a scan refusal is
        // permanent for the whole method at every compile door). A spot
        // check either way is what makes the move visible.
        (0xc4u8, "wide, the prefix arm"),
    ] {
        assert!(arms.contains(&op), "dispatch arm for 0x{op:02x} ({what})");
    }
    // And it must not invent coverage for opcodes nobody lowers here.
    for (op, what) in [
        (0xa8u8, "jsr — unlowered in both walkers"),
        (0xa9u8, "ret — unlowered in both walkers"),
    ] {
        assert!(
            !arms.contains(&op),
            "0x{op:02x} ({what}) must not read as lowered"
        );
    }
}

/// The guard itself: no opcode may be admitted by `jit_scan` and lowered by
/// nothing.
///
/// Admission is probed BEHAVIOURALLY — a one-instruction body per opcode,
/// handed to the real `jit_scan` — so the scanner's own table is never
/// transcribed here and cannot drift from what it actually does.
#[test]
pub(in crate::x64::tests) fn scan_admitted_opcodes_are_lowered_or_declared() {
    let arms = single_pass_dispatch_arms();
    let declared: std::collections::BTreeMap<u8, &str> = SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM
        .iter()
        .copied()
        .collect();

    let mut offenders = Vec::new();
    for op in 0x00u8..=0xc9u8 {
        // A body of just this opcode plus operand padding and a `return`. The
        // scanner walks opcode widths and never simulates the stack, so this
        // is enough to ask it the only question that matters: does it advance
        // past this opcode, or refuse the method?
        let mut code = vec![op, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        code.push(0xb1); // return
        let admitted = super::bytecode_compat::jit_scan(&code, code.len(), "()V").is_some();
        if !admitted || arms.contains(&op) {
            continue;
        }
        if declared.contains_key(&op) {
            continue;
        }
        offenders.push(op);
    }

    assert!(
        offenders.is_empty(),
        "these opcodes are admitted by `jit_scan` and lowered by no single-pass \
         arm, so every method containing one silently never compiles: {}. \
         Either add an arm in `bytecode_walk.rs`, stop admitting them in \
         `jit_scan`, or — only if some OTHER backend lowers them — add them to \
         SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM with the reason.",
        offenders
            .iter()
            .map(|op| format!("0x{op:02x}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // The allowlist is a ratchet in both directions: an entry that has since
    // grown an arm must be removed, or it hides the next real gap behind a
    // stale exemption.
    for (op, reason) in SCAN_ADMITTED_WITHOUT_A_SINGLE_PASS_ARM {
        assert!(
            !arms.contains(op),
            "0x{op:02x} now HAS a single-pass arm; drop its exemption ({reason})"
        );
    }
}

/// The METHOD-ENTRY safepoint poll must not read as "the local-oop dataflow
/// never reached here".
///
/// `emit_safepoint_poll_prologue` deliberately stamps `cur_bc_pc` with
/// `ENTRY_POLL_BC_PC` so its map cannot collide with a genuine bci-0 safepoint
/// in the sp-id-keyed lookup. That is right, and it had a consequence nobody
/// costed: `local_oop_reached.get(ENTRY_POLL_BC_PC)` is `None`, so
/// `moving_young_safepoint_coverage_complete` reported the entry poll as
/// unprovable and its `OopMapEntry` was pushed with
/// `moving_young_coverage_complete: false`.
///
/// `CompiledMethod::fully_shadow_covered` ANDs that flag over every safepoint
/// of a method, so ONE such entry made it false for **every method the fast
/// tier ever compiled** — and through the OSR fallback that refused relocation
/// on 725 of 759 collections of `TestKillProcessWhileWriting`. Measured with
/// `CRATONVM_DBG_OOPCOV=1`, every uncovered method reported exactly
/// `shadow_missing_pcs=[4294967295]`.
///
/// Non-vacuous in three directions: the entry pc answers with the SEEDED mask
/// (not zero, not `local_oop_masks[0]`), a reached bci answers with its own
/// mask, and an unreached bci still answers `None` — so the fix cannot be
/// mistaken for "everything is covered now".
#[test]
pub(in crate::x64::tests) fn the_method_entry_poll_knows_its_own_live_oop_locals() {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "entry-poll-oop-mask-test".to_string(),
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
        test_helpers(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    );

    // Entry state: locals 0 and 2 are reference parameters. bci 0's own entry
    // is deliberately DIFFERENT (a back edge to bci 0 would have intersected
    // it), so reading the mask out of `local_oop_masks[0]` cannot pass this.
    compiler.param_oop_mask = 0b101;
    compiler.local_oop_masks = vec![0b001, 0b010, 0];
    compiler.local_oop_reached = vec![true, true, false];

    compiler.cur_bc_pc = crate::x64::safepoint::ENTRY_POLL_BC_PC;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        Some(0b101),
        "the entry poll must answer with the dataflow's SEEDED entry state — \
         `None` here is the defect, and `Some(0b001)` would mean it read \
         `local_oop_masks[0]`, which a back edge can have narrowed",
    );

    compiler.cur_bc_pc = 1;
    assert_eq!(compiler.local_oop_mask_at_current_pc(), Some(0b010));

    compiler.cur_bc_pc = 2;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        None,
        "a pc the forward dataflow never reached (an exception-handler-only \
         entry) must still refuse to make a precise claim",
    );

    compiler.cur_bc_pc = 99;
    assert_eq!(
        compiler.local_oop_mask_at_current_pc(),
        None,
        "and so must a pc past the end of the vectors",
    );
}

/// A site that inlines something ITSELF must be sized for the whole tree.
///
/// `InlineSite::nested_sites` holds full recursive `InlineSite`s and the
/// emitter splices those bodies into the SAME code buffer as the body that
/// contains them. Both sizing sites in `compile_with_param_slots` — the code
/// buffer estimate (`callee_code_len * 64`) and the spill reservation
/// (`max(callee_max_locals, param_span) + callee_code_len`) — used to read the
/// root site's `callee_code_len` and stop there, reserving the outer body's
/// bytes and none of the nested ones.
///
/// That is not a hypothetical shortfall. `VolumeOps.grad` (kfusion's raycast
/// normal, 1511 bytes / 273 invokes of 5-15 byte getters that each splice a
/// constructor and three field loads) overran its 433,312-byte estimate by
/// 8% — `wanted=468437` — and after `MAX_CODE_BUFFER_RETRIES` stopped being
/// compiled at all, leaving the app's hottest method interpreted.
///
/// TWO arms, because a test that only checked the nested case would pass just
/// as well against a helper that returned some large constant:
/// * a leaf site (no nested bodies) must be sized EXACTLY as the old per-site
///   formula did — this is the no-regression half;
/// * the same site carrying nested bodies must be sized strictly larger, by
///   exactly the nested bodies' own contribution, transitively through two
///   levels (so a one-level-deep walk fails it too).
#[test]
pub(in crate::x64::tests) fn s31_inline_reservations_count_nested_bodies() {
    use crate::x64::driver::{spliced_bytecode_len, spliced_stack_reserve};

    // Named, not spelled 4: if `MAX_INLINE_MERGE_DEPTH` moves this test must
    // move with it rather than becoming a silently disagreeing second copy.
    const MERGE: usize = crate::x64::MAX_INLINE_MERGE_DEPTH;

    // Leaf: 3 bytes of bytecode, 2 locals, static ()I -> param_span 0.
    let leaf = make_inline_site(&[0x12, 0x05, 0xac], 2, 0, true, b'I');
    assert_eq!(
        spliced_bytecode_len(&leaf),
        3,
        "a leaf site is its own bytecode and nothing else"
    );
    assert_eq!(
        spliced_stack_reserve(&leaf),
        2 + 3 + MERGE,
        "a leaf site is locals/params + code_len + the branch-merge area, added 2026-09-02: \
         max(callee_max_locals, param_span) + callee_code_len"
    );

    // Depth 2: leaf <- middle <- outer. Distinct sizes so a walk that stops
    // one level early produces a number no correct walk can also produce.
    let middle_body = [0xb8, 0x00, 0x01, 0xac, 0x00, 0x00, 0x00]; // 7 bytes
    let mut middle = make_inline_site(&middle_body, 4, 0, true, b'I');
    middle.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: leaf.clone(),
        bimorphic_second: None,
    }].into();

    let outer_body = [0xb8, 0x00, 0x02, 0xac, 0x00]; // 5 bytes
    let mut outer = make_inline_site(&outer_body, 6, 0, true, b'I');
    outer.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 0,
        guard_class_id: 0,
        site: middle.clone(),
        bimorphic_second: None,
    }].into();

    // 5 (outer) + 7 (middle) + 3 (leaf). A one-level walk would say 12.
    assert_eq!(
        spliced_bytecode_len(&outer),
        5 + 7 + 3,
        "the buffer estimate must count nested bodies transitively"
    );
    // Each level plus its own merge area: every level of a chain is live at
    // once and each reserves `MAX_INLINE_MERGE_DEPTH` words of its own.
    assert_eq!(
        spliced_stack_reserve(&outer),
        (6 + 5 + MERGE) + (4 + 7 + MERGE) + (2 + 3 + MERGE),
        "a nested body gets its own locals, operand stack AND merge area on top of the \
         body that splices it, so the reserves add transitively"
    );

    // And the fix is strictly a GROWTH: never size a nested tree below what
    // the old root-only formula would have reserved for its root.
    assert!(spliced_bytecode_len(&outer) > spliced_bytecode_len(&leaf));
    assert!(spliced_stack_reserve(&outer) > spliced_stack_reserve(&leaf));
}

/// `emit_cmp_r64_mem_disp` picks the narrowest legal encoding, and the widths
/// it declines to narrow are declined for a reason, not by omission.
///
/// The six containment compares in `emit_guarded_getfield_receiver_check` read
/// table words at displacements 0..40 through RDX. Every one fits a `disp8`;
/// before this encoder existed each paid the disp32 form, three wasted bytes
/// apiece on a guard that runs before every unproven-receiver field access.
///
/// The two x86 base-register special cases are pinned here as well, because
/// both are silent mis-encodings rather than assembler errors: RBP/R13 have no
/// `mod=00` form (that bit pattern is RIP-relative), and RSP/R12 need a SIB
/// byte this emitter does not produce.
#[test]
pub(in crate::x64::tests) fn the_containment_compare_narrows_its_displacement_and_knows_the_two_base_cases() {
    let mk = || {
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        Compiler::new(
            "cmp-disp-width-test".to_string(),
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
            test_helpers(),
            0,
            false,
            false,
            false,
            false,
            false,
            Vec::new(),
        )
    };
    let bytes = |f: &dyn Fn(&mut Compiler)| -> Vec<u8> {
        let mut c = mk();
        let at = c.buf.pos();
        f(&mut c);
        c.buf.as_slice()[at..].to_vec()
    };

    // RDX base, displacement 0: mod=00, no displacement byte at all.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, 0)),
        vec![0x48, 0x3B, 0x02],
    );
    // RDX base, the five remaining table words: mod=01 + one byte.
    for disp in [8i32, 16, 24, 32, 40] {
        assert_eq!(
            bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, disp)),
            vec![0x48, 0x3B, 0x42, disp as u8],
            "displacement {disp} did not take the disp8 form",
        );
    }
    // Past a signed byte: back to mod=10 + disp32, byte-identical to the
    // encoder this one delegates to.
    let wide = bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RDX, 4096));
    assert_eq!(wide, bytes(&|c| c.emit_cmp_r64_mem_disp32(RAX, RDX, 4096)));

    // RBP has no mod=00 form: a zero displacement still emits an explicit
    // disp8 of 0, never the three-byte shape RDX gets.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RBP, 0)),
        vec![0x48, 0x3B, 0x45, 0x00],
    );
    // RSP needs a SIB byte neither form emits, so it is handed to the disp32
    // encoder unchanged rather than narrowed into a wrong operand here.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, RSP, 8)),
        bytes(&|c| c.emit_cmp_r64_mem_disp32(RAX, RSP, 8)),
    );
    // An extended base keeps its REX.B, and R13 inherits RBP's rule.
    assert_eq!(
        bytes(&|c| c.emit_cmp_r64_mem_disp(RAX, R13, 0)),
        vec![0x49, 0x3B, 0x45, 0x00],
    );
}

/// The gated reference store writes the RIGHT CELL for a legacy receiver, and
/// takes no helper call to do it.
///
/// Executed, not inspected: the compiled body runs against a fake object and
/// the assertions read the bytes it wrote.
///
/// This arm used to send every non-compact receiver to `jit_putfield_object`,
/// which made it an arm that essentially never fired — `init_object_header`,
/// the TLAB fast path serving nearly every allocation, writes a LEGACY header
/// unconditionally whatever layout the class has registered. A run-time path
/// census on `RefStoreLoopProbe` measured `inline=0` out of 16,380,000, all of
/// them bailing at the compactness test, while the compile-time census said
/// `gated=2 declined=0` and looked healthy.
///
/// Both halves are asserted. A test that only checked the legacy receiver
/// would pass on an arm that had simply swapped one dead shape for another.
#[test]
pub(in crate::x64::tests) fn the_gated_ref_store_writes_both_cell_shapes_without_a_helper_call() {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const HELPER_CALLS: MarkerWord = MarkerWord(0);
    const BARRIER_CALLS: MarkerWord = MarkerWord(1);
    /// Records the full-barrier path and deliberately does NOT store, so an
    /// inline store and a helper store are trivially distinguishable.
    unsafe extern "C" fn marker_putfield_object(_vm: i64, _obj: i64, _idx: i64, _val: i64) {
        HELPER_CALLS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn marker_write_barrier(_heap: i64, _obj: i64, _val: i64) {
        BARRIER_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    // The published plan: SATB disarmed, old objects present, and the
    // generational MASK shape. `post_active` is deliberately 1 so the mask is
    // what rules the barrier out and not a global "nothing is old" shortcut.
    static PRE_GATE: AtomicU64 = AtomicU64::new(0);
    static POST_GATE: AtomicU64 = AtomicU64::new(1);
    static READ_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];

    // void setRef(Object this, Object v) { this.f = v; }
    let code: Vec<u8> = vec![0x2a, 0x2b, 0xb5, 0x00, 0x01, 0xb1, 0, 0];
    let field_info = vec![(2usize, 0usize, b'L')];
    let mut helpers = test_helpers();
    helpers.putfield_object = marker_putfield_object as *const () as usize; // Cast: fn → slot
    helpers.write_barrier = marker_write_barrier as *const () as usize; // Cast: fn → slot
    helpers.read_bounds_addr = READ_BOUNDS.as_ptr() as usize; // Cast: static address
    helpers.ref_store_pre_gate = std::ptr::addr_of!(PRE_GATE) as usize; // Cast: static address
    helpers.ref_store_post_gate = std::ptr::addr_of!(POST_GATE) as usize; // Cast: static address
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = cratonvm_types::GC_FLAG_OLD_GEN as usize;

    let compiled = compile_with_request(
        BackendRequest::default(),
        vec![(2, 8, true)],
        &code,
        6,
        2,
        2,
        true, // needs_heap — the barrier helper takes it
        Vec::new(),
        field_info,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("reference putfield must compile");

    let val = Box::new([0u64; 8]);
    let val_addr = val.as_ptr() as usize; // Cast: stored reference

    // ---- LEGACY receiver: no GC_FLAG_COMPACT, the 16-byte `Value` cell ----
    let mut obj = Box::new([0u64; 8]);
    // SAFETY: `obj` is 64 bytes and 8-byte aligned; both writes land inside it.
    unsafe {
        let p = obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = 0; // young, LEGACY
        std::ptr::write_unaligned(
            p.add(cratonvm_types::NUM_SLOTS_OFFSET) as *mut u32, // Cast: header field
            4u32,
        );
    }
    let obj_addr = obj.as_mut_ptr() as usize; // Cast: receiver address
    let page = obj_addr & !0xFFF;
    READ_BOUNDS[0].store(page, Ordering::Release);
    READ_BOUNDS[1].store(page + 0x10000, Ordering::Release);

    HELPER_CALLS.store(0, Ordering::SeqCst);
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled code from valid bytecode in an executable mmap; the
    // receiver is a live 64-byte buffer shaped like an object header and
    // neither marker helper stores anything.
    unsafe {
        compiled.call_with_heap(0, &[obj_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        (
            HELPER_CALLS.load(Ordering::SeqCst),
            BARRIER_CALLS.load(Ordering::SeqCst)
        ),
        (0, 0),
        "a young LEGACY receiver must store inline and call NOTHING — before \
         the two-shape store it took the helper on every single execution"
    );
    let word = cratonvm_types::HEADER_SIZE / 8;
    assert_eq!(
        obj[word] as u32,
        cratonvm_types::FIELD_CELL_TAG_OBJECT,
        "the legacy shape must write the cell's TAG; a payload written without \
         it leaves a discriminant saying whatever the field held before, and \
         the reader believes the discriminant"
    );
    assert_eq!(
        obj[word + 1] as usize, // Cast: raw stored pointer word
        val_addr,
        "the legacy shape must write the 64-bit pointer payload"
    );

    // ---- COMPACT receiver: the bare 8-byte pointer at the cell base ----
    let mut c_obj = fake_compact_young_object();
    let c_addr = c_obj.as_mut_ptr() as usize; // Cast: receiver address
    let c_page = c_addr & !0xFFF;
    READ_BOUNDS[0].store(c_page, Ordering::Release);
    READ_BOUNDS[1].store(c_page + 0x10000, Ordering::Release);
    HELPER_CALLS.store(0, Ordering::SeqCst);
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[c_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        (
            HELPER_CALLS.load(Ordering::SeqCst),
            BARRIER_CALLS.load(Ordering::SeqCst)
        ),
        (0, 0),
        "a young COMPACT receiver must still store inline and call nothing"
    );
    assert_eq!(
        fake_object_ref_cell(&c_obj),
        val_addr,
        "the compact shape must survive the addition of the legacy one — the \
         receiver's own header decides, and a genuinely compact object stores \
         the bare pointer"
    );

    // ---- an OLD receiver still reaches the collector's write barrier ----
    // SAFETY: `c_obj` is the buffer built above; this sets its flags byte.
    unsafe {
        let p = c_obj.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
        *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) =
            cratonvm_types::GC_FLAG_COMPACT | cratonvm_types::GC_FLAG_OLD_GEN;
    }
    BARRIER_CALLS.store(0, Ordering::SeqCst);
    // SAFETY: as above.
    unsafe {
        compiled.call_with_heap(0, &[c_addr as i64, val_addr as i64]); // Cast: JIT ABI
    }
    assert_eq!(
        BARRIER_CALLS.load(Ordering::SeqCst),
        1,
        "an OLD receiver is the case the mask exists to catch: the store is \
         still inline, but the collector's own write barrier must run"
    );
}

/// The two post-barrier gate shapes are mutually exclusive, and a plan that
/// supplies neither (or both) is declined.
///
/// The mask shape exists because the age FLOOR cannot express the generational
/// collector's question. Its `write_barrier` cards a store only when the
/// receiver is in the old generation, which is `GC_FLAG_OLD_GEN` — a bit in the
/// flags nibble of the same byte the floor compares. The two do not order: an
/// object allocated straight into old gen has `gc_age == 0`, so its flags byte
/// is `0x01`, BELOW the age-zero floor `0x10`, while a young object that has
/// survived three collections is `0x30`, above it. A single unsigned threshold
/// would therefore have told compiled code to skip the card on exactly the
/// receivers that need one — so publishing both shapes at once is refused
/// rather than silently preferring one.
#[test]
pub(in crate::x64::tests) fn a_ref_store_plan_publishes_exactly_one_post_barrier_shape() {
    let mut helpers = test_helpers();
    helpers.ref_store_pre_gate = 0x1000;
    helpers.ref_store_post_gate = 0x1008;

    let gates = |h: &JitRuntimeHelpers| {
        (
            super::objects::ref_store_gates_of(h).is_some(),
            super::objects::ref_store_post_skip_mask_of(h),
        )
    };

    // Neither shape: nothing can rule the post barrier out.
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = 0;
    assert_eq!(gates(&helpers), (false, None), "neither shape");

    // Floor only — ZGC's shape.
    helpers.ref_store_post_young_floor = 0x1010;
    helpers.ref_store_post_skip_mask = 0;
    assert_eq!(gates(&helpers), (true, None), "floor only");

    // Mask only — the generational collector's shape.
    helpers.ref_store_post_young_floor = 0;
    helpers.ref_store_post_skip_mask = usize::from(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(
        gates(&helpers),
        (true, Some(cratonvm_types::GC_FLAG_OLD_GEN)),
        "mask only",
    );

    // Both: refused. Two independent skips for one question, and for a mask
    // publisher the floor is not merely redundant but wrong.
    helpers.ref_store_post_young_floor = 0x1010;
    helpers.ref_store_post_skip_mask = usize::from(cratonvm_types::GC_FLAG_OLD_GEN);
    assert_eq!(gates(&helpers).0, false, "both shapes");
}



/// The number of fused 16-bit gate compares in a compiled body (RIP or R11
/// form): the positive control of the zs s5/bt2 differential tests.
pub(in crate::x64::tests) fn zs5bt2_fused_compares(code: &[u8]) -> usize {
    let rip = code.windows(3).filter(|w| *w == [0x66, 0x83, 0x3D]).count();
    let r11 = code
        .windows(5)
        .filter(|w| *w == [0x66, 0x41, 0x83, 0x3B, 0x00])
        .count();
    rip + r11
}





/// The exact numbers behind that refusal, as a statement about the flags byte
/// rather than about the emitter: an old-gen receiver can sit BELOW the
/// age-zero floor, so no unsigned threshold separates old from young.
#[test]
pub(in crate::x64::tests) fn no_age_floor_can_separate_an_old_gen_receiver_from_a_young_one() {
    let flags_byte = |age: u8, flags: u8| (age << 4) | flags;
    // Allocated straight into old gen: age 0, and it NEEDS a card.
    let old_new = flags_byte(0, cratonvm_types::GC_FLAG_OLD_GEN);
    // Survived three collections, still young: it needs none.
    let young_old = flags_byte(3, 0);
    assert!(
        old_new < young_old,
        "the receiver that needs a card ({old_new:#04x}) sorts BELOW one that \
         does not ({young_old:#04x}) — which is why the floor shape cannot be \
         used here, and the mask can",
    );
    // The mask answers both correctly.
    assert_ne!(old_new & cratonvm_types::GC_FLAG_OLD_GEN, 0);
    assert_eq!(young_old & cratonvm_types::GC_FLAG_OLD_GEN, 0);
}

/// `(double) i + (T) x` — the `i2d` result is still pending in XMM0 when the
/// narrowing conversion loads its own operand. Each of f2i / f2l / d2i / d2l
/// used to write XMM0 without moving that pending value out, so the method
/// returned roughly `(T) x` instead of the sum.
#[test]
pub(in crate::x64::tests) fn review_fp_to_int_conversions_keep_a_pending_xmm0_operand() {
    // (load x, conversion, widen result back to double)
    let cases: [(u8, u8, u8, &str); 4] = [
        (0x23, 0x8b, 0x87, "f2i"), // fload_1 f2i i2d
        (0x23, 0x8c, 0x8a, "f2l"), // fload_1 f2l l2d
        (0x27, 0x8e, 0x87, "d2i"), // dload_1 d2i i2d
        (0x27, 0x8f, 0x8a, "d2l"), // dload_1 d2l l2d
    ];
    for (load, conv, widen, name) in cases {
        // iload_0 i2d <load> <conv> <widen> dadd dreturn
        let code: Vec<u8> = vec![0x1a, 0x87, load, conv, widen, 0x63, 0xaf, 0, 0];
        let compiled = review_compile!(code, 7, 2, 2, false)
            .unwrap_or_else(|| panic!("{name} fixture should compile"));
        let x_bits = if load == 0x23 {
            3.75f32.to_bits() as i64 // Cast: JIT ABI convention
        } else {
            3.75f64.to_bits() as i64 // Cast: JIT ABI convention
        };
        // SAFETY: calling machine code compiled from the verified-shape fixture above.
        let result = unsafe { compiled.try_call(&[5, x_bits]).expect("test JIT call") };
        assert_eq!(
            f64::from_bits(result as u64), // Cast: JIT ABI convention
            8.0,
            "{name}: (double) 5 + ({name}) 3.75 must be 8.0"
        );
    }
}

/// `static int f(int ok, int a, int b) { return (ok != 0 && a < b) ? a : b; }`
/// javac makes the taken-side `iload_2` the target of the short-circuit
/// `ifeq` as well. The CMOV min/max fusion swallowed that PC, so `ok == 0`
/// jumped past the merged result and returned a stale slot.
#[test]
pub(in crate::x64::tests) fn review_cmov_minmax_declines_a_span_entered_by_another_branch() {
    let code: Vec<u8> = vec![
        0x1a, //              0: iload_0
        0x99, 0x00, 0x0c, //  1: ifeq -> 13
        0x1b, //              4: iload_1
        0x1c, //              5: iload_2
        0xa2, 0x00, 0x07, //  6: if_icmpge -> 13
        0x1b, //              9: iload_1
        0xa7, 0x00, 0x04, // 10: goto -> 14
        0x1c, //             13: iload_2
        0xac, //             14: ireturn
        0x00, 0x00,
    ];
    let compiled = review_compile!(code, 15, 3, 3, false).expect("short-circuit ternary compiles");
    for (ok, a, b) in [
        (0i64, 1i64, 5i64),
        (0, 9, 5),
        (1, 1, 5),
        (1, 9, 5),
        (0, -7, 3),
        (1, -7, 3),
    ] {
        let expected = if ok != 0 && a < b { a } else { b };
        // SAFETY: calling machine code compiled from the fixture above.
        let result = unsafe { compiled.try_call(&[ok, a, b]).expect("test JIT call") };
        assert_eq!(result as i32, expected as i32, "ok={ok} a={a} b={b}"); // Cast: int return
    }
}

/// `CMOVGE EAX, ECX ; MOVSXD RAX, EAX`: the min/max peephole's result select.
pub(in crate::x64::tests) const CMOVGE_EAX_ECX_SEXT: [u8; 6] = [0x0F, 0x4D, 0xC1, 0x48, 0x63, 0xC0];

/// `static int f(int k, int a, int b) { if (k == 0) return 7; return a < b ? a : b; }`
/// as a `tableswitch`. The peephole's foreign-edge scan (`bce::branch_edges`)
/// answered `None` for any method holding a switch, so such a method never got
/// the CMOV (x64loop proposal W9-3). The per-walk edge-count table counts the
/// switch's targets instead: none of them names the taken-side load here.
#[test]
pub(in crate::x64::tests) fn cmov_minmax_fires_in_a_method_that_holds_a_switch() {
    let canonical: Vec<u8> = vec![
        0x1a, 0x1b, 0xa2, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x1b, 0xac, 0x00, 0x00,
    ];
    let control = review_compile!(canonical, 11, 2, 2, false).expect("canonical min compiles");
    assert!(
        control
            .code_bytes()
            .windows(6)
            .any(|w| w == CMOVGE_EAX_ECX_SEXT),
        "control: the canonical `(a < b) ? a : b` is a CMOV"
    );

    let code: Vec<u8> = vec![
        0x1a, //                   0: iload_0
        0xaa, 0x00, 0x00, //       1: tableswitch (+2 pad)
        0x00, 0x00, 0x00, 0x16, // 4: default -> 23
        0x00, 0x00, 0x00, 0x00, // 8: low 0
        0x00, 0x00, 0x00, 0x00, // 12: high 0
        0x00, 0x00, 0x00, 0x13, // 16: case 0 -> 20
        0x10, 0x07, //            20: bipush 7
        0xac, //                  22: ireturn
        0x1b, //                  23: iload_1
        0x1c, //                  24: iload_2
        0xa2, 0x00, 0x07, //      25: if_icmpge -> 32
        0x1b, //                  28: iload_1
        0xa7, 0x00, 0x04, //      29: goto -> 33
        0x1c, //                  32: iload_2
        0xac, //                  33: ireturn
        0x00, 0x00,
    ];
    let compiled = review_compile!(code, 34, 3, 3, false).expect("switch + min compiles");
    assert!(
        compiled
            .code_bytes()
            .windows(6)
            .any(|w| w == CMOVGE_EAX_ECX_SEXT),
        "a switch elsewhere in the method must not cost the min idiom its CMOV"
    );
    for (k, a, b) in [
        (0i64, 1i64, 5i64),
        (1, 1, 5),
        (1, 9, 5),
        (2, -7, 3),
        (3, 4, 4),
    ] {
        let expected = if k == 0 { 7 } else { a.min(b) };
        // SAFETY: calling machine code compiled from the fixture above.
        let result = unsafe { compiled.try_call(&[k, a, b]).expect("test JIT call") };
        assert_eq!(result as i32, expected as i32, "k={k} a={a} b={b}"); // Cast: int return
    }
}

/// Every `if_icmp<cond>` feeding the ternary shape is one CMOV on the same
/// condition. javac writes a user-written `max`, `a > b ? a : b`, as
/// `if_icmple` (taken to the `b` load), which the peephole used to leave as
/// a branch: only `if_icmplt` / `if_icmpge` were fused.
#[test]
pub(in crate::x64::tests) fn cmov_minmax_fuses_every_int_compare_condition() {
    // (opcode, CMOVcc byte, the taken condition on (a, b))
    let cases: [(u8, u8, fn(i32, i32) -> bool); 6] = [
        (0x9f, 0x44, |a, b| a == b),
        (0xa0, 0x45, |a, b| a != b),
        (0xa1, 0x4C, |a, b| a < b),
        (0xa2, 0x4D, |a, b| a >= b),
        (0xa3, 0x4F, |a, b| a > b),
        (0xa4, 0x4E, |a, b| a <= b),
    ];
    for (op, cc, taken) in cases {
        // iload_0; iload_1; if_icmp<cond> -> 9; iload_0; goto -> 10; 9: iload_1; 10: ireturn
        let code: Vec<u8> = vec![
            0x1a, 0x1b, op, 0x00, 0x07, 0x1a, 0xa7, 0x00, 0x04, 0x1b, 0xac, 0x00, 0x00,
        ];
        let compiled = review_compile!(code, 11, 2, 2, false)
            .unwrap_or_else(|| panic!("if_icmp {op:#x} ternary compiles"));
        let cmov = [0x0F, cc, 0xC1, 0x48, 0x63, 0xC0];
        assert!(
            compiled.code_bytes().windows(6).any(|w| w == cmov),
            "if_icmp {op:#x} is fused to CMOVcc {cc:#x}"
        );
        for (a, b) in [
            (3i32, 5i32),
            (5, 3),
            (4, 4),
            (-7, 2),
            (i32::MIN, i32::MAX),
            (-1, -1),
        ] {
            let expected = if taken(a, b) { b } else { a };
            let args = [i64::from(a), i64::from(b)];
            // SAFETY: calling machine code compiled from the fixture above.
            let result = unsafe { compiled.try_call(&args).expect("test JIT call") };
            assert_eq!(result as i32, expected, "op={op:#x} a={a} b={b}"); // Cast: int return
            assert_eq!(
                result,
                i64::from(expected),
                "op={op:#x}: the int result is sign-extended"
            );
        }
    }
}

/// The same fusion over locals past 3, loaded by the two-byte `iload <n>`:
/// `static int f(int p, int q) { int a = p, b = q; return a > b ? a : b; }`
/// with `a`/`b` in slots 4 and 5. Only `iload_0..3` was matched before, so a
/// loop body's locals, which are rarely that low, never got the CMOV.
#[test]
pub(in crate::x64::tests) fn cmov_minmax_fuses_two_byte_iload_operands() {
    let code: Vec<u8> = vec![
        0x1a, 0x36, 0x04, //  0: iload_0; istore 4
        0x1b, 0x36, 0x05, //  3: iload_1; istore 5
        0x15, 0x04, //        6: iload 4
        0x15, 0x05, //        8: iload 5
        0xa4, 0x00, 0x08, // 10: if_icmple -> 18
        0x15, 0x04, //       13: iload 4
        0xa7, 0x00, 0x05, // 15: goto -> 20
        0x15, 0x05, //       18: iload 5
        0xac, //             20: ireturn
        0x00, 0x00,
    ];
    let compiled = review_compile!(code, 21, 2, 6, false).expect("iload <n> max compiles");
    assert!(
        compiled
            .code_bytes()
            .windows(6)
            .any(|w| w == [0x0F, 0x4E, 0xC1, 0x48, 0x63, 0xC0]),
        "`a > b ? a : b` over locals 4 and 5 is one CMOVLE"
    );
    for (a, b) in [(3i64, 5i64), (5, 3), (4, 4), (-7, 2), (-9, -8)] {
        // SAFETY: calling machine code compiled from the fixture above.
        let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
        assert_eq!(result, a.max(b), "a={a} b={b}");
    }
}


/// `long s = 0; for (int i = 3; i < n; i++) s = a[i] + s; return s;`
/// entered with `i > n`. The vectorised pre-header computed the chunk count
/// as an unsigned `(n - i) >> 3`, i.e. ~2^29 chunks past the array.
#[test]
pub(in crate::x64::tests) fn review_simd_int_sum_entered_past_its_bound_runs_zero_iterations() {
    let code: Vec<u8> = vec![
        0x09, //              0: lconst_0
        0x41, //              1: lstore_2
        0x06, //              2: iconst_3
        0x36, 0x04, //        3: istore 4
        0x15, 0x04, //        5: iload 4   (loop header)
        0x1b, //              7: iload_1
        0xa2, 0x00, 0x11, //  8: if_icmpge -> 25
        0x2a, //             11: aload_0
        0x15, 0x04, //       12: iload 4
        0x2e, //             14: iaload
        0x85, //             15: i2l
        0x20, //             16: lload_2
        0x61, //             17: ladd
        0x41, //             18: lstore_2
        0x84, 0x04, 0x01, // 19: iinc 4, 1
        0xa7, 0xff, 0xef, // 22: goto -> 5
        0x20, //             25: lload_2
        0xad, //             26: lreturn
        0x00, 0x00,
    ];
    let compiled = review_compile!(code, 27, 2, 5, false).expect("int-sum loop compiles");

    let run = |len: usize, n: i64| -> i64 {
        let mut words = vec![0u64; (HEADER_SIZE + 4 * len + 7) / 8 + 1];
        let array_ptr = words.as_mut_ptr() as *mut u8;
        // SAFETY: `words` holds the VM array header plus `len` ints.
        unsafe {
            (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(len as i32); // Cast: test length
            for i in 0..len {
                (array_ptr.add(ARRAY_DATA_OFFSET + 4 * i) as *mut i32)
                    .write_unaligned(i as i32 + 1); // Cast: test value
            }
        }
        // SAFETY: the compiled method receives a correctly laid out int[].
        unsafe {
            compiled
                .try_call(&[array_ptr as i64, n])
                .expect("test JIT call")
        } // Cast: pointer as JIT arg
    };
    assert_eq!(run(1, 1), 0, "i = 3 > n = 1 must run zero iterations");
    assert_eq!(run(2, 0), 0, "i = 3 > n = 0 must run zero iterations");
    // Control: a normal entry still sums a[3..16] = 4 + 5 + ... + 16.
    assert_eq!(run(16, 16), (4..=16).sum::<i64>());
}

/// Win64 makes XMM6-XMM15 non-volatile. No scratch XMM may be one of them:
/// neither the prologue nor any stub saves the scratch pool.
#[test]
pub(in crate::x64::tests) fn review_scratch_xmm_pool_is_volatile_under_the_host_abi() {
    let callee_saved: &[u8] = if cfg!(target_os = "windows") {
        &[6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
    } else {
        &[]
    };
    for xmm in SCRATCH_XMMS {
        assert!(
            !callee_saved.contains(&xmm),
            "scratch XMM{xmm} is callee-saved on this ABI"
        );
    }
}

/// `double s = 0; for (int i = 0; i < 3; i++) s += x * (c != 0 ? y : 2.0);`
/// javac lays the `ldc2_w 2.0` directly before the `dmul`, which is also the
/// merge point of the `goto` from the `y` arm. The in-loop strength reduction
/// replaced the dmul with `x + x` regardless of which arm reached it.
#[test]
pub(in crate::x64::tests) fn review_dmul_by_two_strength_reduction_skips_a_merge_point() {
    let code: Vec<u8> = vec![
        0x0e, //              0: dconst_0
        0x4a, //              1: dstore_3
        0x03, //              2: iconst_0
        0x36, 0x05, //        3: istore 5
        0x15, 0x05, //        5: iload 5   (loop header)
        0x06, //              7: iconst_3
        0xa2, 0x00, 0x1b, //  8: if_icmpge -> 35
        0x29, //             11: dload_3
        0x1a, //             12: iload_0
        0x87, //             13: i2d
        0x1c, //             14: iload_2
        0x99, 0x00, 0x08, // 15: ifeq -> 23
        0x1b, //             18: iload_1
        0x87, //             19: i2d
        0xa7, 0x00, 0x06, // 20: goto -> 26
        0x14, 0x00, 0x01, // 23: ldc2_w #1 (2.0)
        0x6b, //             26: dmul      (merge point)
        0x63, //             27: dadd
        0x4a, //             28: dstore_3
        0x84, 0x05, 0x01, // 29: iinc 5, 1
        0xa7, 0xff, 0xe5, // 32: goto -> 5
        0x29, //             35: dload_3
        0xaf, //             36: dreturn
        0x00, 0x00,
    ];
    let compiled = compile(
        &code,
        37,
        3,
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
        vec![(23, 2.0f64.to_bits() as i64)], // Cast: JIT ABI convention
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("ternary-multiply loop compiles");
    for (c, expected) in [(1i64, 105.0f64), (0, 30.0)] {
        // SAFETY: calling machine code compiled from the fixture above.
        let result = unsafe { compiled.try_call(&[5, 7, c]).expect("test JIT call") };
        assert_eq!(f64::from_bits(result as u64), expected, "c={c}"); // Cast: JIT ABI convention
    }
}

/// `long s = 0; for (int i = 0; i < n; i++) s = a[i] + s;` over values whose
/// sum passes 2^32. The vectorised pre-header summed 32-bit lanes and then
/// sign-extended the wrapped total into the long accumulator.
#[test]
pub(in crate::x64::tests) fn review_simd_int_sum_into_a_long_does_not_wrap_at_32_bits() {
    let code: Vec<u8> = vec![
        0x09, 0x41, 0x03, 0x36, 0x04, // lconst_0 lstore_2 iconst_0 istore 4
        0x15, 0x04, 0x1b, 0xa2, 0x00, 0x11, // iload 4; iload_1; if_icmpge -> 25
        0x2a, 0x15, 0x04, 0x2e, 0x85, 0x20, 0x61, 0x41, // a[i] i2l lload_2 ladd lstore_2
        0x84, 0x04, 0x01, 0xa7, 0xff, 0xef, // iinc 4,1; goto -> 5
        0x20, 0xad, 0x00, 0x00, // lload_2 lreturn
    ];
    let compiled = review_compile!(code, 27, 2, 5, false).expect("int-sum loop compiles");
    for (len, value) in [(8usize, i32::MAX), (19, i32::MAX), (16, i32::MIN), (24, -7)] {
        let mut words = vec![0u64; (HEADER_SIZE + 4 * len + 7) / 8 + 1];
        let array_ptr = words.as_mut_ptr() as *mut u8;
        // SAFETY: `words` holds the VM array header plus `len` ints.
        unsafe {
            (array_ptr.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(len as i32); // Cast: test length
            for i in 0..len {
                (array_ptr.add(ARRAY_DATA_OFFSET + 4 * i) as *mut i32).write_unaligned(value);
            }
        }
        // SAFETY: the compiled method receives a correctly laid out int[].
        let result = unsafe {
            compiled
                .try_call(&[array_ptr as i64, len as i64])
                .expect("test JIT call") // Cast: JIT args
        };
        assert_eq!(result, value as i64 * len as i64, "len={len} value={value}");
        // Cast: expected long sum
    }
}

/// Callee-saved XMM save slots are 16 bytes (the whole Win64 non-volatile
/// register) and tile the save area exactly: the first ends where the GPR
/// save area above begins, each follows the previous with no gap or overlap,
/// and the last ends where the region below (`xmm_saved_base + n * 16`) begins.
/// A frame depth `d` names a word spanning `[rbp - d, rbp - d + 8)`.
#[test]
pub(in crate::x64::tests) fn review_xmm_save_slots_tile_the_save_area() {
    assert_eq!(XMM_SAVE_SLOT_BYTES, 16);
    let span = |depth: i32, width: i32| (-depth, -depth + width);
    for base in [8, 40, 104] {
        for n in 1..=8usize {
            let slots: Vec<(i32, i32)> = (0..n)
                .map(|i| span(xmm_save_slot_offset(base, i), XMM_SAVE_SLOT_BYTES))
                .collect();
            // Top of the first slot == bottom of the 8-byte word at depth `base - 8`,
            // the lowest word the area above may own.
            assert_eq!(slots[0].1, span(base - 8, 8).0, "base={base}");
            for w in slots.windows(2) {
                assert_eq!(
                    w[1].1, w[0].0,
                    "slots must be contiguous, base={base} n={n}"
                );
            }
            // Bottom of the last slot == top of the first word of the region below.
            let below = base + n as i32 * XMM_SAVE_SLOT_BYTES; // Cast: n <= 8
            assert_eq!(slots[n - 1].0, span(below, 8).1, "base={base} n={n}");
        }
    }
}

/// Review #88: an OSR-exit snapshot of a slot reused as two kinds is described
/// by LIVENESS AT THE EXIT BCI, not by the whole-method classifier.
///
/// Slot 1 holds an `int`, then a reference, and the loop header at pc 8 reads
/// the reference. The slot is live there, so the published snapshot must name
/// its value: a reference home. `Unsupported` is acceptable only if the artifact
/// then refuses OSR entry at admission (`osr_exit_policy`), which is decided
/// before the body runs. What must never happen is the live slot being published
/// `Undefined`, or an `Unsupported` local in an artifact that still admits entry:
/// the exit-time transfer leaves an `Unsupported` local at the interpreter's
/// stale value.
#[test]
pub(in crate::x64::tests) fn osr_exit_snapshot_describes_a_reused_slot_that_is_live_at_the_exit() {
    use crate::deopt::{DeoptReason, FrameValue};
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x08,             // 0:  iconst_5
        0x3c,             // 1:  istore_1        slot 1 holds an int
        0x1b,             // 2:  iload_1
        0x57,             // 3:  pop
        0x01,             // 4:  aconst_null
        0x4c,             // 5:  astore_1        slot 1 now holds a reference
        0x03,             // 6:  iconst_0
        0x3b,             // 7:  istore_0
        0x2b,             // 8:  aload_1         <- loop header: reads slot 1
        0x57,             // 9:  pop
        0x84, 0x00, 0x01, // 10: iinc 0, 1
        0x1a,             // 13: iload_0
        0x10, 0x0a,       // 14: bipush 10
        0xa1, 0xff, 0xf8, // 16: if_icmplt -8 -> 8
        0xb1,             // 19: return
        0, 0,
    ];
    let code_len = 20;
    assert_eq!(
        super::classify_local_kinds(&code, code_len, 2)[1],
        super::LocalKind::Ambiguous,
        "precondition: the whole-method classifier cannot type slot 1"
    );
    let helpers = test_helpers();
    let compiled = compile(
        &code,
        code_len,
        0,
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("test JIT compile");

    if !crate::deopt_real_enabled() {
        // `CRATONVM_DEOPT_REAL=0` publishes no OSR-exit snapshot at all.
        assert!(compiled.osr_exit_points.is_empty());
        return;
    }
    let exit = compiled
        .deopt_points
        .iter()
        .find(|p| p.bci == 8 && p.reason == DeoptReason::OsrExit)
        .expect("the loop header at pc 8 publishes an OSR-exit snapshot");
    match &exit.frame_state.locals[1] {
        FrameValue::StackSlotRef(_) | FrameValue::RegisterRef(_) => {}
        FrameValue::Unsupported => assert!(
            compiled.osr_exit_policy().is_err(),
            "a live slot published Unsupported must make the exit non-resumable at compile time"
        ),
        other => panic!("slot 1 is a live reference at the exit bci, published as {other:?}"),
    }
    // The artifact-wide half of the contract: no admitted artifact carries an
    // `Unsupported` local in any snapshot.
    let any_unsupported_local = compiled.deopt_points.iter().any(|p| {
        p.frame_state
            .locals
            .iter()
            .any(|v| matches!(v, FrameValue::Unsupported))
    });
    if any_unsupported_local {
        assert!(compiled.osr_exit_policy().is_err());
    }
}

/// Review #88, the tolerated half: the same int/reference reuse, but at the loop
/// header (pc 6) slot 1 is DEAD. The loop never reads it and the code after the
/// loop stores an `int` before reading. The snapshot publishes `Undefined` for
/// it, which the exit-time transfer maps to an inert value, and the slot does not
/// make the exit unresumable.
#[test]
pub(in crate::x64::tests) fn osr_exit_snapshot_publishes_a_reused_slot_that_is_dead_at_the_exit_as_undefined() {
    use crate::deopt::{DeoptReason, FrameValue};
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x01,             // 0:  aconst_null
        0x4c,             // 1:  astore_1        slot 1 holds a reference
        0x2b,             // 2:  aload_1
        0x57,             // 3:  pop
        0x03,             // 4:  iconst_0
        0x3b,             // 5:  istore_0
        0x84, 0x00, 0x01, // 6:  iinc 0, 1       <- loop header: slot 1 not read
        0x1a,             // 9:  iload_0
        0x10, 0x0a,       // 10: bipush 10
        0xa1, 0xff, 0xfa, // 12: if_icmplt -6 -> 6
        0x06,             // 15: iconst_3
        0x3c,             // 16: istore_1        re-stored as an int before any read
        0x1b,             // 17: iload_1
        0x57,             // 18: pop
        0xb1,             // 19: return
        0, 0,
    ];
    let code_len = 20;
    assert_eq!(
        super::classify_local_kinds(&code, code_len, 2)[1],
        super::LocalKind::Ambiguous,
        "precondition: the whole-method classifier cannot type slot 1"
    );
    let helpers = test_helpers();
    let compiled = compile(
        &code,
        code_len,
        0,
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
        Vec::new(),
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("test JIT compile");

    if !crate::deopt_real_enabled() {
        assert!(compiled.osr_exit_points.is_empty());
        return;
    }
    let exit = compiled
        .deopt_points
        .iter()
        .find(|p| p.bci == 6 && p.reason == DeoptReason::OsrExit)
        .expect("the loop header at pc 6 publishes an OSR-exit snapshot");
    assert_eq!(
        exit.frame_state.locals[1],
        FrameValue::Undefined,
        "a slot dead at the exit bci is published Undefined, whatever its whole-method kind"
    );
    assert!(
        crate::deopt::first_unresumable_local(&exit.frame_state).is_none(),
        "the dead reused slot must not make the exit unresumable: {:?}",
        exit.frame_state.locals
    );
}

#[test]
pub(in crate::x64::tests) fn single_pass_baseline_mode_compiles_without_speculative_passes() {
    let helpers = test_helpers();
    // Counted loop over an Object[] with array loads:
    // Locals: 0=m (Object[]), 1=j, 2=n, 3=i.
    //   0: iconst_0 ; 1: istore_3                    (i = 0)
    //   2: iload_3 ; 3: iload_2 ; 4: if_icmpge -> 21  (header)
    //   7: aload_0 ; 8: ifnull -> 15                  (skip if m == null)
    //  11: aload_0 ; 12: iload_1 ; 13: aaload ; 14: pop
    //  15: iinc 3, 1 ; 18: goto -> 2 ; 21: return
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3e, // 1: istore_3
        0x1d, // 2: iload_3
        0x1c, // 3: iload_2
        0xa2, 0x00, 0x11, // 4: if_icmpge +17 -> 21
        0x2a, // 7: aload_0
        0xc6, 0x00, 0x07, // 8: ifnull +7 -> 15
        0x2a, // 11: aload_0
        0x1b, // 12: iload_1
        0x32, // 13: aaload
        0x57, // 14: pop
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf0, // 18: goto -16 -> 2
        0xb1, // 21: return
        0, 0,
    ];

    // 1. Normal (compatible) compilation succeeds
    let req_compat = BackendRequest::default();
    let compiled_compat = compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        22,
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
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(),
        None,
        &[0, 1, 2],
        3,
        0,
        Vec::new(),
        "Test.m:(Ljava/lang/Object;II)V",
        None,
        Vec::new(),
        None,
        req_compat,
    )
    .expect("normal compile must succeed");

    // 2. Baseline compilation succeeds
    let mut req_baseline = BackendRequest::default();
    req_baseline.baseline_mode = true;
    let compiled_baseline = compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        22,
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
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Default::default(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(),
        None,
        &[0, 1, 2],
        3,
        0,
        Vec::new(),
        "Test.m:(Ljava/lang/Object;II)V",
        None,
        Vec::new(),
        None,
        req_baseline,
    )
    .expect("baseline compile must succeed");

    assert!(compiled_compat.code_len() > 0);
    assert!(compiled_baseline.code_len() > 0);
}

// -----------------------------------------------------------------------
// Round 11 wave 13 (lane `spsync`): the single-pass caller-held synchronized
// direct CALL (`JIT_SYNC_DIRECT_GUARD`; page
// `r11w12-calls-single-pass-sync-direct-patch-FIXED-20260924`).
// -----------------------------------------------------------------------

/// Fake addresses, far from any code buffer; never executed, only counted.
pub(in crate::x64::tests) const SPSYNC_ENTRY: usize = 0x5EC0_0000_1000;
pub(in crate::x64::tests) const SPSYNC_MONITOR_ENTER: usize = 0x5EC0_0000_2000;
pub(in crate::x64::tests) const SPSYNC_MONITOR_EXIT: usize = 0x5EC0_0000_3000;
pub(in crate::x64::tests) const SPSYNC_LDC_CLASS_CP: usize = 0x5EC0_0000_4000;
pub(in crate::x64::tests) const SPSYNC_DISPATCH_THREW: usize = 0x5EC0_0000_5000;

/// Every reference to `target` in the code: an imm64 operand (the helper-call
/// form `emit_call_absolute` falls back to, and every runtime-lowering stub)
/// or an `E8 rel32` CALL that lands on it. Counts each occurrence, unlike
/// [`calls_to`].
pub(in crate::x64::tests) fn spsync_refs(compiled: &CompiledMethod, target: usize) -> usize {
    let code = compiled.code_bytes();
    // Cast: address → the exact 8-byte immediate the encoder emits
    let needle = (target as u64).to_le_bytes();
    let imm = code.windows(8).filter(|w| *w == needle).count();
    // Cast: buffer base address for resolving rel32 displacements
    let base = code.as_ptr() as usize;
    let rel = (0..code.len().saturating_sub(4))
        .filter(|&i| {
            code[i] == 0xE8 && {
                let disp = i32::from_le_bytes([code[i + 1], code[i + 2], code[i + 3], code[i + 4]]);
                // Cast: sign-extend the rel32 for wrapping address arithmetic
                base.wrapping_add(i)
                    .wrapping_add(5)
                    .wrapping_add(disp as isize as usize)
                    == target
            }
        })
        .count();
    imm + rel
}

#[test]
pub(in crate::x64::tests) fn single_pass_patchable_calls_emit_an_aligned_abs64_immediate() {
    const TARGET: usize = 0x7fff_0000_1234_5678;
    for prefix_len in 0..8 {
        let mut buf = crate::ExecutableBuffer::new(128).expect("allocate test code buffer");
        buf.emit(&[0x90; 7][..prefix_len]);
        super::emit::emit_patchable_abs64_call(&mut buf, TARGET);

        let bytes = buf.as_slice();
        let ret_off = buf.pos();
        let imm_off = crate::not_entrant::abs64_call_imm_offset(bytes, ret_off)
            .expect("patchable call uses MOV RAX, imm64 ; CALL RAX");
        assert_eq!((buf.as_ptr() as usize + imm_off) % 8, 0);
        let imm = u64::from_le_bytes(bytes[imm_off..imm_off + 8].try_into().unwrap());
        assert_eq!(imm, TARGET as u64);
    }
}

/// `static <ret'> m(int a) { ... sync(a) ... }` with a caller-held row for the
/// `invokestatic` at pc 1 whose callee returns `ret`, the site's
/// `JitInvokeInfo` and a class-`ldc` row at the same pc (as
/// `build_single_pass_tables` plans them), compiled with `protected` as the
/// method's only protected range.
pub(in crate::x64::tests) fn compile_spsync_site(
    code: &[u8],
    ret: u8,
    protected: Option<(u32, u32)>,
) -> Option<CompiledMethod> {
    let descriptor: &'static str = if ret == b'J' { "(I)J" } else { "(I)I" };
    // LEAK(intentional): the compiled code keeps raw pointers to the info.
    let info = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sync",
        descriptor,
        num_jit_args: 1,
        return_type: ret,
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    helpers.ldc_class_cp = SPSYNC_LDC_CLASS_CP;
    helpers.dispatch_threw = SPSYNC_DISPATCH_THREW;
    let mut backend = crate::x64::BackendRequest {
        direct_helpers: crate::DirectHelperTable {
            monitor_enter: SPSYNC_MONITOR_ENTER,
            monitor_exit: SPSYNC_MONITOR_EXIT,
            ..crate::DirectHelperTable::EMPTY
        },
        ..crate::x64::BackendRequest::default()
    };
    backend.protected_ranges = protected.into_iter().collect();
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        code,
        code.len(),
        1,
        1,
        true,
        Vec::new(),                                   // multianewarray_info
        Vec::new(),                                   // field_info
        Vec::new(),                                   // typecheck_info
        Vec::new(),                                   // static_field_info
        Vec::new(),                                   // new_info
        Vec::new(),                                   // new_deferred_info
        Vec::new(),                                   // anewarray_info
        Vec::new(),                                   // anewarray_deferred_info
        vec![(1usize, info as *const JitInvokeInfo)], // invoke_info
        vec![(
            1usize,
            crate::JitDirectCall {
                entry: SPSYNC_ENTRY,
                needs_context: false,
                num_params: 1,
                return_type: ret,
                guard_class_id: crate::JIT_SYNC_DIRECT_GUARD,
            },
        )], // direct_calls
        Vec::new(),                                   // mic_slots
        Vec::new(),                                   // pic_slots
        Vec::new(),                                   // ldc_info
        Vec::new(),                                   // ldc_string_info
        vec![(1usize, 7u32, 3u16)], // ldc_class_info: the mirror, at the invoke pc
        Vec::new(),                 // ldc2w_info
        Default::default(),         // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,
        &[0],
        1,
        0,
        Vec::new(),
        "T.m:(I)I",
        None, // despec: no VM
        Vec::new(),
        None, // elidable_init_pcs
        backend,
    )
}

//  0: iload_0 ; 1: invokestatic T.sync(I)I ; 4: iconst_1 ; 5: iadd ; 6: ireturn
pub(in crate::x64::tests) const SPSYNC_INT_SHAPE: [u8; 7] = [0x1a, 0xb8, 0x00, 0x01, 0x04, 0x60, 0xac];

/// Unprotected pc: one raw CALL to the monitor-free entry, the class monitor
/// taken once before it and released on both edges (normal and exceptional),
/// each monitor operation fetching the mirror through the class-`ldc` helper.
/// An `int` result needs no `dispatch_threw` peek.
#[test]
pub(in crate::x64::tests) fn a_sync_direct_row_calls_the_entry_with_the_class_monitor_held() {
    if !crate::x64::merged_call_sentinel_enabled() {
        // The route needs the merged sentinel; the planner plans no row then.
        return;
    }
    let compiled = compile_spsync_site(&SPSYNC_INT_SHAPE, b'I', None)
        .expect("a caller-held synchronized site compiles");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_ENTRY),
        1,
        "one raw CALL to the entry"
    );
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 1, "one enter");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_MONITOR_EXIT),
        2,
        "normal + exceptional exit"
    );
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_LDC_CLASS_CP),
        3,
        "the mirror per monitor op"
    );
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_DISPATCH_THREW),
        0,
        "no peek for an int"
    );
}

/// A `long` result can legitimately be `i64::MIN`: the exceptional edge peeks
/// `dispatch_threw` once before it releases anything -- and, under
/// `crate::caller_held_callee_resume_enabled`, once more on the callee-deopt
/// service's sentinel answer (a resumed callee's genuine `i64::MIN`).
#[test]
pub(in crate::x64::tests) fn a_wide_sync_direct_result_peeks_dispatch_threw_before_the_release() {
    if !crate::x64::merged_call_sentinel_enabled() {
        return;
    }
    //  0: iload_0 ; 1: invokestatic T.sync(I)J ; 4: l2i ; 5: ireturn
    let code = [0x1a, 0xb8, 0x00, 0x01, 0x88, 0xac];
    let compiled =
        compile_spsync_site(&code, b'J', None).expect("a wide caller-held site compiles");
    assert_eq!(spsync_refs(&compiled, SPSYNC_ENTRY), 1);
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 1);
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_EXIT), 2);
    let peeks = if crate::caller_held_callee_resume_enabled() {
        2
    } else {
        1
    };
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_DISPATCH_THREW),
        peeks,
        "the cold side's peeks only"
    );
}

/// A pc a handler protects takes the dispatch arm: no raw CALL to the
/// monitor-free body, no monitor code, and a `jit_invoke_dispatch` CALL that
/// locks by name.
#[test]
pub(in crate::x64::tests) fn a_protected_sync_direct_row_dispatches_by_name_instead() {
    let helpers = test_helpers();
    let compiled = compile_spsync_site(&SPSYNC_INT_SHAPE, b'I', Some((0, 6)))
        .expect("a protected caller-held row still compiles, as a dispatch");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_ENTRY),
        0,
        "never CALL the body raw"
    );
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 0);
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_EXIT), 0);
    assert!(
        calls_to(&compiled, helpers.invoke_dispatch) >= 1,
        "the dispatch helper"
    );
}

// -----------------------------------------------------------------------
// Round 13 wave 1 (lane `sync`): the INSTANCE twin of the caller-held
// synchronized direct CALL (`crate::sync_direct_instance_guard` rows).
// -----------------------------------------------------------------------

/// `static int m(T t) { return t.sync(); }` with an instance caller-held row
/// for the `invokevirtual` at pc 1 guarded on `exact` (`0`: statically bound),
/// the site's `JitInvokeInfo` (kind `kind`), and `protected` as the method's
/// only protected range. Local 0 is a reference parameter.
pub(in crate::x64::tests) fn compile_spsync_instance_site(
    exact: u32,
    kind: u8,
    protected: Option<(u32, u32)>,
) -> Option<CompiledMethod> {
    //  0: aload_0 ; 1: invokevirtual T.sync()I ; 4: ireturn
    let code = [0x2a, 0xb6, 0x00, 0x01, 0xac];
    // LEAK(intentional): the compiled code keeps raw pointers to the info.
    let info = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "sync",
        descriptor: "()I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: kind,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    helpers.ldc_class_cp = SPSYNC_LDC_CLASS_CP;
    helpers.dispatch_threw = SPSYNC_DISPATCH_THREW;
    let mut backend = crate::x64::BackendRequest {
        direct_helpers: crate::DirectHelperTable {
            monitor_enter: SPSYNC_MONITOR_ENTER,
            monitor_exit: SPSYNC_MONITOR_EXIT,
            ..crate::DirectHelperTable::EMPTY
        },
        ..crate::x64::BackendRequest::default()
    };
    backend.protected_ranges = protected.into_iter().collect();
    compile_with_param_slots(
        &crate::compile_gate::CompileAdmission::for_backend_test(),
        &code,
        code.len(),
        1,
        1,
        true,
        Vec::new(),                                   // multianewarray_info
        Vec::new(),                                   // field_info
        Vec::new(),                                   // typecheck_info
        Vec::new(),                                   // static_field_info
        Vec::new(),                                   // new_info
        Vec::new(),                                   // new_deferred_info
        Vec::new(),                                   // anewarray_info
        Vec::new(),                                   // anewarray_deferred_info
        vec![(1usize, info as *const JitInvokeInfo)], // invoke_info
        vec![(
            1usize,
            crate::JitDirectCall {
                entry: SPSYNC_ENTRY,
                needs_context: false,
                num_params: 0, // receiver excluded, as on every instance row
                return_type: b'I',
                guard_class_id: crate::sync_direct_instance_guard(exact)
                    .expect("an encodable class id"),
            },
        )], // direct_calls
        Vec::new(),                                   // mic_slots
        Vec::new(),                                   // pic_slots
        Vec::new(),                                   // ldc_info
        Vec::new(),                                   // ldc_string_info
        Vec::new(),         // ldc_class_info: an instance row locks no mirror
        Vec::new(),         // ldc2w_info
        Default::default(), // ldc_fp_pcs
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        HashMap::new(), // inline_guard_variants
        None,
        &[0],
        1,
        1, // param_oop_mask: local 0 (the receiver) is a reference
        Vec::new(),
        "T.m:(LT;)I",
        None, // despec: no VM
        Vec::new(),
        None, // elidable_init_pcs
        backend,
    )
}

/// Statically bound: one raw CALL to the monitor-free entry, the RECEIVER's
/// monitor entered once and released on both edges, and no mirror fetched.
#[test]
pub(in crate::x64::tests) fn an_instance_sync_direct_row_calls_the_entry_with_the_receiver_monitor_held() {
    if !crate::x64::merged_call_sentinel_enabled() || !direct_call_arg_maps_enabled() {
        return;
    }
    let compiled = compile_spsync_instance_site(0, 1, None)
        .expect("a statically bound instance caller-held site compiles");
    assert_eq!(spsync_refs(&compiled, SPSYNC_ENTRY), 1, "one raw CALL");
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 1, "one enter");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_MONITOR_EXIT),
        2,
        "both edges release"
    );
    assert_eq!(spsync_refs(&compiled, SPSYNC_LDC_CLASS_CP), 0, "no mirror");
}

/// Guarded on an exact class: the same CALL behind `CMP DWORD [recv], id`,
/// whose miss edge is the site's dispatch.
#[test]
pub(in crate::x64::tests) fn a_guarded_instance_sync_direct_row_compares_the_exact_class_first() {
    if !crate::x64::merged_call_sentinel_enabled() || !direct_call_arg_maps_enabled() {
        return;
    }
    const EXACT: u32 = 0x1234;
    let compiled = compile_spsync_instance_site(EXACT, 0, None)
        .expect("a guarded instance caller-held site compiles");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_ENTRY),
        1,
        "one raw CALL on the hit"
    );
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 1);
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_EXIT), 2);
    let mut guard = vec![0x81, 0x78, 0x00];
    guard.extend_from_slice(&EXACT.to_le_bytes());
    let code = compiled.code_bytes();
    assert_eq!(
        code.windows(guard.len())
            .filter(|w| *w == &guard[..])
            .count(),
        1,
        "one exact-class guard"
    );
}

/// A pc a handler protects keeps the dispatch: no raw CALL to the
/// monitor-free body and no monitor code at all.
#[test]
pub(in crate::x64::tests) fn a_protected_instance_sync_direct_row_dispatches_instead() {
    let compiled = compile_spsync_instance_site(0, 1, Some((0, 5)))
        .expect("a protected site still compiles, on dispatch");
    assert_eq!(
        spsync_refs(&compiled, SPSYNC_ENTRY),
        0,
        "never CALLed unlocked"
    );
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_ENTER), 0);
    assert_eq!(spsync_refs(&compiled, SPSYNC_MONITOR_EXIT), 0);
}

/// Interpreter round i1 wave 22, lane L6: every single-pass method-entry body
/// opens with the not-entrant entry pad, stamps its Java argument count, and
/// once patched sends a later call to its stub (`jit/src/not_entrant.rs`).
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::x64::tests) fn i22_l6_a_single_pass_body_opens_with_the_entry_pad_and_can_be_made_not_entrant() {
    // Stands in for the VM's `jit_not_entrant_entry`.
    extern "C" fn answer(record: i64, regs: i64, stack_args: i64) -> i64 {
        // SAFETY: the stub passes the live record it baked and the spill and
        // stack pointers of the call it intercepted.
        let rec = unsafe { &*(record as *const crate::NotEntrantRecord) };
        // SAFETY: as above.
        let args = unsafe { rec.java_args(regs as *const i64, stack_args as *const i64) };
        args[0] * 100 + args[1]
    }
    // iload_0; iload_1; iadd; ireturn
    let code = [0x1a, 0x1b, 0x60, 0xac];
    let compiled = compile_plain_r11(&code, code.len());
    assert_eq!(&compiled.code_bytes()[..5], &crate::ENTRY_PATCH_PAD);
    assert_eq!(compiled.entry_java_args, 2);
    // SAFETY: a static `(II)I` body with no context.
    assert_eq!(unsafe { compiled.try_call(&[2, 3]) }.expect("call"), 5);
    let record = Box::new(crate::NotEntrantRecord::new(
        &std::sync::Arc::from("i22l6/T"),
        &std::sync::Arc::from("add"),
        &std::sync::Arc::from("(II)I"),
        1,
        true,
        0,
        false,
        2,
    ));
    compiled
        .install_not_entrant_stub(record, answer as usize)
        .expect("a single-pass body is patchable");
    // SAFETY: the entry now jumps to the stub, which calls `answer`.
    assert_eq!(unsafe { compiled.try_call(&[2, 3]) }.expect("call"), 203);
}

// ── Round 12 wave 3, lane vec: X1 element-wise forms, executed ─────────────
//
// `detect_int_array_element_wise_x1` (`x64/simd_analysis.rs`) and the
// invariant-operand arm of `emit_simd_int_array_element_wise` (`x64/simd.rs`).
// Each fixture bounds its loop by `a.length`, so the pre-header's own
// self-guards are its whole coverage proof and it is emitted whenever the host
// has AVX2. Every result is compared with the scalar Java semantics, including
// the exact partial writes before an AIOOBE and the untouched output on an NPE.

/// A fake `int[]` in host memory: header words, then the elements.
pub(in crate::x64::tests) fn r12w3_vec_int_array(values: &[i32]) -> Vec<u64> {
    let mut words = vec![0u64; (ARRAY_DATA_OFFSET + 4 * values.len() + 7) / 8 + 1];
    let p = words.as_mut_ptr() as *mut u8;
    // SAFETY: `words` holds the array header plus `values.len()` ints.
    unsafe {
        // Cast: test array length
        (p.add(ARRAY_LENGTH_OFFSET) as *mut i32).write_unaligned(values.len() as i32);
        for (i, v) in values.iter().enumerate() {
            (p.add(ARRAY_DATA_OFFSET + 4 * i) as *mut i32).write_unaligned(*v);
        }
    }
    words
}

pub(in crate::x64::tests) fn r12w3_vec_read(words: &[u64], len: usize) -> Vec<i32> {
    let p = words.as_ptr() as *const u8;
    // SAFETY: `words` was built by `r12w3_vec_int_array` with `len` elements.
    (0..len)
        .map(|i| unsafe { (p.add(ARRAY_DATA_OFFSET + 4 * i) as *const i32).read_unaligned() })
        .collect()
}

/// Deterministic values that reach both ends of the int range.
pub(in crate::x64::tests) fn r12w3_vec_values(len: usize, seed: i32) -> Vec<i32> {
    (0..len)
        .map(|i| {
            // Cast: test index, and the golden-ratio constant's bits
            let x = (i as i32)
                .wrapping_mul(0x9E37_79B9_u32 as i32)
                .wrapping_add(seed);
            match i % 5 {
                0 => i32::MAX - x.rem_euclid(7),
                1 => i32::MIN + x.rem_euclid(5),
                _ => x,
            }
        })
        .collect()
}

pub(in crate::x64::tests) const R12W3_VEC_LENS: [usize; 11] = [0, 1, 2, 7, 8, 9, 15, 16, 17, 33, 100];

/// `static void f(int[] a, int[] b) { for (int i = 0; i < a.length; i++) a[i] += b[i]; }`
/// javac's compound form: `aload_0; iload_2; dup2; iaload; aload_1; iload_2; iaload; iadd; iastore`.
pub(in crate::x64::tests) fn r12w3_vec_compound_add_code() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3d, //              1: istore_2
        0x1c, //              2: iload_2        <- header
        0x2a, //              3: aload_0
        0xbe, //              4: arraylength
        0xa2, 0x00, 0x12, //  5: if_icmpge -> 23
        0x2a, //              8: aload_0
        0x1c, //              9: iload_2
        0x5c, //             10: dup2
        0x2e, //             11: iaload
        0x2b, //             12: aload_1
        0x1c, //             13: iload_2
        0x2e, //             14: iaload
        0x60, //             15: iadd
        0x4f, //             16: iastore
        0x84, 0x02, 0x01, // 17: iinc 2, 1
        0xa7, 0xff, 0xee, // 20: goto -> 2
        0xb1, //             23: return
        0x00, 0x00,
    ]
}

/// `static void f(int[] c, int[] a, int k) { for (int i = 0; i < a.length; i++) c[i] = a[i] - k; }`
pub(in crate::x64::tests) fn r12w3_vec_sub_local_code() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3e, //              1: istore_3
        0x1d, //              2: iload_3        <- header
        0x2b, //              3: aload_1
        0xbe, //              4: arraylength
        0xa2, 0x00, 0x11, //  5: if_icmpge -> 22
        0x2a, //              8: aload_0
        0x1d, //              9: iload_3
        0x2b, //             10: aload_1
        0x1d, //             11: iload_3
        0x2e, //             12: iaload
        0x1c, //             13: iload_2
        0x64, //             14: isub
        0x4f, //             15: iastore
        0x84, 0x03, 0x01, // 16: iinc 3, 1
        0xa7, 0xff, 0xef, // 19: goto -> 2
        0xb1, //             22: return
        0x00, 0x00,
    ]
}

/// `static void f(int[] c, int[] a, int k) { for (int i = 0; i < a.length; i++) c[i] = 7 - a[i]; }`
pub(in crate::x64::tests) fn r12w3_vec_const_first_sub_code() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3e, //              1: istore_3
        0x1d, //              2: iload_3        <- header
        0x2b, //              3: aload_1
        0xbe, //              4: arraylength
        0xa2, 0x00, 0x12, //  5: if_icmpge -> 23
        0x2a, //              8: aload_0
        0x1d, //              9: iload_3
        0x10, 0x07, //       10: bipush 7
        0x2b, //             12: aload_1
        0x1d, //             13: iload_3
        0x2e, //             14: iaload
        0x64, //             15: isub
        0x4f, //             16: iastore
        0x84, 0x03, 0x01, // 17: iinc 3, 1
        0xa7, 0xff, 0xee, // 20: goto -> 2
        0xb1, //             23: return
        0x00, 0x00,
    ]
}

/// `static void f(int[] a, int[] unused, int k) { for (int i = 0; i < a.length; i++) a[i] *= k; }`
pub(in crate::x64::tests) fn r12w3_vec_compound_mul_local_code() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3e, //              1: istore_3
        0x1d, //              2: iload_3        <- header
        0x2a, //              3: aload_0
        0xbe, //              4: arraylength
        0xa2, 0x00, 0x10, //  5: if_icmpge -> 21
        0x2a, //              8: aload_0
        0x1d, //              9: iload_3
        0x5c, //             10: dup2
        0x2e, //             11: iaload
        0x1c, //             12: iload_2
        0x68, //             13: imul
        0x4f, //             14: iastore
        0x84, 0x03, 0x01, // 15: iinc 3, 1
        0xa7, 0xff, 0xf0, // 18: goto -> 2
        0xb1, //             21: return
        0x00, 0x00,
    ]
}

/// `VMOVDQU [R13], YMM0`, the element-wise batch store: present only when a
/// pre-header was emitted.
pub(in crate::x64::tests) const R12W3_VEC_BATCH_STORE: [u8; 5] = [0xC4, 0xC1, 0x7E, 0x7F, 0x85];
/// `VPBROADCASTD YMM1, XMM1`: present only for the invariant-operand forms.
pub(in crate::x64::tests) const R12W3_VEC_BROADCAST: [u8; 5] = [0xC4, 0xE2, 0x7D, 0x58, 0xC9];

pub(in crate::x64::tests) fn r12w3_vec_has(code: &[u8], needle: &[u8]) -> bool {
    code.windows(needle.len()).any(|w| w == needle)
}

#[test]
pub(in crate::x64::tests) fn r12w3_vec_detector_admits_the_compound_and_invariant_forms() {
    let code = r12w3_vec_compound_add_code();
    let e = detect_int_array_element_wise_x1(&code, 2, 20, 2, true).expect("a[i] += b[i]");
    assert_eq!((e.out_local, e.a_local, e.b_local), (0, 0, 1));
    assert_eq!(
        (e.op, e.rhs, e.invariant_first),
        (ElementWiseOp::Add, ElementWiseRhs::Array, false)
    );
    assert_eq!(e.bound, SimdLoopBound::ArrayLength(0));
    // The `a.length` header needs the vectorization switch.
    assert!(detect_int_array_element_wise_x1(&code, 2, 20, 2, false).is_none());
    // The historical detector does not know `dup2`.
    assert!(detect_int_array_element_wise_forms(&code, 2, 20, 2, true).is_none());

    let code = r12w3_vec_sub_local_code();
    let e = detect_int_array_element_wise_x1(&code, 2, 19, 3, true).expect("c[i] = a[i] - k");
    assert_eq!((e.out_local, e.a_local, e.b_local), (0, 1, 1));
    assert_eq!(
        (e.op, e.rhs, e.invariant_first),
        (ElementWiseOp::Sub, ElementWiseRhs::Local(2), false)
    );

    let code = r12w3_vec_const_first_sub_code();
    let e = detect_int_array_element_wise_x1(&code, 2, 20, 3, true).expect("c[i] = 7 - a[i]");
    assert_eq!(
        (e.op, e.rhs, e.invariant_first),
        (ElementWiseOp::Sub, ElementWiseRhs::Const(7), true)
    );

    let code = r12w3_vec_compound_mul_local_code();
    let e = detect_int_array_element_wise_x1(&code, 2, 18, 3, true).expect("a[i] *= k");
    assert_eq!((e.out_local, e.a_local), (0, 0));
    assert_eq!(
        (e.op, e.rhs, e.invariant_first),
        (ElementWiseOp::Mul, ElementWiseRhs::Local(2), false)
    );
}

#[test]
pub(in crate::x64::tests) fn r12w3_vec_detector_refuses_what_is_not_invariant_or_not_its_shape() {
    // `c[i] = a[i] - i`: the "invariant" is the induction variable.
    let mut code = r12w3_vec_sub_local_code();
    code[13] = 0x1d; // iload_3 (i) instead of iload_2 (k)
    assert!(detect_int_array_element_wise_x1(&code, 2, 19, 3, true).is_none());

    // The back edge must return to the header it was given.
    let code = r12w3_vec_sub_local_code();
    assert!(detect_int_array_element_wise_x1(&code, 3, 19, 3, true).is_none());

    // A negative stride is not the shape.
    let mut code = r12w3_vec_sub_local_code();
    code[18] = 0xff; // iinc 3, -1
    assert!(detect_int_array_element_wise_x1(&code, 2, 19, 3, true).is_none());

    // `c[i] = a[i] + b[i]` belongs to the historical detector alone.
    let (code, _) = ewise_bytecode(0x60);
    assert!(detect_int_array_element_wise_x1(&code, 3, 25, 4, true).is_none());
    assert!(detect_int_array_element_wise_forms(&code, 3, 25, 4, true).is_some());

    // An `idiv` is not an element-wise op (it can throw).
    let mut code = r12w3_vec_sub_local_code();
    code[14] = 0x6c;
    assert!(detect_int_array_element_wise_x1(&code, 2, 19, 3, true).is_none());
}

#[test]
pub(in crate::x64::tests) fn r12w3_vec_compound_add_executes_like_the_scalar_loop() {
    let code = r12w3_vec_compound_add_code();
    let compiled = compile_array_test(&code, 24, 2, 3);
    if has_avx2() {
        assert!(
            r12w3_vec_has(compiled.code_bytes(), &R12W3_VEC_BATCH_STORE),
            "`a[i] += b[i]` must get the AVX2 pre-header on an AVX2 host"
        );
    }
    let call = |a: &mut Vec<u64>, b: i64| -> i64 {
        // SAFETY: `a` is a fake int[] (see `r12w3_vec_int_array`); `b` is one
        // or 0 (null).
        unsafe {
            compiled
                .try_call(&[a.as_mut_ptr() as i64, b])
                .expect("test JIT call")
        }
    };
    for len in R12W3_VEC_LENS {
        let av = r12w3_vec_values(len, 11);
        let bv = r12w3_vec_values(len, -3);
        let mut a = r12w3_vec_int_array(&av);
        let mut b = r12w3_vec_int_array(&bv);
        call(&mut a, b.as_mut_ptr() as i64);
        let want: Vec<i32> = av
            .iter()
            .zip(&bv)
            .map(|(x, y)| x.wrapping_add(*y))
            .collect();
        assert_eq!(r12w3_vec_read(&a, len), want, "len={len}");

        // Aliasing: `a[i] += a[i]`.
        let mut a = r12w3_vec_int_array(&av);
        let alias = a.as_mut_ptr() as i64;
        call(&mut a, alias);
        let want: Vec<i32> = av.iter().map(|x| x.wrapping_add(*x)).collect();
        assert_eq!(r12w3_vec_read(&a, len), want, "aliased len={len}");
    }

    // `b` three shorter than `a`: AIOOBE at b.length, after exactly the scalar
    // loop's writes; the rest of `a` untouched.
    for len in [11usize, 12, 19, 40] {
        let av = r12w3_vec_values(len, 5);
        let bv = r12w3_vec_values(len - 3, 9);
        let mut a = r12w3_vec_int_array(&av);
        let mut b = r12w3_vec_int_array(&bv);
        TEST_AIOOBE_HIT.with(|c| c.set(None));
        let r = call(&mut a, b.as_mut_ptr() as i64);
        assert_eq!(
            r,
            i64::MIN,
            "len={len}: the AIOOBE must leave through the stub"
        );
        let hit = TEST_AIOOBE_HIT.with(|c| c.get()).expect("AIOOBE stub");
        // Cast: test lengths
        assert_eq!(
            (hit.0, hit.1),
            ((len - 3) as i64, (len - 3) as i64),
            "len={len}"
        );
        let mut want = av.clone();
        for i in 0..len - 3 {
            want[i] = av[i].wrapping_add(bv[i]);
        }
        assert_eq!(r12w3_vec_read(&a, len), want, "partial writes, len={len}");
    }

    // A null `b`: NPE on the first element, `a` untouched; zero-trip: nothing.
    let av = r12w3_vec_values(20, 1);
    let mut a = r12w3_vec_int_array(&av);
    TEST_NPE_HIT.with(|c| c.set(false));
    assert_eq!(call(&mut a, 0), i64::MIN);
    assert!(TEST_NPE_HIT.with(|c| c.get()), "null b must raise the NPE");
    assert_eq!(r12w3_vec_read(&a, 20), av);
    let mut empty = r12w3_vec_int_array(&[]);
    TEST_NPE_HIT.with(|c| c.set(false));
    call(&mut empty, 0);
    assert!(
        !TEST_NPE_HIT.with(|c| c.get()),
        "a zero-trip loop reads no b"
    );
}

#[test]
pub(in crate::x64::tests) fn r12w3_vec_invariant_operand_forms_execute_like_the_scalar_loop() {
    type Scalar = fn(i32, i32) -> i32;
    let sub_local: Scalar = |a, k| a.wrapping_sub(k);
    let const_first: Scalar = |a, _| 7i32.wrapping_sub(a);
    // (fixture, code length, scalar `c[i]` from `(a[i], k)`)
    let cases: [(Vec<u8>, usize, Scalar); 2] = [
        (r12w3_vec_sub_local_code(), 23, sub_local),
        (r12w3_vec_const_first_sub_code(), 24, const_first),
    ];
    for (code, code_len, scalar) in cases {
        let compiled = compile_array_test(&code, code_len, 3, 4);
        if has_avx2() {
            assert!(
                r12w3_vec_has(compiled.code_bytes(), &R12W3_VEC_BROADCAST),
                "the invariant form must broadcast its operand on an AVX2 host"
            );
        }
        let call = |c: i64, a: i64, k: i32| -> i64 {
            // SAFETY: fake int[]s (or 0) and an int.
            unsafe {
                compiled
                    .try_call(&[c, a, i64::from(k)])
                    .expect("test JIT call")
            }
        };
        for k in [0, 1, -1, 12345, i32::MIN, i32::MAX] {
            for len in R12W3_VEC_LENS {
                let av = r12w3_vec_values(len, k);
                let mut a = r12w3_vec_int_array(&av);
                let mut c = r12w3_vec_int_array(&vec![0x5a5a; len]);
                call(c.as_mut_ptr() as i64, a.as_mut_ptr() as i64, k);
                let want: Vec<i32> = av.iter().map(|x| scalar(*x, k)).collect();
                assert_eq!(r12w3_vec_read(&c, len), want, "k={k} len={len}");
                assert_eq!(
                    r12w3_vec_read(&a, len),
                    av,
                    "a is read-only, k={k} len={len}"
                );

                // Aliasing: c == a.
                let p = a.as_mut_ptr() as i64;
                call(p, p, k);
                assert_eq!(r12w3_vec_read(&a, len), want, "aliased k={k} len={len}");
            }
            // `c` shorter than `a`: AIOOBE at c.length after c is fully written.
            for len in [9usize, 16, 30] {
                let av = r12w3_vec_values(len, 3);
                let mut a = r12w3_vec_int_array(&av);
                let mut c = r12w3_vec_int_array(&vec![0x5a5a; len - 2]);
                TEST_AIOOBE_HIT.with(|h| h.set(None));
                let r = call(c.as_mut_ptr() as i64, a.as_mut_ptr() as i64, k);
                assert_eq!(r, i64::MIN);
                let hit = TEST_AIOOBE_HIT.with(|h| h.get()).expect("AIOOBE stub");
                // Cast: test lengths
                assert_eq!((hit.0, hit.1), ((len - 2) as i64, (len - 2) as i64));
                let want: Vec<i32> = av[..len - 2].iter().map(|x| scalar(*x, k)).collect();
                assert_eq!(
                    r12w3_vec_read(&c, len - 2),
                    want,
                    "partial, k={k} len={len}"
                );
            }
        }
        // A null `c` with a non-empty `a`: NPE, `a` untouched.
        let av = r12w3_vec_values(12, 0);
        let mut a = r12w3_vec_int_array(&av);
        TEST_NPE_HIT.with(|h| h.set(false));
        assert_eq!(call(0, a.as_mut_ptr() as i64, 5), i64::MIN);
        assert!(TEST_NPE_HIT.with(|h| h.get()));
        assert_eq!(r12w3_vec_read(&a, 12), av);
    }
}

#[test]
pub(in crate::x64::tests) fn r12w3_vec_compound_invariant_mul_executes_like_the_scalar_loop() {
    let code = r12w3_vec_compound_mul_local_code();
    let compiled = compile_array_test(&code, 22, 3, 4);
    if has_avx2() {
        assert!(r12w3_vec_has(compiled.code_bytes(), &R12W3_VEC_BROADCAST));
    }
    for k in [0, 1, -1, 3, 0x10001, i32::MIN, i32::MAX] {
        for len in R12W3_VEC_LENS {
            let av = r12w3_vec_values(len, 7);
            let mut a = r12w3_vec_int_array(&av);
            // SAFETY: a fake int[], an unused null and an int.
            unsafe {
                compiled
                    .try_call(&[a.as_mut_ptr() as i64, 0, i64::from(k)])
                    .expect("test JIT call");
            }
            let want: Vec<i32> = av.iter().map(|x| x.wrapping_mul(k)).collect();
            assert_eq!(r12w3_vec_read(&a, len), want, "k={k} len={len}");
        }
    }
}

/// X6 (round 12 wave 3, lane vec): the arithmetic LICM may not claim a run a
/// branch enters.
///
/// `static int f(int base, boolean c, int x, int y, int n)
///  { int s = 0; for (int i = 0; i < n; i++) s += base + (c ? x : y) * 3; return s; }`
/// Locals: 0 base, 1 c, 2 x, 3 y, 4 n, 5 s, 6 i. javac joins the two arms of
/// the conditional at `iconst_3` (pc 25), so `iload_3 ; iconst_3 ; imul` at
/// 24..27 is invariant but ENTERED at 25 by the `goto` from the `x` arm.
#[test]
pub(in crate::x64::tests) fn r12w3_vec_arith_licm_does_not_claim_an_entered_run() {
    let code: Vec<u8> = vec![
        0x03, //              0: iconst_0
        0x36, 0x05, //        1: istore 5
        0x03, //              3: iconst_0
        0x36, 0x06, //        4: istore 6
        0x15, 0x06, //        6: iload 6         <- header
        0x15, 0x04, //        8: iload 4
        0xa2, 0x00, 0x1b, // 10: if_icmpge -> 37
        0x15, 0x05, //       13: iload 5
        0x1a, //             15: iload_0
        0x1b, //             16: iload_1
        0x99, 0x00, 0x07, // 17: ifeq -> 24
        0x1c, //             20: iload_2
        0xa7, 0x00, 0x04, // 21: goto -> 25
        0x1d, //             24: iload_3
        0x06, //             25: iconst_3        <- join
        0x68, //             26: imul
        0x60, //             27: iadd
        0x60, //             28: iadd
        0x36, 0x05, //       29: istore 5
        0x84, 0x06, 0x01, // 31: iinc 6, 1
        0xa7, 0xff, 0xe4, // 34: goto -> 6
        0x15, 0x05, //       37: iload 5
        0xac, //             39: ireturn
        0x00, 0x00,
    ];
    let code_len = 40;
    let loops = detect_loops(&code, code_len);
    assert_eq!(loops.len(), 1, "{loops:?}");

    // The round-11 matcher claimed 24..27 across the join.
    let untrimmed = find_arith_loop_hoists_with(&code, code_len, &loops, false);
    assert!(
        untrimmed
            .iter()
            .any(|h| (h.seq_start, h.seq_end) == (24, 27)),
        "precondition: the untrimmed matcher claims the entered run"
    );

    // Trimmed, no run crosses a branch target, and this loop has none left.
    let targets = bytecode_analysis::branch_target_map(&code, code_len);
    let trimmed = find_arith_loop_hoists(&code, code_len, &loops);
    for h in &trimmed {
        let mut q = h.seq_start + bytecode_analysis::step(&code, h.seq_start);
        while q < h.seq_end {
            assert!(
                !targets[q],
                "run {}..{} is entered at {q}",
                h.seq_start, h.seq_end
            );
            q += bytecode_analysis::step(&code, q);
        }
    }
    assert!(
        trimmed.is_empty(),
        "{:?}",
        trimmed
            .iter()
            .map(|h| (h.seq_start, h.seq_end))
            .collect::<Vec<_>>()
    );

    // Control: with the `ifeq` / `iload_2` / `goto` at 17..23 turned into
    // `nop`s nothing enters 25 any more, and 24..27 is hoisted again. (A
    // byte-level fixture: the matcher reads bytes, not a verified stack.)
    let mut straight = code.clone();
    for b in &mut straight[17..24] {
        *b = 0x00;
    }
    let loops = detect_loops(&straight, code_len);
    let hoisted = find_arith_loop_hoists(&straight, code_len, &loops);
    assert!(
        hoisted.iter().any(|h| (h.seq_start, h.seq_end) == (24, 27)),
        "an unentered run is still hoisted"
    );
}

// ── Round 12 wave 4, lane vec2: X2 field-array hoist and V6 ref-hoist maps ─
//
// X2 (`escape_analysis::find_field_array_hoists`, the pre-header emitter
// `Compiler::emit_field_array_hoist`) hoists `aload R; getfield F` of an array
// field out of a loop that cannot change it; V6 (`live_ref_hoist_slots`) names
// every ref-hoist slot in the oop maps of the safepoints inside its loop.

/// `int sum() { int s = 0; for (int i = 0; i < data.length; i++) s += data[i]; return s; }`
/// (`this` = local 0, `s` = 1, `i` = 2; `data` is an `int[]` field). Two
/// `aload_0; getfield data` sites: the header's (5..9) and the body's (14..18).
pub(in crate::x64::tests) fn r12w4_vec2_field_sum_code() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3c, //              1: istore_1
        0x03, //              2: iconst_0
        0x3d, //              3: istore_2
        0x1c, //              4: iload_2        <- header
        0x2a, //              5: aload_0
        0xb4, 0x00, 0x01, //  6: getfield data
        0xbe, //              9: arraylength
        0xa2, 0x00, 0x12, // 10: if_icmpge -> 28
        0x1b, //             13: iload_1
        0x2a, //             14: aload_0
        0xb4, 0x00, 0x01, // 15: getfield data
        0x1c, //             18: iload_2
        0x2e, //             19: iaload
        0x60, //             20: iadd
        0x3c, //             21: istore_1
        0x84, 0x02, 0x01, // 22: iinc 2, 1
        0xa7, 0xff, 0xeb, // 25: goto -> 4
        0x1b, //             28: iload_1
        0xac, //             29: ireturn
    ]
}

pub(in crate::x64::tests) fn r12w4_vec2_field_tables() -> (Vec<(usize, usize, u8)>, Vec<(usize, u32, bool)>) {
    // Cast: the compact header size, a small u32 displacement.
    let off = cratonvm_types::COMPACT_HEADER_SIZE as u32;
    (
        vec![(6, 0, b'['), (15, 0, b'[')],
        vec![(6, off, true), (15, off, true)],
    )
}

#[test]
pub(in crate::x64::tests) fn r12w4_vec2_field_hoist_finder_groups_the_sites_and_refuses_what_can_write_the_field() {
    let code = r12w4_vec2_field_sum_code();
    let loops = detect_loops(&code, code.len());
    let (fields, compact) = r12w4_vec2_field_tables();
    let none: FxHashSet<usize> = FxHashSet::default();
    let find = |code: &[u8],
                fields: &[(usize, usize, u8)],
                compact: &[(usize, u32, bool)],
                vol: &FxHashSet<usize>| {
        find_field_array_hoists(code, code.len(), &loops, fields, compact, vol)
    };

    let hoists = find(&code, &fields, &compact, &none);
    assert_eq!(hoists.len(), 1, "one (receiver, field) pair, one hoist");
    let h = &hoists[0];
    assert_eq!((h.loop_header, h.loop_end, h.array_local), (4, 28, 0));
    assert_eq!(h.index_local, FIELD_HOIST_NO_INDEX);
    let field = h.field.as_ref().expect("a field hoist");
    assert_eq!(field.sites, vec![(5, 9), (14, 18)]);
    assert_eq!(field.field_index, 0);
    assert_eq!((h.seq_start, h.seq_end), (5, 9));
    // A site of another field (another index) never shares the slot.
    let mut other = fields.clone();
    other[1].1 = 1;
    let split = find(&code, &other, &compact, &none);
    assert_eq!(split.len(), 2, "two placements, two hoists");

    // Anything in the loop that can store the field, or run Java that can,
    // refuses the loop. Each replaces the 3-byte `iinc` at 22 (the matcher
    // reads bytes, not a verified stack).
    for (what, bytes) in [
        ("putfield", [0xb5u8, 0x00, 0x01]),
        ("invokestatic", [0xb8, 0x00, 0x01]),
        ("invokevirtual", [0xb6, 0x00, 0x01]),
        ("new", [0xbb, 0x00, 0x01]),
        ("newarray", [0xbc, 0x0a, 0x00]),
        ("checkcast", [0xc0, 0x00, 0x01]),
        ("monitorenter", [0xc2, 0x00, 0x00]),
    ] {
        let mut c = code.clone();
        c[22..25].copy_from_slice(&bytes);
        assert!(
            find(&c, &fields, &compact, &none).is_empty(),
            "{what} in the loop must refuse the hoist"
        );
    }
    // A store to the receiver local: the sites no longer name one object.
    let mut c = code.clone();
    c[21] = 0x4b; // astore_0
    assert!(
        find(&c, &fields, &compact, &none).is_empty(),
        "astore_0 in the loop"
    );
    // A volatile site is never a hoist site (the driver's volatile filter then
    // refuses the whole loop).
    let vol: FxHashSet<usize> = [15usize].into_iter().collect();
    let hoists = find(&code, &fields, &compact, &vol);
    assert_eq!(hoists.len(), 1);
    assert_eq!(
        hoists[0].field.as_ref().map(|f| f.sites.clone()),
        Some(vec![(5, 9)])
    );
    // Not an array field, or not a compact reference cell: nothing.
    let objs: Vec<(usize, usize, u8)> = fields.iter().map(|&(p, i, _)| (p, i, b'L')).collect();
    assert!(
        find(&code, &objs, &compact, &none).is_empty(),
        "an `L` field is not X2's"
    );
    let prim: Vec<(usize, u32, bool)> = compact.iter().map(|&(p, o, _)| (p, o, false)).collect();
    assert!(
        find(&code, &fields, &prim, &none).is_empty(),
        "a non-reference cell"
    );
    assert!(
        find(&code, &fields, &[], &none).is_empty(),
        "no compact layout"
    );
}

/// Executed: the hoisted pre-header load computes the sum over the field's
/// array for a COMPACT and a LEGACY receiver, and the loop never reaches the
/// `getfield` helper.
///
/// `read_bounds_addr` is 0 in `test_helpers`, so an unhoisted `getfield` here
/// takes the helper arm. The helper below is a DECOY: it answers a different
/// array (`[1, 2, 3]`, stored in the object's last word, which no field
/// occupies), so an unhoisted compile sums the decoy (6) and any site the
/// hoisted compile still sent to the helper would corrupt its sum. No static
/// counter: `jit/tests/process_global_statics_ratchet.rs` counts test statics
/// too.
#[cfg(target_arch = "x86_64")]
#[test]
pub(in crate::x64::tests) fn r12w4_vec2_field_hoist_executes_and_reads_the_field_once() {
    unsafe extern "C" fn decoy_getfield(_vm: i64, obj: i64, _idx: i64) -> i64 {
        // `obj` is one of the `[u64; 8]` fake objects built below; word 7
        // holds the decoy array.
        (obj as *const i64).add(7).read_unaligned() // Cast: address arithmetic
    }
    // A concurrently armed ZGC barrier (another test) blocks every inline
    // compact read, X2 included; there is nothing to test then.
    if narrow_oops_block_inline_fields() {
        return;
    }
    let code = r12w4_vec2_field_sum_code();
    let mut helpers = test_helpers();
    helpers.getfield = decoy_getfield as *const () as usize; // Cast: fn → slot
    let compile_sum = |helpers: &JitRuntimeHelpers| -> CompiledMethod {
        let (fields, compact) = r12w4_vec2_field_tables();
        compile_with_request(
            BackendRequest::default(),
            compact,
            &code,
            code.len(),
            1,
            3,
            true, // needs_heap: the helper arm passes the VM pointer
            Vec::new(),
            fields,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(), // pic_slots
            Vec::new(), // ldc_info
            Vec::new(), // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None,
        )
        .expect("the field-array sum must compile")
    };
    let hoisted = compile_sum(&helpers);
    let unhoisted = {
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                FIELD_ARRAY_HOIST_FLAG,
                Some("0"),
            )]),
        );
        compile_sum(&helpers)
    };
    if narrow_oops_block_inline_fields() {
        return;
    }
    let decoy = r12w3_vec_int_array(&[1, 2, 3]);
    let decoy_ref = decoy.as_ptr() as u64; // Cast: stored reference
    for len in [0usize, 1, 2, 7, 16, 33] {
        let values = r12w3_vec_values(len, 0x5eed);
        let array = r12w3_vec_int_array(&values);
        let array_ref = array.as_ptr() as u64; // Cast: stored reference
        let mut compact = fake_compact_young_object();
        compact[cratonvm_types::COMPACT_HEADER_SIZE / 8] = array_ref;
        compact[7] = decoy_ref;
        // LEGACY receiver: no `GC_FLAG_COMPACT`, field 0 in the 16-byte
        // `Value` cell after the header, tagged "object" -- the shape the
        // TLAB fast path gives most objects.
        let mut legacy = Box::new([0u64; 8]);
        // SAFETY: `legacy` is 64 bytes and 8-byte aligned; the header byte
        // and the first cell's tag and payload all land inside it, below
        // word 7.
        unsafe {
            let p = legacy.as_mut_ptr() as *mut u8; // Cast: array base → byte cursor
            *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = 0;
            (p.add(HEADER_SIZE + FIELD_CELL_TAG_OFFSET) as *mut u32) // Cast: cell tag
                .write_unaligned(cratonvm_types::FIELD_CELL_TAG_OBJECT);
            (p.add(HEADER_SIZE + FIELD_CELL_PAYLOAD64_OFFSET) as *mut u64) // Cast: cell payload
                .write_unaligned(array_ref);
        }
        legacy[7] = decoy_ref;
        let expected = values.iter().fold(0i32, |s, v| s.wrapping_add(*v));
        let receivers = [
            ("compact", compact.as_mut_ptr() as i64), // Cast: receiver address
            ("legacy", legacy.as_mut_ptr() as i64),   // Cast: receiver address
        ];
        for (shape, this) in receivers {
            // SAFETY: JIT code from valid bytecode; the receiver is a live
            // object-shaped buffer whose field names a live fake `int[]`.
            let got = unsafe { hoisted.call_with_heap(0, &[this]) };
            assert_eq!(
                got as i32, // Cast: ireturn value
                expected,
                "X2 reads the field once, inline, in the pre-header ({shape}, len {len})"
            );
            // SAFETY: as above; the decoy helper returns a live fake `int[]`.
            let got = unsafe { unhoisted.call_with_heap(0, &[this]) };
            assert_eq!(
                got as i32, // Cast: ireturn value
                6,
                "without X2 every site calls the (decoy) helper ({shape}, len {len})"
            );
        }
    }
}

/// `static int f(int[][] m, int r, int n) { for (int i = 0; i < n; i++) m[r][i] = new int[i + 1].length; return 0; }`
/// -- the `aaload` hoist of `m[r]` with a GC-capable `newarray` (pc 14) inside
/// the loop.
pub(in crate::x64::tests) fn r12w4_vec2_row_loop_with_newarray() -> Vec<u8> {
    vec![
        0x03, //              0: iconst_0
        0x3e, //              1: istore_3
        0x1d, //              2: iload_3        <- header
        0x1c, //              3: iload_2
        0xa2, 0x00, 0x14, //  4: if_icmpge -> 24
        0x2a, //              7: aload_0
        0x1b, //              8: iload_1
        0x32, //              9: aaload         (hoisted)
        0x1d, //             10: iload_3
        0x1d, //             11: iload_3
        0x04, //             12: iconst_1
        0x60, //             13: iadd
        0xbc, 0x0a, //       14: newarray int   <- safepoint
        0xbe, //             16: arraylength
        0x4f, //             17: iastore
        0x84, 0x03, 0x01, // 18: iinc 3, 1
        0xa7, 0xff, 0xed, // 21: goto -> 2
        0x03, //             24: iconst_0
        0xac, //             25: ireturn
    ]
}

pub(in crate::x64::tests) fn r12w4_vec2_compile_row_loop() -> CompiledMethod {
    let code = r12w4_vec2_row_loop_with_newarray();
    compile_with_request(
        BackendRequest::default(),
        Vec::new(),
        &code,
        code.len(),
        3,
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
        Vec::new(), // pic_slots
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("the row loop must compile")
}

/// V6: the `newarray` safepoint inside the hoisted loop names the ref-hoist
/// slot, so a moving collection rewrites it; `CRATONVM_JIT_REF_HOIST_OOP_MAPS=0`
/// restores the wave-3 map, which does not.
#[test]
pub(in crate::x64::tests) fn r12w4_vec2_in_loop_safepoint_maps_name_the_ref_hoist_slot() {
    let names_the_slot = |cm: &CompiledMethod| -> Option<bool> {
        let (lo, hi) = (cm.frame_layout.ref_hoist_lo, cm.frame_layout.ref_hoist_hi);
        if hi <= lo {
            return None; // no `aaload` hoist in this compile
        }
        let slot = i16::try_from(lo).ok()?;
        Some(
            cm.oop_maps
                .iter()
                .filter(|m| m.bytecode_pc == 14)
                .any(|m| m.frame_slot_offsets.contains(&slot)),
        )
    };
    let on = r12w4_vec2_compile_row_loop();
    assert_eq!(
        names_the_slot(&on),
        Some(true),
        "the `m[r]` hoist slot must be live (and named) at the in-loop `newarray`"
    );
    // No map outside the loop may name it: bytecode pcs 0..2 run before the
    // pre-header, 24.. after the loop.
    let lo = i16::try_from(on.frame_layout.ref_hoist_lo).unwrap_or(i16::MAX);
    for m in &on.oop_maps {
        if m.bytecode_pc < 2 || m.bytecode_pc >= 24 {
            assert!(
                !m.frame_slot_offsets.contains(&lo),
                "a safepoint outside the loop (pc {}) names the hoist slot",
                m.bytecode_pc
            );
        }
    }
    let off = {
        let _off = cratonvm_types::flags::override_thread(
            cratonvm_types::flags::VmFlags::from_env_with_edits(&[(
                REF_HOIST_OOP_MAPS_FLAG,
                Some("0"),
            )]),
        );
        r12w4_vec2_compile_row_loop()
    };
    assert_eq!(
        names_the_slot(&off),
        Some(false),
        "the kill switch restores the unmapped slot"
    );
}

// -----------------------------------------------------------------------
// Interpreter round i1 wave 26, lane L2
// (`interpreter-L2-single-pass-frames-describe-no-lock-their-code-took-FIXED-20260928`): a
// single-pass frame state names the monitors its code TOOK, through the real
// compile of real bytecode (the analysis alone is
// `deopt_stubs::w26_taken_monitor_tests`).
// -----------------------------------------------------------------------

/// Compile `code` (padded, `code_len` real bytes) through the single-pass
/// backend with live monitor helpers. The body is never run.
pub(in crate::x64::tests) fn w26_compile_with_monitors(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    precise_exception_frames: bool,
    protected_ranges: Vec<(u32, u32)>,
    // JIT round 21 wave 4 (lane monitors): javac's catch-all rows, every one
    // a catch-all (the monitor balance gate needs a block's fallible
    // instructions covered, as HotSpot does).
    catch_all_table: &[(usize, usize, usize)],
) -> CompiledMethod {
    unsafe extern "C" fn monitor_stub(_context: i64, object: i64) -> i64 {
        object
    }
    let direct_helpers = crate::DirectHelperTable {
        monitor_enter: monitor_stub as *const () as usize,
        monitor_exit: monitor_stub as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    compile_with_request(
        BackendRequest {
            direct_helpers,
            precise_exception_frames,
            protected_ranges,
            exception_ranges: catch_all_table.to_vec(),
            exception_catch_all: vec![true; catch_all_table.len()],
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        code,
        code_len,
        num_params,
        max_locals,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect(
        "a synchronized block with live monitor helpers compiles, and its deopt metadata verifies",
    )
}

/// The monitor a frame state at `bci` must name: the javac temp's own
/// description in the same snapshot, taken once, `relock == false`.
pub(in crate::x64::tests) fn w26_assert_names_the_temp(p: &crate::deopt::DeoptimizationPoint, temp: usize) {
    use crate::deopt::FrameValue;
    let fs = &p.frame_state;
    let object = fs.locals[temp].clone();
    assert!(
        matches!(
            object,
            FrameValue::StackSlotRef(_) | FrameValue::RegisterRef(_)
        ),
        "bci {}: the lock temp (local {temp}) is live and a reference, described {object:?}",
        p.bci
    );
    assert_eq!(
        fs.monitors.len(),
        1,
        "bci {} ({:?}): the frame must name the one lock the code took, found {:?}",
        p.bci,
        p.reason,
        fs.monitors
    );
    let m = &fs.monitors[0];
    assert_eq!(
        m.object, object,
        "bci {}: the monitor is the temp's object",
        p.bci
    );
    assert_eq!(m.lock_depth, 1, "bci {}", p.bci);
    assert!(
        !m.relock,
        "bci {}: the code took it; the resume must not take it again",
        p.bci
    );
}

/// The OSR-exit map at a loop header INSIDE a `synchronized` block (the
/// frame the OSR-exit transfer and the back-edge poll exit publish) names
/// the block's lock.
///
/// ```text
///  0: aload_0  1: dup  2: astore_1  3: monitorenter
///  4: iconst_0  5: istore_2
///  6: iinc 2, 1          <- loop header
///  9: iload_2  10: bipush 10  12: if_icmplt 6
/// 15: aload_1  16: monitorexit  17: return
/// ```
#[test]
pub(in crate::x64::tests) fn w26_single_pass_frames_name_the_lock_at_a_loop_header_inside_the_block() {
    use crate::deopt::DeoptReason;
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2a, 0x59, 0x4c, 0xc2,
        0x03, 0x3d,
        0x84, 0x02, 0x01,
        0x1c, 0x10, 0x0a, 0xa1, 0xff, 0xfa,
        0x2b, 0xc3, 0xb1,
        0, 0,
    ];
    let compiled = w26_compile_with_monitors(&code, 18, 1, 3, false, Vec::new(), &[]);
    if !crate::deopt_real_enabled() {
        // `CRATONVM_DEOPT_REAL=0` publishes no OSR-exit snapshot at all.
        return;
    }
    let exit = compiled
        .deopt_points
        .iter()
        .find(|p| p.bci == 6 && p.reason == DeoptReason::OsrExit)
        .expect("the loop header at pc 6 publishes an OSR-exit snapshot");
    w26_assert_names_the_temp(exit, 1);
    // No point outside the block names anything.
    for p in &compiled.deopt_points {
        if !(4..16).contains(&p.bci) {
            assert!(
                p.frame_state.monitors.is_empty(),
                "bci {} holds no lock",
                p.bci
            );
        }
    }
}

/// The page's scenario, reason-9 half: a protected fallible site inside the
/// block publishes a frame naming the lock — which is what every VM path
/// that abandons the frame (the pad-exit release, a propagation, a re-run)
/// and every one that rebuilds it (the handler frame's record) reads.
///
/// ```text
///  0: aload_0  1: dup  2: astore_3  3: monitorenter
///  4: iload_1  5: iload_2  6: idiv   <- protected: a reason-9 (`/ by zero`) frame
///  7: istore_1  8: aload_3  9: monitorexit  10: iload_1  11: ireturn
/// 12: astore 4  14: aload_3  15: monitorexit  16: aload 4  18: athrow
/// Exception table: [4,10)->12 any, [12,16)->12 any
/// ```
///
/// JIT round 21 wave 4 (lane monitors): the catch-all handler is javac's;
/// without it the `idiv` runs under the lock with nothing to release it, which
/// the monitor balance gate refuses as HotSpot does.
#[test]
pub(in crate::x64::tests) fn w26_single_pass_frames_name_the_lock_at_a_protected_site_inside_the_block() {
    use crate::deopt::DeoptReason;
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x2a, 0x59, 0x4e, 0xc2,
        0x1b, 0x1c, 0x6c,
        0x3c, 0x2d, 0xc3, 0x1b, 0xac,
        0x3a, 0x04, 0x2d, 0xc3, 0x19, 0x04, 0xbf,
        0, 0,
    ];
    let table = [(4usize, 10usize, 12usize), (12, 16, 12)];
    let compiled = w26_compile_with_monitors(
        &code,
        19,
        3,
        5,
        true,
        // Cast: classfile pcs fit u32.
        table.iter().map(|&(s, e, _)| (s as u32, e as u32)).collect(),
        &table,
    );
    let frame = compiled
        .deopt_points
        .iter()
        .find(|p| p.bci == 6 && p.reason == DeoptReason::PendingException)
        .expect("the protected idiv publishes its precise frame at its own bci");
    w26_assert_names_the_temp(frame, 3);
}

// -----------------------------------------------------------------------
// Interpreter round i1 wave 27, lane L2
// (`interpreter-L2-single-pass-monitor-analysis-answers-nothing-in-a-loop-around-a-try-synchronized-FIXED-20260928`):
// a `try { synchronized }` inside a loop, through the real compile with the
// request's exception table and catch-all bits (the analysis alone is
// `deopt_stubs::w26_taken_monitor_tests::a_try_synchronized_in_a_loop_*`).
// -----------------------------------------------------------------------

/// javac 25 for
///
/// ```text
/// static int loop(Object lock, int n) {
///     int s = 0;
///     for (int i = 0; i < n; i++) {
///         try { synchronized (lock) { s += 10 / i; } } catch (ArithmeticException e) {}
///     }
///     return s;
/// }
/// ```
///
/// (`javap -c`, bytes copied from the class file):
///
/// ```text
///  0: iconst_0  1: istore_2  2: iconst_0  3: istore_3
///  4: iload_3  5: iload_1  6: if_icmpge 46          <- loop header at 4
///  9: aload_0  10: dup  11: astore 4  13: monitorenter
/// 14: iload_2  15: bipush 10  17: iload_3  18: idiv  <- protected: a reason-9 frame
/// 19: iadd  20: istore_2  21: aload 4  23: monitorexit  24: goto 35
/// 27: astore 5  29: aload 4  31: monitorexit  32: aload 5  34: athrow
/// 35: goto 40  38: astore 4  40: iinc 3, 1  43: goto 4  46: iload_2  47: ireturn
/// Exception table: [14,24)→27 any, [27,32)→27 any, [9,35)→38 ArithmeticException
/// ```
///
/// Before wave 27 the handler at 38 merged `[]` (from 9..13) with `[4]`
/// (from inside the block), the loop carried `Unknown` to every pc, and the
/// idiv's frame named no lock.
#[test]
pub(in crate::x64::tests) fn w27_single_pass_frames_name_the_lock_in_a_loop_around_a_try_synchronized() {
    use crate::deopt::DeoptReason;
    unsafe extern "C" fn monitor_stub(_context: i64, object: i64) -> i64 {
        object
    }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03, 0x3d, 0x03, 0x3e,
        0x1d, 0x1b, 0xa2, 0x00, 0x28,
        0x2a, 0x59, 0x3a, 0x04, 0xc2,
        0x1c, 0x10, 0x0a, 0x1d, 0x6c,
        0x60, 0x3d, 0x19, 0x04, 0xc3, 0xa7, 0x00, 0x0b,
        0x3a, 0x05, 0x19, 0x04, 0xc3, 0x19, 0x05, 0xbf,
        0xa7, 0x00, 0x05, 0x3a, 0x04, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xd9, 0x1c, 0xac,
        0, 0,
    ];
    let table: [(usize, usize, usize); 3] = [(14, 24, 27), (27, 32, 27), (9, 35, 38)];
    let direct_helpers = crate::DirectHelperTable {
        monitor_enter: monitor_stub as *const () as usize,
        monitor_exit: monitor_stub as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    let compiled = compile_with_request(
        BackendRequest {
            direct_helpers,
            precise_exception_frames: true,
            // Cast: classfile pcs fit u32.
            protected_ranges: table
                .iter()
                .map(|&(s, e, _)| (s as u32, e as u32))
                .collect(),
            exception_ranges: table.to_vec(),
            exception_catch_all: vec![true, true, false],
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        48,
        2,
        6,
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
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .expect("javac's try-synchronized loop compiles, and its deopt metadata verifies");
    let frame = compiled
        .deopt_points
        .iter()
        .find(|p| p.bci == 18 && p.reason == DeoptReason::PendingException)
        .expect("the protected idiv publishes its precise frame at its own bci");
    w26_assert_names_the_temp(frame, 4);
    // Outside the block and its catch-all nothing is held — the loop header
    // at 4 and the outer handler at 38 included.
    for p in &compiled.deopt_points {
        if p.bci < 14 || p.bci >= 32 {
            assert!(
                p.frame_state.monitors.is_empty(),
                "bci {} ({:?}) holds no lock, found {:?}",
                p.bci,
                p.reason,
                p.frame_state.monitors
            );
        }
    }
}

/// Round nio20 wave 3 (lane L3, W2L2-3): at a PROTECTED pc the emitter's
/// precise-frames refusal is lifted for a static body that cannot throw, and
/// for nothing else; at a pc no handler covers it is lifted for any body. The
/// splice of a body that cannot throw publishes no deopt point.
#[test]
pub(in crate::x64::tests) fn precise_frames_splice_only_a_static_body_that_cannot_throw() {
    fn precise_compiler(protected: bool) -> Compiler {
        let alloc_result = crate::regalloc::RegAllocResult {
            assignments: Vec::new(),
            xmm_assignments: Vec::new(),
            used_callee_saved: Vec::new(),
            used_xmm_regs: Vec::new(),
            block_live_in: Vec::new(),
        };
        let mut compiler = Compiler::new(
            "precise-nothrow-splice-test".to_string(),
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
        compiler.precise_exception_frames = true;
        if protected {
            compiler.protected_ranges = vec![(0, 64)];
        }
        compiler.push_stack().expect("first argument slot");
        compiler.push_stack().expect("second argument slot");
        compiler
    }

    // `static int first(int a, int b) { return a; }`
    let mut compiler = precise_compiler(true);
    let points_before = compiler.deopt_points.len();
    let leaf = make_inline_site(&[0x1a, 0xac], 2, 2, true, b'I');
    assert!(crate::inline_site_body_cannot_throw(&leaf));
    assert!(compiler.try_emit_inline_site(0, &leaf));
    assert_eq!(compiler.deopt_points.len(), points_before);

    // `static int div(int a, int b) { return a / b; }`: can throw. Offered with
    // its divisor proven non-zero (the divisor-guarded form, which splices
    // without precise frames), so only the interlock can refuse it.
    let mut compiler = precise_compiler(true);
    let div = make_inline_site(&[0x1a, 0x1b, 0x6c, 0xac], 2, 2, true, b'I');
    assert!(!crate::inline_site_body_cannot_throw(&div));
    assert!(!compiler.try_emit_inline_site_with(0, &div, 0b10));

    // The same leaf as an instance method: refused (receiver null check).
    let mut compiler = precise_compiler(true);
    let instance = make_inline_site(&[0x1b, 0xac], 2, 2, false, b'I');
    assert!(!compiler.try_emit_inline_site(0, &instance));

    // No handler covers the invoke: the body that can throw splices as in a
    // compile without precise frames.
    let mut compiler = precise_compiler(false);
    assert!(compiler.try_emit_inline_site_with(0, &div, 0b10));
}

/// gch h3/fd: the VM-context word a test passes to a body compiled with
/// `needs_heap` (`CompiledMethod::try_call_with_context`). Since gch h2/fj
/// (`gcd-d1a-proposal-context-slot-loads-cannot-read-an-unreserved-slot`) a
/// body whose helper call loads the context refuses to compile without the
/// reserved slot, so the tests that compile such bodies say `needs_heap: true`
/// -- as the production scan (`bytecode_compat`) does for every one of those
/// opcodes -- and hand the hidden argument in. Every test stub ignores its
/// `_vm` argument; the word points at zeroed memory (leaked, one per call) so
/// a stray read finds zeros, never a wild address.
pub(crate) fn test_vm_ctx() -> i64 {
    // Cast: a test-owned address as the JIT ABI's context word.
    Box::leak(Box::new([0u64; 64])).as_ptr() as i64
}

/// TAIL819-1 (JIT round 19 wave 9, lane tail9): one guarded nested splice
/// emitted by a bare `Compiler`, with the census switch as given. Returns the
/// emitted bytes.
///
/// `static int leaf(Recv r) { return r.m(); }` spliced at the caller, its
/// `r.m()` at pc 1 guard-spliced for class 77 (`iconst_5; ireturn`), the miss
/// edge the dispatch call.
pub(in crate::x64::tests) fn tail9_guarded_nested_splice_bytes(census: bool) -> Vec<u8> {
    tail9_guarded_nested_splice_compiled(census, false).0
}

/// [`tail9_guarded_nested_splice_bytes`] with TAIL9-20-2's evidence switch
/// (`Compiler::sp_nested_miss_evidence`) as well, handing back the cells the
/// compile kept for its artifact.
pub(in crate::x64::tests) fn tail9_guarded_nested_splice_compiled(
    census: bool,
    evidence: bool,
) -> (Vec<u8>, Vec<crate::SpNestedMissEvidence>) {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    let mut compiler = Compiler::new(
        "tail9-nested-census-test".to_string(),
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
    compiler.sp_nested_splice_census = census;
    compiler.sp_nested_miss_evidence = evidence;
    compiler.stack_push(StackSlot::CalleeSaved(R12), true);

    let mut m = make_inline_site(&[0x08, 0xac], 1, 1, false, b'I');
    m.class_name = "pkg/Recv".to_string();
    m.class_id = 77;
    m.method_name = "m".to_string();
    m.descriptor = "()I".to_string();
    // 0: aload_0; 1: invokevirtual #3; 4: ireturn
    let mut leaf = make_inline_site(&[0x2a, 0xb6, 0x00, 0x03, 0xac], 1, 1, true, b'I');
    leaf.descriptor = "(Lpkg/Recv;)I".to_string();
    leaf.resolved_invoke_infos = vec![resolved_invoke(1, "pkg/Recv", "m", "()I", 1, b'I', 0)];
    leaf.nested_sites = vec![crate::NestedInlineSite {
        callee_pc: 1,
        guard_class_id: 77,
        site: m,
        bimorphic_second: None,
    }].into();

    assert!(
        compiler.try_emit_inline_site(0, &leaf),
        "the enclosing splice must be admitted; a bail makes the comparison vacuous"
    );
    (
        compiler.buf.as_slice()[..compiler.buf.pos()].to_vec(),
        std::mem::take(&mut compiler.sp_nested_miss_cells),
    )
}

pub(in crate::x64::tests) fn tail9_count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

/// TAIL819-1: with `CRATONVM_DBG_SP_NESTED_SPLICE_CENSUS` off the guarded
/// nested splice carries no census byte; on, it carries exactly the two
/// 14-byte bumps and nothing else changes but the jump displacements that
/// cross them.
#[test]
pub(in crate::x64::tests) fn a_census_off_guarded_nested_splice_emits_no_census_bytes() {
    const LOCK_INC_R11: [u8; 4] = [0xF0, 0x49, 0xFF, 0x03];
    let off = tail9_guarded_nested_splice_bytes(false);
    let off_again = tail9_guarded_nested_splice_bytes(false);
    let on = tail9_guarded_nested_splice_bytes(true);

    let mut guard = vec![0x81, 0x78, 0x00];
    guard.extend_from_slice(&77u32.to_le_bytes());
    assert_eq!(
        tail9_count(&off, &guard),
        1,
        "the nested site must be GUARD-spliced (class 77), or the census has no arm to count"
    );
    assert_eq!(off, off_again, "the switch-off emission is deterministic");
    assert_eq!(
        tail9_count(&off, &LOCK_INC_R11),
        0,
        "switch off: no census bump"
    );
    assert_eq!(
        tail9_count(&on, &LOCK_INC_R11),
        2,
        "switch on: one bump per arm"
    );
    assert_eq!(
        on.len(),
        off.len() + 2 * 14,
        "switch on adds exactly the two bumps"
    );

    // Strip each `MOV R11, imm64 ; LOCK INC [R11]` from the census arm.
    let mut stripped = Vec::with_capacity(on.len());
    let mut i = 0;
    while i < on.len() {
        if on[i..].starts_with(&[0x49, 0xBB]) && on.get(i + 10..i + 14) == Some(&LOCK_INC_R11[..]) {
            i += 14;
            continue;
        }
        stripped.push(on[i]);
        i += 1;
    }
    assert_eq!(stripped.len(), off.len());
    // Every remaining difference is inside a rel32 of a `Jcc` (0F 8x) or a
    // `JMP` (E9) in the off bytes: the miss edges and the join jump.
    let rel32_bytes: std::collections::HashSet<usize> = (0..off.len())
        .filter_map(|p| {
            if off[p] == 0x0F && off.get(p + 1).is_some_and(|b| b & 0xF0 == 0x80) {
                Some(p + 2)
            } else if off[p] == 0xE9 {
                Some(p + 1)
            } else {
                None
            }
        })
        .flat_map(|start| start..start + 4)
        .collect();
    let diffs: Vec<usize> = (0..off.len()).filter(|&p| off[p] != stripped[p]).collect();
    assert!(
        !diffs.is_empty(),
        "the miss edges cross the hit bump, so a displacement moves"
    );
    for p in diffs {
        assert!(
            rel32_bytes.contains(&p),
            "byte {p} differs outside a jump displacement: off {:#04x}, on {:#04x}",
            off[p],
            stripped[p]
        );
    }
}

/// TAIL9-20-2 (JIT round 19 wave 11, lane tail11): the evidence switch alone
/// bumps each arm with a plain `INC QWORD [R11]` (13 bytes; no `LOCK`), keeps
/// the guarded construct otherwise as the census emits it, and hands the
/// artifact one cell keyed by the ENCLOSING body, the call's pc and the guard
/// class. With the census too, the bumps are the census's `LOCK` form. Off:
/// no cell and no bump.
#[test]
pub(in crate::x64::tests) fn the_nested_miss_evidence_switch_counts_both_arms_and_hands_the_cell_over() {
    const LOCK_INC_R11: [u8; 4] = [0xF0, 0x49, 0xFF, 0x03];
    const INC_R11: [u8; 3] = [0x49, 0xFF, 0x03];
    let (off, off_cells) = tail9_guarded_nested_splice_compiled(false, false);
    assert!(off_cells.is_empty(), "switch off: no cell");
    assert_eq!(tail9_count(&off, &INC_R11), 0);

    let (on, cells) = tail9_guarded_nested_splice_compiled(false, true);
    assert_eq!(
        tail9_count(&on, &LOCK_INC_R11),
        0,
        "no LOCK without the census"
    );
    assert_eq!(tail9_count(&on, &INC_R11), 2, "one plain bump per arm");
    assert_eq!(on.len(), off.len() + 2 * 13);
    assert_eq!(cells.len(), 1, "one guarded nested site, one cell");
    let row = &cells[0];
    assert_eq!(row.key.1, 1, "the call's pc in the enclosing body");
    assert_eq!(row.key.2, 77, "the guard class");
    assert!(
        row.key.0.ends_with(":(Lpkg/Recv;)I"),
        "keyed by the ENCLOSING body's label, got {}",
        row.key.0
    );
    // The words the code bakes are the words the artifact hands over.
    let hit = std::ptr::addr_of!(*row.counters.hits) as i64;
    let miss = std::ptr::addr_of!(*row.counters.misses) as i64;
    let mut mov_hit = vec![0x49, 0xBB];
    mov_hit.extend_from_slice(&hit.to_le_bytes());
    let mut mov_miss = vec![0x49, 0xBB];
    mov_miss.extend_from_slice(&miss.to_le_bytes());
    assert_eq!(tail9_count(&on, &mov_hit), 1);
    assert_eq!(tail9_count(&on, &mov_miss), 1);

    let (both, both_cells) = tail9_guarded_nested_splice_compiled(true, true);
    assert_eq!(
        tail9_count(&both, &LOCK_INC_R11),
        2,
        "the census's LOCK form wins"
    );
    assert_eq!(both.len(), off.len() + 2 * 14);
    assert_eq!(both_cells.len(), 1);
}

/// TAIL9-20-2: the resolver's verdict -- at or above 5 % past 1024 misses.
#[test]
pub(in crate::x64::tests) fn the_nested_miss_share_bar_needs_both_the_share_and_the_floor() {
    use crate::sp_nested_miss_share_refuses as refuses;
    assert!(!refuses(0, 0));
    assert!(!refuses(10, 1023), "under the floor");
    assert!(refuses(19_456, 1024), "1024 / 20480 = 5 %");
    assert!(!refuses(19_457, 1024), "just under 5 %");
    assert!(refuses(0, u64::MAX), "saturating");
    assert!(!refuses(u64::MAX, 2048));
}

/// TAIL9-20-2: the JIT cache files an artifact's cells by site and sums the
/// bodies that spliced one site, read live; another pc or guard class
/// answers `None`, and a whole-cache flush forgets.
#[test]
pub(in crate::x64::tests) fn the_jit_cache_sums_nested_miss_counters_by_site() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let leak = |v: u64| -> &'static AtomicU64 { Box::leak(Box::new(AtomicU64::new(v))) };
    let row = |h: u64, m: u64| crate::SpNestedMissEvidence {
        key: (Box::from("B.f:(LA;)I"), 4, 7),
        counters: crate::SpNestedMissCounters {
            hits: leak(h),
            misses: leak(m),
        },
    };
    let cache = crate::JitCache::new();
    assert_eq!(cache.sp_nested_miss_counts("B.f:(LA;)I", 4, 7), None);
    let mut a = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    a.sp_nested_miss_cells = vec![row(90, 10)].into_boxed_slice();
    let mut b = CompiledMethod::new(ExecutableBuffer::new(64).unwrap());
    let late = row(10, 30);
    let late_misses = late.counters.misses;
    b.sp_nested_miss_cells = vec![late].into_boxed_slice();
    cache.note_sp_nested_miss_cells(&a);
    cache.note_sp_nested_miss_cells(&b);
    assert_eq!(
        cache.sp_nested_miss_counts("B.f:(LA;)I", 4, 7),
        Some((100, 40))
    );
    late_misses.fetch_add(5, Ordering::Relaxed);
    assert_eq!(
        cache.sp_nested_miss_counts("B.f:(LA;)I", 4, 7),
        Some((100, 45))
    );
    assert_eq!(cache.sp_nested_miss_counts("B.f:(LA;)I", 5, 7), None);
    assert_eq!(cache.sp_nested_miss_counts("B.f:(LA;)I", 4, 8), None);
    cache.clear_all();
    assert_eq!(
        cache.sp_nested_miss_counts("B.f:(LA;)I", 4, 7),
        None,
        "a flush forgets"
    );
}

/// A bare `Compiler` (TAIL9-20-1's arm test), built as
/// [`tail9_guarded_nested_splice_compiled`] builds its own.
pub(in crate::x64::tests) fn tail11_bare_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    Compiler::new(
        "tail11-republish-arm-test".to_string(),
        ExecutableBuffer::new(4096).expect("test executable buffer"),
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
    )
}

/// TAIL9-20-1's measurement arm (JIT round 19 wave 11, lane tail11): the
/// republish stores become NOPs of exactly their length, so only the stores
/// change, and the switch-off emission is unchanged.
#[test]
pub(in crate::x64::tests) fn the_republish_nop_arm_keeps_the_length_and_drops_the_stores() {
    let mk = |arm: bool| {
        let mut c = tail11_bare_compiler();
        c.precise_maps = true;
        c.helpers.frame_record = 0x1000;
        c.inline_rbp_tls_disp = 0x40;
        c.inline_cm_tls_disp = 0x48;
        c.post_call_republish_nop_arm = arm;
        c.emit_post_call_rbp_republish();
        (
            c.buf.as_slice()[..c.buf.pos()].to_vec(),
            c.post_call_republish_nopped,
        )
    };
    let (stores, n_off) = mk(false);
    let (nops, n_on) = mk(true);
    assert_eq!(n_off, 0);
    assert_eq!(n_on, 1);
    assert_eq!(
        stores.len(),
        9 + 12,
        "MOV seg:[disp32], RBP (9) + MOV dword seg:[disp32], imm32 (12)"
    );
    assert_eq!(
        nops.len(),
        stores.len(),
        "same length: the code layout is unchanged"
    );
    assert!(
        !nops.contains(&0x89) && !nops.contains(&0xC7),
        "no store survives: {nops:02x?}"
    );
    assert_eq!(
        &nops[..9],
        &[0x66, 0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

/// TAIL819-1: the exit lines aggregate recompiles of one site, rank by
/// misses and print only engaged sites, and the header keeps the totals.
#[test]
pub(in crate::x64::tests) fn the_nested_splice_census_lines_aggregate_and_rank() {
    use super::inlining::tail9_census_test_cell as cell;
    let a1 = cell("A.run:()V", "B.f:()I", 4, 7, "C.g:()I", 90, 10);
    let a2 = cell("A.run:()V", "B.f:()I", 4, 7, "C.g:()I", 10, 30);
    let b = cell("A.run:()V", "B.f:()I", 9, 8, "D.h:()I", 5, 0);
    let dead = cell("X.y:()V", "B.f:()I", 4, 7, "C.g:()I", 0, 0);
    let lines = super::inlining::tail9_census_lines_for_test(&[b, dead, a1, a2]);
    assert_eq!(
        lines[0],
        "[SP_NESTED_SPLICE_CENSUS] cells=4 engaged_sites=2 hits=105 misses=40"
    );
    assert_eq!(lines.len(), 3, "the never-run cell prints no site line");
    assert!(
        lines[1].contains(
            "callee_pc=4 guard=7 callee=C.g:()I bodies=2 hits=100 misses=40 miss_share=28.57%"
        ),
        "{}",
        lines[1]
    );
    assert!(lines[2].contains("callee_pc=9"), "{}", lines[2]);
}

