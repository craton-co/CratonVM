#[allow(unused_imports)]
use super::*;

#[test]
pub(in crate::x64::tests) fn test_compile_dsub_dmul_ddiv() {
    // double dsub
    let sub_code: Vec<u8> = vec![0x26, 0x27, 0x67, 0xaf, 0, 0];
    let compiled = compile(
        &sub_code,
        4,
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
    let a = 100.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 37.5f64.to_bits() as i64; // Cast: JIT ABI convention
                                      // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                      // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 62.5f64); // Cast: JIT ABI convention

    // double dmul
    let mul_code: Vec<u8> = vec![0x26, 0x27, 0x6b, 0xaf, 0, 0];
    let compiled = compile(
        &mul_code,
        4,
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
    let a = 6.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention

    // double ddiv
    let div_code: Vec<u8> = vec![0x26, 0x27, 0x6f, 0xaf, 0, 0];
    let compiled = compile(
        &div_code,
        4,
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
    let a = 22.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 7.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    let expected = 22.0f64 / 7.0f64;
    assert_eq!(f64::from_bits(result as u64), expected); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_fneg_dneg() {
    // float fneg(float x) { return -x; }
    // fload_0 (0x22), fneg (0x76), freturn (0xae)
    let fcode: Vec<u8> = vec![0x22, 0x76, 0xae, 0, 0];
    let compiled = compile(
        &fcode,
        3,
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
    let input = 3.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), -3.5f32); // Cast: JIT ABI convention
                                                        // Negate negative
    let input = (-7.0f32).to_bits() as i64; // Cast: JIT ABI convention
                                            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 7.0f32); // Cast: JIT ABI convention

    // double dneg(double x) { return -x; }
    // dload_0 (0x26), dneg (0x77), dreturn (0xaf)
    let dcode: Vec<u8> = vec![0x26, 0x77, 0xaf, 0, 0];
    let compiled = compile(
        &dcode,
        3,
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
    let input = 42.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), -42.0f64); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_i2f_i2d() {
    // int → float: iload_0 (0x1a), i2f (0x86), freturn (0xae)
    let i2f_code: Vec<u8> = vec![0x1a, 0x86, 0xae, 0, 0];
    let compiled = compile(
        &i2f_code,
        3,
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
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 42.0f32); // Cast: JIT ABI convention
                                                        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe { compiled.try_call(&[-7]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), -7.0f32); // Cast: JIT ABI convention

    // int → double: iload_0 (0x1a), i2d (0x87), dreturn (0xaf)
    let i2d_code: Vec<u8> = vec![0x1a, 0x87, 0xaf, 0, 0];
    let compiled = compile(
        &i2d_code,
        3,
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
    let result = unsafe { compiled.try_call(&[42]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 42.0f64); // Cast: JIT ABI convention
                                                        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
    let result = unsafe { compiled.try_call(&[-100]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), -100.0f64); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_f2i_f2d_d2i_d2f() {
    // float → int: fload_0 (0x22), f2i (0x8b), ireturn (0xac)
    let f2i_code: Vec<u8> = vec![0x22, 0x8b, 0xac, 0, 0];
    let compiled = compile(
        &f2i_code,
        3,
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
    let input = 3.7f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 3); // truncate toward zero
    let input = (-3.7f32).to_bits() as i64; // Cast: JIT ABI convention
                                            // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                            // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, -3);

    // float → double: fload_0, f2d (0x8d), dreturn
    let f2d_code: Vec<u8> = vec![0x22, 0x8d, 0xaf, 0, 0];
    let compiled = compile(
        &f2d_code,
        3,
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
    let input = 1.5f32.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 1.5f64); // Cast: JIT ABI convention

    // double → int: dload_0 (0x26), d2i (0x8e), ireturn (0xac)
    let d2i_code: Vec<u8> = vec![0x26, 0x8e, 0xac, 0, 0];
    let compiled = compile(
        &d2i_code,
        3,
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
    let input = 9.99f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 9);

    // double → float: dload_0 (0x26), d2f (0x90), freturn (0xae)
    let d2f_code: Vec<u8> = vec![0x26, 0x90, 0xae, 0, 0];
    let compiled = compile(
        &d2f_code,
        3,
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
    let input = 1.5f64.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 1.5f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_l2f_l2d_f2l_d2l() {
    // long → float: lload_0 (0x1e), l2f (0x89), freturn (0xae)
    let l2f_code: Vec<u8> = vec![0x1e, 0x89, 0xae, 0, 0];
    let compiled = compile(
        &l2f_code,
        3,
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
    let result = unsafe { compiled.try_call(&[1000000i64]).expect("test JIT call") };
    assert_eq!(f32::from_bits(result as u32), 1_000_000.0f32); // Cast: JIT ABI convention

    // long → double: lload_0 (0x1e), l2d (0x8a), dreturn (0xaf)
    let l2d_code: Vec<u8> = vec![0x1e, 0x8a, 0xaf, 0, 0];
    let compiled = compile(
        &l2d_code,
        3,
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
    let result = unsafe { compiled.try_call(&[1000000i64]).expect("test JIT call") };
    assert_eq!(f64::from_bits(result as u64), 1_000_000.0f64); // Cast: JIT ABI convention

    // float → long: fload_0 (0x22), f2l (0x8c), lreturn (0xad)
    let f2l_code: Vec<u8> = vec![0x22, 0x8c, 0xad, 0, 0];
    let compiled = compile(
        &f2l_code,
        3,
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
    let input = 42.9f32.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 42i64);

    // double → long: dload_0 (0x26), d2l (0x8f), lreturn (0xad)
    let d2l_code: Vec<u8> = vec![0x26, 0x8f, 0xad, 0, 0];
    let compiled = compile(
        &d2l_code,
        3,
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
    let input = 99.9f64.to_bits() as i64; // Cast: JIT ABI convention
                                          // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                          // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[input]).expect("test JIT call") };
    assert_eq!(result, 99i64);
}

#[test]
pub(in crate::x64::tests) fn test_compile_fcmpl() {
    // int f(float a, float b) { return fcmpl(a, b); }
    // fload_0 (0x22), fload_1 (0x23), fcmpl (0x95), ireturn (0xac)
    let code: Vec<u8> = vec![0x22, 0x23, 0x95, 0xac, 0, 0];
    let compiled = compile(
        &code,
        4,
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

    // a > b → 1
    let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    // a == b → 0
    let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        0
    );

    // a < b → -1
    let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → -1 (fcmpl)
    let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        -1
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        -1
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_fcmpg() {
    // fload_0, fload_1, fcmpg (0x96), ireturn
    let code: Vec<u8> = vec![0x22, 0x23, 0x96, 0xac, 0, 0];
    let compiled = compile(
        &code,
        4,
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

    // a > b → 1
    let a = 5.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    // a < b → -1
    let a = 1.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → 1 (fcmpg)
    let nan = f32::NAN.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        1
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        1
    );
}

/// `compile` with every table empty, for the small branch-shape tests below.
pub(in crate::x64::tests) fn compile_plain_r11(code: &[u8], code_len: usize) -> CompiledMethod {
    compile(
        code,
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
        Vec::new(), // pic_slots
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &test_helpers(),
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("r11 fused-fcmp fixture must compile")
}

/// gcf f2/f (review finding 5 of the gcf f1/f report): a single-pass body
/// with no loop -- no OSR entry to speak of -- still publishes its frame size
/// in `osr_frame_size`, and the frame it names contains the frame-deopt GPR
/// block. The band scan bounds every compiled frame by `[rbp -
/// osr_frame_size, rbp)` and refuses a frame whose size is 0, and
/// `conservative_roots::frame_deopt_image_is_residue` reads the block's RBP
/// image only below that size: a body that published 0 (or a smaller size)
/// would silently lose the band and the residue claim.
#[test]
pub(in crate::x64::tests) fn gcf_f2f_every_single_pass_body_publishes_its_frame_size() {
    // iload_0; iload_1; iadd; ireturn
    let straight = compile_plain_r11(&[0x1a, 0x1b, 0x60, 0xac, 0, 0], 4);
    assert!(
        straight.osr_frame_size > 0,
        "a straight-line body publishes its frame size too"
    );
    let layout = &straight.frame_layout;
    if layout.deopt_gpr_hi > layout.deopt_gpr_lo {
        // `gpr[0]`, the block's deepest word, sits at `[rbp - (deopt_gpr_hi - 8)]`.
        assert!(
            layout.deopt_gpr_hi - 8 <= straight.osr_frame_size,
            "the deopt GPR block [{}, {}) lies inside the published frame of {} bytes",
            layout.deopt_gpr_lo,
            layout.deopt_gpr_hi,
            straight.osr_frame_size
        );
    }
}

/// JVMS `*cmp*`: `-1/0/+1`, an unordered pair giving `+1` for the `g` forms.
pub(in crate::x64::tests) fn java_fp_cmp_r11(a: f64, b: f64, nan_positive: bool) -> i32 {
    if a.is_nan() || b.is_nan() {
        if nan_positive {
            1
        } else {
            -1
        }
    } else if a > b {
        1
    } else if a < b {
        -1
    } else {
        0
    }
}

/// JVMS `if<cond>` against zero.
pub(in crate::x64::tests) fn java_if_r11(if_op: u8, r: i32) -> bool {
    match if_op {
        0x99 => r == 0,
        0x9a => r != 0,
        0x9b => r < 0,
        0x9c => r >= 0,
        0x9d => r > 0,
        _ => r <= 0,
    }
}

/// r11 x64gen: `*cmp*; if<cond>` is fused into `UCOMIS; Jcc`
/// (`try_fused_fcmp_branch`). Every one of the 24 pairs, over NaN, ±0.0,
/// ±inf and ordinary values in both argument orders, must branch exactly as
/// the unfused three-way result tested against zero does — both with the two
/// operands as the whole stack (the pop-before-flush arm) and with a value
/// live beneath them (the canonicalising arm).
#[test]
pub(in crate::x64::tests) fn fused_fp_compare_branch_matches_the_three_way_result() {
    let values: [f64; 9] = [
        1.0,
        2.0,
        -1.5,
        0.0,
        -0.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        2.0,
    ];
    for cmp_op in [0x95u8, 0x96, 0x97, 0x98] {
        let is_double = cmp_op >= 0x97;
        let nan_positive = cmp_op == 0x96 || cmp_op == 0x98;
        let (load0, load1) = if is_double {
            (0x26u8, 0x27u8)
        } else {
            (0x22u8, 0x23u8)
        };
        for if_op in 0x99u8..=0x9e {
            // Whole-stack shape: taken → 0, fall-through → 1.
            //   0 load0; 1 load1; 2 cmp; 3 if +5 (→ 8); 6 iconst_1; 7 ireturn;
            //   8 iconst_0; 9 ireturn
            let whole = [
                load0, load1, cmp_op, if_op, 0x00, 0x05, 0x04, 0xac, 0x03, 0xac, 0, 0,
            ];
            // Live-beneath shape: taken → 7, fall-through → 8.
            //   0 bipush 7; 2 load0; 3 load1; 4 cmp; 5 if +6 (→ 11);
            //   8 iconst_1; 9 iadd; 10 ireturn; 11 ireturn
            let beneath = [
                0x10, 0x07, load0, load1, cmp_op, if_op, 0x00, 0x06, 0x04, 0x60, 0xac, 0xac, 0, 0,
            ];
            let whole_m = compile_plain_r11(&whole, 10);
            let beneath_m = compile_plain_r11(&beneath, 12);
            for &a in &values {
                for &b in &values {
                    let (arg_a, arg_b, fa, fb) = if is_double {
                        // Cast: JIT ABI convention — a double travels as its bits.
                        (a.to_bits() as i64, b.to_bits() as i64, a, b)
                    } else {
                        let (x, y) = (a as f32, b as f32);
                        // Cast: JIT ABI convention — a float travels as its bits.
                        (
                            i64::from(x.to_bits()),
                            i64::from(y.to_bits()),
                            f64::from(x),
                            f64::from(y),
                        )
                    };
                    let taken = java_if_r11(if_op, java_fp_cmp_r11(fa, fb, nan_positive));
                    // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
                    let got = unsafe { whole_m.try_call(&[arg_a, arg_b]).expect("jit call") };
                    assert_eq!(
                        got,
                        if taken { 0 } else { 1 },
                        "cmp {cmp_op:#x} if {if_op:#x} a={a} b={b} (whole stack)"
                    );
                    // SAFETY: as above.
                    let got = unsafe { beneath_m.try_call(&[arg_a, arg_b]).expect("jit call") };
                    assert_eq!(
                        got,
                        if taken { 7 } else { 8 },
                        "cmp {cmp_op:#x} if {if_op:#x} a={a} b={b} (value beneath)"
                    );
                }
            }
        }
    }
}

#[test]
pub(in crate::x64::tests) fn test_compile_dcmpl_dcmpg() {
    // dload_0, dload_1, dcmpl (0x97), ireturn
    let dcmpl_code: Vec<u8> = vec![0x26, 0x27, 0x97, 0xac, 0, 0];
    let compiled = compile(
        &dcmpl_code,
        4,
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

    let a = 5.0f64.to_bits() as i64; // Cast: JIT ABI convention
    let b = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        1
    );

    let a = 3.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        0
    );

    let a = 1.0f64.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[a, b]).expect("test JIT call") },
        -1
    );

    // NaN → -1 (dcmpl)
    let nan = f64::NAN.to_bits() as i64; // Cast: JIT ABI convention
                                         // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                         // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        -1
    );

    // dcmpg: NaN → 1
    let dcmpg_code: Vec<u8> = vec![0x26, 0x27, 0x98, 0xac, 0, 0];
    let compiled = compile(
        &dcmpg_code,
        4,
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
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[nan, b]).expect("test JIT call") },
        1
    );
    assert_eq!(
        // SAFETY: the body was compiled by this test for exactly these argument kinds, and runs on this thread against the test's own live data and helper table.
        unsafe { compiled.try_call(&[b, nan]).expect("test JIT call") },
        1
    );
}

#[test]
pub(in crate::x64::tests) fn test_compile_float_chain() {
    // float f(float a, float b) { return (a + b) * a; }
    // fload_0, fload_1, fadd, fload_0, fmul, freturn
    let code: Vec<u8> = vec![0x22, 0x23, 0x62, 0x22, 0x6a, 0xae, 0, 0];
    let code_len = 6;

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
    let a = 3.0f32.to_bits() as i64; // Cast: JIT ABI convention
    let b = 2.0f32.to_bits() as i64; // Cast: JIT ABI convention
                                     // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
                                     // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[a, b]).expect("test JIT call") };
    // (3.0 + 2.0) * 3.0 = 15.0
    assert_eq!(f32::from_bits(result as u32), 15.0f32); // Cast: JIT ABI convention
}

#[test]
pub(in crate::x64::tests) fn test_compile_i2f_fadd_f2i() {
    // int f(int a, int b) { return (int)((float)a + (float)b); }
    // iload_0, i2f, iload_1, i2f, fadd, f2i, ireturn
    let code: Vec<u8> = vec![
        0x1a, // iload_0
        0x86, // i2f
        0x1b, // iload_1
        0x86, // i2f
        0x62, // fadd
        0x8b, // f2i
        0xac, // ireturn
        0, 0,
    ];
    let code_len = 7;

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
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[10, 20]).expect("test JIT call") };
    assert_eq!(result, 30);

    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe { compiled.try_call(&[7, -3]).expect("test JIT call") };
    assert_eq!(result, 4);
}

// -----------------------------------------------------------------------
// Round 17: getfield/putfield tests
// -----------------------------------------------------------------------

#[test]
pub(in crate::x64::tests) fn test_getfield_int() {
    // Method: int getX(Object this) { return this.x; }
    // Bytecode: aload_0, getfield #1, ireturn
    // We fake field #1 as field_index=0, type='I'
    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;

    // Compile with field_info: pc=1, field_index=0, type_tag='I'
    let field_info = vec![(1usize, 0usize, b'I')];
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
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

    // Create a heap object with one Int field
    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(42));

    // Call the JIT method: pass obj pointer as first arg
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, 42);

    // Test with negative value
    heap.set_field(obj, 0, Value::Int(-123));
    // SAFETY: Calling JIT-compiled machine code in a test; the CompiledMethod was
    // produced by the JIT compiler from valid bytecode and the mmap region is executable.
    let result = unsafe {
        compiled
            // Cast: object/array pointer to i64 for the JIT calling convention
            .try_call_with_context(test_vm_ctx(), &[obj.as_ptr() as i64])
            .expect("test JIT call")
    }; // Cast: JIT ABI convention
    assert_eq!(result, -123);
}

/// Guarded inline getfield (perf/throughput-20260710): with a non-zero
/// `read_bounds_addr` the default arm emits the inline receiver guard +
/// raw field load, falling back to the checked helper only for receivers
/// that fail the guard. Uses a test-local bounds table so the pass/fail
/// routing is deterministic (the process-global table is owned by
/// whichever real heap ran last and is zeroed on heap drop):
///
///   * receiver inside the published range → inline raw load (the marker
///     helper is NOT called);
///   * table zeroed / null receiver / unaligned receiver → the helper IS
///     called (asserted via a marker stub that returns a constant no raw
///     load could produce).
#[test]
pub(in crate::x64::tests) fn test_getfield_guarded_inline_fast_and_fallback() {
    // guarded_inline_getfield_enabled() is default-ON (see its doc
    // comment) -- no env var needed to exercise this path. If a test run
    // sets CRATONVM_JIT_GETFIELD_HELPER=1 to force the helper-only path
    // globally, this test's own assertions about the inline guard would
    // no longer hold; nothing here does that.
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TEST_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    /// Marker helper: returns a constant that a raw field-cell load of the
    /// test object can never produce, so routing is observable.
    unsafe extern "C" fn marker_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        424242
    }

    let code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let code_len = 5;
    let field_info = vec![(1usize, 0usize, b'I')];
    let mut helpers = test_helpers();
    helpers.getfield = marker_getfield as *const () as usize;
    // READ side: the guarded getfield arm bakes `read_bounds_addr`.
    helpers.read_bounds_addr = TEST_BOUNDS.as_ptr() as usize;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
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
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(),
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    heap.set_field(obj, 0, Value::Int(42));
    let obj_addr = obj.as_ptr() as usize;

    // 1. Publish a range covering the object → guard passes → inline raw
    //    load reads the real cell, the marker helper is NOT called.
    TEST_BOUNDS[0].store(obj_addr & !0xFFF, Ordering::Release);
    TEST_BOUNDS[1].store((obj_addr & !0xFFF) + 0x10000, Ordering::Release);
    // SAFETY: JIT-compiled code from valid bytecode; executable mmap region.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj_addr as i64])
            .expect("jit call")
    };
    assert_eq!(result, 42, "in-bounds receiver must take the inline path");

    // 2. Unaligned receiver (inside the range) → guard fails → helper.
    // SAFETY: as above; the guard rejects the pointer before any deref.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[(obj_addr + 1) as i64])
            .expect("jit call")
    };
    assert_eq!(
        result, 424242,
        "unaligned receiver must route to the helper"
    );

    // 3. Zero the table (what GenerationalHeap::drop does) → out-of-heap
    //    receiver → helper.
    TEST_BOUNDS[0].store(0, Ordering::Release);
    TEST_BOUNDS[1].store(0, Ordering::Release);
    // SAFETY: as above.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj_addr as i64])
            .expect("jit call")
    };
    assert_eq!(
        result, 424242,
        "receiver outside every published region must route to the helper"
    );

    // 4. Null receiver → helper (which owns the NPE semantics in prod).
    // SAFETY: as above.
    let result = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("jit call")
    };
    assert_eq!(result, 424242, "null receiver must route to the helper");
}

