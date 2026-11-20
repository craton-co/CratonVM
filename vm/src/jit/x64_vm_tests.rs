// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_jit::x64::{compile, has_avx2};
use cratonvm_types::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[test]
fn test_checkcast_null_passthrough() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // null passes checkcast → returns 0
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

#[test]
fn test_instanceof_null() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    // null → instanceof returns 0
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

#[test]
fn test_bounds_check_iaload_in_bounds() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Method: int f(int[] arr, int idx) { return arr[idx]; }
    // Bytecode: aload_0, iload_1, iaload, ireturn
    // needs_heap = false for inline array access, but we pass needs_heap=true
    // to test the bounds check with a real array.
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate an int[5] = {10, 20, 30, 40, 50}
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .mem
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

#[test]
fn test_bounds_check_bastore_in_bounds() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // Method: void f(byte[] arr, int idx, int val) { arr[idx] = (byte)val; }
    // Bytecode: aload_0, iload_1, iload_2, bastore, return
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    let arr = shared
        .mem
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
    let val = shared.mem.heap.get_array_element(arr, 0).unwrap();
    assert_eq!(val.as_int(), Some(42));
}

#[test]
fn test_bounds_check_loop_compiled() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
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
    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Allocate int[5] = {1, 2, 3, 4, 5}
    let arr = shared
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, 5);
    let arr_ptr = arr.as_ptr();
    for i in 0..5 {
        let _ = shared
            .mem
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

#[test]
fn test_simd_int_array_sum_end_to_end() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    // End-to-end test: compile a long sum_array(int[], int) method
    // and verify SIMD-accelerated execution with a real array.
    if !has_avx2() {
        println!("Skipping SIMD end-to-end test: AVX2 not available");
        return;
    }

    use cratonvm_gc::heap::ArrayElementType;
    use cratonvm_types::ClassId;

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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    let vm_ptr = shared.as_ref() as *const _ as i64; // Cast: function pointer for JIT call target

    // Test 1: empty array (n=0, should return 0) — no SIMD, no scalar
    let arr1 = shared
        .mem
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
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n2);
    let arr2_ptr = arr2.as_ptr();
    for i in 0..n2 {
        let _ = shared.mem.heap.set_array_element(arr2, i, Value::Int(100));
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
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n3);
    let arr3_ptr = arr3.as_ptr();
    for i in 0..n3 {
        let _ = shared.mem.heap.set_array_element(arr3, i, Value::Int(10));
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
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n4);
    let arr4_ptr = arr4.as_ptr();
    for i in 0..n4 {
        let _ = shared
            .mem
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
        .mem
        .heap
        .alloc_array(ClassId::new(0), ArrayElementType::Int, n5);
    let arr5_ptr = arr5.as_ptr();
    for i in 0..n5 {
        // Cast: value to i32 (encoding immediate/displacement)
        let _ = shared
            .mem
            .heap
            .set_array_element(arr5, i, Value::Int(i as i32));
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
fn test_jit_new_object_codegen() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with `new` opcode"
    );

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

#[test]
fn test_jit_anewarray_codegen() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
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
        &crate::jit::helpers::build_helpers_for(&shared),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    );
    assert!(
        compiled.is_some(),
        "Should compile method with `anewarray` opcode"
    );

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
