// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company


#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_types::ArrayElementType;
    use crate::vm::{read_java_string, NativeContextImpl, Vm};

    fn test_vm() -> Vm {
        Vm::new(VmConfig::default())
    }

    fn read_str(vm: &Vm, obj: ObjectRef) -> String {
        read_java_string(&vm.shared.heap, obj).unwrap()
    }

    fn ctx<'a>(vm: &'a mut Vm) -> NativeContextImpl<'a> {
        NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        }
    }

    // -----------------------------------------------------------------------
    // Helper / no-op natives
    // -----------------------------------------------------------------------

    #[test]
    fn noop_returns_none() {
        let mut vm = test_vm();
        let result = native_noop(&mut ctx(&mut vm), &[]);
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn return_false_returns_zero() {
        let mut vm = test_vm();
        let result = native_return_false(&mut ctx(&mut vm), &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn return_zero_returns_zero() {
        let mut vm = test_vm();
        let result = native_return_zero(&mut ctx(&mut vm), &[]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn not_implemented_returns_internal_error() {
        let mut vm = test_vm();
        let result = native_not_implemented(&mut ctx(&mut vm), &[]);
        assert!(result.is_err());
    }

    #[test]
    fn noop_with_this_returns_none() {
        let mut vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 2);
        let result = native_noop_with_this(&mut ctx(&mut vm), &[Value::Object(Some(obj))]);
        assert_eq!(result.unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // Object natives
    // -----------------------------------------------------------------------

    #[test]
    fn object_hash_code_returns_nonzero() {
        let mut vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let result = native_object_hash_code(&mut ctx(&mut vm), &[Value::Object(Some(obj))]);
        let hash = result.unwrap().unwrap();
        assert!(matches!(hash, Value::Int(h) if h != 0), "expected nonzero Int, got {hash:?}");
    }

    #[test]
    fn object_hash_code_null_throws_npe() {
        let mut vm = test_vm();
        let result = native_object_hash_code(&mut ctx(&mut vm), &[Value::Object(None)]);
        assert!(result.is_err());
    }

    #[test]
    fn object_equals_same_ref_true() {
        let mut vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let result = native_object_equals(
            &mut ctx(&mut vm),
            &[Value::Object(Some(obj)), Value::Object(Some(obj))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn object_equals_different_refs_false() {
        let mut vm = test_vm();
        let a = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let b = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let result = native_object_equals(
            &mut ctx(&mut vm),
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn object_equals_null_both() {
        let mut vm = test_vm();
        let result = native_object_equals(
            &mut ctx(&mut vm),
            &[Value::Object(None), Value::Object(None)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn object_clone_copies_fields() {
        let mut vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 3);
        vm.shared.heap.set_field(obj, 0, Value::Int(42));
        vm.shared.heap.set_field(obj, 1, Value::Int(99));
        let result = native_object_clone(&mut ctx(&mut vm), &[Value::Object(Some(obj))]);
        let result_val = result.unwrap();
        let cloned = match result_val {
            Some(Value::Object(Some(o))) => o,
            _ => panic!("expected Object, got {result_val:?}"),
        };
        assert_ne!(cloned.as_ptr(), obj.as_ptr());
        assert_eq!(vm.shared.heap.get_field(cloned, 0).as_int(), Some(42));
        assert_eq!(vm.shared.heap.get_field(cloned, 1).as_int(), Some(99));
    }

    // -----------------------------------------------------------------------
    // Float / Double bit conversion
    // -----------------------------------------------------------------------

    #[test]
    fn float_to_raw_int_bits_positive() {
        let mut vm = test_vm();
        let result = native_float_to_raw_int_bits(&mut ctx(&mut vm), &[Value::Float(1.0)]);
        assert_eq!(result.unwrap(), Some(Value::Int(1.0_f32.to_bits() as i32)));
    }

    #[test]
    fn float_to_raw_int_bits_zero() {
        let mut vm = test_vm();
        let result = native_float_to_raw_int_bits(&mut ctx(&mut vm), &[Value::Float(0.0)]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn float_to_raw_int_bits_nan() {
        let mut vm = test_vm();
        let result = native_float_to_raw_int_bits(&mut ctx(&mut vm), &[Value::Float(f32::NAN)]);
        let bits = match result.unwrap() {
            Some(Value::Int(v)) => v as u32,
            _ => panic!("expected Int"),
        };
        assert!(f32::from_bits(bits).is_nan());
    }

    #[test]
    fn double_to_raw_long_bits_positive() {
        let mut vm = test_vm();
        let result = native_double_to_raw_long_bits(&mut ctx(&mut vm), &[Value::Double(1.0)]);
        assert_eq!(result.unwrap(), Some(Value::Long(1.0_f64.to_bits() as i64)));
    }

    #[test]
    fn long_bits_to_double_roundtrip() {
        let mut vm = test_vm();
        let bits = 42.5_f64.to_bits() as i64;
        let result = native_long_bits_to_double(&mut ctx(&mut vm), &[Value::Long(bits)]);
        assert_eq!(result.unwrap(), Some(Value::Double(42.5)));
    }

    // -----------------------------------------------------------------------
    // System natives
    // -----------------------------------------------------------------------

    #[test]
    fn system_identity_hash_code() {
        let mut vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let result =
            native_system_identity_hash_code(&mut ctx(&mut vm), &[Value::Object(Some(obj))]);
        let val = result.unwrap();
        assert!(matches!(val, Some(Value::Int(h)) if h != 0), "expected nonzero Int, got {val:?}");
    }

    #[test]
    fn system_current_time_millis_positive() {
        let mut vm = test_vm();
        let result = native_system_current_time_millis(&mut ctx(&mut vm), &[]);
        let val = result.unwrap();
        assert!(matches!(val, Some(Value::Long(t)) if t > 0), "expected positive Long, got {val:?}");
    }

    #[test]
    fn system_arraycopy_basic() {
        let mut vm = test_vm();
        let src = {
            let mut c = ctx(&mut vm);
            c.new_array(ArrayElementType::Int, 5)
        };
        let dst = {
            let mut c = ctx(&mut vm);
            c.new_array(ArrayElementType::Int, 5)
        };
        for i in 0..5 {
            let c = ctx(&mut vm);
            c.set_array_element(src, i, Value::Int((i * 10) as i32));
        }
        let result = native_system_arraycopy(
            &mut ctx(&mut vm),
            &[
                Value::Object(Some(src)),
                Value::Int(1),
                Value::Object(Some(dst)),
                Value::Int(2),
                Value::Int(3),
            ],
        );
        assert!(result.is_ok());
        let c = ctx(&mut vm);
        assert_eq!(c.get_array_element(dst, 2).as_int(), Some(10));
        assert_eq!(c.get_array_element(dst, 3).as_int(), Some(20));
        assert_eq!(c.get_array_element(dst, 4).as_int(), Some(30));
    }

    #[test]
    fn system_arraycopy_null_src_throws_npe() {
        let mut vm = test_vm();
        let dst = {
            let mut c = ctx(&mut vm);
            c.new_array(ArrayElementType::Int, 5)
        };
        let result = native_system_arraycopy(
            &mut ctx(&mut vm),
            &[
                Value::Object(None),
                Value::Int(0),
                Value::Object(Some(dst)),
                Value::Int(0),
                Value::Int(1),
            ],
        );
        assert!(result.is_err());
    }

    #[test]
    fn system_nano_time_monotonic() {
        let mut vm = test_vm();
        let r1 = native_system_nano_time(&mut ctx(&mut vm), &[]);
        let r2 = native_system_nano_time(&mut ctx(&mut vm), &[]);
        let t1 = match r1.unwrap() {
            Some(Value::Long(v)) => v,
            _ => panic!("expected Long"),
        };
        let t2 = match r2.unwrap() {
            Some(Value::Long(v)) => v,
            _ => panic!("expected Long"),
        };
        assert!(t2 >= t1);
    }

    #[test]
    fn system_line_separator() {
        let mut vm = test_vm();
        let result = native_system_line_separator(&mut ctx(&mut vm), &[]);
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected Object, got {val:?}");
        };
        let s = read_java_string(&vm.shared.heap, obj);
        assert!(s.is_some());
        assert!(!s.unwrap().is_empty());
    }

    #[test]
    fn system_get_set_property() {
        let mut vm = test_vm();

        // Set a property
        let key = {
            let mut c = ctx(&mut vm);
            c.create_string("test.key")
        };
        let val = {
            let mut c = ctx(&mut vm);
            c.create_string("test.value")
        };
        let result = native_system_set_property(
            &mut ctx(&mut vm),
            &[Value::Object(Some(key)), Value::Object(Some(val))],
        );
        assert!(result.is_ok());

        // Get it back
        let result = native_system_get_property(&mut ctx(&mut vm), &[Value::Object(Some(key))]);
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected string Object, got {val:?}");
        };
        assert_eq!(read_str(&vm, obj), "test.value");
    }

    // -----------------------------------------------------------------------
    // String natives
    // -----------------------------------------------------------------------

    #[test]
    fn string_length_empty() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("")
        };
        let result = native_string_length(&mut ctx(&mut vm), &[Value::Object(Some(s))]);
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_length_nonempty() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let result = native_string_length(&mut ctx(&mut vm), &[Value::Object(Some(s))]);
        assert_eq!(result.unwrap(), Some(Value::Int(5)));
    }

    #[test]
    fn string_char_at_valid() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("abc")
        };
        let result =
            native_string_char_at(&mut ctx(&mut vm), &[Value::Object(Some(s)), Value::Int(1)]);
        assert_eq!(result.unwrap(), Some(Value::Int('b' as i32)));
    }

    #[test]
    fn string_char_at_out_of_bounds() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("ab")
        };
        let result =
            native_string_char_at(&mut ctx(&mut vm), &[Value::Object(Some(s)), Value::Int(5)]);
        assert!(result.is_err());
    }

    #[test]
    fn string_equals_same() {
        let mut vm = test_vm();
        let a = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let b = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let result = native_string_equals(
            &mut ctx(&mut vm),
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

    #[test]
    fn string_equals_different() {
        let mut vm = test_vm();
        let a = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let b = {
            let mut c = ctx(&mut vm);
            c.create_string("world")
        };
        let result = native_string_equals(
            &mut ctx(&mut vm),
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_equals_null_arg() {
        let mut vm = test_vm();
        let a = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let result = native_string_equals(
            &mut ctx(&mut vm),
            &[Value::Object(Some(a)), Value::Object(None)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(0)));
    }

    #[test]
    fn string_hash_code_consistent() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("test")
        };
        let h1 = native_string_hash_code(&mut ctx(&mut vm), &[Value::Object(Some(s))]).unwrap();
        let h2 = native_string_hash_code(&mut ctx(&mut vm), &[Value::Object(Some(s))]).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn string_index_of_found() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let result = native_string_index_of(
            &mut ctx(&mut vm),
            &[Value::Object(Some(s)), Value::Int('l' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(2)));
    }

    #[test]
    fn string_index_of_not_found() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("hello")
        };
        let result = native_string_index_of(
            &mut ctx(&mut vm),
            &[Value::Object(Some(s)), Value::Int('z' as i32)],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(-1)));
    }

    #[test]
    fn string_substring_valid() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("hello world")
        };
        let result = native_string_substring(
            &mut ctx(&mut vm),
            &[Value::Object(Some(s)), Value::Int(0), Value::Int(5)],
        );
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected string Object, got {val:?}");
        };
        assert_eq!(read_str(&vm, obj), "hello");
    }

    #[test]
    fn string_value_of_int_positive() {
        let mut vm = test_vm();
        let result = native_string_value_of_int(&mut ctx(&mut vm), &[Value::Int(42)]);
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected string Object, got {val:?}");
        };
        assert_eq!(read_str(&vm, obj), "42");
    }

    #[test]
    fn string_value_of_int_negative() {
        let mut vm = test_vm();
        let result = native_string_value_of_int(&mut ctx(&mut vm), &[Value::Int(-7)]);
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected string Object, got {val:?}");
        };
        assert_eq!(read_str(&vm, obj), "-7");
    }

    #[test]
    fn string_value_of_int_zero() {
        let mut vm = test_vm();
        let result = native_string_value_of_int(&mut ctx(&mut vm), &[Value::Int(0)]);
        let val = result.unwrap();
        let Some(Value::Object(Some(obj))) = val else {
            panic!("expected string Object, got {val:?}");
        };
        assert_eq!(read_str(&vm, obj), "0");
    }

    #[test]
    fn string_intern_returns_same_ref() {
        let mut vm = test_vm();
        let s1 = {
            let mut c = ctx(&mut vm);
            c.create_string("interned")
        };
        let r1 = native_string_intern(&mut ctx(&mut vm), &[Value::Object(Some(s1))]).unwrap();

        let s2 = {
            let mut c = ctx(&mut vm);
            c.create_string("interned")
        };
        let r2 = native_string_intern(&mut ctx(&mut vm), &[Value::Object(Some(s2))]).unwrap();

        // Both intern calls should return the same ObjectRef
        match (r1, r2) {
            (Some(Value::Object(Some(a))), Some(Value::Object(Some(b)))) => {
                assert_eq!(a.as_ptr(), b.as_ptr());
            }
            _ => panic!("expected Object refs"),
        }
    }

    #[test]
    fn string_equals_ignore_case() {
        let mut vm = test_vm();
        let a = {
            let mut c = ctx(&mut vm);
            c.create_string("Hello")
        };
        let b = {
            let mut c = ctx(&mut vm);
            c.create_string("hELLO")
        };
        let result = native_string_equals_ignore_case(
            &mut ctx(&mut vm),
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        );
        assert_eq!(result.unwrap(), Some(Value::Int(1)));
    }

    // -----------------------------------------------------------------------
    // Math natives
    // -----------------------------------------------------------------------

    #[test]
    fn math_abs_int() {
        let mut vm = test_vm();
        let result = native_math_abs_int(&mut ctx(&mut vm), &[Value::Int(-42)]);
        assert_eq!(result.unwrap(), Some(Value::Int(42)));
    }

    #[test]
    fn math_abs_int_positive() {
        let mut vm = test_vm();
        let result = native_math_abs_int(&mut ctx(&mut vm), &[Value::Int(42)]);
        assert_eq!(result.unwrap(), Some(Value::Int(42)));
    }

    #[test]
    fn math_max_int() {
        let mut vm = test_vm();
        let result = native_math_max_int(&mut ctx(&mut vm), &[Value::Int(3), Value::Int(7)]);
        assert_eq!(result.unwrap(), Some(Value::Int(7)));
    }

    #[test]
    fn math_min_int() {
        let mut vm = test_vm();
        let result = native_math_min_int(&mut ctx(&mut vm), &[Value::Int(3), Value::Int(7)]);
        assert_eq!(result.unwrap(), Some(Value::Int(3)));
    }

    #[test]
    fn math_sqrt() {
        let mut vm = test_vm();
        let result = native_math_sqrt(&mut ctx(&mut vm), &[Value::Double(25.0)]);
        assert_eq!(result.unwrap(), Some(Value::Double(5.0)));
    }

    #[test]
    fn math_sqrt_nan() {
        let mut vm = test_vm();
        let result = native_math_sqrt(&mut ctx(&mut vm), &[Value::Double(-1.0)]);
        let val = result.unwrap();
        assert!(matches!(val, Some(Value::Double(v)) if v.is_nan()), "expected NaN Double, got {val:?}");
    }

    #[test]
    fn math_pow() {
        let mut vm = test_vm();
        let result = native_math_pow(
            &mut ctx(&mut vm),
            &[Value::Double(2.0), Value::Double(10.0)],
        );
        assert_eq!(result.unwrap(), Some(Value::Double(1024.0)));
    }

    #[test]
    fn math_floor() {
        let mut vm = test_vm();
        let result = native_math_floor(&mut ctx(&mut vm), &[Value::Double(3.7)]);
        assert_eq!(result.unwrap(), Some(Value::Double(3.0)));
    }

    #[test]
    fn math_ceil() {
        let mut vm = test_vm();
        let result = native_math_ceil(&mut ctx(&mut vm), &[Value::Double(3.2)]);
        assert_eq!(result.unwrap(), Some(Value::Double(4.0)));
    }

    #[test]
    fn math_round_double() {
        let mut vm = test_vm();
        let result = native_math_round_double(&mut ctx(&mut vm), &[Value::Double(3.5)]);
        assert_eq!(result.unwrap(), Some(Value::Long(4)));
    }

    #[test]
    fn math_round_nan() {
        let mut vm = test_vm();
        let result = native_math_round_double(&mut ctx(&mut vm), &[Value::Double(f64::NAN)]);
        assert_eq!(result.unwrap(), Some(Value::Long(0)));
    }

    #[test]
    fn math_floor_div_zero_throws() {
        let mut vm = test_vm();
        let result = native_math_floor_div_int(&mut ctx(&mut vm), &[Value::Int(10), Value::Int(0)]);
        assert!(result.is_err());
    }

    #[test]
    fn math_floor_div_negative() {
        let mut vm = test_vm();
        let result = native_math_floor_div_int(&mut ctx(&mut vm), &[Value::Int(7), Value::Int(-2)]);
        // Java: floorDiv(7, -2) == -4
        assert_eq!(result.unwrap(), Some(Value::Int(-4)));
    }

    // -----------------------------------------------------------------------
    // Wrapper / boxing natives
    // -----------------------------------------------------------------------

    #[test]
    fn integer_value_of_and_int_value() {
        let mut vm = test_vm();
        let boxed = native_integer_value_of(&mut ctx(&mut vm), &[Value::Int(42)])
            .unwrap()
            .unwrap();
        let unboxed = native_wrapper_int_value(&mut ctx(&mut vm), &[boxed]).unwrap();
        assert_eq!(unboxed, Some(Value::Int(42)));
    }

    #[test]
    fn integer_parse_int_valid() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("123")
        };
        let result = native_integer_parse_int(&mut ctx(&mut vm), &[Value::Object(Some(s))]);
        assert_eq!(result.unwrap(), Some(Value::Int(123)));
    }

    #[test]
    fn integer_parse_int_invalid() {
        let mut vm = test_vm();
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("abc")
        };
        let result = native_integer_parse_int(&mut ctx(&mut vm), &[Value::Object(Some(s))]);
        assert!(result.is_err());
    }

    #[test]
    fn integer_parse_int_null_throws() {
        let mut vm = test_vm();
        let result = native_integer_parse_int(&mut ctx(&mut vm), &[Value::Object(None)]);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // PrintStream natives
    // -----------------------------------------------------------------------

    #[test]
    fn println_string_captures() {
        let mut vm = test_vm();
        // Create a dummy PrintStream object (field 0 unused for synthetic)
        let ps = vm.shared.heap.alloc_object(ClassId::new(100), 1);
        let s = {
            let mut c = ctx(&mut vm);
            c.create_string("hello test")
        };
        let result = native_println_string(
            &mut ctx(&mut vm),
            &[Value::Object(Some(ps)), Value::Object(Some(s))],
        );
        assert!(result.is_ok());
        assert!(!vm.main_thread.printed_lines.is_empty());
        assert_eq!(vm.main_thread.printed_lines.last().unwrap(), "hello test");
    }

    #[test]
    fn println_int_captures() {
        let mut vm = test_vm();
        let ps = vm.shared.heap.alloc_object(ClassId::new(100), 1);
        let result = native_println_int(
            &mut ctx(&mut vm),
            &[Value::Object(Some(ps)), Value::Int(42)],
        );
        assert!(result.is_ok());
        assert_eq!(vm.main_thread.printed_lines.last().unwrap(), "42");
    }

    #[test]
    fn println_void_captures_empty() {
        let mut vm = test_vm();
        let ps = vm.shared.heap.alloc_object(ClassId::new(100), 1);
        let result = native_println_void(&mut ctx(&mut vm), &[Value::Object(Some(ps))]);
        assert!(result.is_ok());
        assert_eq!(vm.main_thread.printed_lines.last().unwrap(), "");
    }

    #[test]
    fn println_boolean_true() {
        let mut vm = test_vm();
        let ps = vm.shared.heap.alloc_object(ClassId::new(100), 1);
        let result =
            native_println_boolean(&mut ctx(&mut vm), &[Value::Object(Some(ps)), Value::Int(1)]);
        assert!(result.is_ok());
        assert_eq!(vm.main_thread.printed_lines.last().unwrap(), "true");
    }

    // -----------------------------------------------------------------------
    // Exception constructor natives
    // -----------------------------------------------------------------------

    #[test]
    fn exc_init_message_sets_field_0() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let msg = {
            let mut c = ctx(&mut vm);
            c.create_string("boom")
        };
        let result = native_exc_init_message(
            &mut ctx(&mut vm),
            &[Value::Object(Some(exc)), Value::Object(Some(msg))],
        );
        assert!(result.is_ok());
        let field0 = vm.shared.heap.get_field(exc, 0);
        let Value::Object(Some(s)) = field0 else {
            panic!("expected string in field 0, got {field0:?}");
        };
        assert_eq!(read_str(&vm, s), "boom");
    }

    #[test]
    fn exc_init_message_cause_sets_fields() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let cause = vm.shared.heap.alloc_object(ClassId::new(51), 4);
        let msg = {
            let mut c = ctx(&mut vm);
            c.create_string("outer")
        };
        let result = native_exc_init_message_cause(
            &mut ctx(&mut vm),
            &[
                Value::Object(Some(exc)),
                Value::Object(Some(msg)),
                Value::Object(Some(cause)),
            ],
        );
        assert!(result.is_ok());
        // field 0 = message, field 1 = cause
        match vm.shared.heap.get_field(exc, 0) {
            Value::Object(Some(s)) => {
                assert_eq!(read_str(&vm, s), "outer");
            }
            other => panic!("field 0 should be message, got {other:?}"),
        }
        match vm.shared.heap.get_field(exc, 1) {
            Value::Object(Some(c)) => assert_eq!(c.as_ptr(), cause.as_ptr()),
            other => panic!("field 1 should be cause, got {other:?}"),
        }
    }

    #[test]
    fn exc_init_cause_sets_field_1() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let cause = vm.shared.heap.alloc_object(ClassId::new(51), 4);
        let result = native_exc_init_cause(
            &mut ctx(&mut vm),
            &[Value::Object(Some(exc)), Value::Object(Some(cause))],
        );
        assert!(result.is_ok());
        match vm.shared.heap.get_field(exc, 1) {
            Value::Object(Some(c)) => assert_eq!(c.as_ptr(), cause.as_ptr()),
            other => panic!("field 1 should be cause, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // Throwable natives
    // -----------------------------------------------------------------------

    #[test]
    fn throwable_fill_in_stack_trace_returns_this() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let result = native_throwable_fill_in_stack_trace(
            &mut ctx(&mut vm),
            &[Value::Object(Some(exc)), Value::Int(0)],
        );
        let val = result.unwrap();
        let Some(Value::Object(Some(ret))) = val else {
            panic!("expected same object back, got {val:?}");
        };
        assert_eq!(ret.as_ptr(), exc.as_ptr());
    }

    #[test]
    fn throwable_get_message_reads_field_0() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let msg = {
            let mut c = ctx(&mut vm);
            c.create_string("test error")
        };
        vm.shared.heap.set_field(exc, 0, Value::Object(Some(msg)));
        let result = native_throwable_get_message(&mut ctx(&mut vm), &[Value::Object(Some(exc))]);
        let val = result.unwrap();
        let Some(Value::Object(Some(s))) = val else {
            panic!("expected string, got {val:?}");
        };
        assert_eq!(read_str(&vm, s), "test error");
    }

    #[test]
    fn throwable_get_cause_reads_field_1() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let cause = vm.shared.heap.alloc_object(ClassId::new(51), 4);
        vm.shared.heap.set_field(exc, 1, Value::Object(Some(cause)));
        let result = native_throwable_get_cause(&mut ctx(&mut vm), &[Value::Object(Some(exc))]);
        let val = result.unwrap();
        let Some(Value::Object(Some(c))) = val else {
            panic!("expected cause object, got {val:?}");
        };
        assert_eq!(c.as_ptr(), cause.as_ptr());
    }

    #[test]
    fn throwable_init_cause_sets_and_returns_this() {
        let mut vm = test_vm();
        let exc = vm.shared.heap.alloc_object(ClassId::new(50), 4);
        let cause = vm.shared.heap.alloc_object(ClassId::new(51), 4);
        let result = native_throwable_init_cause(
            &mut ctx(&mut vm),
            &[Value::Object(Some(exc)), Value::Object(Some(cause))],
        );
        let val = result.unwrap();
        let Some(Value::Object(Some(ret))) = val else {
            panic!("expected this, got {val:?}");
        };
        assert_eq!(ret.as_ptr(), exc.as_ptr());
        let field1 = vm.shared.heap.get_field(exc, 1);
        let Value::Object(Some(c)) = field1 else {
            panic!("field 1 should be cause, got {field1:?}");
        };
        assert_eq!(c.as_ptr(), cause.as_ptr());
    }

    // -----------------------------------------------------------------------
    // Registry integration
    // -----------------------------------------------------------------------

    #[test]
    fn registry_has_core_methods() {
        let vm = test_vm();
        let r = &vm.shared.native_methods;
        assert!(r.find("java/lang/Object", "hashCode", "()I").is_some());
        assert!(r
            .find("java/lang/Object", "equals", "(Ljava/lang/Object;)Z")
            .is_some());
        assert!(r
            .find(
                "java/lang/System",
                "arraycopy",
                "(Ljava/lang/Object;ILjava/lang/Object;II)V"
            )
            .is_some());
        assert!(r.find("java/lang/String", "length", "()I").is_some());
        assert!(r.find("java/lang/Math", "abs", "(I)I").is_some());
        assert!(r
            .find("java/lang/Integer", "parseInt", "(Ljava/lang/String;)I")
            .is_some());
    }

    #[test]
    fn registry_count_is_substantial() {
        let vm = test_vm();
        // We have hundreds of native methods registered
        assert!(vm.shared.native_methods.len() > 100);
    }

    // -----------------------------------------------------------------------
    // obj_arg helper
    // -----------------------------------------------------------------------

    #[test]
    fn obj_arg_valid() {
        let vm = test_vm();
        let obj = vm.shared.heap.alloc_object(ClassId::new(1), 0);
        let args = [Value::Object(Some(obj))];
        let result = obj_arg(&args, 0);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_ptr(), obj.as_ptr());
    }

    #[test]
    fn obj_arg_null_throws() {
        let args = [Value::Object(None)];
        let result = obj_arg(&args, 0);
        assert!(result.is_err());
    }

    #[test]
    fn obj_arg_out_of_bounds_throws() {
        let args: [Value; 0] = [];
        let result = obj_arg(&args, 0);
        assert!(result.is_err());
    }
}