/// How many layout-replacement epoch guards a compiled body carries, counting
/// BOTH encodings.
///
/// The guard is `CMP DWORD [rip+disp32], imm32` where the counter is in
/// disp32 reach and `MOV R11, imm64` + `MOV ECX, [R11]` + `CMP ECX, imm32`
/// where it is not, and which one a given run gets is a property of the
/// process's address space — so a test that recognised only one of them would
/// pass or fail by luck. Both are matched by RESOLVING to the epoch's address,
/// which is also what makes this stronger than counting opcodes: a `81 3D`
/// naming some other global is not a guard.
pub(in crate::x64::tests) fn layout_epoch_guards(compiled: &CompiledMethod) -> usize {
    let (epoch, _) = cratonvm_types::layout_replace_epoch_guard();
    let epoch = epoch as usize; // Cast: the address the guard must name
    let code = compiled.code_bytes();
    let base = code.as_ptr() as usize; // Cast: buffer base for RIP resolution
    let mut n = 0usize;
    for i in 0..code.len() {
        // RIP form: 81 3D <disp32> <imm32>, measured from the END.
        if i + 10 <= code.len() && code[i] == 0x81 && code[i + 1] == 0x3D {
            let d = i32::from_le_bytes([code[i + 2], code[i + 3], code[i + 4], code[i + 5]]);
            // Cast: sign-extend the disp32 for wrapping address arithmetic
            if base
                .wrapping_add(i)
                .wrapping_add(10)
                .wrapping_add(d as isize as usize)
                == epoch
            {
                n += 1;
            }
        }
        // Far fallback: MOV R11, imm64 ; CMP DWORD [R11], imm32.
        if i + 17 <= code.len() && code[i] == 0x49 && code[i + 1] == 0xBB {
            let mut imm = [0u8; 8];
            imm.copy_from_slice(&code[i + 2..i + 10]);
            if u64::from_le_bytes(imm) as usize == epoch
                && code[i + 10..i + 13] == [0x41, 0x81, 0x3B]
            {
                n += 1;
            }
        }
    }
    n
}

/// Every single-pass emitter that BAKES a compact body offset must guard it
/// against a layout replacement — including the two the guard's own census
/// missed.
///
/// `51aee440b` set out to guard "every baked compact cell offset" and listed
/// five sites. The inline compact `getfield` — the hottest field path in the
/// VM — and the UNGATED compact reference `putfield` were not among them, and
/// went on baking `HEADER_SIZE + packed_body_offset` with nothing to stop them
/// using it after `register_class_layout` replaced that layout. The allocation
/// emitters' own comment for the same hazard is "confirmed heap corruption".
///
/// The failure mode this pins is a MISSING guard, not a broken one, so it is
/// worth saying what it deliberately does not do: it does not bump the epoch.
/// That the guard routes correctly when it fires is
/// `a_replaced_layout_routes_the_gated_store_to_the_helper`'s job, in the
/// optimizing tier, by execution; a second test bumping a process-wide counter
/// would race every sibling that compiled an inline compact arm before it.
#[test]
pub(in crate::x64::tests) fn every_single_pass_compact_field_site_guards_its_baked_offset() {
    if !jit_sp_field_layout_guard_enabled() {
        // This process asked for the unguarded shape, which is what that
        // switch is for. Pricing a guard requires being able to turn it off.
        return;
    }
    // ── the inline compact getfield ────────────────────────────────────
    // int get(Obj this) { return this.x; }
    let get_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0xb4, 0x00, 0x01, // 1: getfield #1
        0xac, // 4: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // The guarded arm bakes this; a zero table sends the compact arm's
    // admission test down the helper-only road and there would be nothing to
    // guard.
    static BOUNDS: [std::sync::atomic::AtomicUsize; 6] = [
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
        std::sync::atomic::AtomicUsize::new(0),
    ];
    helpers.read_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address
    let get = compile_with_request(
        BackendRequest::default(),
        vec![(1, 0, false)],
        &get_code,
        5,
        1,
        1,
        true, // needs_heap: the body loads the VM context (gch h3/fd, d1a)
        Vec::new(),
        vec![(1usize, 0usize, b'I')],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("a compact getfield must compile");
    assert_eq!(
        layout_epoch_guards(&get),
        1,
        "the inline compact getfield bakes a body offset and must guard it",
    );

    // ── the UNGATED compact reference putfield ─────────────────────────
    // void set(Obj this, Object v) { this.f = v; }
    //
    // `test_helpers()` publishes no reference-store barrier plan, so
    // `emit_gated_compact_ref_putfield` declines and this is the arm that
    // runs — the one that had no guard.
    let set_code: Vec<u8> = vec![
        0x2a, // 0: aload_0
        0x2b, // 1: aload_1
        0xb5, 0x00, 0x01, // 2: putfield #1 (reference)
        0xb1, // 5: return
        0, 0,
    ];
    let mut set_helpers = test_helpers();
    set_helpers.region_bounds_addr = BOUNDS.as_ptr() as usize; // Cast: static address
    let set = compile_with_request(
        BackendRequest::default(),
        vec![(2, 8, true)],
        &set_code,
        6,
        2,
        2,
        true, // needs_heap — the reference putfield helper takes it
        Vec::new(),
        vec![(2usize, 0usize, b'L')],
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(), // pic_slots — no PIC sites
        Vec::new(), // ldc_info
        Vec::new(), // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &set_helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("a compact reference putfield must compile");
    assert_eq!(
        layout_epoch_guards(&set),
        1,
        "the ungated inline compact reference putfield bakes a body offset \
         and must guard it",
    );
}

/// A REFERENCE field whose 16-byte cell does not hold a reference must not be
/// read as one by the inline arm.
///
/// This is the Tomcat/Derby SIGSEGV of 2026-08-23. The inline legacy `getfield`
/// loaded the cell's 8-byte payload and handed it on as a pointer without ever
/// looking at the cell's discriminant. `SQLChar.rawData` is declared `[C`, its
/// cell did not hold an `Object`, and the payload word — 1 — went straight into
/// the `arraylength` this arm emits three instructions later
/// (`MOV r32,[RAX+4]`), so the process died at `addr=0x5`. Three crashes
/// carried byte-identical registers: the body was deterministic, only its
/// reachability was racy (3 crashes in 38 runs one hour, 0 in 129 the next).
///
/// The checked helper has always looked at the variant, which is why the whole
/// thing was invisible from the helper's side. The inline arm exists to skip
/// the helper, so it has to ask the same question and defer when the answer is
/// no. The marker helper makes that deferral observable.
#[test]
pub(in crate::x64::tests) fn a_reference_getfield_does_not_read_a_non_reference_cell_as_a_pointer() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static TEST_BOUNDS: [AtomicUsize; 6] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    /// A value no cell read could produce, so "went to the helper" is visible.
    unsafe extern "C" fn marker_getfield(_vm: i64, _obj: i64, _idx: i64) -> i64 {
        424242
    }

    // aload_0; getfield #1; areturn
    let code: Vec<u8> = vec![0x2a, 0xb4, 0x00, 0x01, 0xb0, 0, 0];
    let code_len = 5;
    // Slot 1, declared `L` — a reference.
    let field_info = vec![(1usize, 1usize, b'L')];
    let mut helpers = test_helpers();
    helpers.getfield = marker_getfield as *const () as usize;
    helpers.read_bounds_addr = TEST_BOUNDS.as_ptr() as usize;
    let compiled = compile(
        &code,
        code_len,
        1,
        1,
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
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None,
    )
    .unwrap();

    use cratonvm_gc::gen_heap::GenerationalHeap;
    use cratonvm_types::ClassId;
    let heap = GenerationalHeap::new();
    let obj = heap.alloc_object(ClassId::new(0), 2);
    let obj_addr = obj.as_ptr() as usize;
    TEST_BOUNDS[0].store(obj_addr & !0xFFF, Ordering::Release);
    TEST_BOUNDS[1].store((obj_addr & !0xFFF) + 0x10000, Ordering::Release);

    // A genuine reference still takes the inline path: the whole sequence
    // exists to make this case fast, and a "fix" that sent it to the helper
    // would be a performance regression disguised as a correctness one.
    let target = heap.alloc_object(ClassId::new(0), 1);
    heap.set_field(obj, 1, Value::Object(Some(target)));
    // SAFETY: JIT-compiled code from valid bytecode; executable mmap region.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj_addr as i64])
            .expect("jit call")
    };
    assert_eq!(
        got,
        target.as_ptr() as i64,
        "a real reference must still be read inline"
    );

    // A PUNNED cell: the same slot now holds a `Long`, whose 8-byte payload
    // sits at exactly the offset the reference read uses. Pre-fix this
    // returned 0x1234_5678 — a pointer the caller would dereference. It must
    // now reach the helper, which owns the decision.
    heap.set_field(obj, 1, Value::Long(0x1234_5678));
    // SAFETY: as above.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj_addr as i64])
            .expect("jit call")
    };
    assert_eq!(
        got, 424242,
        "a reference load off a non-reference cell returned {got:#x} instead of \
         deferring — that word is what compiled code dereferences next"
    );

    // Null is a reference, and must NOT be pushed to the helper: `Object(None)`
    // is a legitimate inline answer and the common one.
    heap.set_field(obj, 1, Value::Object(None));
    // SAFETY: as above.
    let got = unsafe {
        compiled
            .try_call_with_context(test_vm_ctx(), &[obj_addr as i64])
            .expect("jit call")
    };
    assert_eq!(got, 0, "a null reference must still be read inline as 0");
}

