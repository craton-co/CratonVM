use super::*;

extern "C" fn t_none() -> i64 {
    77
}
extern "C" fn t_neg8(a: i8) -> i32 {
    -(a as i32)
}
extern "C" fn t_narrow_ret(a: i32) -> i8 {
    a as i8
}
extern "C" fn t_six(a: i8, b: i16, c: i32, d: i64, e: u16, f: *const i64) -> i64 {
    // SAFETY: the tests pass a live `i64`.
    let deref = unsafe { *f };
    a as i64 + 10 * b as i64 + 100 * c as i64 + 1000 * d + 10_000 * e as i64 + deref
}
extern "C" fn t_void(p: *mut i64) {
    // SAFETY: the tests pass a live `i64`.
    unsafe { *p = 5 };
}

/// One call both ways: `ffi_call` on a CIF of `kinds` -> `ret`, and the
/// direct stub; answers the two unmarshalled results.
fn both(fn_addr: usize, kinds: &[i32], args: &[Value], ret: Option<i32>) -> (Value, Value) {
    let types: Vec<FfiType> = kinds
        .iter()
        .map(|k| primitive_kind_to_ffi_type(*k).unwrap())
        .collect();
    let cif = Cif::new(
        types,
        ret.map_or_else(FfiType::void, |k| primitive_kind_to_ffi_type(k).unwrap()),
    );
    let mut cells = [0u64; DIRECT_STUB_MAX_ARGS];
    for (i, (kind, arg)) in kinds.iter().zip(args).enumerate() {
        if *kind == LAYOUT_ADDRESS {
            cells[i] = arg.as_long().unwrap() as u64;
        } else {
            assert!(marshal_primitive_into(*kind, arg, &mut cells[i]));
        }
    }
    let mut ptrs = [std::ptr::null_mut::<std::ffi::c_void>(); DIRECT_STUB_MAX_ARGS];
    for (p, c) in ptrs.iter_mut().zip(cells.iter_mut()) {
        *p = c as *mut u64 as *mut std::ffi::c_void;
    }
    let mut slot = [0u64; 2];
    // SAFETY: the CIF is the test function's own shape.
    unsafe {
        libffi::raw::ffi_call(
            cif.as_raw_ptr(),
            Some(
                *libffi::middle::CodePtr::from_ptr(fn_addr as *const std::ffi::c_void)
                    .as_safe_fun(),
            ),
            slot.as_mut_ptr() as *mut std::ffi::c_void,
            ptrs.as_mut_ptr(),
        );
    }
    let mut regs = [0u64; DIRECT_STUB_MAX_ARGS];
    for (i, kind) in kinds.iter().enumerate() {
        regs[i] = direct_stub_widen(*kind, cells[i]);
    }
    // SAFETY: as above.
    let raw = unsafe { direct_stub_call(fn_addr, kinds.len(), &regs) }.unwrap();
    let read = |r: [u64; 2]| -> Value {
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&r[0].to_ne_bytes());
        bytes[8..].copy_from_slice(&r[1].to_ne_bytes());
        ret.map_or(Value::Object(None), |k| {
            unmarshal_return_primitive(k, &bytes)
        })
    };
    (read(slot), read([raw, 0]))
}

#[test]
fn direct_stub_answers_what_ffi_call_answers() {
    let (a, b) = both(t_none as usize, &[], &[], Some(LAYOUT_LONG));
    assert_eq!((a, b), (Value::Long(77), Value::Long(77)));
    for v in [-128, -5, 0, 5, 127] {
        let (a, b) = both(
            t_neg8 as usize,
            &[LAYOUT_BYTE],
            &[Value::Int(v)],
            Some(LAYOUT_INT),
        );
        assert_eq!(a, Value::Int(-v));
        assert_eq!(a, b, "byte {v}");
    }
    for v in [-1, 200, -129, 0x1234_5678] {
        let (a, b) = both(
            t_narrow_ret as usize,
            &[LAYOUT_INT],
            &[Value::Int(v)],
            Some(LAYOUT_BYTE),
        );
        assert_eq!(a, Value::Int(v as i8 as i32));
        assert_eq!(a, b, "narrow return {v}");
    }
    let cell = 3i64;
    let args = [
        Value::Int(-7),
        Value::Int(-300),
        Value::Int(-70_000),
        Value::Long(-9),
        Value::Int(0xFFFF),
        Value::Long(&cell as *const i64 as i64),
    ];
    let kinds = [
        LAYOUT_BYTE,
        LAYOUT_SHORT,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_CHAR,
        LAYOUT_ADDRESS,
    ];
    let (a, b) = both(t_six as usize, &kinds, &args, Some(LAYOUT_LONG));
    assert_eq!(a, b, "six arguments");
    let mut out = 0i64;
    let (a, b) = both(
        t_void as usize,
        &[LAYOUT_ADDRESS],
        &[Value::Long(&mut out as *mut i64 as i64)],
        None,
    );
    assert_eq!((a, b, out), (Value::Object(None), Value::Object(None), 5));
}