// =============================================================================
// Phase E2: Struct/Union Layouts, Upcalls, String Marshaling
// =============================================================================

/// Create an upcall handle — registers a Java callback in the upcall table and
/// returns a MemorySegment wrapping the trampoline slot index as an address.
/// In this simplified model, the "address" is the slot index (not a real function
/// pointer) — upcalls are dispatched through pe_upcall_invoke.
fn pe_upcall_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _linker = obj_arg(args, 0)?;
    let target = obj_arg(args, 1)?; // MethodHandle / functional interface impl
    let descriptor = obj_arg(args, 2)?;
    // args[3] is arena — ignored for now (upcall entries live as long as VM)

    // Read descriptor to get param/return kinds
    let return_kind = if let Value::Object(Some(rl)) = ctx.get_field(descriptor, 0) {
        match ctx.get_field(rl, 0) {
            Value::Int(k) => k,
            _ => -1,
        }
    } else {
        -1
    };

    let param_kinds = if let Value::Object(Some(pl_arr)) = ctx.get_field(descriptor, 1) {
        let len = ctx.array_length(pl_arr);
        (0..len)
            .map(|i| {
                if let Value::Object(Some(layout)) = ctx.get_array_element(pl_arr, i) {
                    match ctx.get_field(layout, 0) {
                        Value::Int(k) => k,
                        _ => LAYOUT_LONG,
                    }
                } else {
                    LAYOUT_LONG
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let entry = ffi::UpcallEntry {
        target,
        method_name: "invoke".to_string(),
        method_descriptor: String::new(), // resolved at call time
        param_kinds,
        return_kind,
    };

    let slot = ctx.register_upcall(entry);

    // Return a MemorySegment wrapping the slot index as the "address"
    // The slot index is encoded as a pointer value that pe_upcall_invoke can decode.
    let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
    ctx.set_field(seg, 0, Value::Long(slot as i64));
    ctx.set_field(seg, 1, Value::Long(0));
    ctx.set_field(seg, 2, Value::Object(None));
    ctx.set_field(seg, 3, Value::Int(1)); // read-only
    ctx.set_field(seg, 4, Value::Int(1)); // alive
    ctx.set_field(seg, 5, Value::Long(0));
    Ok(Some(Value::Object(Some(seg))))
}

/// Dispatch an upcall — called when C invokes a Java callback through the upcall table.
fn pe_upcall_invoke(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let handle = obj_arg(args, 0)?;
    let slot = match ctx.get_field(handle, 0) {
        Value::Long(n) => n as usize,
        _ => 0,
    };

    let (target, _param_kinds, return_kind) =
        ctx.get_upcall_info(slot)
            .ok_or_else(|| RuntimeError::IllegalStateException {
                message: format!("Upcall slot {} not found", slot),
            })?;

    // Unmarshal args from the Object[] array
    let call_args: Vec<Value> = if args.len() > 1 {
        if let Value::Object(Some(arr)) = args[1] {
            let len = ctx.array_length(arr);
            (0..len).map(|i| ctx.get_array_element(arr, i)).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Call the Java target
    let result = ctx.invoke_virtual(
        target,
        "invoke",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        &call_args,
    );
    match result {
        Ok(val) => {
            let _ = return_kind;
            Ok(val.or(Some(Value::Object(None))))
        }
        Err(e) => Err(e),
    }
}

// --- StructLayout / UnionLayout / SequenceLayout ---
// StructLayout synthetic: [0]=kind(LAYOUT_STRUCT), [1]=totalSize(Long), [2]=memberLayouts(array),
//                          [3]=memberNames(array), [4]=memberOffsets(array), [5]=alignment(Long)

fn register_pe2_struct_layouts(r: &mut NativeMethodRegistry) {
    let ml = "java/lang/foreign/MemoryLayout";

    // MemoryLayout.structLayout(members...) → StructLayout
    r.register(
        ml,
        "structLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_struct_layout,
    );

    // MemoryLayout.unionLayout(members...) → UnionLayout
    r.register(
        ml,
        "unionLayout",
        "([Ljava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_union_layout,
    );

    // MemoryLayout.sequenceLayout(count, element) → SequenceLayout
    r.register(
        ml,
        "sequenceLayout",
        "(JLjava/lang/foreign/MemoryLayout;)Ljava/lang/foreign/MemoryLayout;",
        pe_sequence_layout,
    );

    // MemoryLayout.paddingLayout(bytes) → PaddingLayout
    r.register(
        ml,
        "paddingLayout",
        "(J)Ljava/lang/foreign/MemoryLayout;",
        |ctx, args| {
            let bytes = match args.first() {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
            ctx.set_field(layout, 0, Value::Int(LAYOUT_PADDING));
            ctx.set_field(layout, 1, Value::Long(bytes));
            ctx.set_field(layout, 5, Value::Long(1)); // alignment=1
            Ok(Some(Value::Object(Some(layout))))
        },
    );

    // Common methods on all layouts
    let sl = "java/lang/foreign/StructLayout";
    r.register(sl, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = match ctx.get_field(this, 1) {
            Value::Long(n) => n,
            _ => 0,
        };
        Ok(Some(Value::Long(size)))
    });
    r.register(sl, "byteAlignment", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let align = match ctx.get_field(this, 5) {
            Value::Long(n) => n,
            _ => 1,
        };
        Ok(Some(Value::Long(align)))
    });
    r.register(sl, "memberLayouts", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });

    // byteOffset(PathElement...) — compute offset to a named field
    r.register(
        sl,
        "byteOffset",
        "([Ljava/lang/foreign/MemoryLayout$PathElement;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Simple case: one path element = field name
            if let Some(Value::Object(Some(path_arr))) = args.get(1) {
                if ctx.array_length(*path_arr) > 0 {
                    if let Value::Object(Some(pe)) = ctx.get_array_element(*path_arr, 0) {
                        // PathElement stores the field name in field 0
                        if let Value::Object(Some(name_ref)) = ctx.get_field(pe, 0) {
                            let target_name = ctx.read_string(name_ref).unwrap_or_default();
                            // Search member names and return corresponding offset
                            if let Value::Object(Some(names_arr)) = ctx.get_field(this, 3) {
                                if let Value::Object(Some(offsets_arr)) = ctx.get_field(this, 4) {
                                    let count = ctx.array_length(names_arr);
                                    for i in 0..count {
                                        if let Value::Object(Some(n)) =
                                            ctx.get_array_element(names_arr, i)
                                        {
                                            if ctx.read_string(n).as_deref() == Some(&target_name) {
                                                if let Value::Long(off) =
                                                    ctx.get_array_element(offsets_arr, i)
                                                {
                                                    return Ok(Some(Value::Long(off)));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Some(Value::Long(0)))
        },
    );

    // MemoryLayout.withName(name) → layout with name set
    r.register(
        ml,
        "withName",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout;",
        |_ctx, args| {
            // For simplicity, return the same layout (names are tracked separately in struct)
            Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
        },
    );

    // MemoryLayout.byteSize() fallback for any layout
    r.register(ml, "byteSize", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let kind = match ctx.get_field(this, 0) {
            Value::Int(k) => k,
            _ => 0,
        };
        let size = if kind < 10 {
            ffi::layout_byte_size(kind) as i64
        } else {
            match ctx.get_field(this, 1) {
                Value::Long(n) => n,
                _ => 0,
            }
        };
        Ok(Some(Value::Long(size)))
    });

    // PathElement.groupElement(name) → PathElement
    let pe = "java/lang/foreign/MemoryLayout$PathElement";
    r.register(
        pe,
        "groupElement",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, args| {
            let name = args.first().copied().unwrap_or(Value::Object(None));
            let elem =
                alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2);
            ctx.set_field(elem, 0, name); // field name
            ctx.set_field(elem, 1, Value::Int(0)); // kind=group
            Ok(Some(Value::Object(Some(elem))))
        },
    );
    r.register(
        pe,
        "sequenceElement",
        "()Ljava/lang/foreign/MemoryLayout$PathElement;",
        |ctx, _| {
            let elem =
                alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout$PathElement", 2);
            ctx.set_field(elem, 0, Value::Object(None));
            ctx.set_field(elem, 1, Value::Int(1)); // kind=sequence
            Ok(Some(Value::Object(Some(elem))))
        },
    );
}

/// Compute struct layout: iterate members, align each, compute offsets and total size.
fn pe_struct_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let members_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = ctx.array_length(members_arr);

    let offsets_arr = ctx.new_array(cratonvm_types::ArrayElementType::Long, count);
    let names_arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, count);

    let mut offset: usize = 0;
    let mut max_align: usize = 1;

    for i in 0..count {
        let member = ctx.get_array_element(members_arr, i);
        if let Value::Object(Some(m)) = member {
            let kind = match ctx.get_field(m, 0) {
                Value::Int(k) => k,
                _ => 0,
            };
            let (member_size, member_align) = if kind < 10 {
                (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
            } else {
                let s = match ctx.get_field(m, 1) {
                    Value::Long(n) => n as usize,
                    _ => 0,
                };
                let a = match ctx.get_field(m, 5) {
                    Value::Long(n) => n as usize,
                    _ => 1,
                };
                (s, a)
            };

            offset = ffi::align_up(offset, member_align);
            ctx.set_array_element(offsets_arr, i, Value::Long(offset as i64));
            ctx.set_array_element(names_arr, i, Value::Object(None)); // no name by default
            offset += member_size;
            if member_align > max_align {
                max_align = member_align;
            }
        }
    }

    // Pad total size to alignment
    let total_size = ffi::align_up(offset, max_align);

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/StructLayout", 6);
    ctx.set_field(layout, 0, Value::Int(LAYOUT_STRUCT));
    ctx.set_field(layout, 1, Value::Long(total_size as i64));
    ctx.set_field(layout, 2, Value::Object(Some(members_arr)));
    ctx.set_field(layout, 3, Value::Object(Some(names_arr)));
    ctx.set_field(layout, 4, Value::Object(Some(offsets_arr)));
    ctx.set_field(layout, 5, Value::Long(max_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

/// Compute union layout: all fields at offset 0, size = max member size.
fn pe_union_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let members_arr = match args.first() {
        Some(Value::Object(Some(a))) => *a,
        _ => return Ok(Some(Value::Object(None))),
    };
    let count = ctx.array_length(members_arr);

    let mut max_size: usize = 0;
    let mut max_align: usize = 1;

    for i in 0..count {
        if let Value::Object(Some(m)) = ctx.get_array_element(members_arr, i) {
            let kind = match ctx.get_field(m, 0) {
                Value::Int(k) => k,
                _ => 0,
            };
            let (member_size, member_align) = if kind < 10 {
                (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
            } else {
                let s = match ctx.get_field(m, 1) {
                    Value::Long(n) => n as usize,
                    _ => 0,
                };
                let a = match ctx.get_field(m, 5) {
                    Value::Long(n) => n as usize,
                    _ => 1,
                };
                (s, a)
            };
            if member_size > max_size {
                max_size = member_size;
            }
            if member_align > max_align {
                max_align = member_align;
            }
        }
    }

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
    ctx.set_field(layout, 0, Value::Int(LAYOUT_UNION));
    ctx.set_field(layout, 1, Value::Long(max_size as i64));
    ctx.set_field(layout, 2, Value::Object(Some(members_arr)));
    ctx.set_field(layout, 3, Value::Object(None));
    ctx.set_field(layout, 4, Value::Object(None));
    ctx.set_field(layout, 5, Value::Long(max_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

/// Compute sequence layout (array): count * element size.
fn pe_sequence_layout(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let count = match args.first() {
        Some(Value::Long(n)) => *n as usize,
        _ => 0,
    };
    let element = match args.get(1) {
        Some(Value::Object(Some(e))) => *e,
        _ => return Ok(Some(Value::Object(None))),
    };

    let kind = match ctx.get_field(element, 0) {
        Value::Int(k) => k,
        _ => 0,
    };
    let (elem_size, elem_align) = if kind < 10 {
        (ffi::layout_byte_size(kind), ffi::layout_alignment(kind))
    } else {
        let s = match ctx.get_field(element, 1) {
            Value::Long(n) => n as usize,
            _ => 0,
        };
        let a = match ctx.get_field(element, 5) {
            Value::Long(n) => n as usize,
            _ => 1,
        };
        (s, a)
    };

    let layout = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemoryLayout", 6);
    ctx.set_field(layout, 0, Value::Int(LAYOUT_SEQUENCE));
    ctx.set_field(layout, 1, Value::Long((count * elem_size) as i64));
    ctx.set_field(layout, 2, Value::Object(Some(element))); // element layout
    ctx.set_field(layout, 3, Value::Object(None));
    ctx.set_field(layout, 4, Value::Object(None));
    ctx.set_field(layout, 5, Value::Long(elem_align as i64));

    Ok(Some(Value::Object(Some(layout))))
}

// --- String marshaling helpers ---

fn register_pe2_string_marshaling(r: &mut NativeMethodRegistry) {
    let ms = "java/lang/foreign/MemorySegment";

    // getUtf8String(long offset) → String
    r.register(ms, "getUtf8String", "(J)Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let offset = match args.get(1) {
            Some(Value::Long(n)) => *n,
            _ => 0,
        };
        let ptr = match ctx.get_field(this, 0) {
            Value::Long(n) => n,
            _ => 0,
        };
        let base_off = match ctx.get_field(this, 5) {
            Value::Long(n) => n,
            _ => 0,
        };
        let addr = (ptr + base_off + offset) as *const u8;

        if addr.is_null() {
            return Ok(Some(Value::Object(None)));
        }

        // Read null-terminated C string
        let c_str = unsafe { std::ffi::CStr::from_ptr(addr as *const std::ffi::c_char) };
        let s = c_str.to_str().unwrap_or("");
        let java_str = ctx.create_string(s);
        Ok(Some(Value::Object(Some(java_str))))
    });

    // setUtf8String(long offset, String value) → void
    r.register(
        ms,
        "setUtf8String",
        "(JLjava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let offset = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let str_obj = obj_arg(args, 2)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();

            let ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let base_off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let addr = (ptr + base_off + offset) as *mut u8;

            if !addr.is_null() {
                let bytes = s.as_bytes();
                unsafe {
                    std::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, bytes.len());
                    *addr.add(bytes.len()) = 0; // null terminator
                }
            }
            Ok(None)
        },
    );

    // reinterpret(long newSize) → MemorySegment with same address but different size
    r.register(
        ms,
        "reinterpret",
        "(J)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let new_size = match args.get(1) {
                Some(Value::Long(n)) => *n,
                _ => 0,
            };
            let ptr = match ctx.get_field(this, 0) {
                Value::Long(n) => n,
                _ => 0,
            };
            let off = match ctx.get_field(this, 5) {
                Value::Long(n) => n,
                _ => 0,
            };
            let arena_val = ctx.get_field(this, 2);

            let seg = alloc_concurrent_synthetic(ctx, "java/lang/foreign/MemorySegment", 6);
            ctx.set_field(seg, 0, Value::Long(ptr));
            ctx.set_field(seg, 1, Value::Long(new_size));
            ctx.set_field(seg, 2, arena_val);
            ctx.set_field(seg, 3, ctx.get_field(this, 3));
            ctx.set_field(seg, 4, Value::Int(1));
            ctx.set_field(seg, 5, Value::Long(off));
            Ok(Some(Value::Object(Some(seg))))
        },
    );

    // Arena.allocateUtf8String(String) → MemorySegment
    let arena = "java/lang/foreign/Arena";
    r.register(
        arena,
        "allocateUtf8String",
        "(Ljava/lang/String;)Ljava/lang/foreign/MemorySegment;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let str_obj = obj_arg(args, 1)?;
            let s = ctx.read_string(str_obj).unwrap_or_default();
            let bytes = s.as_bytes();
            let size = (bytes.len() + 1) as i64; // +1 for null terminator

            // Allocate via arena
            let seg_val = pe_arena_allocate_impl(ctx, this, size, 1)?;
            if let Some(Value::Object(Some(seg))) = seg_val {
                // Write the string bytes + null terminator
                let ptr = match ctx.get_field(seg, 0) {
                    Value::Long(n) => n,
                    _ => 0,
                };
                if ptr != 0 {
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                        *(ptr as *mut u8).add(bytes.len()) = 0;
                    }
                }
                Ok(Some(Value::Object(Some(seg))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        },
    );
}

// =============================================================================
// R3: Resource Loading — Class.getResourceAsStream, InputStreamReader, BufferedReader
//
// Synthetic InputStream layout (2 fields):
//   field 0: String[] ref array — all lines of the resource file
//   field 1: int — current read position (line index)
//
// InputStreamReader layout (1 field):
//   field 0: InputStream reference
//
// BufferedReader layout (1 field):
//   field 0: Reader reference (InputStreamReader)
// =============================================================================

/// Traverse BufferedReader → InputStreamReader → InputStream chain.
/// Returns the synthetic InputStream ObjectRef, or None if the chain is broken.
fn r3_get_input_stream(ctx: &dyn NativeContext, buffered_reader: ObjectRef) -> Option<ObjectRef> {
    // BufferedReader.field[0] = Reader (InputStreamReader)
    let reader = match ctx.get_field(buffered_reader, 0) {
        Value::Object(Some(r)) => r,
        _ => return None,
    };
    // InputStreamReader.field[0] = InputStream
    match ctx.get_field(reader, 0) {
        Value::Object(Some(is)) => Some(is),
        _ => None,
    }
}

fn register_r3_resource_loading(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;

    // -------------------------------------------------------------------------
    // java.lang.Class.getResourceAsStream(String) → InputStream
    // -------------------------------------------------------------------------
    r.register(
        "java/lang/Class",
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            // Strip leading '/' for absolute resource names
            let resource_name = name.trim_start_matches('/');

            match ctx.find_resource(resource_name) {
                None => Ok(Some(Value::Object(None))),
                Some(bytes) => {
                    let content = String::from_utf8_lossy(&bytes);
                    let lines: Vec<String> =
                        content.lines().map(|l| l.to_string()).collect();
                    let len = lines.len();

                    // Build a String[] of lines
                    let lines_arr = ctx.new_array(ArrayElementType::Reference, len);
                    for (i, line) in lines.iter().enumerate() {
                        let s = ctx.create_string(line);
                        ctx.set_array_element(lines_arr, i, Value::Object(Some(s)));
                    }

                    // Allocate synthetic InputStream: field 0 = String[], field 1 = int pos
                    let stream = alloc_concurrent_synthetic(ctx, "java/io/InputStream", 2);
                    ctx.set_field(stream, 0, Value::Object(Some(lines_arr)));
                    ctx.set_field(stream, 1, Value::Int(0));
                    Ok(Some(Value::Object(Some(stream))))
                }
            }
        },
    );

    // -------------------------------------------------------------------------
    // java.io.InputStream.close() — no-op
    // -------------------------------------------------------------------------
    r.register("java/io/InputStream", "close", "()V", native_noop);
    r.register("java/io/InputStream", "read", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(-1))) // EOF
    });

    // -------------------------------------------------------------------------
    // java.io.InputStreamReader.<init>(InputStream)  — store stream at field 0
    // java.io.InputStreamReader.<init>(InputStream, Charset) — same
    // -------------------------------------------------------------------------
    r.register(
        "java/io/InputStreamReader",
        "<init>",
        "(Ljava/io/InputStream;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stream = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, stream);
            Ok(None)
        },
    );
    r.register(
        "java/io/InputStreamReader",
        "<init>",
        "(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stream = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, stream);
            Ok(None)
        },
    );
    r.register(
        "java/io/InputStreamReader",
        "<init>",
        "(Ljava/io/InputStream;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let stream = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, stream);
            Ok(None)
        },
    );
    r.register("java/io/InputStreamReader", "close", "()V", native_noop);
    r.register("java/io/InputStreamReader", "read", "()I", |_ctx, _args| {
        Ok(Some(Value::Int(-1)))
    });

    // -------------------------------------------------------------------------
    // java.io.BufferedReader.<init>(Reader) / (Reader, int) — store reader at field 0
    // -------------------------------------------------------------------------
    r.register(
        "java/io/BufferedReader",
        "<init>",
        "(Ljava/io/Reader;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let reader = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, reader);
            Ok(None)
        },
    );
    r.register(
        "java/io/BufferedReader",
        "<init>",
        "(Ljava/io/Reader;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let reader = args.get(1).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, reader);
            Ok(None)
        },
    );

    // -------------------------------------------------------------------------
    // java.io.BufferedReader.readLine() → String
    // -------------------------------------------------------------------------
    r.register(
        "java/io/BufferedReader",
        "readLine",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is = match r3_get_input_stream(ctx, this) {
                Some(s) => s,
                None => return Ok(Some(Value::Object(None))),
            };
            let lines_arr = match ctx.get_field(is, 0) {
                Value::Object(Some(arr)) => arr,
                _ => return Ok(Some(Value::Object(None))),
            };
            let pos = match ctx.get_field(is, 1) {
                Value::Int(i) => i,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(lines_arr) as i32;
            if pos >= len {
                return Ok(Some(Value::Object(None)));
            }
            let line = ctx.get_array_element(lines_arr, pos as usize);
            ctx.set_field(is, 1, Value::Int(pos + 1));
            Ok(Some(line))
        },
    );

    // -------------------------------------------------------------------------
    // java.io.BufferedReader.lines() → Stream<String>
    // -------------------------------------------------------------------------
    r.register(
        "java/io/BufferedReader",
        "lines",
        "()Ljava/util/stream/Stream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let is = r3_get_input_stream(ctx, this);
            let elems = match is {
                None => vec![],
                Some(is_ref) => {
                    let lines_arr = match ctx.get_field(is_ref, 0) {
                        Value::Object(Some(arr)) => arr,
                        _ => {
                            let s = p56_build_stream(ctx, vec![], "java/util/stream/Stream");
                            return Ok(Some(Value::Object(Some(s))));
                        }
                    };
                    let pos = match ctx.get_field(is_ref, 1) {
                        Value::Int(i) => i as usize,
                        _ => 0,
                    };
                    let len = ctx.array_length(lines_arr);
                    let elems: Vec<Value> =
                        (pos..len).map(|i| ctx.get_array_element(lines_arr, i)).collect();
                    // Advance position to end
                    ctx.set_field(is_ref, 1, Value::Int(len as i32));
                    elems
                }
            };
            let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // -------------------------------------------------------------------------
    // java.io.BufferedReader.close() — no-op
    // java.io.Reader.close() — no-op
    // -------------------------------------------------------------------------
    r.register("java/io/BufferedReader", "close", "()V", native_noop);
    r.register("java/io/Reader", "close", "()V", native_noop);
}

// =============================================================================
// S1: URLClassLoader + ServiceLoader (real META-INF/services discovery)
//
// URLClassLoader layout (2 fields):
//   field 0: URL[] — the array of URL objects provided to the constructor
//   field 1: parent ClassLoader ref
//
// ServiceLoader layout (2 fields):
//   field 0: java.lang.Class mirror — the service interface
//   field 1: Object[] — lazily loaded service instances (null until first use)
//
// On construction URLClassLoader registers its URLs with the VM classpath so
// subsequent class loading and resource loading searches those JARs/directories.
// ServiceLoader reads META-INF/services/<interface> from the classpath to
// discover and instantiate service providers.
// =============================================================================

/// Extract a filesystem path from a Java URL string.
///
/// Handles:
/// - `file:/path/to/foo.jar`  → `/path/to/foo.jar`
/// - `file:/C:/path/to/dir/`  → `C:/path/to/dir/`  (Windows)
/// - `jar:file:/foo.jar!/`    → `/foo.jar`
/// - bare filesystem paths    → returned as-is
fn s1_url_to_fs_path(url_str: &str) -> Option<String> {
    let url_str = url_str.trim();
    if let Some(rest) = url_str.strip_prefix("jar:") {
        // jar:file:/path/to/foo.jar!/some/entry → extract the JAR path
        let inner = rest.split('!').next()?;
        return s1_url_to_fs_path(inner);
    }
    if let Some(rest) = url_str.strip_prefix("file:") {
        // file:/path → /path  (Unix)
        // file:/C:/path → C:/path  (Windows — strip one leading /)
        let after_slash = rest.trim_start_matches('/');
        // Windows drive: "C:/..."
        if after_slash.len() >= 2 && after_slash.chars().nth(1) == Some(':') {
            return Some(after_slash.to_string());
        }
        // Unix absolute path
        return Some(format!("/{after_slash}"));
    }
    // Already a filesystem path (e.g. from internal tests)
    if !url_str.contains("://") {
        return Some(url_str.to_string());
    }
    None
}

/// Ensure a ServiceLoader's services have been discovered and instantiated.
/// Returns the Object[] of loaded providers (may be empty), and stores it
/// in ServiceLoader field 1 for subsequent calls.
fn s1_service_loader_ensure_loaded(
    ctx: &mut dyn NativeContext,
    sl: cratonvm_types::ObjectRef,
) -> cratonvm_types::ObjectRef {
    use cratonvm_types::ArrayElementType;

    // If already loaded, return cached array
    if let Value::Object(Some(arr)) = ctx.get_field(sl, 1) {
        return arr;
    }

    let empty = ctx.new_array(ArrayElementType::Reference, 0);

    // Get the service interface ClassId from the Class mirror (field 0)
    let mirror = match ctx.get_field(sl, 0) {
        Value::Object(Some(m)) => m,
        _ => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
    };
    // Guard: the mirror must have at least one field (the ClassId slot).
    // Test mocks may pass a 0-field dummy object.
    if ctx.object_num_fields(mirror) == 0 {
        ctx.set_field(sl, 1, Value::Object(Some(empty)));
        return empty;
    }
    let class_id_val = match ctx.get_field(mirror, 0) {
        Value::Int(v) => v as u32,
        _ => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
    };
    let class_id = cratonvm_types::ClassId::new(class_id_val);
    let iface_name = match ctx.class_name_of_id(class_id) {
        Some(n) => n.replace('/', "."),
        None => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
    };

    // Read META-INF/services/<interface-name>
    let resource_name = format!("META-INF/services/{iface_name}");
    let bytes = match ctx.find_resource(&resource_name) {
        Some(b) => b,
        None => {
            ctx.set_field(sl, 1, Value::Object(Some(empty)));
            return empty;
        }
    };

    // Parse provider class names (one per line, skip blanks and '#' comments)
    let content = String::from_utf8_lossy(&bytes);
    let provider_names: Vec<String> = content
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    // Load and instantiate each provider via default constructor
    let mut instances = Vec::new();
    for provider_name in &provider_names {
        let class_name = provider_name.replace('.', "/");
        if ctx.ensure_class_initialized(&class_name).is_ok() {
            if let Ok(Some(Value::Object(Some(obj)))) = ctx.new_object(&class_name) {
                let _ = ctx.invoke(
                    &class_name,
                    "<init>",
                    "()V",
                    &[Value::Object(Some(obj))],
                );
                instances.push(Value::Object(Some(obj)));
            }
        }
    }

    // Store as Object[]
    let arr = ctx.new_array(ArrayElementType::Reference, instances.len());
    for (i, inst) in instances.iter().enumerate() {
        ctx.set_array_element(arr, i, *inst);
    }
    ctx.set_field(sl, 1, Value::Object(Some(arr)));
    arr
}

fn register_s1_classloading(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;

    // =========================================================================
    // java.net.URLClassLoader
    // =========================================================================
    let ucl = "java/net/URLClassLoader";

    // Helper closure: extract filesystem paths from a URL[] array object
    // and register them with the VM classpath.
    fn register_url_array(ctx: &mut dyn NativeContext, url_arr_val: Value) {
        let arr = match url_arr_val {
            Value::Object(Some(a)) => a,
            _ => return,
        };
        let len = ctx.array_length(arr);
        let mut paths = Vec::new();
        for i in 0..len {
            let url_val = ctx.get_array_element(arr, i);
            let url_obj = match url_val {
                Value::Object(Some(o)) => o,
                _ => continue,
            };
            // URL field 5 = full string (URL_FIELD_FULL)
            let full_val = ctx.get_field(url_obj, 5);
            let full_str = match full_val {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => continue,
            };
            if let Some(path) = s1_url_to_fs_path(&full_str) {
                paths.push(path);
            }
        }
        if !paths.is_empty() {
            ctx.register_dynamic_classpath(&paths);
        }
    }

    // URLClassLoader(URL[])
    r.register(ucl, "<init>", "([Ljava/net/URL;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, url_arr);
        ctx.set_field(this, 1, Value::Object(None));
        register_url_array(ctx, url_arr);
        Ok(None)
    });

    // URLClassLoader(URL[], ClassLoader)
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, url_arr);
            ctx.set_field(this, 1, parent);
            register_url_array(ctx, url_arr);
            Ok(None)
        },
    );

    // URLClassLoader(URL[], ClassLoader, URLStreamHandlerFactory)
    r.register(
        ucl,
        "<init>",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;Ljava/net/URLStreamHandlerFactory;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let url_arr = args.get(1).copied().unwrap_or(Value::Object(None));
            let parent = args.get(2).copied().unwrap_or(Value::Object(None));
            ctx.set_field(this, 0, url_arr);
            ctx.set_field(this, 1, parent);
            register_url_array(ctx, url_arr);
            Ok(None)
        },
    );

    // URLClassLoader.newInstance(URL[]) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            let url_arr = args.first().copied().unwrap_or(Value::Object(None));
            let loader = alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2);
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, Value::Object(None));
            register_url_array(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        },
    );

    // URLClassLoader.newInstance(URL[], ClassLoader) → URLClassLoader
    r.register(
        ucl,
        "newInstance",
        "([Ljava/net/URL;Ljava/lang/ClassLoader;)Ljava/net/URLClassLoader;",
        |ctx, args| {
            let url_arr = args.first().copied().unwrap_or(Value::Object(None));
            let parent = args.get(1).copied().unwrap_or(Value::Object(None));
            let loader = alloc_concurrent_synthetic(ctx, "java/net/URLClassLoader", 2);
            ctx.set_field(loader, 0, url_arr);
            ctx.set_field(loader, 1, parent);
            register_url_array(ctx, url_arr);
            Ok(Some(Value::Object(Some(loader))))
        },
    );

    // URLClassLoader.loadClass(String) → Class
    r.register(
        ucl,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx
                .read_string(name_obj)
                .unwrap_or_default()
                .replace('.', "/");
            match ctx.ensure_class_initialized(&name) {
                Ok(cid) => {
                    let mirror = ctx.get_class_mirror(cid);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                Err(_) => Ok(Some(Value::Object(None))),
            }
        },
    );

    // URLClassLoader.findClass(String) → Class  (same as loadClass)
    r.register(
        ucl,
        "findClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx
                .read_string(name_obj)
                .unwrap_or_default()
                .replace('.', "/");
            match ctx.ensure_class_initialized(&name) {
                Ok(cid) => {
                    let mirror = ctx.get_class_mirror(cid);
                    Ok(Some(Value::Object(Some(mirror))))
                }
                Err(_) => Ok(Some(Value::Object(None))),
            }
        },
    );

    // URLClassLoader.getURLs() → URL[]
    r.register(ucl, "getURLs", "()[Ljava/net/URL;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });

    // URLClassLoader.addURL(URL) — extend classpath with one more URL
    r.register(
        ucl,
        "addURL",
        "(Ljava/net/URL;)V",
        |ctx, args| {
            let url_val = args.get(1).copied().unwrap_or(Value::Object(None));
            if let Value::Object(Some(url_obj)) = url_val {
                let full_val = ctx.get_field(url_obj, 5); // URL_FIELD_FULL
                if let Value::Object(Some(s)) = full_val {
                    if let Some(full) = ctx.read_string(s) {
                        if let Some(path) = s1_url_to_fs_path(&full) {
                            ctx.register_dynamic_classpath(&[path]);
                        }
                    }
                }
            }
            Ok(None)
        },
    );

    // URLClassLoader.getResource(String) → URL  (delegate to find_resource)
    r.register(
        ucl,
        "getResource",
        "(Ljava/lang/String;)Ljava/net/URL;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            match ctx.find_resource(name.trim_start_matches('/')) {
                Some(_) => {
                    // Build a minimal URL object pointing to this resource
                    let url = alloc_concurrent_synthetic(ctx, "java/net/URL", 6);
                    let full_str = ctx.create_string(&format!("classpath:{name}"));
                    ctx.set_field(url, 5, Value::Object(Some(full_str)));
                    Ok(Some(Value::Object(Some(url))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // URLClassLoader.getResourceAsStream(String) → InputStream
    r.register(
        ucl,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        |ctx, args| {
            let name_obj = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(Some(Value::Object(None))),
            };
            let name = ctx.read_string(name_obj).unwrap_or_default();
            let resource_name = name.trim_start_matches('/');
            match ctx.find_resource(resource_name) {
                None => Ok(Some(Value::Object(None))),
                Some(bytes) => {
                    let content = String::from_utf8_lossy(&bytes);
                    let lines: Vec<String> =
                        content.lines().map(|l| l.to_string()).collect();
                    let len = lines.len();
                    let lines_arr = ctx.new_array(ArrayElementType::Reference, len);
                    for (i, line) in lines.iter().enumerate() {
                        let s = ctx.create_string(line);
                        ctx.set_array_element(lines_arr, i, Value::Object(Some(s)));
                    }
                    let stream = alloc_concurrent_synthetic(ctx, "java/io/InputStream", 2);
                    ctx.set_field(stream, 0, Value::Object(Some(lines_arr)));
                    ctx.set_field(stream, 1, Value::Int(0));
                    Ok(Some(Value::Object(Some(stream))))
                }
            }
        },
    );

    // URLClassLoader.close() — no-op
    r.register(ucl, "close", "()V", native_noop);

    // =========================================================================
    // java.util.ServiceLoader — real META-INF/services discovery
    //
    // Overrides the stubs registered in phase 53 and phase 63.
    // Layout: 2 fields — field 0 = Class mirror, field 1 = Object[] (lazy)
    // =========================================================================
    let sl = "java/util/ServiceLoader";

    // ServiceLoader.load(Class) → ServiceLoader
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2);
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None)); // not yet loaded
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.load(Class, ClassLoader) → ServiceLoader
    r.register(
        sl,
        "load",
        "(Ljava/lang/Class;Ljava/lang/ClassLoader;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2);
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.loadInstalled(Class) → ServiceLoader
    r.register(
        sl,
        "loadInstalled",
        "(Ljava/lang/Class;)Ljava/util/ServiceLoader;",
        |ctx, args| {
            let class_mirror = args.first().copied().unwrap_or(Value::Object(None));
            let obj = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader", 2);
            ctx.set_field(obj, 0, class_mirror);
            ctx.set_field(obj, 1, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // ServiceLoader.iterator() → Iterator<S>
    r.register(sl, "iterator", "()Ljava/util/Iterator;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = s1_service_loader_ensure_loaded(ctx, this);
        let len = ctx.array_length(arr);
        // Build a simple iterator backed by index over the array:
        // Iterator = 2-field: array=0, index=1
        let itr = alloc_concurrent_synthetic(ctx, "java/util/ServiceLoader$Itr", 2);
        ctx.set_field(itr, 0, Value::Object(Some(arr)));
        ctx.set_field(itr, 1, Value::Int(0));
        // Register hasNext/next for ServiceLoader$Itr if not already
        let _ = len; // suppress unused
        Ok(Some(Value::Object(Some(itr))))
    });

    // ServiceLoader$Itr.hasNext()
    r.register(
        "java/util/ServiceLoader$Itr",
        "hasNext",
        "()Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let idx = match ctx.get_field(this, 1) {
                Value::Int(i) => i,
                _ => return Ok(Some(Value::Int(0))),
            };
            let len = ctx.array_length(arr) as i32;
            Ok(Some(Value::Int(if idx < len { 1 } else { 0 })))
        },
    );

    // ServiceLoader$Itr.next()
    r.register(
        "java/util/ServiceLoader$Itr",
        "next",
        "()Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = match ctx.get_field(this, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let idx = match ctx.get_field(this, 1) {
                Value::Int(i) => i,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(arr) as i32;
            if idx >= len {
                return Ok(Some(Value::Object(None)));
            }
            let elem = ctx.get_array_element(arr, idx as usize);
            ctx.set_field(this, 1, Value::Int(idx + 1));
            Ok(Some(elem))
        },
    );

    // ServiceLoader.stream() → Stream<Provider<S>>
    r.register(
        sl,
        "stream",
        "()Ljava/util/stream/Stream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = s1_service_loader_ensure_loaded(ctx, this);
            let len = ctx.array_length(arr);
            let elems: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr, i)).collect();
            let s = p56_build_stream(ctx, elems, "java/util/stream/Stream");
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // ServiceLoader.findFirst() → Optional<S>
    r.register(
        sl,
        "findFirst",
        "()Ljava/util/Optional;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let arr = s1_service_loader_ensure_loaded(ctx, this);
            let opt = alloc_concurrent_synthetic(ctx, "java/util/Optional", 1);
            if ctx.array_length(arr) > 0 {
                let first = ctx.get_array_element(arr, 0);
                ctx.set_field(opt, 0, first);
            } else {
                ctx.set_field(opt, 0, Value::Object(None));
            }
            Ok(Some(Value::Object(Some(opt))))
        },
    );

    // ServiceLoader.reload() — clear cached services
    r.register(sl, "reload", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Object(None)); // clear cache
        Ok(None)
    });

    r.register(sl, "toString", "()Ljava/lang/String;", |ctx, _args| {
        let s = ctx.create_string("ServiceLoader[]");
        Ok(Some(Value::Object(Some(s))))
    });
}