/// Inline (helper-free) `getstatic`: with a resolver wired, the default `0xb2`
/// arm emits the two-load direct form and leaves NO call to `jit_getstatic`
/// behind; a site the resolver declines keeps the helper.
///
/// Both directions are asserted from one table: the test resolver answers for
/// exactly one `(class, field)` pair and declines everything else. The table
/// travels on this test's own compiles, so no other test sees it.
#[test]
pub(in crate::x64::tests) fn test_getstatic_inline_direct_load_and_fallback() {
    use std::sync::atomic::AtomicPtr;

    /// Marker helper: returns a constant no direct load of the block below
    /// could produce, so routing is observable.
    unsafe extern "C" fn marker_getstatic(_vm: i64, _cid: i64, _idx: i64) -> i64 {
        424_242
    }

    /// Stands in for `jit_resolve_static_base`. `ctx` IS the address of the
    /// base-pointer cell here, so the test needs no VM.
    unsafe extern "C" fn test_resolver(ctx: i64, class_id: i64, field_index: i64) -> i64 {
        if class_id == 0x5EED && field_index == 1 {
            ctx
        } else {
            0
        }
    }

    // A leaked statics block plus the `AtomicPtr` cell that names it — the same
    // two-level shape `StaticsIndex` publishes.
    static CELL_ADDR: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let cell_addr = *CELL_ADDR.get_or_init(|| {
        let block: &'static mut [Value] =
            Box::leak(vec![Value::Int(1), Value::Int(-7), Value::Int(2)].into_boxed_slice());
        let cell: &'static AtomicPtr<Value> =
            Box::leak(Box::new(AtomicPtr::new(block.as_mut_ptr())));
        // Cast: the address the backend bakes as an immediate.
        cell as *const AtomicPtr<Value> as usize
    });
    let direct_helpers = crate::DirectHelperTable {
        static_base_resolver: test_resolver as *const () as usize,
        static_base_resolver_ctx: cell_addr,
        ..crate::DirectHelperTable::EMPTY
    };

    // getstatic #1 ; ireturn
    let code: Vec<u8> = vec![0xb2, 0x00, 0x01, 0xac, 0, 0];
    let code_len = 4;
    let mut helpers = test_helpers();
    helpers.getstatic = marker_getstatic as *const () as usize;

    let build = |field_index: usize| {
        compile_with_request(
            BackendRequest {
                direct_helpers,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            &code,
            code_len,
            1,
            1,
            true,       // needs_heap: the body loads the VM context (gch h3/fd, d1a)
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            // static_field_info: (pc, class_id, field_index, type_tag, volatile)
            vec![(0usize, 0x5EEDu32, field_index, b'I', false)],
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };

    // 1. Resolved site → direct load of slot 1, sign-extended.
    let inlined = build(1);
    // SAFETY: JIT-compiled machine code from valid bytecode in an executable
    // mmap region, as in every other codegen test here.
    let v = unsafe {
        inlined
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("test JIT call")
    };
    assert_eq!(
        v, -7,
        "a resolved getstatic must read the block directly (MOVSXD of the Int payload)"
    );
    assert_eq!(
        calls_to(&inlined, helpers.getstatic),
        0,
        "an inlined getstatic must leave no CALL to jit_getstatic"
    );

    // 2. Declined site (field 0) → the helper still owns it.
    let fallback = build(0);
    // SAFETY: as above.
    let v = unsafe {
        fallback
            .try_call_with_context(test_vm_ctx(), &[0])
            .expect("test JIT call")
    };
    assert_eq!(
        v, 424_242,
        "a site the resolver declines must keep the jit_getstatic path"
    );
}

/// Round 16 (lane fieldguard): with `CRATONVM_JIT_INLINE_PUTSTATIC=1` and a
/// resolver that answers, the single-pass `0xb3` arm stores a primitive static
/// directly -- the cell's two words are the ones `write_value_atomic` (the
/// helper's writer) leaves, a sub-int value is narrowed, and no CALL to
/// `jit_putstatic_*` remains. With the switch off the helper call is
/// unchanged.
/// An OSR-exit map belongs at a loop header — at EVERY bytecode boundary it is
/// what made OSR impossible.
///
/// `deopt-osr` Step 7 emitted one per non-dead pc, so a method's `deopt_points`
/// held a snapshot taken mid-expression, with a partially-built operand stack.
/// `CompiledMethod::osr_exit_policy` is an artifact-wide veto — one unresumable
/// point refuses OSR entry at every pc — and a mid-expression stack entry is
/// unresumable in any method that touches a `long`/`float`/`double`, because
/// the operand stack has no per-entry width source there. Net effect: every
/// counted loop in such a method was refused `osr-entry-unresumable-exit`, and
/// a once-invoked method with a hot loop never left the interpreter (~90 ns/op
/// vs ~1.6 compiled — `probes/StaticFieldProbe.java` measured the interpreter
/// for two full rounds of a known-issue doc because of it).
///
/// The loop below is deliberately the simplest counted shape; the assertion is
/// that exactly ONE map is recorded, at the back-edge target.
#[test]
pub(in crate::x64::tests) fn osr_exit_maps_are_emitted_at_loop_headers_only() {
    // 0: iconst_0        1: istore_0
    // 2: iload_0   <-- back-edge target (the only loop header)
    // 3: iconst_1        4: iadd         5: istore_0
    // 6: goto -4  (to 2)
    // 9: return
    let code: Vec<u8> = vec![
        0x03, 0x3b, 0x1a, 0x04, 0x60, 0x3b, 0xa7, 0xff, 0xfc, 0xb1, 0, 0,
    ];
    let code_len = 10;
    let helpers = test_helpers();
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
        // `CRATONVM_DEOPT_REAL=0` builds no OSR-exit metadata at all; the
        // assertion below would then pass vacuously, so say so instead.
        assert!(compiled.osr_exit_points.is_empty());
        return;
    }
    assert_eq!(
        compiled.osr_exit_points,
        vec![2],
        "exactly one OSR-exit map, at the back-edge target — not one per pc"
    );
}

/// Interpreter round i1 wave 12, lane L4
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`): an OSR-tier body's
/// `goto` back-edge poll leaves through the loop header's OSR-exit map when the
/// poll's slow path answers non-zero, carrying the loop state the body had
/// reached. With a zero verdict, or in a body not compiled for the OSR tier,
/// the same verdict is ignored and the loop runs to completion.
#[test]
pub(in crate::x64::tests) fn a_nonzero_back_edge_poll_verdict_leaves_an_osr_body_at_its_loop_header() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        // No OSR-exit maps, or no polls: nothing to leave through.
        return;
    }
    // The stop-the-world byte, always raised, so every poll takes its slow path.
    static FLAG: u8 = 1;
    static VERDICT: AtomicI64 = AtomicI64::new(0);
    extern "C" fn verdict_poll(_flag_addr: i64) -> i64 {
        VERDICT.load(Ordering::SeqCst)
    }
    // int i = 0; while (i < 1000) { i++; } return i;  -- javac's `goto` shape.
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
        0x84, 0x00, 0x01, // 9:  iinc 0, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1a,             // 15: iload_0
        0xac,             // 16: ireturn
        0, 0,
    ];
    let code_len = 17;
    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = verdict_poll as *const () as usize; // Cast: helper address
    let build = |mode_exit_polls: bool| {
        compile_with_request(
            BackendRequest {
                mode_exit_polls,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            &code,
            code_len,
            0,
            1,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };
    let osr_tier = build(true);
    let method_entry = build(false);
    assert!(
        osr_tier.osr_exit_points.contains(&2),
        "precondition: the loop header carries an OSR-exit map"
    );
    let _ = crate::deopt::take_last_deopt_with_point();

    // Verdict 0: every poll runs its slow path and the loop completes.
    VERDICT.store(0, Ordering::SeqCst);
    // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
    assert_eq!(unsafe { osr_tier.try_call(&[]) }, Ok(1000));
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // Verdict 1 in a body not compiled for the OSR tier: ignored.
    VERDICT.store(1, Ordering::SeqCst);
    // SAFETY: as above.
    assert_eq!(unsafe { method_entry.try_call(&[]) }, Ok(1000));
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // Verdict 1 in the OSR-tier body: it leaves at the first back edge,
    // through the header's own `OsrExit` point, mid-loop.
    // SAFETY: as above.
    let raw = unsafe { osr_tier.try_call(&[]) };
    VERDICT.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2, "resumes at the loop header");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(
        osr_tier.owns_deopt_point(point),
        "fired by one of this artifact's own points"
    );
    assert!(frame.stack.is_empty());
    match frame.locals.first() {
        Some(FrameValue::Int(i)) => assert!(
            (1..1000).contains(i),
            "the loop state the body had reached, mid-loop: i = {i}"
        ),
        other => panic!("local 0 (the counter) reconstructed as {other:?}"),
    }
}

/// Interpreter round i1 wave 25, lane L6
/// (`interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`):
/// the real single-pass emitter records its exit-capable back-edge poll
/// (`CompiledMethod::exit_poll_sites`) at the byte it wrote, and forcing it
/// (`CompiledMethod::force_exit_polls`, what a class redefinition does to a
/// body it withdrew) makes that poll run its slow path with the
/// stop-the-world flag CLEAR: once per back edge on a zero verdict, and out
/// through the header's exit map at the first back edge on a non-zero one --
/// which is how a frame whose thread was inside a call during the
/// redefinition's handshake leaves once the call returns.
#[test]
pub(in crate::x64::tests) fn a_forced_exit_poll_asks_the_slow_path_with_the_flag_clear() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet): the slow path reaches its
    // state through the flag address every poll passes it.
    #[repr(C)]
    struct PollState {
        flag: u8,
        calls: AtomicU64,
        verdict: AtomicI64,
    }
    extern "C" fn counting_poll(flag_addr: i64, _body_id: i64) -> i64 {
        // SAFETY: every poll of this test's body passes the address of
        // `PollState::flag`, the first field of the leaked `#[repr(C)]` state
        // below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const PollState) };
        state.calls.fetch_add(1, Ordering::SeqCst);
        state.verdict.load(Ordering::SeqCst)
    }
    let state: &'static PollState = Box::leak(Box::new(PollState {
        flag: 0,
        calls: AtomicU64::new(0),
        verdict: AtomicI64::new(0),
    }));
    // int i = 0; while (i < 1000) { i++; } return i;  -- javac's `goto` shape.
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
        0x84, 0x00, 0x01, // 9:  iinc 0, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1a,             // 15: iload_0
        0xac,             // 16: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = counting_poll as *const () as usize;
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        17,
        0,
        1,
        false,
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        Vec::new(), // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host (the inline frame record is
        // off): the poll cannot name its body, so nothing is recorded.
        assert!(body.exit_poll_sites.is_empty());
        return;
    }
    assert!(
        body.osr_exit_points.contains(&2),
        "precondition: the loop header carries an OSR-exit map"
    );
    assert_eq!(
        body.exit_poll_sites.len(),
        1,
        "the goto's poll, and only it"
    );
    let _ = crate::deopt::take_last_deopt_with_point();

    // Not forced: the flag is clear, so no poll runs its slow path.
    // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
    assert_eq!(unsafe { body.try_call(&[]) }, Ok(1000));
    assert_eq!(state.calls.load(Ordering::SeqCst), 0);

    assert_eq!(body.force_exit_polls(), Ok(1));
    assert_eq!(body.force_exit_polls(), Ok(0), "forced once");

    // Forced, verdict 0: every back edge asks, and the loop completes.
    // SAFETY: as above.
    assert_eq!(unsafe { body.try_call(&[]) }, Ok(1000));
    // One slow path per EXECUTED back edge. The backend may unroll this
    // empty counted loop (the wave-25 host run took 250 back edges, four
    // iterations each), so the count divides 1000 rather than equals it.
    let calls = state.calls.load(Ordering::SeqCst);
    assert!(
        calls > 0 && calls <= 1000 && 1000 % calls == 0,
        "one slow path per back edge, the flag still clear: {calls}"
    );
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // Forced, verdict 1: the body leaves at its first back edge.
    state.verdict.store(1, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call(&[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2, "resumes at the loop header");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    // The iterations one back edge closes: one, or the unroll factor when the
    // backend unrolled the loop (see the count above).
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(n)) if *n >= 1 && 1000 % *n == 0),
        "one back edge's iterations ran: {:?}",
        frame.locals.first()
    );
}