#[test]
fn direct_stub_serves_integer_class_up_to_six_only() {
    assert!(direct_stubs_supported());
    for kind in [
        LAYOUT_BYTE,
        LAYOUT_BOOLEAN,
        LAYOUT_SHORT,
        LAYOUT_CHAR,
        LAYOUT_INT,
        LAYOUT_LONG,
        LAYOUT_ADDRESS,
    ] {
        assert!(direct_stub_kind_ok(kind));
    }
    assert!(!direct_stub_kind_ok(LAYOUT_FLOAT));
    assert!(!direct_stub_kind_ok(LAYOUT_DOUBLE));
    assert!(!direct_stub_kind_ok(LAYOUT_UNKNOWN));
    let regs = [0u64; DIRECT_STUB_MAX_ARGS];
    // SAFETY: an arity above six calls nothing.
    assert_eq!(unsafe { direct_stub_call(t_none as usize, 7, &regs) }, None);
    assert_eq!(direct_stub_widen(LAYOUT_BYTE, 0xFF), u64::MAX);
    assert_eq!(direct_stub_widen(LAYOUT_SHORT, 0xFFFF), u64::MAX);
    let char_bits = if char_arg_unsigned() {
        0xFFFF
    } else {
        u64::MAX
    };
    assert_eq!(direct_stub_widen(LAYOUT_CHAR, 0xFFFF), char_bits);
    assert_eq!(
        direct_stub_widen(LAYOUT_INT, 0x8000_0000),
        0xFFFF_FFFF_8000_0000
    );
    assert_eq!(direct_stub_widen(LAYOUT_LONG, 0x8000_0000), 0x8000_0000);
}

extern "C" fn t_mixed(a: f64, b: i32, c: f32, d: i64, e: f64) -> f64 {
    a * 1000.0 + f64::from(b) * 100.0 + f64::from(c) * 10.0 + d as f64 + e / 10.0
}

extern "C" fn t_float_ret(a: i32, b: f32) -> f32 {
    a as f32 + b
}

extern "C" fn t_int_ret(a: f64, b: i64) -> i64 {
    a as i64 * 10 + b
}

/// Round 19: the float-class stub passes interleaved integer and vector
/// arguments, each class in its own parameter order, and reads a `double`
/// or `float` return from `xmm0` (SysV x86-64 only).
#[test]
fn direct_stub_fp_passes_each_class_in_order() {
    assert!(direct_stub_fp_kind(LAYOUT_FLOAT) && direct_stub_fp_kind(LAYOUT_DOUBLE));
    assert!(!direct_stub_fp_kind(LAYOUT_INT) && !direct_stub_fp_kind(LAYOUT_ADDRESS));
    let mut ints = [0u64; DIRECT_STUB_MAX_ARGS];
    let mut fps = [0u64; DIRECT_STUB_MAX_FP_ARGS];
    ints[0] = direct_stub_widen(LAYOUT_INT, (-3i32) as u32 as u64);
    ints[1] = 7;
    fps[0] = 2.0f64.to_bits();
    fps[1] = u64::from(0.5f32.to_bits());
    fps[2] = 40.0f64.to_bits();
    // SAFETY: `t_mixed`'s own shape: two integer and three vector parameters.
    let got = unsafe { direct_stub_call_fp(t_mixed as usize, &ints, &fps, true) };
    if !direct_stub_fp_supported() {
        assert_eq!(got, None);
        return;
    }
    assert_eq!(
        f64::from_bits(got.unwrap()),
        2000.0 - 300.0 + 5.0 + 7.0 + 4.0
    );
    let mut ints = [0u64; DIRECT_STUB_MAX_ARGS];
    let mut fps = [0u64; DIRECT_STUB_MAX_FP_ARGS];
    ints[0] = 4;
    fps[0] = u64::from(0.25f32.to_bits());
    // SAFETY: `t_float_ret`'s shape.
    let got = unsafe { direct_stub_call_fp(t_float_ret as usize, &ints, &fps, true) }.unwrap();
    assert_eq!(f32::from_bits(got as u32), 4.25);
    fps[0] = 3.0f64.to_bits();
    ints[0] = 5;
    // SAFETY: `t_int_ret`'s shape.
    let got = unsafe { direct_stub_call_fp(t_int_ret as usize, &ints, &fps, false) }.unwrap();
    assert_eq!(got as i64, 35);
}