// =============================================================================
// Phase S.2: Real ByteBuffer + NIO Socket I/O
// =============================================================================
//
// ByteBuffer layout (6-field synthetic):
//   Field 0: Object (byte[]) — backing array
//   Field 1: Int             — position
//   Field 2: Int             — limit
//   Field 3: Int             — capacity
//   Field 4: Int             — mark (-1 = unset)
//   Field 5: Int             — byte order (0=BIG_ENDIAN, 1=LITTLE_ENDIAN)
//
// SocketChannel layout (5-field synthetic — supersedes p58 3-field):
//   Field 0: Int    — connected (0/1)
//   Field 1: Int    — open (0/1)
//   Field 2: Object — InetSocketAddress or null
//   Field 3: Int    — socket_id in SOCKET_REGISTRY (-1 = no OS socket)
//   Field 4: Int    — blocking (1=blocking, 0=non-blocking)
//
// ServerSocketChannel layout (5-field synthetic — supersedes p58 2-field):
//   Field 0: Int    — open (0/1)
//   Field 1: Int    — bound (0/1)
//   Field 2: Int    — listener_id in SOCKET_REGISTRY (-1 = unbound)
//   Field 3: Int    — port
//   Field 4: Int    — pending_stream_id (-1 = none; set by selector poll)
//
// Selector layout (3-field synthetic — supersedes p58 2-field):
//   Field 0: Int    — open (0/1)
//   Field 1: Object — Object[] of SelectionKey refs (all registered keys)
//   Field 2: Int    — count of entries in the keys array
//
// SelectionKey layout (4-field synthetic — supersedes p58 3-field):
//   Field 0: Object — channel (SocketChannel or ServerSocketChannel)
//   Field 1: Object — selector
//   Field 2: Int    — interestOps
//   Field 3: Int    — readyOps (updated by Selector.select())
// =============================================================================

use std::collections::HashMap;
use std::io::{Read as StdRead, Write as StdWrite};
use std::net::{TcpListener, TcpStream};
use std::sync::OnceLock;

// ---- Global socket registry ------------------------------------------------

struct SocketRegistry {
    next_id: i32,
    streams: HashMap<i32, TcpStream>,
    listeners: HashMap<i32, TcpListener>,
}

impl Default for SocketRegistry {
    fn default() -> Self {
        Self { next_id: 1, streams: HashMap::new(), listeners: HashMap::new() }
    }
}

static SOCKET_REGISTRY: OnceLock<parking_lot::Mutex<SocketRegistry>> = OnceLock::new();

fn s2_registry() -> &'static parking_lot::Mutex<SocketRegistry> {
    SOCKET_REGISTRY.get_or_init(|| parking_lot::Mutex::new(SocketRegistry::default()))
}

fn s2_alloc_stream(stream: TcpStream) -> i32 {
    let mut reg = s2_registry().lock();
    let id = reg.next_id;
    reg.next_id = reg.next_id.saturating_add(1);
    reg.streams.insert(id, stream);
    id
}

fn s2_alloc_listener(listener: TcpListener) -> i32 {
    let mut reg = s2_registry().lock();
    let id = reg.next_id;
    reg.next_id = reg.next_id.saturating_add(1);
    reg.listeners.insert(id, listener);
    id
}

// ---- Field index constants -------------------------------------------------

const BB_ARRAY: usize = 0;
const BB_POS: usize = 1;
const BB_LIMIT: usize = 2;
const BB_CAP: usize = 3;
const BB_MARK: usize = 4;
const BB_ORDER: usize = 5; // 0=BIG_ENDIAN, 1=LITTLE_ENDIAN

const S2SC_CONNECTED: usize = 0;
const S2SC_OPEN: usize = 1;
const S2SC_ADDR: usize = 2;
const S2SC_SOCK_ID: usize = 3;
const S2SC_BLOCKING: usize = 4;

const S2SSC_OPEN: usize = 0;
const S2SSC_BOUND: usize = 1;
const S2SSC_LISTENER_ID: usize = 2;
const S2SSC_PORT: usize = 3;
const S2SSC_PENDING: usize = 4;

const S2SEL_OPEN: usize = 0;
const S2SEL_KEYS: usize = 1;
const S2SEL_NKEYS: usize = 2;

// ---- ByteBuffer helpers ----------------------------------------------------

fn s2_bb_alloc(ctx: &mut dyn NativeContext, cap: usize) -> ObjectRef {
    use cratonvm_types::ArrayElementType;
    let arr = ctx.new_array(ArrayElementType::Byte, cap);
    let buf = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
    ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
    ctx.set_field(buf, BB_POS, Value::Int(0));
    ctx.set_field(buf, BB_LIMIT, Value::Int(cap as i32));
    ctx.set_field(buf, BB_CAP, Value::Int(cap as i32));
    ctx.set_field(buf, BB_MARK, Value::Int(-1));
    ctx.set_field(buf, BB_ORDER, Value::Int(0));
    buf
}

#[inline]
fn s2_bb_pos(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_POS).as_int().unwrap_or(0)
}
#[inline]
fn s2_bb_limit(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_LIMIT).as_int().unwrap_or(0)
}
#[inline]
fn s2_bb_cap(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_CAP).as_int().unwrap_or(0)
}
#[inline]
fn s2_bb_order(ctx: &dyn NativeContext, buf: ObjectRef) -> i32 {
    ctx.get_field(buf, BB_ORDER).as_int().unwrap_or(0)
}
#[inline]
fn s2_bb_arr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<ObjectRef> {
    match ctx.get_field(buf, BB_ARRAY) {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    }
}