/// The exit-poll shape check (`not_entrant::forced_exit_poll_cc`) recognises
/// each of the three poll forms the two tiers emit against the recorded flag,
/// and nothing else (interpreter round i1 wave 25, lane L6).
#[test]
pub(in crate::x64::tests) fn the_exit_poll_shape_check_accepts_the_three_poll_forms_only() {
    use crate::not_entrant::forced_exit_poll_cc;
    let flag = 0x7000_0000usize;
    let at = 0x1000usize;
    // RIP form: F6 05 <disp32> FF, ending at offset 7, then 0F 84.
    let disp = i32::try_from(flag - (at + 7)).expect("in reach");
    let mut rip = vec![0xF6, 0x05];
    rip.extend_from_slice(&disp.to_le_bytes());
    rip.extend_from_slice(&[0xFF, 0x0F, 0x84, 0, 0, 0, 0]);
    assert_eq!(forced_exit_poll_cc(&rip, at, 8, flag), Some(0x80));
    assert_eq!(
        forced_exit_poll_cc(&rip, at, 8, flag + 1),
        None,
        "another flag"
    );
    // The single-pass R11 form, and the IR tier's outlined JNZ after its own.
    let sp = [0x41, 0xF6, 0x43, 0x00, 0xFF, 0x0F, 0x84, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&sp, at, 6, flag), Some(0x80));
    let ir = [0x41, 0xF6, 0x03, 0xFF, 0x0F, 0x85, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&ir, at, 5, flag), Some(0x81));
    // A JZ after another instruction, and an offset off by one.
    let other = [0x48, 0x85, 0xC0, 0xFF, 0x0F, 0x84, 0, 0, 0, 0];
    assert_eq!(forced_exit_poll_cc(&other, at, 5, flag), None);
    assert_eq!(forced_exit_poll_cc(&sp, at, 5, flag), None);
}

/// The post-call exit site's shape check (`not_entrant::post_call_exit_jump`,
/// interpreter round i1 wave 27, lane L6): the unwritten five-byte `NOP`,
/// inside one 8-aligned word of the emitted code, with the stub's `CALL`
/// past it -- and the `JMP rel32` it becomes lands on that stub.
#[test]
pub(in crate::x64::tests) fn the_post_call_exit_shape_check_wants_an_aligned_unwritten_nop() {
    use crate::not_entrant::post_call_exit_jump;
    let base = 0x1000usize;
    // 0..3 filler, 3..8 the NOP (offset 3: bytes 3..8 are one aligned word's
    // tail), 8..16 filler, 16 the stub's CALL.
    let mut code = vec![0x90u8; 24];
    code[3..8].copy_from_slice(&crate::ENTRY_PATCH_PAD);
    code[16] = 0xE8;
    let jmp = post_call_exit_jump(&code, base, 3, 16).expect("an admissible site");
    assert_eq!(jmp[0], 0xE9);
    assert_eq!(i32::from_le_bytes([jmp[1], jmp[2], jmp[3], jmp[4]]), 16 - 8);
    // At offset 4 the five bytes cross into the next word.
    let mut straddles = vec![0x90u8; 24];
    straddles[4..9].copy_from_slice(&crate::ENTRY_PATCH_PAD);
    straddles[16] = 0xE8;
    assert_eq!(post_call_exit_jump(&straddles, base, 4, 16), None);
    // Already written, a stub that is not the CALL, a stub before the site.
    let mut written = code.clone();
    written[3] = 0xE9;
    assert_eq!(post_call_exit_jump(&written, base, 3, 16), None);
    assert_eq!(post_call_exit_jump(&code, base, 3, 15), None);
    assert_eq!(post_call_exit_jump(&code, base, 3, 2), None);
    // A site at the very start: admissible when the code is 8-aligned, not
    // when its word would start before the code.
    let mut at_start = vec![0x90u8; 24];
    at_start[..5].copy_from_slice(&crate::ENTRY_PATCH_PAD);
    at_start[16] = 0xE8;
    assert!(post_call_exit_jump(&at_start, base, 0, 16).is_some());
    assert_eq!(post_call_exit_jump(&at_start, base + 2, 0, 16), None);
}

/// Interpreter round i1 wave 27, lane L6
/// (`i26-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies`, stages
/// 1-2): an OSR-tier compile puts a patchable `NOP` after a real call whose
/// successor an exit map can describe. Unforced it asks nothing; forced (the
/// same pass that forces the exit polls) it asks the slow path for the
/// verdict ALONE (`POST_CALL_EXIT_VERDICT_ONLY`) each time the call returns,
/// and on a non-zero one leaves through the SUCCESSOR's map with the frame as
/// the call left it -- before the rest of the iteration runs, which is where
/// `RedefineSpliceAfterTheCallProbe`'s read of a spliced callee sits.
#[test]
pub(in crate::x64::tests) fn a_forced_post_call_exit_leaves_at_the_calls_successor() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet): the slow path reaches its
    // state through the flag address every caller passes it.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        polls: AtomicU64,
        post_calls: AtomicU64,
        verdict: AtomicI64,
    }
    extern "C" fn counting_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.post_calls.fetch_add(1, Ordering::SeqCst);
            state.verdict.load(Ordering::SeqCst)
        } else {
            // A forced back-edge poll: stay, so only the post-call exit leaves.
            state.polls.fetch_add(1, Ordering::SeqCst);
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        polls: AtomicU64::new(0),
        post_calls: AtomicU64::new(0),
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // static int f() { int i = 0; while (i < 1000) { tick(); i++; } return i; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x0c, // 6:  if_icmpge +12 -> 18
        0xb8, 0x00, 0x01, // 9:  invokestatic tick()V (the dispatch helper)
        0x84, 0x00, 0x01, // 12: iinc 0, 1           <- the call's successor
        0xa7, 0xff, 0xf3, // 15: goto -13 -> 2
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = counting_slow_path as *const () as usize;
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        20,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        vec![(9usize, info as *const JitInvokeInfo)], // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: nothing can name the body to
        // the verdict, so no site is emitted.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call's site, and only it"
    );
    assert!(
        body.osr_exit_points.contains(&12),
        "the site's map is filed at the call's successor"
    );
    let (site, stub) = body.post_call_exit_sites[0];
    // Cast: buffer offsets, widened.
    let (site, stub) = (site as usize, stub as usize);
    assert_eq!(
        &body._buffer.as_slice()[site..site + 5],
        &crate::ENTRY_PATCH_PAD,
        "the fast path is the NOP"
    );
    let _ = crate::deopt::take_last_deopt_with_point();

    // Not forced: the flag is clear and the site is a NOP.
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(1000));
    assert_eq!(state.post_calls.load(Ordering::SeqCst), 0);
    assert_eq!(state.polls.load(Ordering::SeqCst), 0);

    let forced = body.force_exit_polls();
    assert_eq!(
        forced,
        Ok(body.exit_poll_sites.len() + 1),
        "every poll and the site"
    );
    assert_eq!(body.force_exit_polls(), Ok(0), "forced once");
    let bytes = body._buffer.as_slice();
    assert_eq!(bytes[site], 0xE9, "the site is a JMP now");
    let rel = i32::from_le_bytes([
        bytes[site + 1],
        bytes[site + 2],
        bytes[site + 3],
        bytes[site + 4],
    ]);
    // Cast: a buffer offset and a signed displacement, for the target.
    assert_eq!(
        (site as i64) + 5 + i64::from(rel),
        stub as i64,
        "to its stub"
    );

    // Forced, verdict 0: every return from the call asks, and the loop
    // completes compiled.
    // SAFETY: as above.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(1000));
    let asked = state.post_calls.load(Ordering::SeqCst);
    assert!(
        asked > 0 && asked <= 1000,
        "one verdict per return through the recorded site: {asked}"
    );
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // Forced, a leave verdict: out at the first call's return, at bci 12,
    // with `i` still 0 -- the `iinc` after the call has not run.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 12, "resumes at the call's successor");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert!(
        frame.stack.is_empty(),
        "a void call leaves nothing on the stack"
    );
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "the first call's return, before its iteration's iinc: {:?}",
        frame.locals.first()
    );
}

/// Interpreter round i1 wave 47, lane L2
/// (`interpreter-L2-a-post-call-exit-before-a-rotated-loops-entry-goto-shares-its-bci-with-the-pre-header-guards`):
/// a call whose successor is a rotated loop's entry `goto`, where the walk
/// files the loop's relocatable pre-header guards. Wave 47 refused the site
/// by prediction; since wave 48 the claim decides by what was filed at the
/// `goto` (the predicate and its switch were deleted in interpreter round i2
/// wave 62), so the bci never carries two non-rethrow reasons.
#[test]
pub(in crate::x64::tests) fn a_call_before_a_rotated_loops_entry_goto_gets_no_post_call_exit() {
    use super::escape_analysis::{find_rotation_preheader_entries, rotated_preheader_enabled};
    if !crate::deopt_real_enabled()
        || !jit_safepoint_polls_enabled()
        || !rotated_preheader_enabled()
    {
        return;
    }
    extern "C" fn stay(_flag_addr: i64, _body_id: i64) -> i64 {
        0
    }
    // No `static` (the crate's statics ratchet): the flag is leaked.
    // LEAK(intentional): the compiled code bakes the flag's address.
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // int i = 0; tick(); goto COND; BODY: i++; COND: if (i < 1000) goto BODY;
    // return i;
    #[rustfmt::skip]
    let rotated: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0xb8, 0x00, 0x01, // 2:  invokestatic tick()V
        0xa7, 0x00, 0x06, // 5:  goto +6 -> 11     <- the call's successor
        0x84, 0x00, 0x01, // 8:  iinc 0, 1         <- loop header
        0x1a,             // 11: iload_0
        0x11, 0x03, 0xe8, // 12: sipush 1000
        0xa1, 0xff, 0xf9, // 15: if_icmplt -7 -> 8
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    // The control: a `nop` after the call, then the same rotated loop.
    #[rustfmt::skip]
    let control: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0xb8, 0x00, 0x01, // 2:  invokestatic tick()V
        0x00,             // 5:  nop               <- the call's successor
        0xa7, 0x00, 0x06, // 6:  goto +6 -> 12
        0x84, 0x00, 0x01, // 9:  iinc 0, 1         <- loop header
        0x1a,             // 12: iload_0
        0x11, 0x03, 0xe8, // 13: sipush 1000
        0xa1, 0xff, 0xf9, // 16: if_icmplt -7 -> 9
        0x1a,             // 19: iload_0
        0xac,             // 20: ireturn
        0, 0,
    ];
    // The precondition: the driver reads both `goto`s as rotated entries.
    let loops = detect_loops(&rotated, 20);
    assert_eq!(
        find_rotation_preheader_entries(&rotated, 20, &loops, &[]).get(&8),
        Some(&5),
        "the goto at 5 enters the rotated header at 8"
    );
    let loops = detect_loops(&control, 21);
    assert_eq!(
        find_rotation_preheader_entries(&control, 21, &loops, &[]).get(&9),
        Some(&6),
        "the goto at 6 enters the rotated header at 9"
    );
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = stay as *const () as usize;
    let build = |code: &[u8], code_len: usize, call_pc: usize| {
        compile_with_request(
            BackendRequest {
                mode_exit_polls: true,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            code,
            code_len,
            0,
            1,
            true,       // needs_heap: the dispatch helper takes the context
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            vec![(call_pc, info as *const JitInvokeInfo)], // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };
    let body = build(&rotated, 20, 2);
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    // Wave 48: the site is decided by what the walk filed at the `goto`, not
    // predicted. This loop reads no array or field, so its pre-header files
    // no guard there and the claim keeps its own map; whatever was filed, the
    // bci carries one non-rethrow reason.
    assert!(
        !matches!(
            crate::osr_exit::deopt_reason_at_bci(&body.deopt_points, 5),
            crate::osr_exit::ReasonAtBci::Ambiguous
        ),
        "one reason at the goto's bci"
    );
    assert!(body.post_call_exit_sites.len() <= 1);
    assert_eq!(
        body.post_call_exit_sites.len(),
        body.osr_exit_points.iter().filter(|&&b| b == 5).count(),
        "a kept site has its map at the goto, a dropped one none: {:?}",
        body.osr_exit_points
    );
    // The control (a `nop` between the call and the `goto`) gets its site,
    // with the map at the `nop`.
    let control_body = build(&control, 21, 2);
    assert_eq!(
        control_body.post_call_exit_sites.len(),
        1,
        "the control's call gets its site"
    );
    assert!(
        control_body.osr_exit_points.contains(&5),
        "the control's map is at the nop: {:?}",
        control_body.osr_exit_points
    );
}

/// Interpreter round i1 wave 47, lane L2: in `do { i++; } while (f() <
/// 1000)` the call's successor is the constant the walk fuses with the
/// backward `if_icmplt`, whose conditional back edge files its exit map at
/// that constant's bci. The post-call site used to take the bci first, and
/// the back-edge poll then kept no exit. Wave 47 refused the site by
/// prediction; since wave 48 its claim SHARES the back edge's map (the
/// prediction was deleted in interpreter round i2 wave 62), and the
/// back-edge poll owns the map (a poll that may leave).
#[test]
pub(in crate::x64::tests) fn a_call_before_a_fused_backward_compare_leaves_the_bci_to_the_back_edge() {
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    extern "C" fn stay(_flag_addr: i64, _body_id: i64) -> i64 {
        0
    }
    // No `static` (the crate's statics ratchet): the flag is leaked.
    // LEAK(intentional): the compiled code bakes the flag's address.
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "f",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // int i = 0; do { i++; } while (f() < 1000); return i;
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x84, 0x00, 0x01, // 2:  iinc 0, 1          <- loop header
        0xb8, 0x00, 0x01, // 5:  invokestatic f()I
        0x11, 0x03, 0xe8, // 8:  sipush 1000        <- the call's successor
        0xa1, 0xff, 0xf7, // 11: if_icmplt -9 -> 2
        0x1a,             // 14: iload_0
        0xac,             // 15: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = stay as *const () as usize;
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        16,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        vec![(5usize, info as *const JitInvokeInfo)], // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no forceable site either way.
        return;
    }
    // Wave 48: the site's claim shares the back edge's map, which describes
    // the same frame -- never a second map there (the run is in
    // `claimed_post_call_sites_admit_a_branch_and_a_call_successor`).
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call's site, sharing the map: {:?}",
        body.post_call_exit_sites
    );
    assert_eq!(
        body.osr_exit_points.iter().filter(|&&b| b == 8).count(),
        1,
        "the back edge's map, and only it, is at the constant's bci: {:?}",
        body.osr_exit_points
    );
    assert_eq!(
        body.exit_poll_sites.len(),
        1,
        "the conditional back edge's poll may leave"
    );
}