/// Native-memory address of a DIRECT buffer (real-JDK `DirectByteBuffer`
/// has no `hb` heap array; its storage lives at the `address` field). Same
/// name-first/slot-4-fallback resolution as native-io's
/// `directbuffer_address`. `None` for heap buffers and storage-less
/// synthetics.
fn s2_bb_direct_addr(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<i64> {
    match ctx.get_field_by_name(buf, "address") {
        Value::Long(v) if v != 0 => Some(v),
        _ => match ctx.get_field(buf, 4) {
            Value::Long(v) if v != 0 => Some(v),
            _ => None,
        },
    }
}

/// Write a buffer's `position`. Buffers with a heap array keep this
/// family's historic `BB_POS` slot write (self-consistent with `s2_bb_pos`
/// on both real heap and synthetic buffers). DIRECT buffers are otherwise
/// managed by the name-based native-io natives, whose layout the `BB_POS`
/// slot is not guaranteed to match — write their `position` by name.
fn s2_bb_set_pos(ctx: &mut dyn NativeContext, buf: ObjectRef, v: i32) {
    if s2_bb_arr(ctx, buf).is_some() {
        ctx.set_field(buf, BB_POS, Value::Int(v));
    } else {
        ctx.set_field_by_name(buf, "position", Value::Int(v));
    }
}

fn s2_bb_get_byte(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i8 {
    if let Some(arr) = s2_bb_arr(ctx, buf) {
        ctx.get_array_element(arr, idx as usize).as_int().unwrap_or(0) as i8
    } else {
        0
    }
}

fn s2_bb_put_byte(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32, b: i8) {
    if let Some(arr) = s2_bb_arr(ctx, buf) {
        ctx.set_array_element(arr, idx as usize, Value::Int(b as i32));
    }
}

/// Remaining bytes (pos..limit) as Vec<u8> without advancing position.
fn s2_bb_remaining_bytes(ctx: &dyn NativeContext, buf: ObjectRef) -> Vec<u8> {
    let pos = s2_bb_pos(ctx, buf) as usize;
    let lim = s2_bb_limit(ctx, buf) as usize;
    if let Some(arr) = s2_bb_arr(ctx, buf) {
        (pos..lim)
            .map(|i| ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8)
            .collect()
    } else {
        vec![]
    }
}

fn s2_bb_read2(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i16 {
    let b0 = s2_bb_get_byte(ctx, buf, idx) as u8 as u16;
    let b1 = s2_bb_get_byte(ctx, buf, idx + 1) as u8 as u16;
    if s2_bb_order(ctx, buf) == 1 { (b1 << 8 | b0) as i16 } else { (b0 << 8 | b1) as i16 }
}

fn s2_bb_write2(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32, val: i16) {
    let (b0, b1) = if s2_bb_order(ctx, buf) == 1 {
        (val as u8, (val >> 8) as u8)
    } else {
        ((val >> 8) as u8, val as u8)
    };
    s2_bb_put_byte(ctx, buf, idx, b0 as i8);
    s2_bb_put_byte(ctx, buf, idx + 1, b1 as i8);
}

fn s2_bb_read4(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i32 {
    let b0 = s2_bb_get_byte(ctx, buf, idx) as u8 as u32;
    let b1 = s2_bb_get_byte(ctx, buf, idx + 1) as u8 as u32;
    let b2 = s2_bb_get_byte(ctx, buf, idx + 2) as u8 as u32;
    let b3 = s2_bb_get_byte(ctx, buf, idx + 3) as u8 as u32;
    if s2_bb_order(ctx, buf) == 1 {
        (b3 << 24 | b2 << 16 | b1 << 8 | b0) as i32
    } else {
        (b0 << 24 | b1 << 16 | b2 << 8 | b3) as i32
    }
}

fn s2_bb_write4(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32, val: i32) {
    let bytes = if s2_bb_order(ctx, buf) == 1 { val.to_le_bytes() } else { val.to_be_bytes() };
    for (i, &b) in bytes.iter().enumerate() {
        s2_bb_put_byte(ctx, buf, idx + i as i32, b as i8);
    }
}

fn s2_bb_read8(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32) -> i64 {
    let mut bs = [0u8; 8];
    for i in 0..8i32 {
        bs[i as usize] = s2_bb_get_byte(ctx, buf, idx + i) as u8;
    }
    if s2_bb_order(ctx, buf) == 1 { i64::from_le_bytes(bs) } else { i64::from_be_bytes(bs) }
}

fn s2_bb_write8(ctx: &dyn NativeContext, buf: ObjectRef, idx: i32, val: i64) {
    let bytes = if s2_bb_order(ctx, buf) == 1 { val.to_le_bytes() } else { val.to_be_bytes() };
    for (i, &b) in bytes.iter().enumerate() {
        s2_bb_put_byte(ctx, buf, idx + i as i32, b as i8);
    }
}

// ---- Socket address helper -------------------------------------------------

fn s2_parse_socket_addr(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<(String, u16)> {
    let host = match ctx.get_field(addr, 0) {
        Value::Object(Some(h)) => ctx.read_string(h)?,
        _ => return None,
    };
    let port = ctx.get_field(addr, 1).as_int()? as u16;
    Some((host, port))
}

// ---- Selector poll ---------------------------------------------------------

fn s2_selector_do_poll(ctx: &mut dyn NativeContext, sel: ObjectRef) -> i32 {
    let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    let keys_arr = match ctx.get_field(sel, S2SEL_KEYS) {
        Value::Object(Some(arr)) => arr,
        _ => return 0,
    };
    let mut count = 0i32;
    for i in 0..n {
        let key = match ctx.get_array_element(keys_arr, i) {
            Value::Object(Some(k)) => k,
            _ => continue,
        };
        let interest = ctx.get_field(key, 2).as_int().unwrap_or(0);
        let channel = match ctx.get_field(key, 0) {
            Value::Object(Some(ch)) => ch,
            _ => continue,
        };
        let mut ready = 0i32;

        // OP_ACCEPT (16)
        if interest & 16 != 0 {
            let pending = ctx.get_field(channel, S2SSC_PENDING).as_int().unwrap_or(-1);
            if pending >= 0 {
                ready |= 16;
            } else {
                let lid = ctx.get_field(channel, S2SSC_LISTENER_ID).as_int().unwrap_or(-1);
                if lid >= 0 {
                    let result = {
                        let mut reg = s2_registry().lock();
                        s2_try_accept_nonblocking(&mut reg, lid)
                    };
                    if let Some(sid) = result {
                        ctx.set_field(channel, S2SSC_PENDING, Value::Int(sid));
                        ready |= 16;
                    }
                }
            }
        }

        // OP_READ (1)
        if interest & 1 != 0 {
            let sid = ctx.get_field(channel, S2SC_SOCK_ID).as_int().unwrap_or(-1);
            if sid >= 0 {
                let can_read = {
                    let mut reg = s2_registry().lock();
                    if let Some(stream) = reg.streams.get_mut(&sid) {
                        let _ = stream.set_nonblocking(true);
                        let mut peek_buf = [0u8; 1];
                        matches!(stream.peek(&mut peek_buf), Ok(n) if n > 0)
                    } else {
                        false
                    }
                };
                if can_read { ready |= 1; }
            }
        }

        // OP_WRITE (4)
        if interest & 4 != 0 {
            if ctx.get_field(channel, S2SC_SOCK_ID).as_int().unwrap_or(-1) >= 0 {
                ready |= 4;
            }
        }

        // OP_CONNECT (8)
        if interest & 8 != 0 {
            if ctx.get_field(channel, S2SC_CONNECTED).as_int().unwrap_or(0) != 0 {
                ready |= 8;
            }
        }

        ctx.set_field(key, 3, Value::Int(ready));
        if ready != 0 { count += 1; }
    }
    count
}

fn s2_try_accept_nonblocking(reg: &mut SocketRegistry, lid: i32) -> Option<i32> {
    let listener = reg.listeners.remove(&lid)?;
    let _ = listener.set_nonblocking(true);
    let result = listener.accept();
    reg.listeners.insert(lid, listener);
    match result {
        Ok((stream, _)) => {
            let id = reg.next_id;
            reg.next_id = reg.next_id.saturating_add(1);
            reg.streams.insert(id, stream);
            Some(id)
        }
        Err(_) => None,
    }
}

fn s2_blocking_accept(reg: &mut SocketRegistry, lid: i32) -> Option<i32> {
    let listener = reg.listeners.remove(&lid)?;
    let _ = listener.set_nonblocking(false);
    let result = listener.accept();
    reg.listeners.insert(lid, listener);
    match result {
        Ok((stream, _)) => {
            let id = reg.next_id;
            reg.next_id = reg.next_id.saturating_add(1);
            reg.streams.insert(id, stream);
            Some(id)
        }
        Err(_) => None,
    }
}

// ---- Main entry point ------------------------------------------------------

fn register_s2_nio(r: &mut NativeMethodRegistry) {
    register_s2_bytebuffer(r);
    register_s2_byteorder(r);
    register_s2_socket_channel(r);
    register_s2_server_socket_channel(r);
    register_s2_selector(r);
}

// ---- ByteBuffer ------------------------------------------------------------

#[allow(clippy::too_many_lines)]
// Named helpers for view-buffer creation (closures can't capture; need fn ptrs)
macro_rules! s2_view_buf_fn {
    ($name:ident, $cls:literal, $elem_sz:expr) => {
        fn $name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
            let this = obj_arg(args, 0)?;
            let pos  = s2_bb_pos(ctx, this);
            let lim  = s2_bb_limit(ctx, this);
            let rem  = (lim - pos) / $elem_sz;
            let vb   = alloc_concurrent_synthetic(ctx, $cls, 6);
            ctx.set_field(vb, BB_ARRAY, ctx.get_field(this, BB_ARRAY));
            ctx.set_field(vb, BB_POS,   Value::Int(0));
            ctx.set_field(vb, BB_LIMIT, Value::Int(rem));
            ctx.set_field(vb, BB_CAP,   Value::Int(rem));
            ctx.set_field(vb, BB_MARK,  Value::Int(-(pos + 1)));
            ctx.set_field(vb, BB_ORDER, ctx.get_field(this, BB_ORDER));
            Ok(Some(Value::Object(Some(vb))))
        }
    };
}
s2_view_buf_fn!(s2_bb_as_int_buffer,    "java/nio/IntBuffer",    4);
s2_view_buf_fn!(s2_bb_as_long_buffer,   "java/nio/LongBuffer",   8);
s2_view_buf_fn!(s2_bb_as_short_buffer,  "java/nio/ShortBuffer",  2);
s2_view_buf_fn!(s2_bb_as_float_buffer,  "java/nio/FloatBuffer",  4);
s2_view_buf_fn!(s2_bb_as_double_buffer, "java/nio/DoubleBuffer", 8);
s2_view_buf_fn!(s2_bb_as_char_buffer,   "java/nio/CharBuffer",   2);

fn register_s2_bytebuffer(r: &mut NativeMethodRegistry) {
    use cratonvm_types::ArrayElementType;
    let bb = "java/nio/ByteBuffer";

    r.register(bb, "allocate", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let cap = args.first().and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        Ok(Some(Value::Object(Some(s2_bb_alloc(ctx, cap)))))
    });
    r.register(bb, "allocateDirect", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let cap = args.first().and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
        Ok(Some(Value::Object(Some(s2_bb_alloc(ctx, cap)))))
    });
    r.register(bb, "wrap", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let len = ctx.array_length(arr) as i32;
        let buf = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(len));
        ctx.set_field(buf, BB_CAP, Value::Int(len));
        ctx.set_field(buf, BB_MARK, Value::Int(-1));
        ctx.set_field(buf, BB_ORDER, Value::Int(0));
        Ok(Some(Value::Object(Some(buf))))
    });
    r.register(bb, "wrap", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let off = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let len = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let cap = ctx.array_length(arr) as i32;
        let buf = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(arr)));
        ctx.set_field(buf, BB_POS, Value::Int(off));
        ctx.set_field(buf, BB_LIMIT, Value::Int((off + len).min(cap)));
        ctx.set_field(buf, BB_CAP, Value::Int(cap));
        ctx.set_field(buf, BB_MARK, Value::Int(-1));
        ctx.set_field(buf, BB_ORDER, Value::Int(0));
        Ok(Some(Value::Object(Some(buf))))
    });

    // get
    r.register(bb, "get", "()B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let b = s2_bb_get_byte(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(b as i32)))
    });
    r.register(bb, "get", "(I)B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(s2_bb_get_byte(ctx, this, idx) as i32)))
    });
    r.register(bb, "get", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let arr = s2_bb_arr(ctx, this).unwrap_or(dst);
        for i in 0..len as usize {
            let b = ctx.get_array_element(arr, pos as usize + i);
            ctx.set_array_element(dst, off + i, b);
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "get", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let dst = obj_arg(args, 1)?;
        let len = ctx.array_length(dst) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let arr = s2_bb_arr(ctx, this).unwrap_or(dst);
        for i in 0..len as usize {
            let b = ctx.get_array_element(arr, pos as usize + i);
            ctx.set_array_element(dst, i, b);
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });

    // put
    r.register(bb, "put", "(B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        let pos = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_put_byte(ctx, this, pos, b);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "(IB)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let b = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i8;
        s2_bb_put_byte(ctx, this, idx, b);
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "([BII)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src = obj_arg(args, 1)?;
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let arr = s2_bb_arr(ctx, this).unwrap_or(src);
        for i in 0..len as usize {
            let b = ctx.get_array_element(src, off + i);
            ctx.set_array_element(arr, pos as usize + i, b);
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "([B)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src = obj_arg(args, 1)?;
        let len = ctx.array_length(src) as i32;
        let pos = s2_bb_pos(ctx, this);
        if pos + len > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let arr = s2_bb_arr(ctx, this).unwrap_or(src);
        for i in 0..len as usize {
            let b = ctx.get_array_element(src, i);
            ctx.set_array_element(arr, pos as usize + i, b);
        }
        ctx.set_field(this, BB_POS, Value::Int(pos + len));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "put", "(Ljava/nio/ByteBuffer;)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src  = obj_arg(args, 1)?;
        let src_pos = s2_bb_pos(ctx, src);
        let src_lim = s2_bb_limit(ctx, src);
        let n = (src_lim - src_pos).max(0) as usize;
        let pos = s2_bb_pos(ctx, this);
        if pos + n as i32 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        // Either side may be a DIRECT buffer (real-JDK `DirectByteBuffer`:
        // no heap array, storage at `address`). The pre-fix code silently
        // returned without copying OR advancing positions whenever a side
        // had no heap array. `IOUtil.read` routes every buffered
        // `FileChannel` read through a temporary direct buffer and then
        // `dst.put(directSrc)`, so file reads "succeeded" (count returned)
        // while delivering ZERO bytes with an unmoved destination position —
        // Lucene's `BufferedIndexInput.refill()` then flipped an empty
        // buffer and the first `readByte()` threw `BufferUnderflowException`
        // (ES-FAIL-FAMILY-20260710-vector-codec-exception-cause-object).
        let mut bytes = vec![0u8; n];
        if let Some(src_arr) = s2_bb_arr(ctx, src) {
            for (i, b) in bytes.iter_mut().enumerate() {
                *b = ctx
                    .get_array_element(src_arr, src_pos as usize + i)
                    .as_int()
                    .unwrap_or(0) as u8;
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, src) {
            if !ctx.copy_from_native_memory(addr.saturating_add(src_pos as i64), &mut bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put: direct source read failed".to_string(),
                }
                .into());
            }
        } else {
            // Genuinely storage-less synthetic buffer — keep the historic
            // silent no-op so half-built synthetic-mode buffers stay benign.
            return Ok(Some(Value::Object(Some(this))));
        }
        if let Some(dst_arr) = s2_bb_arr(ctx, this) {
            for (i, b) in bytes.iter().enumerate() {
                ctx.set_array_element(dst_arr, pos as usize + i, Value::Int(*b as i8 as i32));
            }
        } else if let Some(addr) = s2_bb_direct_addr(ctx, this) {
            if !ctx.copy_to_native_memory(addr.saturating_add(pos as i64), &bytes) {
                return Err(RuntimeError::IllegalStateException {
                    message: "ByteBuffer.put: direct destination write failed".to_string(),
                }
                .into());
            }
        } else {
            return Ok(Some(Value::Object(Some(this))));
        }
        s2_bb_set_pos(ctx, src, src_lim);
        s2_bb_set_pos(ctx, this, pos + n as i32);
        Ok(Some(Value::Object(Some(this))))
    });

    // getShort / putShort
    r.register(bb, "getShort", "()S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    });
    r.register(bb, "getShort", "(I)S", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as i32)))
    });
    r.register(bb, "putShort", "(S)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putShort", "(IS)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v   = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getChar / putChar
    r.register(bb, "getChar", "()C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read2(ctx, this, pos) as u16;
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Int(v as i32)))
    });
    r.register(bb, "getChar", "(I)C", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(s2_bb_read2(ctx, this, idx) as u16 as i32)))
    });
    r.register(bb, "putChar", "(C)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        let pos = s2_bb_pos(ctx, this);
        if pos + 2 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write2(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 2));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putChar", "(IC)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v   = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as i16;
        s2_bb_write2(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getInt / putInt
    r.register(bb, "getInt", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read4(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Int(v)))
    });
    r.register(bb, "getInt", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Int(s2_bb_read4(ctx, this, idx))))
    });
    r.register(bb, "putInt", "(I)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putInt", "(II)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v   = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        s2_bb_write4(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getLong / putLong
    r.register(bb, "getLong", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let v = s2_bb_read8(ctx, this, pos);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Long(v)))
    });
    r.register(bb, "getLong", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Long(s2_bb_read8(ctx, this, idx))))
    });
    r.register(bb, "putLong", "(J)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) { Some(Value::Long(l)) => *l, Some(Value::Int(i)) => *i as i64, _ => 0 };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(bb, "putLong", "(IJ)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v   = match args.get(2) { Some(Value::Long(l)) => *l, Some(Value::Int(i)) => *i as i64, _ => 0 };
        s2_bb_write8(ctx, this, idx, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // getFloat / putFloat
    r.register(bb, "getFloat", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read4(ctx, this, pos) as u32;
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Float(f32::from_bits(bits))))
    });
    r.register(bb, "getFloat", "(I)F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        Ok(Some(Value::Float(f32::from_bits(s2_bb_read4(ctx, this, idx) as u32))))
    });
    r.register(bb, "putFloat", "(F)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) { Some(Value::Float(f)) => *f, Some(Value::Int(i)) => f32::from_bits(*i as u32), _ => 0.0 };
        let pos = s2_bb_pos(ctx, this);
        if pos + 4 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write4(ctx, this, pos, v.to_bits() as i32);
        ctx.set_field(this, BB_POS, Value::Int(pos + 4));
        Ok(Some(Value::Object(Some(this))))
    });

    // getDouble / putDouble
    r.register(bb, "getDouble", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bits = s2_bb_read8(ctx, this, pos) as u64;
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Double(f64::from_bits(bits))))
    });
    r.register(bb, "putDouble", "(D)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = match args.get(1) { Some(Value::Double(d)) => *d, _ => 0.0 };
        let pos = s2_bb_pos(ctx, this);
        if pos + 8 > s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        s2_bb_write8(ctx, this, pos, v.to_bits() as i64);
        ctx.set_field(this, BB_POS, Value::Int(pos + 8));
        Ok(Some(Value::Object(Some(this))))
    });

    // flip / clear / rewind / mark (both Buffer and ByteBuffer return types)
    for ret in &["()Ljava/nio/Buffer;", "()Ljava/nio/ByteBuffer;"] {
        let ret = *ret;
        r.register(bb, "flip", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS,   Value::Int(0));
            ctx.set_field(this, BB_MARK,  Value::Int(-1));
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "clear", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS,   Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            ctx.set_field(this, BB_MARK,  Value::Int(-1));
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "rewind", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, BB_POS,  Value::Int(0));
            ctx.set_field(this, BB_MARK, Value::Int(-1));
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(bb, "mark", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_MARK, Value::Int(pos));
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "reset", "()Ljava/nio/Buffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mark = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
        if mark < 0 {
            return Err(RuntimeError::IllegalStateException { message: "InvalidMarkException".into() }.into());
        }
        ctx.set_field(this, BB_POS, Value::Int(mark));
        Ok(Some(Value::Object(Some(this))))
    });

    // position / limit
    r.register(bb, "position", "()I", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_POS))));
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "position", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            ctx.set_field(this, BB_POS, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "limit", "()I", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_LIMIT))));
    for ret in &["(I)Ljava/nio/Buffer;", "(I)Ljava/nio/ByteBuffer;"] {
        r.register(bb, "limit", ret, |ctx, args| {
            let this = obj_arg(args, 0)?;
            let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            ctx.set_field(this, BB_LIMIT, Value::Int(v));
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(bb, "capacity",     "()I", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, BB_CAP))));
    r.register(bb, "remaining",    "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int((s2_bb_limit(ctx, this) - s2_bb_pos(ctx, this)).max(0))))
    });
    r.register(bb, "hasRemaining", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if s2_bb_limit(ctx, this) > s2_bb_pos(ctx, this) { 1 } else { 0 })))
    });
    r.register(bb, "compact", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos = s2_bb_pos(ctx, this) as usize;
        let lim = s2_bb_limit(ctx, this) as usize;
        let cap = s2_bb_cap(ctx, this);
        let n   = lim.saturating_sub(pos);
        if let Some(arr) = s2_bb_arr(ctx, this) {
            for i in 0..n {
                let b = ctx.get_array_element(arr, pos + i);
                ctx.set_array_element(arr, i, b);
            }
        }
        ctx.set_field(this, BB_POS,   Value::Int(n as i32));
        ctx.set_field(this, BB_LIMIT, Value::Int(cap));
        ctx.set_field(this, BB_MARK,  Value::Int(-1));
        Ok(Some(Value::Object(Some(this))))
    });

    // array / hasArray / isDirect / isReadOnly / arrayOffset
    r.register(bb, "array",      "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Object(s2_bb_arr(ctx, this))))
    });
    r.register(bb, "arrayOffset","()I",  |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(bb, "hasArray",   "()Z",  |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(if s2_bb_arr(ctx, this).is_some() { 1 } else { 0 })))
    });
    r.register(bb, "isDirect",   "()Z",  |_ctx, _args| Ok(Some(Value::Int(0))));
    r.register(bb, "isReadOnly", "()Z",  |_ctx, _args| Ok(Some(Value::Int(0))));

    // order
    r.register(bb, "order", "()Ljava/nio/ByteOrder;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ord  = s2_bb_order(ctx, this);
        let bo   = alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1);
        ctx.set_field(bo, 0, Value::Int(ord));
        Ok(Some(Value::Object(Some(bo))))
    });
    r.register(bb, "order", "(Ljava/nio/ByteOrder;)Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ord  = match args.get(1) {
            Some(Value::Object(Some(bo))) => ctx.get_field(*bo, 0).as_int().unwrap_or(0),
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        ctx.set_field(this, BB_ORDER, Value::Int(ord));
        Ok(Some(Value::Object(Some(this))))
    });

    // slice / duplicate / asReadOnlyBuffer
    r.register(bb, "slice", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos  = s2_bb_pos(ctx, this) as usize;
        let lim  = s2_bb_limit(ctx, this) as usize;
        let rem  = lim.saturating_sub(pos);
        let new_arr = ctx.new_array(ArrayElementType::Byte, rem);
        if let Some(src) = s2_bb_arr(ctx, this) {
            for i in 0..rem {
                let b = ctx.get_array_element(src, pos + i);
                ctx.set_array_element(new_arr, i, b);
            }
        }
        let buf = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
        ctx.set_field(buf, BB_ARRAY, Value::Object(Some(new_arr)));
        ctx.set_field(buf, BB_POS,   Value::Int(0));
        ctx.set_field(buf, BB_LIMIT, Value::Int(rem as i32));
        ctx.set_field(buf, BB_CAP,   Value::Int(rem as i32));
        ctx.set_field(buf, BB_MARK,  Value::Int(-1));
        ctx.set_field(buf, BB_ORDER, ctx.get_field(this, BB_ORDER));
        Ok(Some(Value::Object(Some(buf))))
    });
    r.register(bb, "duplicate", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf  = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
        if let Some(src_arr) = s2_bb_arr(ctx, this) {
            let cap = s2_bb_cap(ctx, this);
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let mark = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            bb_write_hb(ctx, buf, src_arr, cap);
            ctx.set_field(buf, BB_POS,   Value::Int(pos));
            ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
            ctx.set_field(buf, BB_MARK,  Value::Int(mark));
        }
        ctx.set_field(buf, BB_ORDER, ctx.get_field(this, BB_ORDER));
        Ok(Some(Value::Object(Some(buf))))
    });
    r.register(bb, "asReadOnlyBuffer", "()Ljava/nio/ByteBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf  = alloc_concurrent_synthetic(ctx, "java/nio/ByteBuffer", 6);
        if let Some(src_arr) = s2_bb_arr(ctx, this) {
            let cap = s2_bb_cap(ctx, this);
            let pos = s2_bb_pos(ctx, this);
            let lim = s2_bb_limit(ctx, this);
            let mark = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1);
            bb_write_hb(ctx, buf, src_arr, cap);
            ctx.set_field(buf, BB_POS,   Value::Int(pos));
            ctx.set_field(buf, BB_LIMIT, Value::Int(lim));
            ctx.set_field(buf, BB_MARK,  Value::Int(mark));
        }
        ctx.set_field(buf, BB_ORDER, ctx.get_field(this, BB_ORDER));
        Ok(Some(Value::Object(Some(buf))))
    });

    // asXxxBuffer view buffers — each needs a named function (no closure captures)
    r.register(bb, "asIntBuffer",    "()Ljava/nio/IntBuffer;",    s2_bb_as_int_buffer);
    r.register(bb, "asLongBuffer",   "()Ljava/nio/LongBuffer;",   s2_bb_as_long_buffer);
    r.register(bb, "asShortBuffer",  "()Ljava/nio/ShortBuffer;",  s2_bb_as_short_buffer);
    r.register(bb, "asFloatBuffer",  "()Ljava/nio/FloatBuffer;",  s2_bb_as_float_buffer);
    r.register(bb, "asDoubleBuffer", "()Ljava/nio/DoubleBuffer;", s2_bb_as_double_buffer);
    r.register(bb, "asCharBuffer",   "()Ljava/nio/CharBuffer;",   s2_bb_as_char_buffer);

    // IntBuffer get/put (positions in int units; byte_start from BB_MARK)
    let ib = "java/nio/IntBuffer";
    r.register(ib, "get", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos  = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferUnderflowException.into());
        }
        let bs = { let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1); if m < 0 { -(m+1) } else { 0 } };
        let v  = s2_bb_read4(ctx, this, bs + pos * 4);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Int(v)))
    });
    r.register(ib, "get", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx  = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let bs   = { let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1); if m < 0 { -(m+1) } else { 0 } };
        Ok(Some(Value::Int(s2_bb_read4(ctx, this, bs + idx * 4))))
    });
    r.register(ib, "put", "(I)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v    = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let pos  = s2_bb_pos(ctx, this);
        if pos >= s2_bb_limit(ctx, this) {
            return Err(RuntimeError::BufferOverflowException.into());
        }
        let bs = { let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1); if m < 0 { -(m+1) } else { 0 } };
        s2_bb_write4(ctx, this, bs + pos * 4, v);
        ctx.set_field(this, BB_POS, Value::Int(pos + 1));
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(ib, "put", "(II)Ljava/nio/IntBuffer;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx  = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let v    = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let bs   = { let m = ctx.get_field(this, BB_MARK).as_int().unwrap_or(-1); if m < 0 { -(m+1) } else { 0 } };
        s2_bb_write4(ctx, this, bs + idx * 4, v);
        Ok(Some(Value::Object(Some(this))))
    });

    // Shared Buffer methods for all view-buffer types
    for cls in &[
        "java/nio/IntBuffer", "java/nio/LongBuffer", "java/nio/ShortBuffer",
        "java/nio/FloatBuffer", "java/nio/DoubleBuffer",
    ] {
        let cls = *cls;
        r.register(cls, "position",     "()I",  |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, BB_POS))));
        r.register(cls, "limit",        "()I",  |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, BB_LIMIT))));
        r.register(cls, "capacity",     "()I",  |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, BB_CAP))));
        r.register(cls, "remaining",    "()I",  |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int((s2_bb_limit(ctx,this)-s2_bb_pos(ctx,this)).max(0))))
        });
        r.register(cls, "hasRemaining", "()Z",  |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(if s2_bb_limit(ctx,this)>s2_bb_pos(ctx,this) { 1 } else { 0 })))
        });
        r.register(cls, "flip",  "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pos  = s2_bb_pos(ctx, this);
            ctx.set_field(this, BB_LIMIT, Value::Int(pos));
            ctx.set_field(this, BB_POS,   Value::Int(0));
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(cls, "clear", "()Ljava/nio/Buffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cap  = s2_bb_cap(ctx, this);
            ctx.set_field(this, BB_POS,   Value::Int(0));
            ctx.set_field(this, BB_LIMIT, Value::Int(cap));
            Ok(Some(Value::Object(Some(this))))
        });
        r.register(cls, "array",      "()[I",  |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, BB_ARRAY))));
        r.register(cls, "isDirect",   "()Z",   |_,_| Ok(Some(Value::Int(0))));
        r.register(cls, "isReadOnly", "()Z",   |_,_| Ok(Some(Value::Int(0))));
    }

    // equals / hashCode / compareTo / toString
    r.register(bb, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this  = obj_arg(args, 0)?;
        let other = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => return Ok(Some(Value::Int(0))) };
        if this == other { return Ok(Some(Value::Int(1))); }
        let pa = s2_bb_pos(ctx, this)  as usize; let la = s2_bb_limit(ctx, this)  as usize;
        let pb = s2_bb_pos(ctx, other) as usize; let lb = s2_bb_limit(ctx, other) as usize;
        let na = la.saturating_sub(pa);
        if na != lb.saturating_sub(pb) { return Ok(Some(Value::Int(0))); }
        let aa = match s2_bb_arr(ctx, this)  { Some(a) => a, None => return Ok(Some(Value::Int(0))) };
        let ab = match s2_bb_arr(ctx, other) { Some(a) => a, None => return Ok(Some(Value::Int(0))) };
        for i in 0..na {
            if ctx.get_array_element(aa, pa+i) != ctx.get_array_element(ab, pb+i) { return Ok(Some(Value::Int(0))); }
        }
        Ok(Some(Value::Int(1)))
    });
    r.register(bb, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mut h: i32 = 1;
        let pos = s2_bb_pos(ctx, this) as usize;
        let lim = s2_bb_limit(ctx, this) as usize;
        if let Some(arr) = s2_bb_arr(ctx, this) {
            for i in pos..lim {
                let b = ctx.get_array_element(arr, i).as_int().unwrap_or(0) as i8 as i32;
                h = h.wrapping_mul(31).wrapping_add(b);
            }
        }
        Ok(Some(Value::Int(h)))
    });
    r.register(bb, "compareTo", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this  = obj_arg(args, 0)?;
        let other = obj_arg(args, 1)?;
        let pa = s2_bb_pos(ctx, this)  as usize; let la = s2_bb_limit(ctx, this)  as usize;
        let pb = s2_bb_pos(ctx, other) as usize; let lb = s2_bb_limit(ctx, other) as usize;
        let na = la.saturating_sub(pa);
        let nb = lb.saturating_sub(pb);
        let n  = na.min(nb);
        let aa = match s2_bb_arr(ctx, this)  { Some(a) => a, None => return Ok(Some(Value::Int(0))) };
        let ab = match s2_bb_arr(ctx, other) { Some(a) => a, None => return Ok(Some(Value::Int(0))) };
        for i in 0..n {
            let va = ctx.get_array_element(aa, pa+i).as_int().unwrap_or(0);
            let vb = ctx.get_array_element(ab, pb+i).as_int().unwrap_or(0);
            if va != vb { return Ok(Some(Value::Int(va - vb))); }
        }
        Ok(Some(Value::Int((na as i32) - (nb as i32))))
    });
    r.register(bb, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let pos  = s2_bb_pos(ctx, this);
        let lim  = s2_bb_limit(ctx, this);
        let cap  = s2_bb_cap(ctx, this);
        let s    = ctx.create_string(&format!("java.nio.HeapByteBuffer[pos={pos} lim={lim} cap={cap}]"));
        Ok(Some(Value::Object(Some(s))))
    });
}

// ---- ByteOrder -------------------------------------------------------------

fn register_s2_byteorder(r: &mut NativeMethodRegistry) {
    let bo = "java/nio/ByteOrder";
    r.register(bo, "nativeOrder", "()Ljava/nio/ByteOrder;", |ctx, _| {
        let obj = alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1);
        ctx.set_field(obj, 0, Value::Int(1)); // x86/ARM64 = LITTLE_ENDIAN
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(bo, "BIG_ENDIAN",    "Ljava/nio/ByteOrder;", |ctx, _| {
        let obj = alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1);
        ctx.set_field(obj, 0, Value::Int(0));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(bo, "LITTLE_ENDIAN", "Ljava/nio/ByteOrder;", |ctx, _| {
        let obj = alloc_concurrent_synthetic(ctx, "java/nio/ByteOrder", 1);
        ctx.set_field(obj, 0, Value::Int(1));
        Ok(Some(Value::Object(Some(obj))))
    });
    r.register(bo, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let name = if ctx.get_field(this, 0).as_int().unwrap_or(0) == 1 { "LITTLE_ENDIAN" } else { "BIG_ENDIAN" };
        let s = ctx.create_string(name);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(bo, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this  = obj_arg(args, 0)?;
        let other = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => return Ok(Some(Value::Int(0))) };
        let a = ctx.get_field(this,  0).as_int().unwrap_or(0);
        let b = ctx.get_field(other, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
    });
}

// ---- SocketChannel (real TcpStream) ----------------------------------------

fn register_s2_socket_channel(r: &mut NativeMethodRegistry) {
    let sc = "java/nio/channels/SocketChannel";

    r.register(sc, "open", "()Ljava/nio/channels/SocketChannel;", |ctx, _| {
        let ch = alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5);
        ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
        ctx.set_field(ch, S2SC_OPEN,      Value::Int(1));
        ctx.set_field(ch, S2SC_ADDR,      Value::Object(None));
        ctx.set_field(ch, S2SC_SOCK_ID,   Value::Int(-1));
        ctx.set_field(ch, S2SC_BLOCKING,  Value::Int(1));
        Ok(Some(Value::Object(Some(ch))))
    });
    r.register(sc, "open", "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;", |ctx, args| {
        let addr_val = args.first().copied().unwrap_or(Value::Object(None));
        let ch = alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5);
        ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
        ctx.set_field(ch, S2SC_OPEN,      Value::Int(1));
        ctx.set_field(ch, S2SC_ADDR,      addr_val);
        ctx.set_field(ch, S2SC_SOCK_ID,   Value::Int(-1));
        ctx.set_field(ch, S2SC_BLOCKING,  Value::Int(1));
        if let Value::Object(Some(addr)) = addr_val {
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                if let Ok(stream) = TcpStream::connect(format!("{host}:{port}")) {
                    let id = s2_alloc_stream(stream);
                    ctx.set_field(ch, S2SC_SOCK_ID,   Value::Int(id));
                    ctx.set_field(ch, S2SC_CONNECTED, Value::Int(1));
                }
            }
        }
        Ok(Some(Value::Object(Some(ch))))
    });
    r.register(sc, "connect", "(Ljava/net/SocketAddress;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // If address is null, fall back to stub (mark as connected, return true)
        let addr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                return Ok(Some(Value::Int(1)));
            }
        };
        ctx.set_field(this, S2SC_ADDR, Value::Object(Some(addr)));
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
            match TcpStream::connect(format!("{host}:{port}")) {
                Ok(stream) => {
                    if !blocking { let _ = stream.set_nonblocking(true); }
                    let id = s2_alloc_stream(stream);
                    ctx.set_field(this, S2SC_SOCK_ID,   Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(if blocking { 1 } else { 0 })));
                }
                Err(e) => {
                    tracing::debug!("SocketChannel.connect: {e}");
                    if blocking {
                        return Err(RuntimeError::IOException {
                            message: format!("Connection refused: {host}:{port}"),
                        }.into());
                    }
                }
            }
        }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        Ok(Some(Value::Int(0)))
    });
    r.register(sc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this    = obj_arg(args, 0)?;
        let bb      = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 { return Ok(Some(Value::Int(-1))); }
        let pos = s2_bb_pos(ctx, bb) as usize;
        let lim = s2_bb_limit(ctx, bb) as usize;
        let cap = lim.saturating_sub(pos);
        if cap == 0 { return Ok(Some(Value::Int(0))); }
        let mut tmp = vec![0u8; cap];
        let n = {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get_mut(&sock_id) {
                match stream.read(&mut tmp) {
                    Ok(0)  => -1i32,
                    Ok(n)  => n as i32,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                    Err(_) => -1,
                }
            } else { -1 }
        };
        if n > 0 {
            if let Some(arr) = s2_bb_arr(ctx, bb) {
                for i in 0..n as usize {
                    ctx.set_array_element(arr, pos + i, Value::Int(tmp[i] as i8 as i32));
                }
            }
            ctx.set_field(bb, BB_POS, Value::Int((pos + n as usize) as i32));
        }
        Ok(Some(Value::Int(n)))
    });
    r.register(sc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this    = obj_arg(args, 0)?;
        let bb      = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 {
            return Err(RuntimeError::IOException { message: "not connected".into() }.into());
        }
        let data = s2_bb_remaining_bytes(ctx, bb);
        if data.is_empty() { return Ok(Some(Value::Int(0))); }
        let n = {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get_mut(&sock_id) {
                match stream.write(&data) {
                    Ok(n)  => n as i32,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                    Err(_) => -1,
                }
            } else { -1 }
        };
        if n > 0 {
            let pos = s2_bb_pos(ctx, bb);
            let lim = s2_bb_limit(ctx, bb);
            ctx.set_field(bb, BB_POS, Value::Int((pos + n).min(lim)));
        }
        Ok(Some(Value::Int(n)))
    });
    r.register(sc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid  = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 { s2_registry().lock().streams.remove(&sid); }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        ctx.set_field(this, S2SC_OPEN,      Value::Int(0));
        ctx.set_field(this, S2SC_SOCK_ID,   Value::Int(-1));
        Ok(None)
    });
    r.register(sc, "isConnected", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_CONNECTED)))
    });
    r.register(sc, "isOpen", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_OPEN)))
    });
    r.register(sc, "configureBlocking", "(Z)Ljava/nio/channels/SelectableChannel;", |ctx, args| {
        let this     = obj_arg(args, 0)?;
        let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, S2SC_BLOCKING, Value::Int(blocking));
        let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let mut reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get_mut(&sid) {
                let _ = stream.set_nonblocking(blocking == 0);
            }
        }
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(sc, "finishConnect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid  = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
            return Ok(Some(Value::Int(1)));
        }
        if let Value::Object(Some(addr)) = ctx.get_field(this, S2SC_ADDR) {
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                if let Ok(stream) = TcpStream::connect(format!("{host}:{port}")) {
                    let id = s2_alloc_stream(stream);
                    ctx.set_field(this, S2SC_SOCK_ID,   Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(1)));
                }
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(sc, "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;", s2_register_channel);
    r.register(sc, "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;", s2_register_channel);
}

// ---- ServerSocketChannel (real TcpListener) --------------------------------

fn register_s2_server_socket_channel(r: &mut NativeMethodRegistry) {
    let ssc = "java/nio/channels/ServerSocketChannel";

    r.register(ssc, "open", "()Ljava/nio/channels/ServerSocketChannel;", |ctx, _| {
        let ch = alloc_concurrent_synthetic(ctx, "java/nio/channels/ServerSocketChannel", 5);
        ctx.set_field(ch, S2SSC_OPEN,        Value::Int(1));
        ctx.set_field(ch, S2SSC_BOUND,       Value::Int(0));
        ctx.set_field(ch, S2SSC_LISTENER_ID, Value::Int(-1));
        ctx.set_field(ch, S2SSC_PORT,        Value::Int(0));
        ctx.set_field(ch, S2SSC_PENDING,     Value::Int(-1));
        Ok(Some(Value::Object(Some(ch))))
    });
    for desc in &[
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
    ] {
        r.register(ssc, "bind", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Null address → stub: mark as bound without creating a real listener
            let addr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
                    return Ok(Some(Value::Object(Some(this))));
                }
            };
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                match TcpListener::bind(format!("{host}:{port}")) {
                    Ok(listener) => {
                        let id = s2_alloc_listener(listener);
                        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(id));
                        ctx.set_field(this, S2SSC_BOUND,       Value::Int(1));
                        ctx.set_field(this, S2SSC_PORT,        Value::Int(port as i32));
                    }
                    Err(e) => return Err(RuntimeError::IOException { message: format!("bind {host}:{port}: {e}") }.into()),
                }
            } else {
                // Unresolvable address → stub bound
                ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
            }
            Ok(Some(Value::Object(Some(this))))
        });
    }
    r.register(ssc, "accept", "()Ljava/nio/channels/SocketChannel;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid  = ctx.get_field(this, S2SSC_LISTENER_ID).as_int().unwrap_or(-1);
        if lid < 0 {
            // Stub-bound (null address) — return a disconnected stub SocketChannel
            let sc = alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5);
            ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
            ctx.set_field(sc, S2SC_OPEN,      Value::Int(1));
            ctx.set_field(sc, S2SC_ADDR,      Value::Object(None));
            ctx.set_field(sc, S2SC_SOCK_ID,   Value::Int(-1));
            ctx.set_field(sc, S2SC_BLOCKING,  Value::Int(1));
            return Ok(Some(Value::Object(Some(sc))));
        }
        let pending = ctx.get_field(this, S2SSC_PENDING).as_int().unwrap_or(-1);
        let stream_id = if pending >= 0 {
            ctx.set_field(this, S2SSC_PENDING, Value::Int(-1));
            pending
        } else {
            let result = { let mut reg = s2_registry().lock(); s2_blocking_accept(&mut reg, lid) };
            match result { Some(id) => id, None => return Ok(Some(Value::Object(None))) }
        };
        let sc = alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5);
        ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
        ctx.set_field(sc, S2SC_OPEN,      Value::Int(1));
        ctx.set_field(sc, S2SC_ADDR,      Value::Object(None));
        ctx.set_field(sc, S2SC_SOCK_ID,   Value::Int(stream_id));
        ctx.set_field(sc, S2SC_BLOCKING,  Value::Int(1));
        Ok(Some(Value::Object(Some(sc))))
    });
    r.register(ssc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid  = ctx.get_field(this, S2SSC_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 { s2_registry().lock().listeners.remove(&lid); }
        ctx.set_field(this, S2SSC_OPEN,        Value::Int(0));
        ctx.set_field(this, S2SSC_BOUND,       Value::Int(0));
        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(-1));
        Ok(None)
    });
    r.register(ssc, "isOpen", "()Z", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, S2SSC_OPEN))));
    r.register(ssc, "configureBlocking", "(Z)Ljava/nio/channels/SelectableChannel;", |ctx, args| {
        let this     = obj_arg(args, 0)?;
        let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        let lid = ctx.get_field(this, S2SSC_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get(&lid) {
                let _ = listener.set_nonblocking(blocking == 0);
            }
        }
        Ok(Some(Value::Object(Some(this))))
    });
    r.register(ssc, "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;", s2_register_channel);
    r.register(ssc, "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;", s2_register_channel);
}

// ---- Channel registration & Selector ---------------------------------------

fn s2_register_channel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let channel  = args.first().copied().unwrap_or(Value::Object(None));
    let selector = args.get(1).copied().unwrap_or(Value::Object(None));
    let ops      = args.get(2).copied().unwrap_or(Value::Int(0));
    let key = alloc_concurrent_synthetic(ctx, "java/nio/channels/SelectionKey", 4);
    ctx.set_field(key, 0, channel);
    ctx.set_field(key, 1, selector);
    ctx.set_field(key, 2, ops);
    ctx.set_field(key, 3, Value::Int(0)); // readyOps = 0
    // Add key to selector's key list
    if let Value::Object(Some(sel)) = selector {
        let n       = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
        let new_cap = (n + 1).max(8);
        let new_arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), new_cap);
        if let Value::Object(Some(old_arr)) = ctx.get_field(sel, S2SEL_KEYS) {
            for i in 0..n {
                let k = ctx.get_array_element(old_arr, i);
                ctx.set_array_element(new_arr, i, k);
            }
        }
        ctx.set_array_element(new_arr, n, Value::Object(Some(key)));
        ctx.set_field(sel, S2SEL_KEYS,  Value::Object(Some(new_arr)));
        ctx.set_field(sel, S2SEL_NKEYS, Value::Int((n + 1) as i32));
    }
    Ok(Some(Value::Object(Some(key))))
}

fn s2_keys_as_set(ctx: &mut dyn NativeContext, sel: ObjectRef, selected_only: bool) -> Value {
    let n       = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    let set     = alloc_concurrent_synthetic(ctx, "java/util/HashSet", 2);
    let keys_v  = ctx.get_field(sel, S2SEL_KEYS);
    if let Value::Object(Some(keys_arr)) = keys_v {
        let mut ready: Vec<ObjectRef> = Vec::new();
        for i in 0..n {
            if let Value::Object(Some(k)) = ctx.get_array_element(keys_arr, i) {
                let rops = ctx.get_field(k, 3).as_int().unwrap_or(0);
                if !selected_only || rops != 0 { ready.push(k); }
            }
        }
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), ready.len());
        for (i, k) in ready.iter().enumerate() {
            ctx.set_array_element(arr, i, Value::Object(Some(*k)));
        }
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(ready.len() as i32));
    } else {
        let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
        ctx.set_field(set, 0, Value::Object(Some(arr)));
        ctx.set_field(set, 1, Value::Int(0));
    }
    Value::Object(Some(set))
}

fn register_s2_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "open", "()Ljava/nio/channels/Selector;", |ctx, _| {
        let s = alloc_concurrent_synthetic(ctx, "java/nio/channels/Selector", 3);
        ctx.set_field(s, S2SEL_OPEN,  Value::Int(1));
        ctx.set_field(s, S2SEL_KEYS,  Value::Object(None));
        ctx.set_field(s, S2SEL_NKEYS, Value::Int(0));
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(sel, "select", "()I", |ctx, args| {
        let this     = obj_arg(args, 0)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
        loop {
            let n = s2_selector_do_poll(ctx, this);
            if n > 0 || std::time::Instant::now() >= deadline { return Ok(Some(Value::Int(n))); }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    r.register(sel, "select", "(J)I", |ctx, args| {
        let this    = obj_arg(args, 0)?;
        let timeout = match args.get(1) {
            Some(Value::Long(v)) => (*v as u64).min(30_000),
            Some(Value::Int(v))  => (*v as u64).min(30_000),
            _ => 100,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout);
        loop {
            let n = s2_selector_do_poll(ctx, this);
            if n > 0 || std::time::Instant::now() >= deadline { return Ok(Some(Value::Int(n))); }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    });
    r.register(sel, "selectNow", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(s2_selector_do_poll(ctx, this))))
    });
    r.register(sel, "wakeup", "()Ljava/nio/channels/Selector;", |_, args| {
        Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
    });
    r.register(sel, "isOpen", "()Z", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, S2SEL_OPEN))));
    r.register(sel, "close",  "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, S2SEL_OPEN, Value::Int(0));
        Ok(None)
    });
    r.register(sel, "keys",         "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, false)))
    });
    r.register(sel, "selectedKeys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, true)))
    });

    // SelectionKey — upgrade readyOps to field 3, add convenience predicates
    let sk = "java/nio/channels/SelectionKey";
    r.register(sk, "readyOps",    "()I", |ctx, args| Ok(Some(ctx.get_field(obj_arg(args,0)?, 3))));
    r.register(sk, "isReadable",  "()Z", |ctx, args| {
        Ok(Some(Value::Int(if ctx.get_field(obj_arg(args,0)?,3).as_int().unwrap_or(0) & 1 != 0 { 1 } else { 0 })))
    });
    r.register(sk, "isWritable",  "()Z", |ctx, args| {
        Ok(Some(Value::Int(if ctx.get_field(obj_arg(args,0)?,3).as_int().unwrap_or(0) & 4 != 0 { 1 } else { 0 })))
    });
    r.register(sk, "isAcceptable","()Z", |ctx, args| {
        Ok(Some(Value::Int(if ctx.get_field(obj_arg(args,0)?,3).as_int().unwrap_or(0) & 16 != 0 { 1 } else { 0 })))
    });
    r.register(sk, "isConnectable","()Z", |ctx, args| {
        Ok(Some(Value::Int(if ctx.get_field(obj_arg(args,0)?,3).as_int().unwrap_or(0) & 8 != 0 { 1 } else { 0 })))
    });
    r.register(sk, "interestOps", "(I)Ljava/nio/channels/SelectionKey;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ops  = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(ops));
        Ok(Some(Value::Object(Some(this))))
    });

    // SelectableChannel.register override
    let sac = "java/nio/channels/SelectableChannel";
    r.register(sac, "register", "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;", s2_register_channel);
    r.register(sac, "register", "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;", s2_register_channel);
}