/// Interpreter round i1 wave 48, lane L2
/// (`interpreter-L2-proposal-post-call-exit-maps-are-filed-by-the-successors-own-arm-FIXED-20261012`,
/// `op_invoke.rs::PostCallExitClaim`): with claims, a call whose
/// successor is a FORWARD conditional branch (refused by wave 47's
/// prediction, since a branch may file a back-edge map at its bci) gets its
/// site, because nothing was filed there; and a call whose successor is
/// another call through the dispatch helper YIELDS the bci to that call's
/// own `REEXECUTE` snapshot, decided by what the arm filed (item 2 of the
/// i24-L6 page's "Progress (wave 40)"). Forced with a leave verdict, the body leaves at the
/// branch's own bci, before it runs, with the call's result as the one
/// operand -- the `iinc` it would have skipped or taken has not run.
#[test]
pub(in crate::x64::tests) fn claimed_post_call_sites_admit_a_branch_and_a_call_successor() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `a_forced_post_call_exit_leaves_at_the_calls_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        verdict: AtomicI64,
    }
    extern "C" fn verdict_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's bodies passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let f_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "f",
        descriptor: "()I",
        num_jit_args: 0,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let tick_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = verdict_slow_path as *const () as usize;
    let build = |code: &[u8], code_len: usize, invokes: Vec<(usize, *const JitInvokeInfo)>| {
        compile_with_request(
            BackendRequest {
                mode_exit_polls: true,
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            code,
            code_len,
            0,
            1,
            true,       // needs_heap: the dispatch helper takes the context
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            invokes,    // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };

    // static int b() { int i = 0; while (i < 1000) { if (f() != 0) i += 2; i++; } return i; }
    #[rustfmt::skip]
    let branch: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x12, // 6:  if_icmpge +18 -> 24
        0xb8, 0x00, 0x01, // 9:  invokestatic f()I
        0x99, 0x00, 0x06, // 12: ifeq +6 -> 18       <- the call's successor
        0x84, 0x00, 0x02, // 15: iinc 0, 2 (skipped: f() is 0)
        0x84, 0x00, 0x01, // 18: iinc 0, 1
        0xa7, 0xff, 0xed, // 21: goto -19 -> 2
        0x1a,             // 24: iload_0
        0xac,             // 25: ireturn
        0, 0,
    ];
    let body = build(&branch, 26, vec![(9usize, f_info as *const JitInvokeInfo)]);
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call before the forward ifeq gets its site"
    );
    assert!(
        body.osr_exit_points.contains(&12),
        "its map at the branch's bci: {:?}",
        body.osr_exit_points
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 1));
    // Forced, verdict 0: the loop completes compiled. `f` returns 0 through
    // the test dispatch helper, so `ifeq` skips the `iinc 0, 2` and every
    // iteration adds one: exactly 1000 -- a stay through the site's stub
    // (whose verdict tail clobbers the flags) that misdirected the branch
    // would add three and end at 1002.
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    let stays = unsafe { body.try_call_with_context(0, &[]) };
    assert_eq!(stays, Ok(1000), "completes compiled, the branch unharmed");
    assert!(crate::deopt::take_last_deopt_with_point().is_none());
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 12, "resumes at the branch, before it runs");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert_eq!(
        frame.stack.len(),
        1,
        "the call's result is the branch's operand"
    );
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "the first call's return, before either iinc: {:?}",
        frame.locals.first()
    );

    // static int c() { int i = 0; while (i < 1000) { tick(); tick(); i++; } return i; }
    #[rustfmt::skip]
    let calls: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x0f, // 6:  if_icmpge +15 -> 21
        0xb8, 0x00, 0x01, // 9:  invokestatic tick()V
        0xb8, 0x00, 0x01, // 12: invokestatic tick()V <- the first call's successor
        0x84, 0x00, 0x01, // 15: iinc 0, 1            <- the second's
        0xa7, 0xff, 0xf0, // 18: goto -16 -> 2
        0x1a,             // 21: iload_0
        0xac,             // 22: ireturn
        0, 0,
    ];
    let body = build(
        &calls,
        23,
        vec![
            (9usize, tick_info as *const JitInvokeInfo),
            (12usize, tick_info as *const JitInvokeInfo),
        ],
    );
    // The second call goes through the dispatch helper, whose arm files its
    // own `REEXECUTE` snapshot at its bci (`snapshot_pre_intrinsic_call`):
    // the first call's claim yields that bci (a second reason there would
    // make the reason lookup ambiguous) and keeps no site. A SPLICED
    // successor files no such point (`emit_inline_invoke_into_rax` omits the
    // snapshot), which is the shape the claim is for. The second call's
    // successor, the `iinc`, keeps its site.
    //
    // Wave 49 (`osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`): the snapshot is a
    // `REEXECUTE` guard of the claim's own frame, so the claim files its map
    // beside it (outcome `Own`) and both calls keep a site; the by-bci
    // reason lookup names the guard there, never `Ambiguous`. The run is in
    // `a_post_call_exit_beside_the_next_calls_snapshot_leaves_before_it`.
    if crate::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS {
        assert_eq!(
            body.post_call_exit_sites.len(),
            2,
            "both calls' sites; the first one's map sits beside the snapshot"
        );
        assert!(
            body.osr_exit_points.contains(&12) && body.osr_exit_points.contains(&15),
            "a map at the second call and at the iinc: {:?}",
            body.osr_exit_points
        );
        assert_eq!(
            crate::osr_exit::deopt_reason_at_bci(&body.deopt_points, 12),
            crate::osr_exit::ReasonAtBci::Unique(DeoptReason::ReceiverTypeChanged),
            "the guard is what a cause-less trap at bci 12 is charged"
        );
    } else {
        assert_eq!(
            body.post_call_exit_sites.len(),
            1,
            "the second call's site; the first one's claim yielded"
        );
        assert!(
            !body.osr_exit_points.contains(&12) && body.osr_exit_points.contains(&15),
            "the map at the iinc only: {:?}",
            body.osr_exit_points
        );
    }
    for bci in [12u32, 15] {
        assert!(
            !matches!(
                crate::osr_exit::deopt_reason_at_bci(&body.deopt_points, bci),
                crate::osr_exit::ReasonAtBci::Ambiguous
            ),
            "one reason at bci {bci}"
        );
    }

    // int i = 0; do { i++; } while (f() < 1000); return i; -- the call's
    // successor is the constant the walk fuses with the backward
    // `if_icmplt`, whose back edge files its `OsrExit` map at that bci with
    // the call's result as the operand: the claim SHARES it (wave 47 refused
    // the site there). Forced, the site leaves through that one map.
    #[rustfmt::skip]
    let fused: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x84, 0x00, 0x01, // 2:  iinc 0, 1          <- loop header
        0xb8, 0x00, 0x01, // 5:  invokestatic f()I
        0x11, 0x03, 0xe8, // 8:  sipush 1000        <- the call's successor
        0xa1, 0xff, 0xf7, // 11: if_icmplt -9 -> 2
        0x1a,             // 14: iload_0
        0xac,             // 15: ireturn
        0, 0,
    ];
    let body = build(&fused, 16, vec![(5usize, f_info as *const JitInvokeInfo)]);
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call's site, sharing the map"
    );
    assert_eq!(
        body.osr_exit_points.iter().filter(|&&b| b == 8).count(),
        1,
        "one map at the constant: {:?}",
        body.osr_exit_points
    );
    assert_eq!(
        body.exit_poll_sites.len(),
        1,
        "the back edge keeps its exit"
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 2));
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 8, "resumes at the constant, before the compare");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert_eq!(frame.stack.len(), 1, "the call's result");
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(1))),
        "after the first iteration's iinc: {:?}",
        frame.locals.first()
    );
}

/// Interpreter round i2 wave 60, lane L2 (item 2 of the i24-L6 page,
/// `op_invoke::post_call_exits_at_forward_joins_enabled`): which branch
/// targets a post-call exit claim may name. A forward-only join may; a loop
/// header (a back edge's target), pc 0 and a handler entry may not.
#[test]
pub(in crate::x64::tests) fn only_a_forward_only_join_is_a_post_call_claims_branch_target() {
    // static void j() { int i = 0; while (i < 1000) { if (i >= 0) { tick(); } i++; } }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header (back edge from 19)
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x10, // 6:  if_icmpge +16 -> 22
        0x1a,             // 9:  iload_0
        0x9b, 0x00, 0x06, // 10: iflt +6 -> 16
        0xb8, 0x00, 0x01, // 13: invokestatic tick()V
        0x84, 0x00, 0x01, // 16: iinc 0, 1           <- forward-only join
        0xa7, 0xff, 0xef, // 19: goto -17 -> 2
        0xb1,             // 22: return
    ];
    let len = code.len();
    let mut targets = crate::bytecode_analysis::branch_target_map(&code, len);
    targets.resize(len + 1, false);
    targets[0] = true;
    let none = rustc_hash::FxHashSet::default();
    let joins = super::op_invoke::forward_only_joins(&code, len, &targets, &[], &none);
    assert!(joins[16], "the if's join");
    assert!(!joins[2], "the loop header");
    assert!(joins[22], "the loop's exit, a forward target too");
    assert!(
        !joins[0] && !joins[13],
        "the entry, and a pc no branch names"
    );
    let headers: rustc_hash::FxHashSet<usize> = [16usize].into_iter().collect();
    let joins = super::op_invoke::forward_only_joins(&code, len, &targets, &[16], &none);
    // 2026-10-07 (item 2 of the i24-L6 page): a local handler's entry is a
    // join too -- its implicit edge never passes the claim's `NOP` --, under
    // `POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES`.
    assert_eq!(
        joins[16],
        super::op_invoke::POST_CALL_EXIT_CLAIMS_AT_HANDLER_ENTRIES,
        "a handler entry, reached by its exception edge and the fall-through"
    );
    let joins = super::op_invoke::forward_only_joins(&code, len, &targets, &[2], &none);
    assert!(!joins[2], "a handler entry that is also a loop header");
    let joins = super::op_invoke::forward_only_joins(&code, len, &targets, &[], &headers);
    assert!(!joins[16], "a loop-boundary map's bci");
}

/// Interpreter round i2 wave 60, lane L2 (item 2 of the i24-L6 page): a call
/// that ends an `if` arm with no `else` has the JOIN as its successor. Under
/// `CRATONVM_JIT_POST_CALL_EXITS_AT_FORWARD_JOINS` its claim keeps a site
/// whose map sits at the join; forced with a leave verdict, the body leaves
/// there, before the `iinc` runs. With the switch off the join keeps no site.
#[test]
pub(in crate::x64::tests) fn a_call_ending_an_if_arm_gets_a_post_call_exit_at_the_join() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `claimed_post_call_sites_admit_a_branch_and_a_call_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        verdict: AtomicI64,
    }
    extern "C" fn verdict_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let tick_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = verdict_slow_path as *const () as usize;
    // static int j() { int i = 0; while (i < 1000) { if (i >= 0) { tick(); } i++; } return i; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x10, // 6:  if_icmpge +16 -> 22
        0x1a,             // 9:  iload_0
        0x9b, 0x00, 0x06, // 10: iflt +6 -> 16
        0xb8, 0x00, 0x01, // 13: invokestatic tick()V
        0x84, 0x00, 0x01, // 16: iinc 0, 1           <- the join: the call's successor
        0xa7, 0xff, 0xef, // 19: goto -17 -> 2
        0x1a,             // 22: iload_0
        0xac,             // 23: ireturn
        0, 0,
    ];
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        24,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        vec![(13usize, tick_info as *const JitInvokeInfo)],
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    if !super::op_invoke::post_call_exits_at_forward_joins_enabled() {
        assert!(
            body.post_call_exit_sites.is_empty() && !body.osr_exit_points.contains(&16),
            "the switch off refuses the join"
        );
        return;
    }
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call before the join gets its site"
    );
    assert!(
        body.osr_exit_points.contains(&16),
        "its map at the join's bci: {:?}",
        body.osr_exit_points
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 1));
    // Forced, verdict 0: a stay jumps back past the NOP onto the join.
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    let stays = unsafe { body.try_call_with_context(0, &[]) };
    assert_eq!(stays, Ok(1000), "completes compiled");
    assert!(crate::deopt::take_last_deopt_with_point().is_none());
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 16, "resumes at the join, before the iinc");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert!(frame.stack.is_empty(), "nothing on the stack at the join");
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "the first iteration's call, before its iinc: {:?}",
        frame.locals.first()
    );
}