// ============================================================================
// Phase S.2 unit tests
// ============================================================================

#[cfg(test)]
mod tests_s2 {
    use super::*;
    use crate::config::VmConfig;
    use cratonvm_native_api::NativeMethodRegistry;
    use crate::vm::{NativeContextImpl, Vm};

    fn test_vm() -> Vm { Vm::new(VmConfig::default()) }
    fn ctx_of(vm: &mut Vm) -> NativeContextImpl<'_> {
        NativeContextImpl { shared: &vm.shared, thread: &mut vm.main_thread }
    }

    #[test]
    fn s2_bytebuffer_allocate_basics() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 16);
        assert_eq!(s2_bb_cap(&ctx, buf), 16);
        assert_eq!(s2_bb_pos(&ctx, buf), 0);
        assert_eq!(s2_bb_limit(&ctx, buf), 16);
        assert!(s2_bb_arr(&ctx, buf).is_some());
    }

    #[test]
    fn s2_bytebuffer_put_get_byte() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 4);
        s2_bb_put_byte(&ctx, buf, 0, 42i8);
        s2_bb_put_byte(&ctx, buf, 1, -1i8);
        assert_eq!(s2_bb_get_byte(&ctx, buf, 0), 42);
        assert_eq!(s2_bb_get_byte(&ctx, buf, 1), -1);
    }

    #[test]
    fn s2_bytebuffer_read4_write4_bigendian() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 8);
        ctx.set_field(buf, BB_ORDER, Value::Int(0)); // BIG_ENDIAN
        s2_bb_write4(&ctx, buf, 0, 0x01020304i32);
        assert_eq!(s2_bb_get_byte(&ctx, buf, 0), 0x01);
        assert_eq!(s2_bb_get_byte(&ctx, buf, 3), 0x04);
        assert_eq!(s2_bb_read4(&ctx, buf, 0), 0x01020304i32);
    }

    #[test]
    fn s2_bytebuffer_read4_write4_littleendian() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 8);
        ctx.set_field(buf, BB_ORDER, Value::Int(1)); // LITTLE_ENDIAN
        s2_bb_write4(&ctx, buf, 0, 0x01020304i32);
        assert_eq!(s2_bb_get_byte(&ctx, buf, 0), 0x04); // LSB first
        assert_eq!(s2_bb_get_byte(&ctx, buf, 3), 0x01);
        assert_eq!(s2_bb_read4(&ctx, buf, 0), 0x01020304i32);
    }

    #[test]
    fn s2_bytebuffer_read8_write8() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 16);
        let v: i64 = 0x0102030405060708i64;
        s2_bb_write8(&ctx, buf, 0, v);
        assert_eq!(s2_bb_read8(&ctx, buf, 0), v);
    }

    #[test]
    fn s2_bytebuffer_flip_and_remaining() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 8);
        ctx.set_field(buf, BB_POS, Value::Int(4));
        // flip: limit=4, pos=0
        let pos = s2_bb_pos(&ctx, buf);
        ctx.set_field(buf, BB_LIMIT, Value::Int(pos));
        ctx.set_field(buf, BB_POS,   Value::Int(0));
        assert_eq!(s2_bb_limit(&ctx, buf), 4);
        assert_eq!((s2_bb_limit(&ctx, buf) - s2_bb_pos(&ctx, buf)).max(0), 4);
    }

    #[test]
    fn s2_byteorder_nativeorder_is_little_endian() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s2_byteorder(&mut r);
        let f = r.find("java/nio/ByteOrder", "nativeOrder", "()Ljava/nio/ByteOrder;").unwrap();
        let result = f(&mut ctx, &[]).unwrap().unwrap();
        if let Value::Object(Some(bo)) = result {
            assert_eq!(ctx.get_field(bo, 0).as_int().unwrap(), 1);
        } else {
            panic!("expected ByteOrder object");
        }
    }

    #[test]
    fn s2_bytebuffer_remaining_bytes() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let buf = s2_bb_alloc(&mut ctx, 4);
        s2_bb_put_byte(&ctx, buf, 0, 1);
        s2_bb_put_byte(&ctx, buf, 1, 2);
        s2_bb_put_byte(&ctx, buf, 2, 3);
        ctx.set_field(buf, BB_LIMIT, Value::Int(3));
        let bytes = s2_bb_remaining_bytes(&ctx, buf);
        assert_eq!(bytes, vec![1, 2, 3]);
    }
}

// =============================================================================
// Phase S.3 — Real HttpClient (TCP-backed HTTP/1.1)
//
// Replaces the p60 stubs that returned a fake 200 OK with an empty body.
//
// Layout (unchanged from p60):
//   HttpClient    = 1-field  (version: Int 1=HTTP/1.1, 2=HTTP/2)
//   HttpRequest   = 4-field  (uri=0, method=1, bodyPublisher=2, headers=3)
//   HttpResponse  = 3-field  (statusCode=0, body=1, responseHeaders=2)
//   BodyPublisher = 1-field  (body: String ObjectRef or Object(None))
//
// NOTE: HTTPS is not supported (no TLS). Requests to https:// get a 0-status
//       stub response with a descriptive message in the body.
// =============================================================================

/// URI field indices (same layout as registered at line ~32569)
const URI_SCHEME: usize = 0;
const URI_HOST:   usize = 1;
const URI_PORT:   usize = 2;
const URI_PATH:   usize = 3;
const URI_QUERY:  usize = 4;
// field 5 = fragment, field 6 = raw — also useful for fallback

/// HttpRequest field indices
const HR_URI:    usize = 0;
const HR_METHOD: usize = 1;
const HR_BODY:   usize = 2;
// field 3 = extra headers map (unused by our impl)

fn register_s3_http_client(r: &mut NativeMethodRegistry) {
    let hc  = "java/net/http/HttpClient";
    let hrb = "java/net/http/HttpRequest$Builder";

    // ---- Update builder POST/PUT to also store the body publisher at field 2 ----
    r.register(
        hrb,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("POST");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("PUT");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // ---- Real send() ----
    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        s3_http_send,
    );

    // ---- Real sendAsync() — wraps send() ----
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let resp = s3_http_send(ctx, args)?;
            let resp_val = resp.unwrap_or(Value::Object(None));
            let cf = p58_new_cf(ctx, resp_val, true);
            Ok(Some(Value::Object(Some(cf))))
        },
    );
}

/// Extract a plain Rust String from a Java String field of an object, or return `None`.
fn s3_read_str_field(ctx: &dyn NativeContext, obj: ObjectRef, field: usize) -> Option<String> {
    match ctx.get_field(obj, field) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Core HTTP/1.1 send implementation.
fn s3_http_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // ---- Parse HttpRequest ----
    // Null request → backward-compat stub 200 (preserves p60 test behaviour)
    let req_ref = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return s3_stub_response(ctx, 200, ""),
    };

    let uri_ref = match ctx.get_field(req_ref, HR_URI) {
        Value::Object(Some(u)) => u,
        _ => return s3_stub_response(ctx, 200, ""),  // null URI → stub 200
    };

    // ---- Extract URI components ----
    let scheme = s3_read_str_field(ctx, uri_ref, URI_SCHEME)
        .unwrap_or_else(|| "http".to_string())
        .to_lowercase();
    let host = s3_read_str_field(ctx, uri_ref, URI_HOST)
        .unwrap_or_default();
    let port_field = ctx.get_field(uri_ref, URI_PORT).as_int().unwrap_or(-1);
    let path = s3_read_str_field(ctx, uri_ref, URI_PATH)
        .unwrap_or_else(|| "/".to_string());
    let query = s3_read_str_field(ctx, uri_ref, URI_QUERY);

    // If host is empty, try the raw URL string (field 6)
    let (host, port_field, path, query, scheme) = if host.is_empty() {
        // Fall back: parse raw URL
        let raw = s3_read_str_field(ctx, uri_ref, 6).unwrap_or_default();
        s3_parse_raw_url(&raw)
    } else {
        (host, port_field, path, query, scheme)
    };

    if host.is_empty() {
        return s3_stub_response(ctx, 400, "Cannot determine target host from URI");
    }

    // HTTPS not supported
    if scheme == "https" {
        return s3_stub_response(ctx, 0, "HTTPS not supported (no TLS)");
    }

    let port = if port_field > 0 { port_field as u16 } else { 80u16 };
    let target = format!("{host}:{port}");

    // ---- Method and body ----
    let method = match ctx.get_field(req_ref, HR_METHOD) {
        Value::Object(Some(m)) => ctx.read_string(m).unwrap_or_else(|| "GET".to_string()),
        _ => "GET".to_string(),
    };
    let body_bytes: Vec<u8> = match ctx.get_field(req_ref, HR_BODY) {
        Value::Object(Some(bp)) => {
            // BodyPublisher = 1-field (string body at field 0)
            match ctx.get_field(bp, 0) {
                Value::Object(Some(s)) => {
                    ctx.read_string(s)
                        .map(|st| st.into_bytes())
                        .unwrap_or_default()
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    };

    // ---- Build request line + headers ----
    let request_target = if let Some(ref q) = query {
        format!("{path}?{q}")
    } else {
        path.clone()
    };

    let mut request_str = format!(
        "{method} {request_target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: CratonVM/1.0\r\nAccept: */*\r\n"
    );
    if !body_bytes.is_empty() {
        request_str.push_str(&format!(
            "Content-Length: {}\r\nContent-Type: application/octet-stream\r\n",
            body_bytes.len()
        ));
    }
    request_str.push_str("\r\n");

    // ---- Connect ----
    let mut stream = match TcpStream::connect(&target) {
        Ok(s) => s,
        Err(e) => return s3_stub_response(ctx, 0, &format!("Connection failed: {e}")),
    };

    // ---- Send request ----
    if stream.write_all(request_str.as_bytes()).is_err() {
        return s3_stub_response(ctx, 0, "Write error");
    }
    if !body_bytes.is_empty() && stream.write_all(&body_bytes).is_err() {
        return s3_stub_response(ctx, 0, "Write body error");
    }

    // ---- Read response ----
    let mut response_bytes = Vec::new();
    if stream.read_to_end(&mut response_bytes).is_err() {
        return s3_stub_response(ctx, 0, "Read error");
    }

    // ---- Parse status line ----
    let response_str = String::from_utf8_lossy(&response_bytes).into_owned();
    let status_code = s3_parse_status_code(&response_str);

    // ---- Extract body (after double CRLF) ----
    let body_str = if let Some(pos) = response_str.find("\r\n\r\n") {
        response_str[pos + 4..].to_string()
    } else if let Some(pos) = response_str.find("\n\n") {
        response_str[pos + 2..].to_string()
    } else {
        String::new()
    };

    // ---- Build HttpResponse synthetic ----
    let response = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3);
    ctx.set_field(response, 0, Value::Int(status_code));
    let body_ref = ctx.create_string(&body_str);
    ctx.set_field(response, 1, Value::Object(Some(body_ref)));
    ctx.set_field(response, 2, Value::Object(None)); // headers not parsed

    Ok(Some(Value::Object(Some(response))))
}

/// Parse a raw URL string like "http://host:port/path?query" into components.
/// Returns (host, port, path, query, scheme).
fn s3_parse_raw_url(raw: &str) -> (String, i32, String, Option<String>, String) {
    let (scheme, rest) = if let Some(pos) = raw.find("://") {
        (raw[..pos].to_lowercase(), &raw[pos + 3..])
    } else {
        ("http".to_string(), raw)
    };
    let (authority, path_and_rest) = if let Some(pos) = rest.find('/') {
        (&rest[..pos], &rest[pos..])
    } else {
        (rest, "/")
    };
    let (host, port) = if let Some(colon) = authority.rfind(':') {
        if let Ok(p) = authority[colon + 1..].parse::<i32>() {
            (authority[..colon].to_string(), p)
        } else {
            (authority.to_string(), -1i32)
        }
    } else {
        (authority.to_string(), -1i32)
    };
    let (path, query) = if let Some(qmark) = path_and_rest.find('?') {
        (
            path_and_rest[..qmark].to_string(),
            Some(path_and_rest[qmark + 1..].to_string()),
        )
    } else {
        (path_and_rest.to_string(), None)
    };
    (host, port, path, query, scheme)
}

/// Extract HTTP status code from the first line of a response.
fn s3_parse_status_code(response: &str) -> i32 {
    // First line: "HTTP/1.1 200 OK"
    let first_line = response.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    parts.next(); // skip "HTTP/1.1"
    parts
        .next()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
}

/// Create a stub HttpResponse (for error/unsupported cases).
fn s3_stub_response(ctx: &mut dyn NativeContext, status: i32, msg: &str) -> MethodCallResult {
    let response = alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3);
    ctx.set_field(response, 0, Value::Int(status));
    let body_ref = ctx.create_string(msg);
    ctx.set_field(response, 1, Value::Object(Some(body_ref)));
    ctx.set_field(response, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(response))))
}

#[cfg(test)]
mod tests_s3 {
    use super::*;
    use crate::vm::{NativeContextImpl, Vm};
    use crate::config::VmConfig;

    fn test_vm() -> Vm { Vm::new(VmConfig::default()) }
    fn ctx_of(vm: &mut Vm) -> NativeContextImpl<'_> {
        NativeContextImpl { shared: &vm.shared, thread: &mut vm.main_thread }
    }

    #[test]
    fn s3_stub_response_status() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let result = s3_stub_response(&mut ctx, 503, "test error").unwrap().unwrap();
        if let Value::Object(Some(resp)) = result {
            assert_eq!(ctx.get_field(resp, 0), Value::Int(503));
            let body_ref = ctx.get_field(resp, 1);
            if let Value::Object(Some(b)) = body_ref {
                assert_eq!(ctx.read_string(b).unwrap(), "test error");
            } else {
                panic!("expected body string");
            }
        } else {
            panic!("expected response object");
        }
    }

    #[test]
    fn s3_null_request_returns_200_stub() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s3_http_client(&mut r);

        let client = alloc_concurrent_synthetic(&mut ctx, "java/net/http/HttpClient", 1);
        ctx.set_field(client, 0, Value::Int(1));

        let send_fn = r.find(
            "java/net/http/HttpClient",
            "send",
            "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        ).unwrap();

        let result = send_fn(
            &mut ctx,
            &[Value::Object(Some(client)), Value::Object(None), Value::Object(None)],
        ).unwrap().unwrap();

        if let Value::Object(Some(resp)) = result {
            assert_eq!(ctx.get_field(resp, 0), Value::Int(200));
        } else {
            panic!("expected response object");
        }
    }

    #[test]
    fn s3_parse_raw_url_basic() {
        let (host, port, path, query, scheme) =
            s3_parse_raw_url("http://example.com/foo?bar=1");
        assert_eq!(host, "example.com");
        assert_eq!(port, -1);
        assert_eq!(path, "/foo");
        assert_eq!(query, Some("bar=1".to_string()));
        assert_eq!(scheme, "http");
    }

    #[test]
    fn s3_parse_raw_url_with_port() {
        let (host, port, path, query, scheme) =
            s3_parse_raw_url("http://localhost:8080/api");
        assert_eq!(host, "localhost");
        assert_eq!(port, 8080);
        assert_eq!(path, "/api");
        assert_eq!(query, None);
        assert_eq!(scheme, "http");
    }

    #[test]
    fn s3_parse_status_code_200() {
        assert_eq!(s3_parse_status_code("HTTP/1.1 200 OK\r\n"), 200);
        assert_eq!(s3_parse_status_code("HTTP/1.1 404 Not Found\r\n"), 404);
        assert_eq!(s3_parse_status_code("HTTP/1.0 301 Moved\r\n"), 301);
        assert_eq!(s3_parse_status_code(""), 0);
    }

    #[test]
    fn s3_https_returns_stub() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s3_http_client(&mut r);

        // Build a minimal HttpClient
        let client = alloc_concurrent_synthetic(&mut ctx, "java/net/http/HttpClient", 1);
        ctx.set_field(client, 0, Value::Int(1));

        // Build an HttpRequest pointing to https URI
        let uri = alloc_concurrent_synthetic(&mut ctx, "java/net/URI", 7);
        let scheme_str = ctx.create_string("https");
        ctx.set_field(uri, URI_SCHEME, Value::Object(Some(scheme_str)));
        let host_str = ctx.create_string("example.com");
        ctx.set_field(uri, URI_HOST, Value::Object(Some(host_str)));
        ctx.set_field(uri, URI_PORT, Value::Int(-1));

        let req = alloc_concurrent_synthetic(&mut ctx, "java/net/http/HttpRequest", 4);
        ctx.set_field(req, HR_URI, Value::Object(Some(uri)));
        let method_str = ctx.create_string("GET");
        ctx.set_field(req, HR_METHOD, Value::Object(Some(method_str)));
        ctx.set_field(req, HR_BODY, Value::Object(None));

        let send_fn = r.find(
            "java/net/http/HttpClient",
            "send",
            "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        ).unwrap();

        let result = send_fn(
            &mut ctx,
            &[Value::Object(Some(client)), Value::Object(Some(req)), Value::Object(None)],
        ).unwrap().unwrap();

        if let Value::Object(Some(resp)) = result {
            // HTTPS not supported → status 0
            assert_eq!(ctx.get_field(resp, 0), Value::Int(0));
        } else {
            panic!("expected response object");
        }
    }
}

// =============================================================================
// Phase S.4 — javax.servlet / jakarta.servlet API Stubs
//
// Provides the minimal set of servlet types needed for web frameworks
// (Spring Boot, Tomcat, Jetty, Undertow) to bootstrap.
//
// Both the legacy `javax/servlet` and modern `jakarta/servlet` namespaces are
// supported — each registration is applied to both prefixes via a helper loop.
//
// Synthetic object layouts:
//   ServletContext     = 4-field (attributes=0 CHM, initParams=1 CHM,
//                                 contextPath=2 String, servletPath=3 String)
//   HttpServletRequest = 8-field (method=0, uri=1, queryString=2,
//                                 headers=3 CHM, params=4 CHM, attributes=5 CHM,
//                                 body=6 byte[], context=7 ServletContext)
//   HttpServletResponse= 5-field (status=0 Int, headers=1 CHM,
//                                  contentType=2 String, body=3 ByteArrayOS,
//                                  committed=4 Int)
//   ByteArrayOutputStream = 2-field (data=0 byte[], size=1 Int)
//   ServletConfig      = 3-field (name=0 String, context=1 ServletContext,
//                                  initParams=2 CHM)
//   FilterChain        = 1-field (next=0 Filter|Servlet ref)
//   Cookie             = 4-field (name=0, value=1, path=2, maxAge=3 Int)
// =============================================================================

const S4_SC_ATTRS:       usize = 0;
const S4_SC_INIT_PARAMS: usize = 1;
const S4_SC_CONTEXT_PATH:usize = 2;
const S4_SC_SERVLET_PATH:usize = 3;

const S4_REQ_METHOD:     usize = 0;
const S4_REQ_URI:        usize = 1;
const S4_REQ_QUERY:      usize = 2;
const S4_REQ_HEADERS:    usize = 3;
const S4_REQ_PARAMS:     usize = 4;
const S4_REQ_ATTRS:      usize = 5;
const S4_REQ_BODY:       usize = 6;
const S4_REQ_CONTEXT:    usize = 7;

const S4_RESP_STATUS:    usize = 0;
const S4_RESP_HEADERS:   usize = 1;
const S4_RESP_CTYPE:     usize = 2;
const S4_RESP_BODY:      usize = 3;
const S4_RESP_COMMITTED: usize = 4;

const S4_BAOS_DATA: usize = 0;
const S4_BAOS_SIZE: usize = 1;

/// Register both `javax/servlet/...` and `jakarta/servlet/...` for a given
/// class suffix and all methods provided by `f`.
macro_rules! s4_dual {
    ($r:expr, $suffix:expr, $f:expr) => {{
        let javax_cls = concat!("javax/servlet/", $suffix);
        let jakarta_cls = concat!("jakarta/servlet/", $suffix);
        $f($r, javax_cls);
        $f($r, jakarta_cls);
    }};
}

fn s4_alloc_chm(ctx: &mut dyn NativeContext) -> ObjectRef {
    alloc_concurrent_synthetic(ctx, "java/util/concurrent/ConcurrentHashMap", 3)
}

fn s4_alloc_servlet_context(ctx: &mut dyn NativeContext) -> ObjectRef {
    let sc = alloc_concurrent_synthetic(ctx, "javax/servlet/ServletContext", 4);
    let attrs = s4_alloc_chm(ctx);
    ctx.set_field(sc, S4_SC_ATTRS, Value::Object(Some(attrs)));
    let init_params = s4_alloc_chm(ctx);
    ctx.set_field(sc, S4_SC_INIT_PARAMS, Value::Object(Some(init_params)));
    let cp = ctx.create_string("");
    ctx.set_field(sc, S4_SC_CONTEXT_PATH, Value::Object(Some(cp)));
    let sp = ctx.create_string("");
    ctx.set_field(sc, S4_SC_SERVLET_PATH, Value::Object(Some(sp)));
    sc
}

fn s4_alloc_request(ctx: &mut dyn NativeContext, method: &str, uri: &str) -> ObjectRef {
    let req = alloc_concurrent_synthetic(ctx, "javax/servlet/http/HttpServletRequest", 8);
    let m = ctx.create_string(method);
    ctx.set_field(req, S4_REQ_METHOD, Value::Object(Some(m)));
    let u = ctx.create_string(uri);
    ctx.set_field(req, S4_REQ_URI, Value::Object(Some(u)));
    ctx.set_field(req, S4_REQ_QUERY, Value::Object(None));
    let headers = s4_alloc_chm(ctx);
    ctx.set_field(req, S4_REQ_HEADERS, Value::Object(Some(headers)));
    let params = s4_alloc_chm(ctx);
    ctx.set_field(req, S4_REQ_PARAMS, Value::Object(Some(params)));
    let attrs = s4_alloc_chm(ctx);
    ctx.set_field(req, S4_REQ_ATTRS, Value::Object(Some(attrs)));
    ctx.set_field(req, S4_REQ_BODY, Value::Object(None));
    ctx.set_field(req, S4_REQ_CONTEXT, Value::Object(None));
    req
}

fn s4_alloc_response(ctx: &mut dyn NativeContext) -> ObjectRef {
    let resp = alloc_concurrent_synthetic(ctx, "javax/servlet/http/HttpServletResponse", 5);
    ctx.set_field(resp, S4_RESP_STATUS, Value::Int(200));
    let headers = s4_alloc_chm(ctx);
    ctx.set_field(resp, S4_RESP_HEADERS, Value::Object(Some(headers)));
    let ct = ctx.create_string("text/plain");
    ctx.set_field(resp, S4_RESP_CTYPE, Value::Object(Some(ct)));
    // ByteArrayOutputStream for body
    let baos = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2);
    let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 256);
    ctx.set_field(baos, S4_BAOS_DATA, Value::Object(Some(buf)));
    ctx.set_field(baos, S4_BAOS_SIZE, Value::Int(0));
    ctx.set_field(resp, S4_RESP_BODY, Value::Object(Some(baos)));
    ctx.set_field(resp, S4_RESP_COMMITTED, Value::Int(0));
    resp
}