/// Item 2 of the i24-L6 page (2026-10-07,
/// `op_invoke::POST_CALL_EXIT_CLAIMS_AT_INDY_AND_MONITORS`): a call whose
/// successor is a `monitorenter` or a `monitorexit` gets its claimed site,
/// its map at that instruction with the call's result (the lock object) as
/// the one operand, so a forced frame re-executes the monitor instruction in
/// the interpreter. Generated bytecode only (javac stores the lock object
/// first); measured end to end by `I24PostCallExitSuccessors.java`.
/// Interpreter round i2 wave 61, lane L2
/// (`interpreter-L2-proposal-a-post-call-claim-before-a-loop-header-FIXED-20261025`): a call whose
/// successor is a LOOP HEADER. Under
/// `CRATONVM_JIT_POST_CALL_EXITS_AT_LOOP_HEADERS` its claim SHARES the
/// header's own `OsrExit` map (one map at the header, never a second);
/// forced with a leave verdict, the body leaves there, before the loop's
/// first test runs. With the switch off the header keeps no post-call site.
#[test]
pub(in crate::x64::tests) fn a_call_before_a_loop_header_shares_the_headers_exit_map() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet), as in
    // `a_call_ending_an_if_arm_gets_a_post_call_exit_at_the_join`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        verdict: AtomicI64,
    }
    extern "C" fn verdict_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let tick_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = verdict_slow_path as *const () as usize;
    // static int h() { int i = 0; tick(); while (i < 1000) { i++; } return i; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0xb8, 0x00, 0x01, // 2:  invokestatic tick()V
        0x1a,             // 5:  iload_0             <- loop header: the call's successor
        0x11, 0x03, 0xe8, // 6:  sipush 1000
        0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
        0x84, 0x00, 0x01, // 12: iinc 0, 1
        0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        20,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        vec![(2usize, tick_info as *const JitInvokeInfo)],
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(
        body.osr_exit_points.iter().filter(|&&b| b == 5).count(),
        1,
        "one map at the header: {:?}",
        body.osr_exit_points
    );
    if !super::op_invoke::post_call_exits_at_loop_headers_enabled() {
        assert!(
            body.post_call_exit_sites.is_empty(),
            "the switch off refuses the header"
        );
        return;
    }
    assert_eq!(
        body.post_call_exit_sites.len(),
        1,
        "the call before the header shares its map"
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 1));
    // Forced, verdict 0: a stay jumps back past the NOP onto the header.
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    let stays = unsafe { body.try_call_with_context(0, &[]) };
    assert_eq!(stays, Ok(1000), "completes compiled");
    assert!(crate::deopt::take_last_deopt_with_point().is_none());
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(
        frame.bci, 5,
        "resumes at the header, before the loop's first test"
    );
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert!(frame.stack.is_empty(), "nothing on the stack at the header");
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "before the first iteration: {:?}",
        frame.locals.first()
    );
}

/// Interpreter round i1 wave 49, lane L2
/// (`interpreter-L2-proposal-a-trap-is-charged-by-the-point-its-stub-baked-FIXED-20261013`,
/// `osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS`): in `tick(); tick();` the first
/// call's successor is the second call, whose dispatch-helper arm files its
/// `ReceiverTypeChanged` `REEXECUTE` snapshot at its own bci. The first
/// call's claim now files its `OsrExit` map beside that guard (the two
/// describe one frame), and, forced with a leave verdict, the body leaves
/// there: at the second call's bci, before it runs, through the MAP's stub
/// (the stash's cause is `OsrExit`, the point is the map, not the guard).
#[test]
pub(in crate::x64::tests) fn a_post_call_exit_beside_the_next_calls_snapshot_leaves_before_it() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::osr_exit::OSR_EXIT_MAPS_BESIDE_GUARDS
        || !crate::deopt_real_enabled()
        || !jit_safepoint_polls_enabled()
    {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `claimed_post_call_sites_admit_a_branch_and_a_call_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        verdict: AtomicI64,
    }
    extern "C" fn verdict_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let tick_info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = verdict_slow_path as *const () as usize;
    // static int c() { int i = 0; while (i < 1000) { tick(); tick(); i++; } return i; }
    #[rustfmt::skip]
    let calls: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x0f, // 6:  if_icmpge +15 -> 21
        0xb8, 0x00, 0x01, // 9:  invokestatic tick()V
        0xb8, 0x00, 0x01, // 12: invokestatic tick()V <- the first call's successor
        0x84, 0x00, 0x01, // 15: iinc 0, 1            <- the second's
        0xa7, 0xff, 0xf0, // 18: goto -16 -> 2
        0x1a,             // 21: iload_0
        0xac,             // 22: ireturn
        0, 0,
    ];
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &calls,
        23,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        vec![
            (9usize, tick_info as *const JitInvokeInfo),
            (12usize, tick_info as *const JitInvokeInfo),
        ], // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(body.post_call_exit_sites.len(), 2, "a site after each call");
    let at_12: Vec<DeoptReason> = body
        .deopt_points
        .iter()
        .filter(|p| p.bci == 12 && !p.semantics.rethrow_exception)
        .map(|p| p.reason)
        .collect();
    assert!(
        at_12.contains(&DeoptReason::ReceiverTypeChanged) && at_12.contains(&DeoptReason::OsrExit),
        "the snapshot and the claim's map share bci 12: {at_12:?}"
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 2));
    // Forced, verdict 0: the loop completes compiled through both sites.
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    let stays = unsafe { body.try_call_with_context(0, &[]) };
    assert_eq!(stays, Ok(1000), "completes compiled");
    assert!(crate::deopt::take_last_deopt_with_point().is_none());
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 12, "resumes at the second call, before it runs");
    assert_eq!(
        cause.map(|c| c.reason),
        Some(DeoptReason::OsrExit),
        "the map's stub fired, not the guard's"
    );
    assert!(body.owns_deopt_point(point));
    assert!(
        frame.stack.is_empty(),
        "a void call leaves nothing on the stack"
    );
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "the first iteration, before the iinc: {:?}",
        frame.locals.first()
    );
}

/// Interpreter round i2 wave 55, lane L2
/// (`interpreter-L2-proposal-single-pass-post-call-stubs-bake-their-own-point-FIXED-20261020`,
/// `op_invoke.rs::post_call_own_point_exits_enabled`): in the METHOD-ENTRY
/// body of `static double m(double x) { return g(f(x)); }` the first call's
/// successor is the second call, whose dispatch-helper arm files its
/// `ReceiverTypeChanged` snapshot at bci 4 after flushing `f`'s result to its
/// frame word; the claim's map saw that result in its XMM register, so the
/// two frames differ (an `int` or a reference result is already in its frame
/// word at the call's return, and those claims are wave 49's `Own`). Without
/// the switch the claim is `Dropped` and the body keeps no site; with it the
/// claim is `OwnBeside`: its point is in `deopt_points` but not in
/// `osr_exit_points`, and, forced with the method-entry verdict, the body
/// leaves at bci 4 through the claim's OWN point (cause `OsrExit`, `f`'s
/// result the one operand).
/// Interpreter round i2 wave 56, lane L2 (the OSR-tier stage of
/// `interpreter-L2-proposal-single-pass-post-call-stubs-bake-their-own-point-FIXED-20261020`,
/// `op_invoke.rs::osr_post_call_own_point_exits_enabled`): the shape of
/// [`a_method_entry_post_call_exit_beside_another_frames_guard_bakes_its_own_point`]
/// inside a loop of an OSR-tier compile, `s = g(f(s))`. Without the OSR
/// switch the first call's claim is `Dropped` (one site, the second call's);
/// with it the claim is `OwnBeside`: its point is in `deopt_points`, listed in
/// `post_call_own_exit_bcis` and not in `osr_exit_points`, the OSR door's
/// classification names it `PostCallOwnPoint` (not the `ExitMapMissing`
/// cross-check), the entry contract at its bci is still the guard's, the
/// artifact's OSR exit policy admits it, and, forced with the OSR verdict,
/// the body leaves at bci 15 through the claim's own point.
/// Interpreter round i1 wave 48, lane L2
/// (`interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
/// item 4 of "Progress (wave 40)"; `op_invoke.rs::POST_CALL_EXIT_SITES_WIDE_CAP`):
/// a loop body with 70 calls gets a site after each of them, not after the
/// first 64 only. The cap still bounds it (`POST_CALL_EXIT_SITES_MAX`).
#[test]
pub(in crate::x64::tests) fn a_loop_with_seventy_calls_keeps_a_post_call_site_per_call() {
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    extern "C" fn stay(_flag_addr: i64, _body_id: i64) -> i64 {
        0
    }
    const CALLS: usize = 70;
    // No `static` (the crate's statics ratchet): the flag is leaked.
    // LEAK(intentional): the compiled code bakes the flag's address.
    let flag: &'static u8 = Box::leak(Box::new(0u8));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    // int i = 0; while (i < 1000) { tick(); i += 0; ... (70 times) i++; } return i;
    let mut code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3b, // 1: istore_0
        0x1a, // 2: iload_0       <- loop header
        0x11, 0x03, 0xe8, // 3: sipush 1000
        0xa2, 0x00, 0x00, // 6: if_icmpge -> exit (patched below)
    ];
    let mut invokes: Vec<(usize, *const JitInvokeInfo)> = Vec::with_capacity(CALLS);
    for _ in 0..CALLS {
        invokes.push((code.len(), info as *const JitInvokeInfo));
        code.extend_from_slice(&[0xb8, 0x00, 0x01]); // invokestatic tick()V
        code.extend_from_slice(&[0x84, 0x00, 0x00]); // iinc 0, 0 <- its successor
    }
    code.extend_from_slice(&[0x84, 0x00, 0x01]); // iinc 0, 1
    let goto_pc = code.len();
    let back = i16::try_from(2i64 - i64::try_from(goto_pc).expect("small")).expect("in range");
    code.push(0xa7); // goto -> 2
    code.extend_from_slice(&back.to_be_bytes());
    let exit_pc = code.len();
    code.extend_from_slice(&[0x1a, 0xac]); // iload_0; ireturn
    let forward = i16::try_from(exit_pc - 6).expect("in range");
    code[7..9].copy_from_slice(&forward.to_be_bytes());
    let code_len = code.len();
    code.extend_from_slice(&[0, 0]);
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = stay as *const () as usize;
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        code_len,
        0,
        1,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        Vec::new(), // static_field_info
        Vec::new(), // new_info
        Vec::new(), // anewarray_info
        invokes,    // invoke_info
        Vec::new(), // direct_calls
        Vec::new(), // mic_slots
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
    .expect("test JIT compile");
    if body.compile_id == 0 {
        // No compile-id mirror on this host: no site either way.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(
        body.post_call_exit_sites.len(),
        CALLS.min(super::op_invoke::POST_CALL_EXIT_SITES_MAX),
        "one site per call, up to the cap"
    );
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(1000));
}