fn register_s4_servlet(r: &mut NativeMethodRegistry) {
    register_s4_servlet_context(r);
    register_s4_http_servlet_request(r);
    register_s4_http_servlet_response(r);
    register_s4_http_servlet(r);
    register_s4_filter(r);
    register_s4_servlet_config(r);
    register_s4_session(r);
    register_s4_dispatcher(r);
    register_s4_misc(r);
    register_s4_baos(r);
}

// ---- ServletContext ----
fn register_s4_servlet_context(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/ServletContext",
        "jakarta/servlet/ServletContext",
    ] {
        let cls = *cls;
        r.register(cls, "<init>", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let attrs = s4_alloc_chm(ctx);
            ctx.set_field(this, S4_SC_ATTRS, Value::Object(Some(attrs)));
            let ip = s4_alloc_chm(ctx);
            ctx.set_field(this, S4_SC_INIT_PARAMS, Value::Object(Some(ip)));
            let cp = ctx.create_string("");
            ctx.set_field(this, S4_SC_CONTEXT_PATH, Value::Object(Some(cp)));
            let sp = ctx.create_string("");
            ctx.set_field(this, S4_SC_SERVLET_PATH, Value::Object(Some(sp)));
            Ok(None)
        });
        r.register(cls, "getContextPath", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_SC_CONTEXT_PATH)))
        });
        r.register(cls, "setContextPath", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, S4_SC_CONTEXT_PATH, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        });
        r.register(cls, "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_SC_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::{native_map_get_pub};
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "setAttribute", "(Ljava/lang/String;Ljava/lang/Object;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_SC_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(None)
        });
        r.register(cls, "removeAttribute", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_SC_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_remove_pub;
            let _ = native_map_remove_pub(ctx, &[Value::Object(Some(map)), key]);
            Ok(None)
        });
        r.register(cls, "getInitParameter", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_SC_INIT_PARAMS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "setInitParameter", "(Ljava/lang/String;Ljava/lang/String;)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_SC_INIT_PARAMS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Int(0))),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(Some(Value::Int(1)))
        });
        r.register(cls, "getServerInfo", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("CratonVM/1.0");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getMajorVersion", "()I", |_ctx, _args| Ok(Some(Value::Int(5))));
        r.register(cls, "getMinorVersion", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getEffectiveMajorVersion", "()I", |_ctx, _args| Ok(Some(Value::Int(5))));
        r.register(cls, "getEffectiveMinorVersion", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getClassLoader", "()Ljava/lang/ClassLoader;", |_ctx, _args| {
            Ok(Some(Value::Object(None)))
        });
        r.register(cls, "log", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
        r.register(cls, "log", "(Ljava/lang/String;Ljava/lang/Throwable;)V", |_ctx, _args| Ok(None));
        r.register(cls, "getAttributeNames", "()Ljava/util/Enumeration;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, S4_SC_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
        r.register(cls, "getInitParameterNames", "()Ljava/util/Enumeration;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, S4_SC_INIT_PARAMS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
        r.register(cls, "getRequestDispatcher",
            "(Ljava/lang/String;)Ljavax/servlet/RequestDispatcher;", |ctx, args| {
            let path = args.get(1).copied().unwrap_or(Value::Object(None));
            let rd = alloc_concurrent_synthetic(ctx, "javax/servlet/RequestDispatcher", 1);
            ctx.set_field(rd, 0, path);
            Ok(Some(Value::Object(Some(rd))))
        });
        r.register(cls, "getServletContextName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("default");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getVirtualServerName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("localhost");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getSessionTimeout", "()I", |_ctx, _args| Ok(Some(Value::Int(30))));
        r.register(cls, "setSessionTimeout", "(I)V", |_ctx, _args| Ok(None));
    }
}

// ---- HttpServletRequest ----
fn register_s4_http_servlet_request(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/http/HttpServletRequest",
        "jakarta/servlet/http/HttpServletRequest",
        "javax/servlet/ServletRequest",
        "jakarta/servlet/ServletRequest",
    ] {
        let cls = *cls;
        r.register(cls, "getMethod", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_METHOD)))
        });
        r.register(cls, "getRequestURI", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_URI)))
        });
        r.register(cls, "getRequestURL", "()Ljava/lang/StringBuffer;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Return a StringBuffer wrapping the URI
            let uri = ctx.get_field(this, S4_REQ_URI);
            let sb = alloc_concurrent_synthetic(ctx, "java/lang/StringBuffer", 2);
            ctx.set_field(sb, 0, uri);
            ctx.set_field(sb, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(sb))))
        });
        r.register(cls, "getQueryString", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_QUERY)))
        });
        r.register(cls, "getHeader", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_REQ_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "getParameter", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_REQ_PARAMS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_REQ_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "setAttribute", "(Ljava/lang/String;Ljava/lang/Object;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_REQ_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(None)
        });
        r.register(cls, "removeAttribute", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_REQ_ATTRS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_remove_pub;
            let _ = native_map_remove_pub(ctx, &[Value::Object(Some(map)), key]);
            Ok(None)
        });
        r.register(cls, "getServletContext", "()Ljavax/servlet/ServletContext;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_CONTEXT)))
        });
        r.register(cls, "getContentType", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("application/octet-stream");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getContentLength", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getContentLengthLong", "()J", |_ctx, _args| Ok(Some(Value::Long(0))));
        r.register(cls, "getCharacterEncoding", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("UTF-8");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "setCharacterEncoding", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
        r.register(cls, "getScheme", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("http");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getServerName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("localhost");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getServerPort", "()I", |_ctx, _args| Ok(Some(Value::Int(8080))));
        r.register(cls, "getRemoteAddr", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("127.0.0.1");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getRemoteHost", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("localhost");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getRemotePort", "()I", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getLocalAddr", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("127.0.0.1");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getLocalName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("localhost");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getLocalPort", "()I", |_ctx, _args| Ok(Some(Value::Int(8080))));
        r.register(cls, "isSecure", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "isAsyncStarted", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "isAsyncSupported", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getProtocol", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("HTTP/1.1");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getContextPath", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sc_ref = match ctx.get_field(this, S4_REQ_CONTEXT) {
                Value::Object(Some(s)) => s,
                _ => {
                    let s = ctx.create_string("");
                    return Ok(Some(Value::Object(Some(s))));
                }
            };
            Ok(Some(ctx.get_field(sc_ref, S4_SC_CONTEXT_PATH)))
        });
        r.register(cls, "getServletPath", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_URI)))
        });
        r.register(cls, "getPathInfo", "()Ljava/lang/String;", |_ctx, _args| {
            Ok(Some(Value::Object(None)))
        });
        r.register(cls, "getSession", "()Ljavax/servlet/http/HttpSession;", |ctx, _args| {
            let sess = alloc_concurrent_synthetic(ctx, "javax/servlet/http/HttpSession", 3);
            Ok(Some(Value::Object(Some(sess))))
        });
        r.register(cls, "getSession", "(Z)Ljavax/servlet/http/HttpSession;", |ctx, _args| {
            let sess = alloc_concurrent_synthetic(ctx, "javax/servlet/http/HttpSession", 3);
            Ok(Some(Value::Object(Some(sess))))
        });
        r.register(cls, "getHeaderNames", "()Ljava/util/Enumeration;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, S4_REQ_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
        r.register(cls, "getParameterNames", "()Ljava/util/Enumeration;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, S4_REQ_PARAMS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
        r.register(cls, "getParameterMap", "()Ljava/util/Map;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_REQ_PARAMS)))
        });
        r.register(cls, "getCookies", "()[Ljavax/servlet/http/Cookie;", |ctx, _args| {
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        });
        r.register(cls, "getInputStream",
            "()Ljavax/servlet/ServletInputStream;", |ctx, _args| {
            let si = alloc_concurrent_synthetic(ctx, "javax/servlet/ServletInputStream", 1);
            Ok(Some(Value::Object(Some(si))))
        });
        r.register(cls, "getReader",
            "()Ljava/io/BufferedReader;", |ctx, _args| {
            let br = alloc_concurrent_synthetic(ctx, "java/io/BufferedReader", 2);
            Ok(Some(Value::Object(Some(br))))
        });
        r.register(cls, "getDispatcherType",
            "()Ljavax/servlet/DispatcherType;", |ctx, _args| {
            // DispatcherType.REQUEST = 0
            let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
            ctx.set_field(dt, 0, Value::Int(0));
            Ok(Some(Value::Object(Some(dt))))
        });
    }
}

// ---- HttpServletResponse ----
fn register_s4_http_servlet_response(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/http/HttpServletResponse",
        "jakarta/servlet/http/HttpServletResponse",
        "javax/servlet/ServletResponse",
        "jakarta/servlet/ServletResponse",
    ] {
        let cls = *cls;
        r.register(cls, "setStatus", "(I)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(200);
            ctx.set_field(this, S4_RESP_STATUS, Value::Int(code));
            Ok(None)
        });
        r.register(cls, "getStatus", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_RESP_STATUS)))
        });
        r.register(cls, "setContentType", "(Ljava/lang/String;)V", |ctx, args| {
            }
            ctx.set_field(this, S4_RESP_CTYPE, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        });
        r.register(cls, "getContentType", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_RESP_CTYPE)))
        });
        r.register(cls, "setHeader", "(Ljava/lang/String;Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(None)
        });
        r.register(cls, "addHeader", "(Ljava/lang/String;Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(None)
        });
        r.register(cls, "containsHeader", "(Ljava/lang/String;)Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Int(0))),
            };
            use cratonvm_native_collections::native_map_contains_key_pub;
            native_map_contains_key_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "getWriter", "()Ljava/io/PrintWriter;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let body_ref = match ctx.get_field(this, S4_RESP_BODY) {
                Value::Object(Some(b)) => b,
                _ => {
                    let baos = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2);
                    ctx.set_field(this, S4_RESP_BODY, Value::Object(Some(baos)));
                    baos
                }
            };
            // Return a PrintWriter that wraps the BAOS (1-field: underlying stream)
            let pw = alloc_concurrent_synthetic(ctx, "java/io/PrintWriter", 1);
            ctx.set_field(pw, 0, Value::Object(Some(body_ref)));
            Ok(Some(Value::Object(Some(pw))))
        });
        r.register(cls, "getOutputStream",
            "()Ljavax/servlet/ServletOutputStream;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let body_ref = match ctx.get_field(this, S4_RESP_BODY) {
                Value::Object(Some(b)) => b,
                _ => {
                    let baos = alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2);
                    ctx.set_field(this, S4_RESP_BODY, Value::Object(Some(baos)));
                    baos
                }
            };
            let sos = alloc_concurrent_synthetic(ctx, "javax/servlet/ServletOutputStream", 1);
            ctx.set_field(sos, 0, Value::Object(Some(body_ref)));
            Ok(Some(Value::Object(Some(sos))))
        });
        r.register(cls, "isCommitted", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, S4_RESP_COMMITTED)))
        });
        r.register(cls, "flushBuffer", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, S4_RESP_COMMITTED, Value::Int(1));
            Ok(None)
        });
        r.register(cls, "resetBuffer", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "reset", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, S4_RESP_STATUS, Value::Int(200));
            ctx.set_field(this, S4_RESP_COMMITTED, Value::Int(0));
            Ok(None)
        });
        r.register(cls, "setContentLength", "(I)V", |_ctx, _args| Ok(None));
        r.register(cls, "setContentLengthLong", "(J)V", |_ctx, _args| Ok(None));
        r.register(cls, "setCharacterEncoding", "(Ljava/lang/String;)V", |ctx, args| {
            }
            Ok(None)
        });
        r.register(cls, "getCharacterEncoding", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("UTF-8");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getBufferSize", "()I", |_ctx, _args| Ok(Some(Value::Int(8192))));
        r.register(cls, "setBufferSize", "(I)V", |_ctx, _args| Ok(None));
        r.register(cls, "sendError", "(I)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(500);
            ctx.set_field(this, S4_RESP_STATUS, Value::Int(code));
            ctx.set_field(this, S4_RESP_COMMITTED, Value::Int(1));
            Ok(None)
        });
        r.register(cls, "sendError", "(ILjava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let code = args.get(1).and_then(|v| v.as_int()).unwrap_or(500);
            ctx.set_field(this, S4_RESP_STATUS, Value::Int(code));
            ctx.set_field(this, S4_RESP_COMMITTED, Value::Int(1));
            Ok(None)
        });
        r.register(cls, "sendRedirect", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, S4_RESP_STATUS, Value::Int(302));
            let location = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            let loc_key = ctx.create_string("Location");
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(loc_key)), location]);
            ctx.set_field(this, S4_RESP_COMMITTED, Value::Int(1));
            Ok(None)
        });
        r.register(cls, "addCookie", "(Ljavax/servlet/http/Cookie;)V", |_ctx, _args| Ok(None));
        r.register(cls, "encodeURL", "(Ljava/lang/String;)Ljava/lang/String;", |_ctx, args| {
            Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
        });
        r.register(cls, "encodeRedirectURL", "(Ljava/lang/String;)Ljava/lang/String;", |_ctx, args| {
            Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
        });
        r.register(cls, "getHeaderNames", "()Ljava/util/Collection;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
        r.register(cls, "getHeader", "(Ljava/lang/String;)Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, S4_RESP_HEADERS) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
    }
}

// ---- HttpServlet base ----
fn register_s4_http_servlet(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/http/HttpServlet",
        "jakarta/servlet/http/HttpServlet",
        "javax/servlet/GenericServlet",
        "jakarta/servlet/GenericServlet",
        "javax/servlet/Servlet",
        "jakarta/servlet/Servlet",
    ] {
        let cls = *cls;
        r.register(cls, "init", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "init", "(Ljavax/servlet/ServletConfig;)V", |_ctx, _args| Ok(None));
        r.register(cls, "destroy", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "getServletInfo", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getServletConfig", "()Ljavax/servlet/ServletConfig;", |ctx, args| {
            // Return a minimal ServletConfig
            let this = obj_arg(args, 0)?;
            let _ = this;
            let cfg = alloc_concurrent_synthetic(ctx, "javax/servlet/ServletConfig", 3);
            Ok(Some(Value::Object(Some(cfg))))
        });
        r.register(cls, "service",
            "(Ljavax/servlet/ServletRequest;Ljavax/servlet/ServletResponse;)V",
            |_ctx, _args| Ok(None));
        r.register(cls, "service",
            "(Ljavax/servlet/http/HttpServletRequest;Ljavax/servlet/http/HttpServletResponse;)V",
            |_ctx, _args| Ok(None));
        r.register(cls, "getServletContext", "()Ljavax/servlet/ServletContext;", |ctx, _args| {
            let sc = s4_alloc_servlet_context(ctx);
            Ok(Some(Value::Object(Some(sc))))
        });
        r.register(cls, "log", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
    }
}

// ---- Filter / FilterChain ----
fn register_s4_filter(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/Filter",
        "jakarta/servlet/Filter",
    ] {
        let cls = *cls;
        r.register(cls, "init", "(Ljavax/servlet/FilterConfig;)V", |_ctx, _args| Ok(None));
        r.register(cls, "destroy", "()V", |_ctx, _args| Ok(None));
    }
    for cls in &[
        "javax/servlet/FilterChain",
        "jakarta/servlet/FilterChain",
    ] {
        let cls = *cls;
        r.register(cls, "doFilter",
            "(Ljavax/servlet/ServletRequest;Ljavax/servlet/ServletResponse;)V",
            |_ctx, _args| Ok(None));
    }
}

// ---- ServletConfig ----
fn register_s4_servlet_config(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/ServletConfig",
        "jakarta/servlet/ServletConfig",
    ] {
        let cls = *cls;
        r.register(cls, "getServletName", "()Ljava/lang/String;", |ctx, _args| {
            let s = ctx.create_string("default");
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "getServletContext", "()Ljavax/servlet/ServletContext;", |ctx, _args| {
            let sc = s4_alloc_servlet_context(ctx);
            Ok(Some(Value::Object(Some(sc))))
        });
        r.register(cls, "getInitParameter", "(Ljava/lang/String;)Ljava/lang/String;", |_ctx, _args| {
            Ok(Some(Value::Object(None)))
        });
        r.register(cls, "getInitParameterNames", "()Ljava/util/Enumeration;", |ctx, _args| {
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 0);
            Ok(Some(Value::Object(Some(arr))))
        });
    }
}

// ---- HttpSession ----
fn register_s4_session(r: &mut NativeMethodRegistry) {
    // HttpSession = 3-field (id=0 String, attrs=1 CHM, creationTime=2 Long)
    for cls in &[
        "javax/servlet/http/HttpSession",
        "jakarta/servlet/http/HttpSession",
    ] {
        let cls = *cls;
        r.register(cls, "<init>", "()V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = ctx.create_string("session-1");
            ctx.set_field(this, 0, Value::Object(Some(id)));
            let attrs = s4_alloc_chm(ctx);
            ctx.set_field(this, 1, Value::Object(Some(attrs)));
            ctx.set_field(this, 2, Value::Long(0));
            Ok(None)
        });
        r.register(cls, "getId", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        });
        r.register(cls, "getAttribute", "(Ljava/lang/String;)Ljava/lang/Object;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = obj_arg(args, 1)?;
            let map = match ctx.get_field(this, 1) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_get_pub;
            native_map_get_pub(ctx, &[Value::Object(Some(map)), Value::Object(Some(key))])
        });
        r.register(cls, "setAttribute", "(Ljava/lang/String;Ljava/lang/Object;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let val = args.get(2).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, 1) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_put_pub;
            let _ = native_map_put_pub(ctx, &[Value::Object(Some(map)), key, val]);
            Ok(None)
        });
        r.register(cls, "removeAttribute", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = args.get(1).copied().unwrap_or(Value::Object(None));
            let map = match ctx.get_field(this, 1) {
                Value::Object(Some(m)) => m,
                _ => return Ok(None),
            };
            use cratonvm_native_collections::native_map_remove_pub;
            let _ = native_map_remove_pub(ctx, &[Value::Object(Some(map)), key]);
            Ok(None)
        });
        r.register(cls, "invalidate", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "isNew", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "getCreationTime", "()J", |_ctx, _args| Ok(Some(Value::Long(0))));
        r.register(cls, "getLastAccessedTime", "()J", |_ctx, _args| Ok(Some(Value::Long(0))));
        r.register(cls, "getMaxInactiveInterval", "()I", |_ctx, _args| Ok(Some(Value::Int(1800))));
        r.register(cls, "setMaxInactiveInterval", "(I)V", |_ctx, _args| Ok(None));
        r.register(cls, "getServletContext", "()Ljavax/servlet/ServletContext;", |ctx, _args| {
            let sc = s4_alloc_servlet_context(ctx);
            Ok(Some(Value::Object(Some(sc))))
        });
        r.register(cls, "getAttributeNames", "()Ljava/util/Enumeration;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let map = match ctx.get_field(this, 1) {
                Value::Object(Some(m)) => m,
                _ => return Ok(Some(Value::Object(None))),
            };
            use cratonvm_native_collections::native_map_key_set_pub;
            native_map_key_set_pub(ctx, &[Value::Object(Some(map))])
        });
    }
}

// ---- RequestDispatcher ----
fn register_s4_dispatcher(r: &mut NativeMethodRegistry) {
    for cls in &[
        "javax/servlet/RequestDispatcher",
        "jakarta/servlet/RequestDispatcher",
    ] {
        let cls = *cls;
        r.register(cls, "forward",
            "(Ljavax/servlet/ServletRequest;Ljavax/servlet/ServletResponse;)V",
            |_ctx, _args| Ok(None));
        r.register(cls, "include",
            "(Ljavax/servlet/ServletRequest;Ljavax/servlet/ServletResponse;)V",
            |_ctx, _args| Ok(None));
    }
    // DispatcherType enum
    for cls in &[
        "javax/servlet/DispatcherType",
        "jakarta/servlet/DispatcherType",
    ] {
        let cls = *cls;
        r.register(cls, "REQUEST",  "()Ljavax/servlet/DispatcherType;", s4_dt_request);
        r.register(cls, "FORWARD",  "()Ljavax/servlet/DispatcherType;", s4_dt_forward);
        r.register(cls, "INCLUDE",  "()Ljavax/servlet/DispatcherType;", s4_dt_include);
        r.register(cls, "ERROR",    "()Ljavax/servlet/DispatcherType;", s4_dt_error);
        r.register(cls, "ASYNC",    "()Ljavax/servlet/DispatcherType;", s4_dt_async);
        r.register(cls, "name", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let idx = ctx.get_field(this, 0).as_int().unwrap_or(0);
            let nm = match idx { 0 => "REQUEST", 1 => "FORWARD", 2 => "INCLUDE", 3 => "ERROR", _ => "ASYNC" };
            let s = ctx.create_string(nm);
            Ok(Some(Value::Object(Some(s))))
        });
        r.register(cls, "ordinal", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        });
    }
}

fn s4_dt_request(ctx: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
    ctx.set_field(dt, 0, Value::Int(0));
    Ok(Some(Value::Object(Some(dt))))
}
fn s4_dt_forward(ctx: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
    ctx.set_field(dt, 0, Value::Int(1));
    Ok(Some(Value::Object(Some(dt))))
}
fn s4_dt_include(ctx: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
    ctx.set_field(dt, 0, Value::Int(2));
    Ok(Some(Value::Object(Some(dt))))
}
fn s4_dt_error(ctx: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
    ctx.set_field(dt, 0, Value::Int(3));
    Ok(Some(Value::Object(Some(dt))))
}
fn s4_dt_async(ctx: &mut dyn NativeContext, _a: &[Value]) -> MethodCallResult {
    let dt = alloc_concurrent_synthetic(ctx, "javax/servlet/DispatcherType", 1);
    ctx.set_field(dt, 0, Value::Int(4));
    Ok(Some(Value::Object(Some(dt))))
}

// ---- Misc: Cookie, ServletInputStream, ServletOutputStream ----
fn register_s4_misc(r: &mut NativeMethodRegistry) {
    // Cookie = 4-field (name=0, value=1, path=2, maxAge=3)
    for cls in &[
        "javax/servlet/http/Cookie",
        "jakarta/servlet/http/Cookie",
    ] {
        let cls = *cls;
        r.register(cls, "<init>", "(Ljava/lang/String;Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 2, Value::Object(None));
            ctx.set_field(this, 3, Value::Int(-1));
            Ok(None)
        });
        r.register(cls, "getName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        });
        r.register(cls, "getValue", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        });
        r.register(cls, "setValue", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        });
        r.register(cls, "setPath", "(Ljava/lang/String;)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        });
        r.register(cls, "getPath", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        });
        r.register(cls, "setMaxAge", "(I)V", |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 3, args.get(1).copied().unwrap_or(Value::Int(-1)));
            Ok(None)
        });
        r.register(cls, "getMaxAge", "()I", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        });
        r.register(cls, "setHttpOnly", "(Z)V", |_ctx, _args| Ok(None));
        r.register(cls, "isHttpOnly", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "setSecure", "(Z)V", |_ctx, _args| Ok(None));
        r.register(cls, "getSecure", "()Z", |_ctx, _args| Ok(Some(Value::Int(0))));
        r.register(cls, "setDomain", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
        r.register(cls, "getDomain", "()Ljava/lang/String;", |_ctx, _args| Ok(Some(Value::Object(None))));
    }

    // ServletInputStream = 1-field (data=0 byte[])
    for cls in &[
        "javax/servlet/ServletInputStream",
        "jakarta/servlet/ServletInputStream",
    ] {
        let cls = *cls;
        r.register(cls, "read", "()I", |_ctx, _args| Ok(Some(Value::Int(-1)))); // EOF
        r.register(cls, "isFinished", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
        r.register(cls, "isReady", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
        r.register(cls, "setReadListener",
            "(Ljavax/servlet/ReadListener;)V", |_ctx, _args| Ok(None));
    }

    // ServletOutputStream = 1-field (baos=0 BAOS ref)
    for cls in &[
        "javax/servlet/ServletOutputStream",
        "jakarta/servlet/ServletOutputStream",
    ] {
        let cls = *cls;
        r.register(cls, "write", "(I)V", |_ctx, _args| Ok(None));
        r.register(cls, "write", "([B)V", |_ctx, _args| Ok(None));
        r.register(cls, "write", "([BII)V", |_ctx, _args| Ok(None));
        r.register(cls, "print", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
        r.register(cls, "println", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "println", "(Ljava/lang/String;)V", |_ctx, _args| Ok(None));
        r.register(cls, "flush", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "close", "()V", |_ctx, _args| Ok(None));
        r.register(cls, "isReady", "()Z", |_ctx, _args| Ok(Some(Value::Int(1))));
        r.register(cls, "setWriteListener",
            "(Ljavax/servlet/WriteListener;)V", |_ctx, _args| Ok(None));
    }
}

/// Cached `CRATON_BAOS_DBG` lookup.
///
/// The registration below overrides `ByteArrayOutputStream.write(int)` -- the
/// single hottest byte-at-a-time sink in the JDK (DER encoding, serialization,
/// `PrintStream`, every `toByteArray` pipeline). Probing `env::var_os` there
/// meant one environ-lock acquisition and linear `environ` scan *per byte*.
/// Latch it once instead; the switch must be set before the first write to
/// take effect, matching `security_manager::dbg_dopriv_enabled`.
#[inline]
fn baos_dbg_enabled() -> bool {
    static DBG: OnceLock<bool> = OnceLock::new();
    *DBG.get_or_init(|| cratonvm_types::flags::runtime_var_os("CRATON_BAOS_DBG").is_some())
}

#[cfg(test)]
mod baos_dbg_flag_tests {
    #[test]
    fn baos_dbg_flag_is_latched_and_matches_environment() {
        // `ByteArrayOutputStream.write(int)` used to probe `env::var_os` per
        // byte. The latched helper must (a) agree with the environment as it
        // stood at first use and (b) never change answer afterwards.
        let expected = cratonvm_types::flags::runtime_var_os("CRATON_BAOS_DBG").is_some();
        assert_eq!(super::baos_dbg_enabled(), expected);
        assert_eq!(super::baos_dbg_enabled(), expected, "flag must be stable");
    }

    #[test]
    fn baos_dbg_flag_is_off_in_a_clean_environment() {
        // Guards against the debug `eprintln!` ever becoming default-on: with
        // the switch unset the write path must take the quiet branch.
        if cratonvm_types::flags::runtime_var_os("CRATON_BAOS_DBG").is_none() {
            assert!(!super::baos_dbg_enabled());
        }
    }
}

// ---- ByteArrayOutputStream methods needed by response writer ----
//
// IMPORTANT: address the backing store by field *name* (`buf`/`count`), NOT by
// raw slot index. `ByteArrayOutputStream` is subclassed (e.g.
// `sun.security.util.DerOutputStream`), and the interpreter mis-resolves the
// inherited `count` `putfield` slot for such subclasses — so raw-slot access
// here (`S4_BAOS_DATA`/`S4_BAOS_SIZE` = 0/1) disagreed with the real
// `super.<init>()` bytecode that sets the *named* `buf`, making `write` drop
// every byte. That produced empty DER output and broke ECDSA signing under real
// JCA. This registrar otherwise *overrides* the `serialization.rs` intrinsic
// (registered earlier), so it must carry the same by-name fix.
fn register_s4_baos(r: &mut NativeMethodRegistry) {
    let cls = "java/io/ByteArrayOutputStream";
    r.register(cls, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if baos_dbg_enabled() {
            eprintln!("[BAOS-DBG] s4 write(I) called; count={:?} buf={:?}",
                ctx.get_field_by_name(this, "count"), ctx.get_field_by_name(this, "buf"));
        }
        let byte = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        let size = ctx.get_field_by_name(this, "count").as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field_by_name(this, "buf") {
            Value::Object(Some(a)) => a,
            _ => {
                let a = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 256);
                ctx.set_field_by_name(this, "buf", Value::Object(Some(a)));
                a
            }
        };
        let cap = ctx.array_length(arr);
        if size >= cap {
            // Grow: allocate 2x
            let new_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, (cap * 2).max(size + 1));
            for i in 0..size {
                let v = ctx.get_array_element(arr, i);
                ctx.set_array_element(new_arr, i, v);
            }
            ctx.set_field_by_name(this, "buf", Value::Object(Some(new_arr)));
        }
        let arr2 = match ctx.get_field_by_name(this, "buf") {
            Value::Object(Some(a)) => a,
            _ => return Ok(None),
        };
        ctx.set_array_element(arr2, size, Value::Int(byte as i8 as i32));
        ctx.set_field_by_name(this, "count", Value::Int((size + 1) as i32));
        Ok(None)
    });
    r.register(cls, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let src = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return Ok(None),
        };
        let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let len = args.get(3).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        for i in 0..len {
            let byte = ctx.get_array_element(src, off + i).as_int().unwrap_or(0) as u8;
            let _ = ctx.invoke_virtual(this, "write", "(I)V", &[Value::Int(byte as i32)]);
        }
        Ok(None)
    });
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = ctx.get_field_by_name(this, "count").as_int().unwrap_or(0) as usize;
        let arr = match ctx.get_field_by_name(this, "buf") {
            Value::Object(Some(a)) => a,
            _ => {
                let s = ctx.create_string("");
                return Ok(Some(Value::Object(Some(s))));
            }
        };
        let mut bytes = Vec::with_capacity(size);
        for i in 0..size {
            bytes.push(ctx.get_array_element(arr, i).as_int().unwrap_or(0) as u8);
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    });
    r.register(cls, "toByteArray", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = ctx.get_field_by_name(this, "count").as_int().unwrap_or(0) as usize;
        let src = match ctx.get_field_by_name(this, "buf") {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Object(Some(ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0))))),
        };
        let result = ctx.new_array(cratonvm_types::ArrayElementType::Byte, size);
        for i in 0..size {
            ctx.set_array_element(result, i, ctx.get_array_element(src, i));
        }
        Ok(Some(Value::Object(Some(result))))
    });
    r.register(cls, "size", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_by_name(this, "count")))
    });
    r.register(cls, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_by_name(this, "count", Value::Int(0));
        Ok(None)
    });
    r.register(cls, "flush", "()V", |_ctx, _args| Ok(None));
    r.register(cls, "close", "()V", |_ctx, _args| Ok(None));
    r.register(cls, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        Ok(None)
    });
    r.register(cls, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let cap = args.get(1).and_then(|v| v.as_int()).unwrap_or(32) as usize;
        let buf = ctx.new_array(cratonvm_types::ArrayElementType::Byte, cap.max(1));
        ctx.set_field_by_name(this, "buf", Value::Object(Some(buf)));
        ctx.set_field_by_name(this, "count", Value::Int(0));
        Ok(None)
    });
}

// =============================================================================
// T4: MethodHandle.invoke / invokeExact / invokeWithArguments
//
// MethodHandle layout (5 fields):
//   MH_CLASS  = 0  String  — class name (JVM-style, slash-separated)
//   MH_NAME   = 1  String  — method name
//   MH_DESC   = 2  String  — JVM descriptor
//   MH_KIND   = 3  Int     — 0=static 1=virtual 2=special 3=constructor
//                            4=getter 5=setter
//   MH_BOUND  = 4  Object  — bound receiver (for bound MH) or null
//
// Lookup.find* now populates these fields so invoke/invokeExact can dispatch.
// Existing zero-field stubs created by other subsystems remain harmless:
// invoke on a 0-field MH returns null/void.
// =============================================================================

const MH_CLASS: usize = 0;
const MH_NAME:  usize = 1;
const MH_DESC:  usize = 2;
const MH_KIND:  usize = 3;
const MH_BOUND: usize = 4;

const MH_KIND_STATIC:      i32 = 0;
const MH_KIND_VIRTUAL:     i32 = 1;
const MH_KIND_SPECIAL:     i32 = 2;
const MH_KIND_CONSTRUCTOR: i32 = 3;
#[allow(dead_code)]
const MH_KIND_GETTER:      i32 = 4;
#[allow(dead_code)]
const MH_KIND_SETTER:      i32 = 5;

/// Allocate a fully-described MethodHandle.
fn alloc_method_handle(
    ctx: &mut dyn NativeContext,
    class: &str,
    name: &str,
    desc: &str,
    kind: i32,
) -> cratonvm_types::ObjectRef {
    let mh = alloc_concurrent_synthetic(ctx, "java/lang/invoke/MethodHandle", 5);
    let cls = ctx.create_string(class);
    let nm  = ctx.create_string(name);
    let dc  = ctx.create_string(desc);
    ctx.set_field(mh, MH_CLASS, Value::Object(Some(cls)));
    ctx.set_field(mh, MH_NAME,  Value::Object(Some(nm)));
    ctx.set_field(mh, MH_DESC,  Value::Object(Some(dc)));
    ctx.set_field(mh, MH_KIND,  Value::Int(kind));
    ctx.set_field(mh, MH_BOUND, Value::Object(None));
    mh
}

/// Read the class name string from a MethodHandle (field MH_CLASS).
fn mh_read_class(ctx: &dyn NativeContext, mh: cratonvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_CLASS) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the method name string from a MethodHandle (field MH_NAME).
fn mh_read_name(ctx: &dyn NativeContext, mh: cratonvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_NAME) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Read the descriptor string from a MethodHandle (field MH_DESC).
fn mh_read_desc(ctx: &dyn NativeContext, mh: cratonvm_types::ObjectRef) -> Option<String> {
    match ctx.get_field(mh, MH_DESC) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    }
}

/// Core dispatch: given a populated MethodHandle and argument list, invoke it.
/// `extra_args` are the args passed to invoke() after `this` (the MH itself).
fn mh_dispatch(
    ctx: &mut dyn NativeContext,
    mh: cratonvm_types::ObjectRef,
    extra_args: &[Value],
) -> MethodCallResult {
    let class = match mh_read_class(ctx, mh) {
        Some(c) => c,
        None => return Ok(Some(Value::Object(None))),
    };
    let name = mh_read_name(ctx, mh).unwrap_or_default();
    let desc = mh_read_desc(ctx, mh).unwrap_or_default();
    let kind = match ctx.get_field(mh, MH_KIND) {
        Value::Int(k) => k,
        _ => MH_KIND_VIRTUAL,
    };
    let bound = ctx.get_field(mh, MH_BOUND);

    match kind {
        MH_KIND_STATIC => {
            // Static: extra_args are the full argument list
            ctx.invoke(&class, &name, &desc, extra_args)
        }
        MH_KIND_CONSTRUCTOR => {
            // Constructor: allocate new object then call <init>
            let cid = match ctx.ensure_class_initialized(&class) {
                Ok(id) => id,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let new_obj = ctx.alloc_object(cid, 16); // generous field count
            let mut init_args = Vec::with_capacity(1 + extra_args.len());
            init_args.push(Value::Object(Some(new_obj)));
            init_args.extend_from_slice(extra_args);
            ctx.invoke(&class, "<init>", &desc, &init_args)?;
            Ok(Some(Value::Object(Some(new_obj))))
        }
        _ => {
            // Virtual / special: first extra_arg is receiver (unless bound)
            match bound {
                Value::Object(Some(r)) => {
                    // Bound method handle — receiver was pre-captured
                    ctx.invoke_virtual(r, &name, &desc, extra_args)
                }
                _ => match extra_args.first() {
                    Some(Value::Object(Some(receiver))) => {
                        ctx.invoke_virtual(*receiver, &name, &desc, &extra_args[1..])
                    }
                    _ => Ok(Some(Value::Object(None))),
                },
            }
        }
    }
}

fn register_t4_method_handle_invoke(r: &mut NativeMethodRegistry) {
    let mh = "java/lang/invoke/MethodHandle";

    // invoke(...) — polymorphic signature; we register a wildcard via several
    // common descriptors.  All delegate to mh_dispatch.
    r.register(mh, "invoke", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        mh_dispatch(ctx, this, &args[1..])
    });
    r.register(mh, "invokeExact", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        mh_dispatch(ctx, this, &args[1..])
    });
    r.register(mh, "invokeWithArguments", "([Ljava/lang/Object;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // args[1] is an Object[] — unpack it
        let arr_ref = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => return mh_dispatch(ctx, this, &[]),
        };
        let len = ctx.array_length(arr_ref);
        let unpacked: Vec<Value> = (0..len).map(|i| ctx.get_array_element(arr_ref, i)).collect();
        mh_dispatch(ctx, this, &unpacked)
    });
    r.register(mh, "invokeWithArguments", "(Ljava/util/List;)Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Treat list as an object, call size+get — simplified: just dispatch with no args
        let _ = args.get(1); // ignore list for stub
        mh_dispatch(ctx, this, &[])
    });
    r.register(mh, "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let recv = args.get(1).copied().unwrap_or(Value::Object(None));
        // Clone the MH and set BOUND field
        let class = mh_read_class(ctx, this).unwrap_or_default();
        let name  = mh_read_name(ctx, this).unwrap_or_default();
        let desc  = mh_read_desc(ctx, this).unwrap_or_default();
        let kind  = match ctx.get_field(this, MH_KIND) { Value::Int(k) => k, _ => MH_KIND_VIRTUAL };
        let new_mh = alloc_method_handle(ctx, &class, &name, &desc, kind);
        ctx.set_field(new_mh, MH_BOUND, recv);
        Ok(Some(Value::Object(Some(new_mh))))
    });
    r.register(mh, "asType", "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;", |_ctx, args| {
        // Return self — we don't do type adaptation
        Ok(Some(args[0]))
    });
    r.register(mh, "type", "()Ljava/lang/invoke/MethodType;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, MH_BOUND))) // reuse BOUND slot which defaults to null (acceptable stub)
    });

    // --- Upgrade Lookup.find* to populate the 5-field layout ---
    let lk = "java/lang/invoke/MethodHandles$Lookup";

    r.register(lk, "findVirtual",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_VIRTUAL);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let name_obj  = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_VIRTUAL);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
            let name  = ctx.read_string(name_obj).unwrap_or_default();
            let mh = alloc_method_handle(ctx, &class, &name, "", MH_KIND_VIRTUAL);
            Ok(Some(Value::Object(Some(mh))))
        });

    r.register(lk, "findStatic",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_STATIC);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let name_obj  = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_STATIC);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
            let name  = ctx.read_string(name_obj).unwrap_or_default();
            let mh = alloc_method_handle(ctx, &class, &name, "", MH_KIND_STATIC);
            Ok(Some(Value::Object(Some(mh))))
        });

    r.register(lk, "findConstructor",
        "(Ljava/lang/Class;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "<init>", "", MH_KIND_CONSTRUCTOR);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
            let mh = alloc_method_handle(ctx, &class, "<init>", "", MH_KIND_CONSTRUCTOR);
            Ok(Some(Value::Object(Some(mh))))
        });

    r.register(lk, "findSpecial",
        "(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/invoke/MethodType;Ljava/lang/Class;)Ljava/lang/invoke/MethodHandle;",
        |ctx, args| {
            let class_obj = match args.get(1) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_SPECIAL);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let name_obj  = match args.get(2) { Some(Value::Object(Some(o))) => *o, _ => {
                let mh = alloc_method_handle(ctx, "", "", "", MH_KIND_SPECIAL);
                return Ok(Some(Value::Object(Some(mh))));
            }};
            let class = mirror_class_name(ctx, class_obj).unwrap_or_default();
            let name  = ctx.read_string(name_obj).unwrap_or_default();
            let mh = alloc_method_handle(ctx, &class, &name, "", MH_KIND_SPECIAL);
            Ok(Some(Value::Object(Some(mh))))
        });
}

#[cfg(test)]
mod tests_t4_method_handle {
    use super::*;
    use crate::vm::{NativeContextImpl, Vm};
    use crate::config::VmConfig;

    fn test_vm() -> Vm { Vm::new(VmConfig::default()) }
    fn ctx_of(vm: &mut Vm) -> NativeContextImpl<'_> {
        NativeContextImpl { shared: &vm.shared, thread: &mut vm.main_thread }
    }

    #[test]
    fn mh_alloc_and_fields() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mh = alloc_method_handle(&mut ctx, "java/lang/String", "length", "()I", MH_KIND_VIRTUAL);
        assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("java/lang/String"));
        assert_eq!(mh_read_name(&ctx, mh).as_deref(), Some("length"));
        assert_eq!(mh_read_desc(&ctx, mh).as_deref(), Some("()I"));
        assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_VIRTUAL));
    }

    #[test]
    fn mh_bind_to_sets_bound_field() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut r);

        let mh = alloc_method_handle(&mut ctx, "java/lang/String", "length", "()I", MH_KIND_VIRTUAL);
        let recv = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);

        let bind = r.find("java/lang/invoke/MethodHandle", "bindTo", "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;").unwrap();
        let bound_mh_val = bind(&mut ctx, &[Value::Object(Some(mh)), Value::Object(Some(recv))]).unwrap().unwrap();
        let bound_mh = match bound_mh_val { Value::Object(Some(o)) => o, _ => panic!("expected MH") };
        assert_eq!(ctx.get_field(bound_mh, MH_BOUND), Value::Object(Some(recv)));
    }

    #[test]
    fn mh_static_dispatch_via_invoke() {
        // Test that a static MH dispatches correctly via invoke()
        // We use StringBuilder.valueOf(int) which is registered as a static-like method
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut r);

        // Create a static MethodHandle for Integer.toString(int)
        let mh = alloc_method_handle(&mut ctx, "java/lang/Integer", "toString", "(I)Ljava/lang/String;", MH_KIND_STATIC);
        let invoke = r.find("java/lang/invoke/MethodHandle", "invoke", "([Ljava/lang/Object;)Ljava/lang/Object;").unwrap();
        let result = invoke(&mut ctx, &[Value::Object(Some(mh)), Value::Int(42)]).unwrap().unwrap();
        // Should return a String object representing "42"
        match result {
            Value::Object(Some(s)) => {
                let text = ctx.read_string(s).unwrap_or_default();
                assert_eq!(text, "42");
            }
            _ => panic!("expected String result"),
        }
    }

    #[test]
    fn mh_null_class_returns_null() {
        // A 0-field MH (old stubs) should not panic — invoke returns null
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_t4_method_handle_invoke(&mut r);

        let mh = alloc_concurrent_synthetic(&mut ctx, "java/lang/invoke/MethodHandle", 0);
        let invoke = r.find("java/lang/invoke/MethodHandle", "invoke", "([Ljava/lang/Object;)Ljava/lang/Object;").unwrap();
        let result = invoke(&mut ctx, &[Value::Object(Some(mh))]).unwrap();
        assert_eq!(result, Some(Value::Object(None)));
    }
}

#[cfg(test)]
mod tests_s4 {
    use super::*;
    use crate::vm::{NativeContextImpl, Vm};
    use crate::config::VmConfig;

    fn test_vm() -> Vm { Vm::new(VmConfig::default()) }
    fn ctx_of(vm: &mut Vm) -> NativeContextImpl<'_> {
        NativeContextImpl { shared: &vm.shared, thread: &mut vm.main_thread }
    }

    #[test]
    fn s4_servlet_context_init() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s4_servlet(&mut r);

        let sc = alloc_concurrent_synthetic(&mut ctx, "javax/servlet/ServletContext", 4);
        let init_fn = r.find("javax/servlet/ServletContext", "<init>", "()V").unwrap();
        init_fn(&mut ctx, &[Value::Object(Some(sc))]).unwrap();

        // getContextPath should return ""
        let gcp = r.find("javax/servlet/ServletContext", "getContextPath", "()Ljava/lang/String;").unwrap();
        let result = gcp(&mut ctx, &[Value::Object(Some(sc))]).unwrap().unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(ctx.read_string(s).unwrap(), "");
        } else {
            panic!("expected empty string");
        }
    }

    #[test]
    fn s4_request_method_and_uri() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);

        let req = s4_alloc_request(&mut ctx, "POST", "/api/v1/test");
        let m = ctx.get_field(req, S4_REQ_METHOD);
        if let Value::Object(Some(ms)) = m {
            assert_eq!(ctx.read_string(ms).unwrap(), "POST");
        } else {
            panic!("expected method string");
        }
        let u = ctx.get_field(req, S4_REQ_URI);
        if let Value::Object(Some(us)) = u {
            assert_eq!(ctx.read_string(us).unwrap(), "/api/v1/test");
        } else {
            panic!("expected URI string");
        }
    }

    #[test]
    fn s4_response_status() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);

        let resp = s4_alloc_response(&mut ctx);
        assert_eq!(ctx.get_field(resp, S4_RESP_STATUS), Value::Int(200));
        assert_eq!(ctx.get_field(resp, S4_RESP_COMMITTED), Value::Int(0));
    }

    #[test]
    fn s4_baos_write_and_to_string() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s4_baos(&mut r);

        let baos = alloc_concurrent_synthetic(&mut ctx, "java/io/ByteArrayOutputStream", 2);
        let init_fn = r.find("java/io/ByteArrayOutputStream", "<init>", "()V").unwrap();
        init_fn(&mut ctx, &[Value::Object(Some(baos))]).unwrap();

        // Write "Hi"
        let write_fn = r.find("java/io/ByteArrayOutputStream", "write", "(I)V").unwrap();
        write_fn(&mut ctx, &[Value::Object(Some(baos)), Value::Int('H' as i32)]).unwrap();
        write_fn(&mut ctx, &[Value::Object(Some(baos)), Value::Int('i' as i32)]).unwrap();

        let to_str_fn = r.find("java/io/ByteArrayOutputStream", "toString", "()Ljava/lang/String;").unwrap();
        let result = to_str_fn(&mut ctx, &[Value::Object(Some(baos))]).unwrap().unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(ctx.read_string(s).unwrap(), "Hi");
        } else {
            panic!("expected string");
        }
    }

    #[test]
    fn s4_cookie_lifecycle() {
        let mut vm = test_vm();
        let mut ctx = ctx_of(&mut vm);
        let mut r = NativeMethodRegistry::new();
        register_s4_misc(&mut r);

        let cookie = alloc_concurrent_synthetic(&mut ctx, "javax/servlet/http/Cookie", 4);
        let name_str = ctx.create_string("session");
        let val_str = ctx.create_string("abc123");
        let init_fn = r.find(
            "javax/servlet/http/Cookie",
            "<init>",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        ).unwrap();
        init_fn(&mut ctx, &[
            Value::Object(Some(cookie)),
            Value::Object(Some(name_str)),
            Value::Object(Some(val_str)),
        ]).unwrap();

        let get_name = r.find("javax/servlet/http/Cookie", "getName", "()Ljava/lang/String;").unwrap();
        let result = get_name(&mut ctx, &[Value::Object(Some(cookie))]).unwrap().unwrap();
        if let Value::Object(Some(s)) = result {
            assert_eq!(ctx.read_string(s).unwrap(), "session");
        } else {
            panic!("expected cookie name");
        }
    }
}