/// Interpreter round i1 wave 29, lane L3
/// (`interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
/// the proposal's "next single-pass step"): a METHOD-ENTRY compile whose loop
/// header filed its exit map before the call gets the same post-call site
/// (`op_invoke.rs::METHOD_ENTRY_POST_CALL_EXITS_ENABLED`), and its stub tests
/// the method-entry verdict bit (`SAFEPOINT_VERDICT_POLLING_BODY`), the one the
/// body's own polls test: an innermost-frame verdict keeps it compiled, a
/// polling-body verdict sends it out at the call's successor. Since wave 39
/// (lane L3) a loop-free method-entry body that keeps a dispatch record gets
/// its site too, filing its first exit map at the call's successor
/// (`op_invoke.rs::METHOD_ENTRY_FIRST_MAP_POST_CALL_EXITS_ENABLED`), and
/// leaves there the same way.
#[test]
pub(in crate::x64::tests) fn a_method_entry_body_with_a_loop_gets_a_post_call_exit() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `a_forced_post_call_exit_leaves_at_the_calls_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        post_calls: AtomicU64,
        verdict: AtomicI64,
    }
    extern "C" fn counting_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's bodies passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.post_calls.fetch_add(1, Ordering::SeqCst);
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        post_calls: AtomicU64::new(0),
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = counting_slow_path as *const () as usize;
    let compile = |code: &[u8], code_len: usize, locals: usize, call_pc: usize| {
        compile_with_request(
            BackendRequest::default(), // a method-entry compile
            Vec::new(),                // compact_field_info
            code,
            code_len,
            0,
            locals,
            true,       // needs_heap: the dispatch helper takes the context
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            vec![(call_pc, info as *const JitInvokeInfo)], // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };

    // static int g() { tick(); return 7; }: no loop, no map before the call.
    #[rustfmt::skip]
    let loop_free: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic tick()V
        0x10, 0x07,       // 3: bipush 7
        0xac,             // 5: ireturn
        0, 0,
    ];
    let plain = compile(&loop_free[..], 6, 0, 0);
    if plain.compile_id == 0 || !plain.osr_exit_points.contains(&3) {
        // No compile-id mirror, or the successor's map was refused on this
        // host: no site then.
        assert!(plain.post_call_exit_sites.is_empty());
    } else {
        assert_eq!(
            plain.post_call_exit_sites.len(),
            1,
            "wave 39: the loop-free body's call gets its site"
        );
        let _ = crate::deopt::take_last_deopt_with_point();
        assert!(plain.force_exit_polls().is_ok_and(|n| n >= 1));
        state
            .verdict
            .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
        // SAFETY: JIT-compiled machine code from valid bytecode; the only
        // helper it calls is `stub_invoke_dispatch`, which reads no argument.
        assert_eq!(unsafe { plain.try_call_with_context(0, &[]) }, Ok(7));
        assert!(crate::deopt::take_last_deopt_with_point().is_none());
        state
            .verdict
            .store(crate::SAFEPOINT_VERDICT_POLLING_BODY, Ordering::SeqCst);
        // SAFETY: as above.
        let raw = unsafe { plain.try_call_with_context(0, &[]) };
        state.verdict.store(0, Ordering::SeqCst);
        assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
        let (frame, cause, point) =
            crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
        assert_eq!(
            frame.bci, 3,
            "resumes at the call's successor, before `bipush 7`"
        );
        assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
        assert!(plain.owns_deopt_point(point));
        assert!(
            frame.stack.is_empty(),
            "a void call leaves nothing on the stack"
        );
    }

    // static int f() { int i = 0; while (i < 1000) { tick(); i++; } return i; }
    #[rustfmt::skip]
    let looped: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header (its map first)
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x0c, // 6:  if_icmpge +12 -> 18
        0xb8, 0x00, 0x01, // 9:  invokestatic tick()V (the dispatch helper)
        0x84, 0x00, 0x01, // 12: iinc 0, 1           <- the call's successor
        0xa7, 0xff, 0xf3, // 15: goto -13 -> 2
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    let body = compile(&looped[..], 20, 1, 9);
    if body.compile_id == 0 || !body.osr_exit_points.contains(&2) {
        // No compile-id mirror, or the header's map was refused on this host:
        // the rule gives the call no site then.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(body.post_call_exit_sites.len(), 1, "the call's site");
    assert!(
        body.osr_exit_points.contains(&12),
        "its map at the successor"
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(body.force_exit_polls().is_ok_and(|n| n >= 1));

    // The OSR tier's bit is not this body's: it stays and finishes compiled.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `stub_invoke_dispatch`, which reads no argument here.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(1000));
    assert!(
        state.post_calls.load(Ordering::SeqCst) > 0,
        "the forced site asked"
    );
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // The method-entry bit: out at the first call's return, at bci 12.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_POLLING_BODY, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 12, "resumes at the call's successor");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(0))),
        "before the iteration's iinc: {:?}",
        frame.locals.first()
    );
}

/// Interpreter round i1 wave 46, lane L2
/// (`interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`,
/// item 2): a call whose successor is a field READ gets a post-call exit site
/// too (its wave-46 switch went with the pre-claim admission in interpreter
/// round i2 wave 62; a claim admits every field read). The read files
/// only its `RETHROW` exception frame, so the site's `REEXECUTE` `OsrExit`
/// map is the one non-rethrow point at that bci. Unforced the read runs
/// (through its helper here); forced with the body's verdict bit the body
/// leaves at the read's own bci, before it runs, with the stack the call left.
#[test]
pub(in crate::x64::tests) fn a_call_followed_by_a_getstatic_gets_a_post_call_exit() {
    use crate::deopt::DeoptReason;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `a_forced_post_call_exit_leaves_at_the_calls_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        post_calls: AtomicU64,
        verdict: AtomicI64,
    }
    extern "C" fn counting_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.post_calls.fetch_add(1, Ordering::SeqCst);
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    /// The static's value, whatever it is asked.
    unsafe extern "C" fn marker_getstatic(_vm: i64, _cid: i64, _idx: i64) -> i64 {
        4_646
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        post_calls: AtomicU64::new(0),
        verdict: AtomicI64::new(0),
    }));
    // LEAK(intentional): compiled code stores a raw pointer to it.
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "T",
        method_name: "tick",
        descriptor: "()V",
        num_jit_args: 0,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0,
        owner_class_id: 0,
    }));
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper address.
    helpers.safepoint_slow_path = counting_slow_path as *const () as usize;
    // Cast: helper address.
    helpers.getstatic = marker_getstatic as *const () as usize;
    // static int g() { tick(); return S; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0xb8, 0x00, 0x01, // 0: invokestatic tick()V
        0xb2, 0x00, 0x02, // 3: getstatic S:I        <- the call's successor
        0xac,             // 6: ireturn
        0, 0,
    ];
    let body = compile_with_request(
        BackendRequest::default(), // a method-entry compile
        Vec::new(),                // compact_field_info
        &code,
        7,
        0,
        0,
        true,       // needs_heap: the dispatch helper takes the context
        Vec::new(), // multianewarray_info
        Vec::new(), // field_info
        Vec::new(), // typecheck_info
        // static_field_info: (pc, class_id, field_index, type_tag, volatile)
        vec![(3usize, 0x4646u32, 0usize, b'I', false)],
        Vec::new(),                                   // new_info
        Vec::new(),                                   // anewarray_info
        vec![(0usize, info as *const JitInvokeInfo)], // invoke_info
        Vec::new(),                                   // direct_calls
        Vec::new(),                                   // mic_slots
        Vec::new(),                                   // pic_slots
        Vec::new(),                                   // ldc_info
        Vec::new(),                                   // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("test JIT compile");
    if body.compile_id == 0 || !body.osr_exit_points.contains(&3) {
        // No compile-id mirror, or the successor's map was refused on this
        // host: no site then.
        assert!(body.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(body.post_call_exit_sites.len(), 1, "the call's site");
    let _ = crate::deopt::take_last_deopt_with_point();

    // Not forced: the read runs through its helper.
    // SAFETY: JIT-compiled machine code from valid bytecode; it calls only
    // `stub_invoke_dispatch` (which reads no argument here) and the marker.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(4_646));
    assert_eq!(state.post_calls.load(Ordering::SeqCst), 0);

    assert!(body.force_exit_polls().is_ok_and(|n| n >= 1));
    // The OSR tier's bit is not this body's: it stays, and the read runs.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(4_646));
    assert!(
        state.post_calls.load(Ordering::SeqCst) > 0,
        "the forced site asked"
    );
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // The method-entry bit: out at the read's own bci, before it runs.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_POLLING_BODY, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 3, "resumes at the getstatic, which has not run");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert!(
        frame.stack.is_empty(),
        "a void call leaves nothing on the stack"
    );
    // The one non-rethrow point at the read's bci is the site's map.
    assert_eq!(
        crate::osr_exit::deopt_reason_at_bci(&body.deopt_points, 3),
        crate::osr_exit::ReasonAtBci::Unique(DeoptReason::OsrExit)
    );
}

/// Interpreter round i1 wave 44, lane L2
/// (`interpreter-L2-proposal-exits-at-a-withdrawn-bodys-constant-pool-helper-calls-RETIRED-20261015`):
/// a run-time constant-pool helper call (`multianewarray` here) gets a
/// patchable site BEFORE the call whose map is at the instruction itself
/// (`op_invoke.rs::emit_cp_helper_exit_site`, behind
/// `CP_HELPER_EXITS_ENABLED`). Unforced it is a NOP and the helper runs;
/// forced (the pass that forces the post-call exits) it asks the verdict
/// alone, and on a non-zero one leaves at the instruction's own bci with its
/// operands (the two dimensions) still on the stack, so the interpreter
/// re-executes it.
#[test]
pub(in crate::x64::tests) fn a_forced_cp_helper_exit_leaves_before_the_helper_runs() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet); as in
    // `a_forced_post_call_exit_leaves_at_the_calls_successor`.
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        post_calls: AtomicU64,
        verdict: AtomicI64,
    }
    extern "C" fn counting_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's body passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.post_calls.fetch_add(1, Ordering::SeqCst);
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    /// The `multianewarray_n` helper's shape: a non-zero, 16-aligned word
    /// that names the dimensions it read (outermost first), so the test sees
    /// the helper ran with the site's operands.
    extern "C" fn fake_multianewarray(_vm: i64, _site: i64, ndims: i64, dims: *const i64) -> i64 {
        if ndims != 2 || dims.is_null() {
            return 0;
        }
        // SAFETY: the emitter pins `ndims` consecutive words at `dims`
        // (`op_object.rs`, the `multianewarray` arm) for the call.
        let (outer, inner) = unsafe { (*dims, *dims.add(1)) };
        0x1_0000 + (outer << 8) + (inner << 4)
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        post_calls: AtomicU64::new(0),
        verdict: AtomicI64::new(0),
    }));
    // static Object f() { return new int[2][3]; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x05,                   // 0: iconst_2
        0x06,                   // 1: iconst_3
        0xc5, 0x00, 0x0d, 0x02, // 2: multianewarray #13, 2   <- the site's map
        0xb0,                   // 6: areturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper addresses.
    helpers.safepoint_slow_path = counting_slow_path as *const () as usize;
    helpers.multianewarray_n = fake_multianewarray as *const () as usize;
    let body = compile_with_request(
        BackendRequest {
            mode_exit_polls: true,
            ..BackendRequest::default()
        },
        Vec::new(), // compact_field_info
        &code,
        7,
        0,
        0,
        true,                                 // needs_heap: the helper takes the context
        vec![(2usize, 0x000d_0000_0001_i64)], // multianewarray_info
        Vec::new(),                           // field_info
        Vec::new(),                           // typecheck_info
        Vec::new(),                           // static_field_info
        Vec::new(),                           // new_info
        Vec::new(),                           // anewarray_info
        Vec::new(),                           // invoke_info
        Vec::new(),                           // direct_calls
        Vec::new(),                           // mic_slots
        Vec::new(),                           // pic_slots
        Vec::new(),                           // ldc_info
        Vec::new(),                           // ldc2w_info
        HashMap::new(),
        HashMap::new(),
        &helpers,
        std::collections::HashSet::new(),
        HashMap::new(),
        None, // string_layout
    )
    .expect("test JIT compile");
    let expected = 0x1_0000 + (2 << 8) + (3 << 4);
    if !super::op_invoke::CP_HELPER_EXITS_ENABLED
        || body.compile_id == 0
        || !body.osr_exit_points.contains(&2)
    {
        // Switched off, no compile-id mirror on this host, or the map was
        // refused here: no site, and the helper runs as before.
        assert_eq!(body.cp_helper_exit_sites, 0);
        assert!(body.post_call_exit_sites.is_empty());
        // SAFETY: JIT-compiled machine code from valid bytecode; the only
        // helper it calls is `fake_multianewarray`, which reads its own
        // dimension words only.
        assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(expected));
        return;
    }
    assert_eq!(body.cp_helper_exit_sites, 1, "the helper's site");
    assert_eq!(body.post_call_exit_sites.len(), 1, "and no other");
    let (site, stub) = body.post_call_exit_sites[0];
    // Cast: buffer offsets, widened.
    let (site, stub) = (site as usize, stub as usize);
    assert_eq!(
        &body._buffer.as_slice()[site..site + 5],
        &crate::ENTRY_PATCH_PAD,
        "the fast path is the NOP"
    );
    let _ = crate::deopt::take_last_deopt_with_point();

    // Not forced: the NOP, and the helper allocates.
    // SAFETY: as above.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(expected));
    assert_eq!(state.post_calls.load(Ordering::SeqCst), 0);

    let forced = body.force_exit_polls();
    assert_eq!(
        forced,
        Ok(body.exit_poll_sites.len() + 1),
        "every poll and the site"
    );
    let bytes = body._buffer.as_slice();
    assert_eq!(bytes[site], 0xE9, "the site is a JMP now");
    let rel = i32::from_le_bytes([
        bytes[site + 1],
        bytes[site + 2],
        bytes[site + 3],
        bytes[site + 4],
    ]);
    // Cast: a buffer offset and a signed displacement, for the target.
    assert_eq!(
        (site as i64) + 5 + i64::from(rel),
        stub as i64,
        "to its stub"
    );

    // Forced, verdict 0: the site asks and stays; the helper still runs.
    // SAFETY: as above.
    assert_eq!(unsafe { body.try_call_with_context(0, &[]) }, Ok(expected));
    assert_eq!(
        state.post_calls.load(Ordering::SeqCst),
        1,
        "one verdict asked"
    );
    assert!(crate::deopt::take_last_deopt_with_point().is_none());

    // Forced, a leave verdict: out at the `multianewarray` itself, its two
    // dimensions on the stack, before the helper ran.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { body.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, point) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2, "resumes AT the instruction, to re-execute it");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(body.owns_deopt_point(point));
    assert_eq!(
        frame.stack,
        vec![FrameValue::Int(2), FrameValue::Int(3)],
        "the dimensions, as the instruction pops them"
    );
}

/// Interpreter round i2 wave 54, lane L2 (item 1 of
/// `interpreter-L6-a-compiled-frame-in-a-call-at-the-redefinition-keeps-its-old-splice-FIXED-20261007`):
/// a METHOD-ENTRY body that keeps no dispatch record and files no exit map
/// before its helper call gets no site while the door does not vouch that
/// the method is unsynchronized (the wave-39 rule: its first deopt exit is
/// kept off it for the synchronized routes); with
/// `BackendRequest::method_known_unsynchronized` (and
/// `op_invoke.rs::METHOD_ENTRY_UNSYNCHRONIZED_FIRST_MAP_POST_CALL_EXITS`) it
/// gets the site, which leaves on the method-entry verdict bit alone.
#[test]
pub(in crate::x64::tests) fn an_unsynchronized_entry_body_without_a_call_record_gets_its_first_map_site() {
    use crate::deopt::DeoptReason;
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    // No `static` (the crate's statics ratchet).
    #[repr(C)]
    struct SlowPathState {
        flag: u8,
        verdict: AtomicI64,
    }
    extern "C" fn verdict_slow_path(flag_addr: i64, body_id: i64) -> i64 {
        // SAFETY: every caller in this test's bodies passes the address of
        // `SlowPathState::flag`, the first field of the leaked `#[repr(C)]`
        // state below, which lives for the rest of the process.
        let state = unsafe { &*(flag_addr as *const SlowPathState) };
        if body_id & crate::POST_CALL_EXIT_VERDICT_ONLY != 0 {
            state.verdict.load(Ordering::SeqCst)
        } else {
            0
        }
    }
    extern "C" fn fake_multianewarray(_vm: i64, _site: i64, _ndims: i64, _dims: *const i64) -> i64 {
        0x1_0000
    }
    // LEAK(intentional): the compiled code bakes the state's address.
    let state: &'static SlowPathState = Box::leak(Box::new(SlowPathState {
        flag: 0,
        verdict: AtomicI64::new(0),
    }));
    // static Object f() { return new int[2][3]; }
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x05,                   // 0: iconst_2
        0x06,                   // 1: iconst_3
        0xc5, 0x00, 0x0d, 0x02, // 2: multianewarray #13, 2
        0xb0,                   // 6: areturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    // Cast: the baked flag address.
    helpers.safepoint_flag_addr = &state.flag as *const u8 as usize;
    // Cast: helper addresses.
    helpers.safepoint_slow_path = verdict_slow_path as *const () as usize;
    helpers.multianewarray_n = fake_multianewarray as *const () as usize;
    let compile_with = |unsynchronized: bool, keeps_a_call_record: bool| {
        compile_with_request(
            BackendRequest {
                method_known_unsynchronized: unsynchronized,
                artifact_keeps_a_call_record: keeps_a_call_record,
                ..BackendRequest::default() // a method-entry compile
            },
            Vec::new(), // compact_field_info
            &code,
            7,
            0,
            0,
            true,                                 // needs_heap: the helper takes the context
            vec![(2usize, 0x000d_0000_0001_i64)], // multianewarray_info
            Vec::new(),                           // field_info
            Vec::new(),                           // typecheck_info
            Vec::new(),                           // static_field_info
            Vec::new(),                           // new_info
            Vec::new(),                           // anewarray_info
            Vec::new(),                           // invoke_info
            Vec::new(),                           // direct_calls
            Vec::new(),                           // mic_slots
            Vec::new(),                           // pic_slots
            Vec::new(),                           // ldc_info
            Vec::new(),                           // ldc2w_info
            HashMap::new(),
            HashMap::new(),
            &helpers,
            std::collections::HashSet::new(),
            HashMap::new(),
            None, // string_layout
        )
        .expect("test JIT compile")
    };
    let compile = |unsynchronized: bool| compile_with(unsynchronized, false);
    let unknown = compile(false);
    assert!(
        unknown.post_call_exit_sites.is_empty() && unknown.osr_exit_points.is_empty(),
        "unvouched: no first exit map, as in wave 39"
    );
    // 2026-10-07 (item 1 of the i24-L6 page): a SYNCHRONIZED body whose
    // artifact keeps a call record the backend's tables miss (a direct bind,
    // a cycle cell, a splice's own call target) -- the VM's synchronized
    // routes refuse it anyway -- gets the site too.
    let recorded = compile_with(false, true);
    let plain = compile(true);
    if plain.post_call_exit_sites.len() == 1 && recorded.compile_id != 0 {
        assert_eq!(
            (
                recorded.cp_helper_exit_sites,
                recorded.post_call_exit_sites.len()
            ),
            (1, 1),
            "the synchronized body with a call record gets the unsynchronized one's site"
        );
        assert!(recorded.osr_exit_points.contains(&2));
    }
    if !super::op_invoke::METHOD_ENTRY_UNSYNCHRONIZED_FIRST_MAP_POST_CALL_EXITS
        || !super::op_invoke::CP_HELPER_EXITS_ENABLED
        || plain.compile_id == 0
        || !plain.osr_exit_points.contains(&2)
    {
        // Switched off, no compile-id mirror on this host, or the map was
        // refused here: no site.
        assert!(plain.post_call_exit_sites.is_empty());
        return;
    }
    assert_eq!(
        plain.cp_helper_exit_sites, 1,
        "the helper's site, the body's first map"
    );
    assert_eq!(plain.post_call_exit_sites.len(), 1);
    let _ = crate::deopt::take_last_deopt_with_point();
    assert!(plain.force_exit_polls().is_ok_and(|n| n >= 1));
    // An innermost-frame verdict is not this method-entry body's bit: stays.
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_INNERMOST_FRAME, Ordering::SeqCst);
    // SAFETY: JIT-compiled machine code from valid bytecode; the only helper
    // it calls is `fake_multianewarray`, which reads nothing.
    assert_eq!(unsafe { plain.try_call_with_context(0, &[]) }, Ok(0x1_0000));
    assert!(crate::deopt::take_last_deopt_with_point().is_none());
    state
        .verdict
        .store(crate::SAFEPOINT_VERDICT_POLLING_BODY, Ordering::SeqCst);
    // SAFETY: as above.
    let raw = unsafe { plain.try_call_with_context(0, &[]) };
    state.verdict.store(0, Ordering::SeqCst);
    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, _) =
        crate::deopt::take_last_deopt_with_point().expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2, "resumes AT the instruction, to re-execute it");
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
}

/// Interpreter round i1 wave 19, lane L1
/// (`interpreter-resumed-frames-show-dead-locals-as-zero`), and wave 21, lane
/// L3 (`i19-L1-proposal-keep-assigned-locals-alive-under-a-debugger`, stage
/// B): `t` is assigned before the loop and never read, so without a debugger
/// the header's exit map publishes it `Undefined` and the resumed frame shows
/// `t = 0`, which is exact for the program. While a debugger may read locals
/// (`BackendRequest::debugger_observes_locals`) the compile keeps `t` in its
/// home and the map describes it, so the body leaves with `t = 42` — what
/// HotSpot shows under `can_access_local_variables`. The counter-only loop,
/// where no assigned local is dead at the header, leaves under the flag too;
/// a loop whose unread `int` parameter no kind source can type still refuses
/// the exit and finishes compiled. Wave 22, lane L1: given the descriptor's
/// parameter kinds (`BackendRequest::param_slot_tags`) the same loop leaves,
/// with the parameter's value in the resumed frame.
#[test]
pub(in crate::x64::tests) fn a_mode_exit_under_a_debugger_describes_a_dead_assigned_local() {
    use crate::deopt::{DeoptReason, FrameValue};
    use std::sync::atomic::{AtomicI64, Ordering};
    if !crate::deopt_real_enabled() || !jit_safepoint_polls_enabled() {
        return;
    }
    static FLAG: u8 = 1;
    static VERDICT: AtomicI64 = AtomicI64::new(0);
    extern "C" fn verdict_poll(_flag_addr: i64) -> i64 {
        VERDICT.load(Ordering::SeqCst)
    }
    // int t = 42; int i = 0; while (i < 1000) { i++; } return i;
    #[rustfmt::skip]
    let hides_t: Vec<u8> = vec![
        0x10, 42,         // 0:  bipush 42
        0x3c,             // 2:  istore_1            t, never read
        0x03,             // 3:  iconst_0
        0x3b,             // 4:  istore_0
        0x1a,             // 5:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 6:  sipush 1000
        0xa2, 0x00, 0x09, // 9:  if_icmpge +9 -> 18
        0x84, 0x00, 0x01, // 12: iinc 0, 1
        0xa7, 0xff, 0xf6, // 15: goto -10 -> 5
        0x1a,             // 18: iload_0
        0xac,             // 19: ireturn
        0, 0,
    ];
    // int i = 0; while (i < 1000) { i++; } return i;
    #[rustfmt::skip]
    let counter_only: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3b,             // 1:  istore_0
        0x1a,             // 2:  iload_0             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
        0x84, 0x00, 0x01, // 9:  iinc 0, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1a,             // 15: iload_0
        0xac,             // 16: ireturn
        0, 0,
    ];
    // static int f(int p) { int i = 0; while (i < 1000) { i++; } return i; }
    // `p` is never read: assigned, dead, and of no kind the scan can see.
    #[rustfmt::skip]
    let unread_param: Vec<u8> = vec![
        0x03,             // 0:  iconst_0
        0x3c,             // 1:  istore_1
        0x1b,             // 2:  iload_1             <- loop header
        0x11, 0x03, 0xe8, // 3:  sipush 1000
        0xa2, 0x00, 0x09, // 6:  if_icmpge +9 -> 15
        0x84, 0x01, 0x01, // 9:  iinc 1, 1
        0xa7, 0xff, 0xf6, // 12: goto -10 -> 2
        0x1b,             // 15: iload_1
        0xac,             // 16: ireturn
        0, 0,
    ];
    let mut helpers = test_helpers();
    helpers.safepoint_flag_addr = &FLAG as *const u8 as usize; // Cast: baked flag address
    helpers.safepoint_slow_path = verdict_poll as *const () as usize; // Cast: helper address
    let build = |code: &[u8],
                 code_len: usize,
                 params: usize,
                 max_locals: usize,
                 debugger: bool,
                 tags: &[u8]| {
        compile_with_request(
            BackendRequest {
                mode_exit_polls: true,
                debugger_observes_locals: debugger,
                param_slot_tags: tags.to_vec(),
                ..BackendRequest::default()
            },
            Vec::new(), // compact_field_info
            code,
            code_len,
            params,
            max_locals,
            false,
            Vec::new(), // multianewarray_info
            Vec::new(), // field_info
            Vec::new(), // typecheck_info
            Vec::new(), // static_field_info
            Vec::new(), // new_info
            Vec::new(), // anewarray_info
            Vec::new(), // invoke_info
            Vec::new(), // direct_calls
            Vec::new(), // mic_slots
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
        .expect("test JIT compile")
    };
    let plain = build(&hides_t, 20, 0, 2, false, &[]);
    let observed = build(&hides_t, 20, 0, 2, true, &[]);
    let counter_observed = build(&counter_only, 17, 0, 1, true, &[]);
    let param_observed = build(&unread_param, 17, 1, 2, true, &[]);
    // `(I)I`, static: the descriptor types slot 0.
    let param_typed = build(&unread_param, 17, 1, 2, true, b"I");
    // `(F)I`, static: the same body with an unread `float`, which the colouring
    // could put in a GPR; the debugger's compile keeps it in its frame slot.
    let float_typed = build(&unread_param, 17, 1, 2, true, b"F");
    assert!(
        plain.osr_exit_points.contains(&5),
        "precondition: the loop header carries an OSR-exit map"
    );
    let _ = crate::deopt::take_last_deopt_with_point();
    VERDICT.store(1, Ordering::SeqCst);

    // No debugger: the body leaves at the header, `t` described `Undefined`.
    // SAFETY: JIT-compiled machine code from valid bytecode; no arguments.
    let raw = unsafe { plain.try_call(&[]) };
    let left = crate::deopt::take_last_deopt_with_point();
    // A debugger: `t` is kept and described, so the body leaves too.
    // SAFETY: as above.
    let observed_raw = unsafe { observed.try_call(&[]) };
    let observed_left = crate::deopt::take_last_deopt_with_point();
    // A debugger, nothing dead: the counter-only loop leaves.
    // SAFETY: as above.
    let counter_raw = unsafe { counter_observed.try_call(&[]) };
    let counter_left = crate::deopt::take_last_deopt_with_point();
    // A debugger, an untypable unread parameter: the exit is refused.
    // SAFETY: as above; one `int` argument.
    let param_raw = unsafe { param_observed.try_call(&[7]) };
    let param_left = crate::deopt::take_last_deopt_with_point();
    // A debugger, the parameter typed from the descriptor: the body leaves.
    // SAFETY: as above; one `int` argument.
    let typed_raw = unsafe { param_typed.try_call(&[7]) };
    let typed_left = crate::deopt::take_last_deopt_with_point();
    // SAFETY: as above; one `float` argument, passed as its bit pattern.
    let float_raw = unsafe { float_typed.try_call(&[i64::from(2.5f32.to_bits())]) };
    let float_left = crate::deopt::take_last_deopt_with_point();
    VERDICT.store(0, Ordering::SeqCst);

    assert_eq!(raw, Ok(i64::MIN), "the exit returns the deopt sentinel");
    let (frame, cause, _) = left.expect("the exit stashes a frame");
    assert_eq!(frame.bci, 5);
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(
        matches!(frame.locals.get(1), Some(FrameValue::Undefined)),
        "t is dead at the header: {:?}",
        frame.locals.get(1)
    );
    assert_eq!(observed_raw, Ok(i64::MIN), "kept alive: the body leaves");
    let (frame, cause, _) = observed_left.expect("the exit stashes a frame");
    assert_eq!(frame.bci, 5);
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(
        matches!(frame.locals.get(1), Some(FrameValue::Int(42))),
        "t keeps its value in the resumed frame: {:?}",
        frame.locals.get(1)
    );
    assert_eq!(counter_raw, Ok(i64::MIN), "no assigned local is hidden");
    let (frame, _, _) = counter_left.expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2);
    assert_eq!(param_raw, Ok(1000), "refused: the loop finishes compiled");
    assert!(param_left.is_none());
    assert_eq!(typed_raw, Ok(i64::MIN), "typed parameter: the body leaves");
    let (frame, cause, _) = typed_left.expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2);
    assert_eq!(cause.map(|c| c.reason), Some(DeoptReason::OsrExit));
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Int(7))),
        "the unread parameter keeps the value passed: {:?}",
        frame.locals.first()
    );
    assert_eq!(
        float_raw,
        Ok(i64::MIN),
        "typed float parameter: the body leaves"
    );
    let (frame, _, _) = float_left.expect("the exit stashes a frame");
    assert_eq!(frame.bci, 2);
    assert!(
        matches!(frame.locals.first(), Some(FrameValue::Float(b)) if *b == u64::from(2.5f32.to_bits())),
        "the unread float parameter keeps the value passed: {:?}",
        frame.locals.first()
    );
}

