use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn t2_thread_local_object_values_are_global_rooted() {
    let mut ctx = mock_ctx();
    let obj1 = ctx.alloc_object(ClassId::new(41), 0);
    let stored1 = tl_value_from_java(&mut ctx, Value::Object(Some(obj1)));
    assert_eq!(ctx.global_root_count(), 1);
    assert_eq!(tl_value_to_java(&ctx, stored1), Value::Object(Some(obj1)));

    let obj2 = ctx.alloc_object(ClassId::new(42), 0);
    let stored2 = tl_value_from_java(&mut ctx, Value::Object(Some(obj2)));
    assert_eq!(ctx.global_root_count(), 2);

    tl_drop_value_root(&mut ctx, stored1);
    assert_eq!(ctx.global_root_count(), 1);
    assert_eq!(tl_value_to_java(&ctx, stored2), Value::Object(Some(obj2)));

    tl_drop_value_root(&mut ctx, stored2);
    assert_eq!(ctx.global_root_count(), 0);

    let plain = tl_value_from_java(&mut ctx, Value::Int(7));
    assert_eq!(ctx.global_root_count(), 0);
    assert_eq!(tl_value_to_java(&ctx, plain), Value::Int(7));
}

// -----------------------------------------------------------------------
// T2.3.13: StringTokenizer.countTokens — O(n) single pass
// -----------------------------------------------------------------------

fn make_tokenizer(
    ctx: &mut dyn NativeContext,
    input: &str,
    delims: &str,
) -> cratonvm_types::ObjectRef {
    let st =
        crate::try_alloc_concurrent_synthetic(ctx, "java/util/StringTokenizer", 3).unwrap();
    let input_s = ctx.create_string(input);
    let delim_s = ctx.create_string(delims);
    ctx.set_field(st, ST_FIELD_INPUT, Value::Object(Some(input_s)));
    ctx.set_field(st, ST_FIELD_POS, Value::Int(0));
    ctx.set_field(st, ST_FIELD_DELIMS, Value::Object(Some(delim_s)));
    st
}

#[test]
fn t2_st_count_tokens_basic_whitespace() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "one two three", " ");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

#[test]
fn t2_st_count_tokens_leading_and_trailing_delims() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "   alpha   beta   ", " ");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(2)));
}

#[test]
fn t2_st_count_tokens_empty_string() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "", " ");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn t2_st_count_tokens_only_delims() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "     ", " ");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(0)));
}

#[test]
fn t2_st_count_tokens_multi_delim_set() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "a,b;c,d;e", ",;");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(5)));
}

#[test]
fn t2_st_count_tokens_single_token_no_delim() {
    let mut ctx = mock_ctx();
    let st = make_tokenizer(&mut ctx, "solo", " ");
    let r = native_st_count_tokens(&mut ctx, &[Value::Object(Some(st))]);
    assert_eq!(r.unwrap(), Some(Value::Int(1)));
}

// -----------------------------------------------------------------------
// T2.3.1: ArraysSupport.vectorizedHashCode
// -----------------------------------------------------------------------

fn make_int_array(ctx: &mut dyn NativeContext, data: &[i32]) -> cratonvm_types::ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Int, data.len());
    for (i, &v) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(v));
    }
    arr
}

fn make_byte_array(ctx: &mut dyn NativeContext, data: &[u8]) -> cratonvm_types::ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Byte, data.len());
    for (i, &v) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(v as i8 as i32));
    }
    arr
}

fn read_char_array(ctx: &dyn NativeContext, arr: cratonvm_types::ObjectRef) -> Vec<u16> {
    (0..ctx.array_length(arr))
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v as u16,
            _ => 0,
        })
        .collect()    }

#[test]
fn t2_string_latin1_inflate_zero_extends_bytes_into_chars() {
    let mut ctx = mock_ctx();
    let src = make_byte_array(&mut ctx, &[0x41, 0x80, 0xff, 0x42]);
    let dst = ctx.new_array(ArrayElementType::Char, 6);
    native_string_latin1_inflate(
        &mut ctx,
        &[
            Value::Object(Some(src)),
            Value::Int(1),
            Value::Object(Some(dst)),
            Value::Int(2),
            Value::Int(3),
        ],
    )
    .unwrap();
    assert_eq!(read_char_array(&ctx, dst), vec![0, 0, 0x80, 0xff, 0x42, 0]);
}


#[test]
fn t2_string_latin1_inflate_registered() {
    let mut registry = NativeMethodRegistry::new();
    register_string_latin1_natives(&mut registry);
    assert!(registry
        .find("java/lang/StringLatin1", "inflate", "([BI[CII)V")
        .is_some());
}

fn mock_field_zero_hash_code(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    _args: &[Value],
) -> Option<MethodCallResult> {
    if (method_name, descriptor) != ("hashCode", "()I") {
        return None;
    }
    match ctx.get_field(receiver, 0) {
        Value::Int(v) => Some(Ok(Some(Value::Int(v)))),
        _ => Some(Ok(Some(Value::Int(0)))),
    }
}

#[test]
fn t2_arrays_object_hash_code_uses_virtual_hash_and_null_zero() {
    let mut ctx = mock_ctx();
    ctx.set_invoke_virtual_hook(mock_field_zero_hash_code);
    let first = ctx.fresh_object_ref();
    let second = ctx.fresh_object_ref();
    ctx.set_field(first, 0, Value::Int(7));
    ctx.set_field(second, 0, Value::Int(11));
    let arr = ctx.new_array(ArrayElementType::Reference, 3);
    ctx.set_array_element(arr, 0, Value::Object(Some(first)));
    ctx.set_array_element(arr, 1, Value::Object(None));
    ctx.set_array_element(arr, 2, Value::Object(Some(second)));

    let mut registry = NativeMethodRegistry::new();
    register_core_stdlib_extras(&mut registry);
    let hash_code = registry
        .find("java/util/Arrays", "hashCode", "([Ljava/lang/Object;)I")
        .expect("Arrays.hashCode(Object[]) native should be registered");
    let result = hash_code(&mut ctx, &[Value::Object(Some(arr))]);

    let expected = 1i32
        .wrapping_mul(31)
        .wrapping_add(7)
        .wrapping_mul(31)
        .wrapping_add(0)
        .wrapping_mul(31)
        .wrapping_add(11);
    assert_eq!(result.unwrap(), Some(Value::Int(expected)));
}

#[test]
fn t2_arrays_object_hash_code_uses_value_hash_for_jdk_wrappers() {
    let mut ctx = mock_ctx();
    let int_class = ctx.ensure_class_initialized("java/lang/Integer").unwrap();
    let float_class = ctx.ensure_class_initialized("java/lang/Float").unwrap();
    let string = ctx.create_string("field");
    let int_obj = ctx.alloc_object(int_class, 1);
    ctx.set_field(int_obj, 0, Value::Int(2));
    let float_obj = ctx.alloc_object(float_class, 1);
    ctx.set_field(float_obj, 0, Value::Float(0.0));
    let arr = ctx.new_array(ArrayElementType::Reference, 5);
    ctx.set_array_element(arr, 0, Value::Object(Some(string)));
    ctx.set_array_element(arr, 1, Value::Object(Some(int_obj)));
    ctx.set_array_element(arr, 2, Value::Object(Some(int_obj)));
    ctx.set_array_element(arr, 3, Value::Object(None));
    ctx.set_array_element(arr, 4, Value::Object(Some(float_obj)));

    let mut registry = NativeMethodRegistry::new();
    register_core_stdlib_extras(&mut registry);
    let hash_code = registry
        .find("java/util/Arrays", "hashCode", "([Ljava/lang/Object;)I")
        .expect("Arrays.hashCode(Object[]) native should be registered");
    let first = hash_code(&mut ctx, &[Value::Object(Some(arr))]);
    let second = hash_code(&mut ctx, &[Value::Object(Some(arr))]);

    let expected = 1i32
        .wrapping_mul(31)
        .wrapping_add(97_427_706)
        .wrapping_mul(31)
        .wrapping_add(2)
        .wrapping_mul(31)
        .wrapping_add(2)
        .wrapping_mul(31)
        .wrapping_add(0)
        .wrapping_mul(31)
        .wrapping_add(0);
    assert_eq!(first.unwrap(), Some(Value::Int(expected)));
    assert_eq!(second.unwrap(), Some(Value::Int(expected)));
}

static ATOMIC_REF_TARGET_OLD: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_TARGET_NEW: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_OP_OLD: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_OP_NEW: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_CUR_OLD: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_CUR_NEW: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_APPLIED: AtomicUsize = AtomicUsize::new(0);
static ATOMIC_REF_APPLY_CALLS: AtomicUsize = AtomicUsize::new(0);

fn relocating_atomic_ref_apply(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if (method_name, descriptor) != ("apply", "(Ljava/lang/Object;)Ljava/lang/Object;") {
        return None;
    }
    ATOMIC_REF_APPLY_CALLS.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        receiver.as_ptr() as usize,
        ATOMIC_REF_OP_OLD.load(Ordering::SeqCst)
    );
    assert_eq!(
        args.first().copied(),
        Some(Value::Object(Some(unsafe {
            ObjectRef::from_raw(ATOMIC_REF_CUR_OLD.load(Ordering::SeqCst) as *mut u8)
        })))
    );
    ctx.remap_native_pin_addr_for_test(
        ATOMIC_REF_TARGET_OLD.load(Ordering::SeqCst),
        ATOMIC_REF_TARGET_NEW.load(Ordering::SeqCst),
    );
    ctx.remap_native_pin_addr_for_test(
        ATOMIC_REF_OP_OLD.load(Ordering::SeqCst),
        ATOMIC_REF_OP_NEW.load(Ordering::SeqCst),
    );
    ctx.remap_native_pin_addr_for_test(
        ATOMIC_REF_CUR_OLD.load(Ordering::SeqCst),
        ATOMIC_REF_CUR_NEW.load(Ordering::SeqCst),
    );
    Some(Ok(Some(Value::Object(Some(unsafe {
        ObjectRef::from_raw(ATOMIC_REF_APPLIED.load(Ordering::SeqCst) as *mut u8)
    })))))
}

#[test]
fn atomic_reference_update_rereads_pins_after_operator_gc() {
    let mut ctx = mock_ctx();
    let target_old = ctx.fresh_object_ref();
    let target_new = ctx.fresh_object_ref();
    let op_old = ctx.fresh_object_ref();
    let op_new = ctx.fresh_object_ref();
    let cur_old = ctx.fresh_object_ref();
    let cur_new = ctx.fresh_object_ref();
    let applied = ctx.fresh_object_ref();

    ctx.set_field(target_old, 0, Value::Object(Some(cur_old)));
    ctx.set_field(target_new, 0, Value::Object(Some(cur_new)));

    ATOMIC_REF_TARGET_OLD.store(target_old.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_TARGET_NEW.store(target_new.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_OP_OLD.store(op_old.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_OP_NEW.store(op_new.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_CUR_OLD.store(cur_old.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_CUR_NEW.store(cur_new.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_APPLIED.store(applied.as_ptr() as usize, Ordering::SeqCst);
    ATOMIC_REF_APPLY_CALLS.store(0, Ordering::SeqCst);
    ctx.set_invoke_virtual_hook(relocating_atomic_ref_apply);

    let result = atomic_reference_update_with_operator(&mut ctx, target_old, op_old, true);

    assert_eq!(result.unwrap(), Some(Value::Object(Some(applied))));
    assert_eq!(ATOMIC_REF_APPLY_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(ctx.get_field(target_new, 0), Value::Object(Some(applied)));
    assert_eq!(ctx.get_field(target_old, 0), Value::Object(Some(cur_old)));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// Reference 31-mul-accumulating hash that mirrors the Java formula.
/// Used to independently verify the native.
fn ref_hash_int(data: &[i32], from: usize, len: usize, initial: i32) -> i32 {
    let mut acc = initial;
    for &v in data.iter().skip(from).take(len) {
        acc = acc.wrapping_mul(31).wrapping_add(v);
    }
    acc
}

/// str16: the bulk fold decodes every shape as the slot loop does --
/// unsigned bytes for `T_BOOLEAN` (the `StringLatin1` entry), signed for
/// `T_BYTE`, zero-extended `char[]`, sign-extended `short[]`, raw `int[]`,
/// and little-endian pairs for `T_CHAR` over a `byte[]`.
#[test]
fn str16_bulk_hash_fold_matches_the_slot_decode() {
    let bytes = [0x6bu8, 0x31, 0xff, 0x80, 0x2d, 0x00];
    let slot = |init: i32, f: &dyn Fn(u8) -> i32| {
        bytes
            .iter()
            .fold(init, |a, &b| a.wrapping_mul(31).wrapping_add(f(b)))
    };
    assert_eq!(
        fold_hash_payload(0, &bytes, 1, false, HOTSPOT_T_BOOLEAN, false),
        slot(0, &|b| i32::from(b)),
        "Latin-1 hashes its bytes unsigned, as Java's String.hashCode"
    );
    assert_eq!(
        fold_hash_payload(1, &bytes, 1, false, HOTSPOT_T_BYTE, false),
        slot(1, &|b| b as i8 as i32)
    );
    let chars: Vec<u8> = [0xFFFEu16, 0x03A3]
        .iter()
        .flat_map(|c| c.to_le_bytes())
        .collect();
    assert_eq!(
        fold_hash_payload(0, &chars, 2, false, HOTSPOT_T_CHAR, false),
        0xFFFE_i32.wrapping_mul(31).wrapping_add(0x03A3)
    );
    assert_eq!(
        fold_hash_payload(0, &chars, 1, false, HOTSPOT_T_CHAR, true),
        0xFFFE_i32.wrapping_mul(31).wrapping_add(0x03A3),
        "T_CHAR over a byte[]: little-endian pairs"
    );
    assert_eq!(
        fold_hash_payload(0, &chars, 2, true, HOTSPOT_T_SHORT, false),
        (-2i32).wrapping_mul(31).wrapping_add(0x03A3)
    );
    let ints: Vec<u8> = [-7i32, 1 << 30]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    assert_eq!(
        fold_hash_payload(1, &ints, 4, false, HOTSPOT_T_INT, false),
        1i32.wrapping_mul(31)
            .wrapping_add(-7)
            .wrapping_mul(31)
            .wrapping_add(1 << 30)
    );
}

#[test]
fn t2_arrays_support_hash_code_int_matches_reference() {
    let mut ctx = mock_ctx();
    let data = &[1, 2, 3, 4, 5];
    let arr = make_int_array(&mut ctx, data);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(5),
            Value::Int(1),
            Value::Int(HOTSPOT_T_INT),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(ref_hash_int(data, 0, 5, 1))));
}

#[test]
fn t2_arrays_support_hash_code_char_zero_extends() {
    // A char element with value 0xFFFE should contribute as 65534,
    // not as -2 (the i16 sign-extension).
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[0xFFFE]);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
            Value::Int(HOTSPOT_T_CHAR),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(0xFFFE)));
}

/// `T_CHAR` over a **byte[]** is the `StringUTF16.hashCode` shape: two
/// slots per element, little-endian, and `length` counting CHARS.
///
/// This is the case that was wrong until 2026-08-05 -- the loop read one
/// slot per iteration for every `BasicType`, so a 3-char string hashed
/// bytes 0,1,2 and `(elem as u16)` sign-extended each signed byte.
/// `"\u03A3\u039F\u03A3".hashCode()` returned 62956255 where the JLS and
/// HotSpot say 924359.
///
/// Note what the sibling `t2_arrays_support_hash_code_char_zero_extends`
/// pins: `T_CHAR` over a **char[]** (`Arrays.hashCode(char[])`) is ONE
/// slot per element. The two shapes share a `basicType` and differ only in
/// the array, which is why the first attempt at this fix keyed on
/// `basicType` alone and broke the other one. Keep both tests adjacent.
#[test]
fn t2_arrays_support_hash_code_char_over_byte_array_pairs_little_endian() {
    let mut ctx = mock_ctx();
    // "\u03A3\u039F\u03A3" as CratonVM stores it: low byte first.
    let arr = make_byte_array(&mut ctx, &[0xA3, 0x03, 0x9F, 0x03, 0xA3, 0x03]);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(3), // CHAR count, not byte count
            Value::Int(0),
            Value::Int(HOTSPOT_T_CHAR),
        ],
    );
    assert_eq!(
        r.unwrap(),
        Some(Value::Int(924359)),
        "T_CHAR over a byte[] must pair the bytes little-endian. 62956255 is the \
         pre-2026-08-05 answer: one slot per iteration, each signed byte sign-extended \
         to a char. 161735 would be pairing without masking; 41407139 would be \
         big-endian pairing."
    );
}

/// The byte[] path honours `fromIndex` in CHARS, and refuses to read past
/// the end when `fromIndex + 2*length` would.
///
/// The bounds guard has to scale with the same stride as the reader. It did
/// not in the first version of this fix -- it compared `fromIndex + length`
/// against the array length, which for the byte[] shape is half the bytes
/// actually touched, so an over-long `length` read off the end instead of
/// returning the initial value.
#[test]
fn t2_arrays_support_hash_code_char_over_byte_array_bounds_use_the_pair_stride() {
    let mut ctx = mock_ctx();
    let arr = make_byte_array(&mut ctx, &[0xA3, 0x03, 0x9F, 0x03, 0xA3, 0x03]);

    // fromIndex is a CHAR index: start at char 1 -> bytes 2..6 -> 2 chars.
    let from_one = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(1),
            Value::Int(2),
            Value::Int(0),
            Value::Int(HOTSPOT_T_CHAR),
        ],
    );
    // 31 * 0x039F + 0x03A3 = 29727 + ... computed as the JLS fold.
    let expected = 31i32.wrapping_mul(0x039F).wrapping_add(0x03A3);
    assert_eq!(from_one.unwrap(), Some(Value::Int(expected)));

    // 4 chars over a 6-byte array needs 8 bytes: refuse, return initial.
    let too_long = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(4),
            Value::Int(7),
            Value::Int(HOTSPOT_T_CHAR),
        ],
    );
    assert_eq!(
        too_long.unwrap(),
        Some(Value::Int(7)),
        "the bounds guard must scale by the pair stride, not the char count"
    );
}

#[test]
fn t2_arrays_support_hash_code_byte_sign_extends() {
    // A byte element with value 0xFF should contribute as -1 under
    // T_BYTE semantics, not 255.
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[0xFF]);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
            Value::Int(HOTSPOT_T_BYTE),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

#[test]
fn t2_arrays_support_hash_code_zero_length_returns_initial() {
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[1, 2, 3]);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(0),
            Value::Int(42),
            Value::Int(HOTSPOT_T_INT),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(42)));
}

/// hash21: a null array answers the caller's seed (0 for the String paths,
/// 1 for `Arrays.hashCode`), as the JDK body does for an empty range; it
/// used to answer 1 whatever the seed was.
#[test]
fn hash21_arrays_support_hash_code_null_array_returns_the_seed() {
    let mut ctx = mock_ctx();
    for seed in [0, 1, 42, -7] {
        let r = native_arrays_support_vectorized_hash_code(
            &mut ctx,
            &[
                Value::Object(None),
                Value::Int(0),
                Value::Int(0),
                Value::Int(seed),
                Value::Int(HOTSPOT_T_BYTE),
            ],
        );
        assert_eq!(r.unwrap(), Some(Value::Int(seed)), "seed {seed}");
    }
}

#[test]
fn t2_arrays_support_hash_code_out_of_bounds_returns_initial() {
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[1, 2, 3]);
    let r = native_arrays_support_vectorized_hash_code(
        &mut ctx,
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(100),
            Value::Int(7),
            Value::Int(HOTSPOT_T_INT),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(7)));
}

// -----------------------------------------------------------------------
// T2.3.1: ArraysSupport.vectorizedMismatch
// -----------------------------------------------------------------------

#[test]
fn t2_arrays_support_mismatch_all_equal_returns_neg_one() {
    let mut ctx = mock_ctx();
    let a = make_int_array(&mut ctx, &[1, 2, 3, 4]);
    let b = make_int_array(&mut ctx, &[1, 2, 3, 4]);
    // log2Scale = 2 for int (4-byte elements), offsets = 0.
    let r = native_arrays_support_vectorized_mismatch(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(0),
            Value::Object(Some(b)),
            Value::Long(0),
            Value::Int(4),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

#[test]
fn t2_arrays_support_mismatch_first_diff_at_index_two() {
    let mut ctx = mock_ctx();
    let a = make_int_array(&mut ctx, &[1, 2, 3, 4]);
    let b = make_int_array(&mut ctx, &[1, 2, 99, 4]);
    let r = native_arrays_support_vectorized_mismatch(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(0),
            Value::Object(Some(b)),
            Value::Long(0),
            Value::Int(4),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(2)));
}

#[test]
fn t2_arrays_support_mismatch_empty_length_returns_neg_one() {
    let mut ctx = mock_ctx();
    let a = make_int_array(&mut ctx, &[1]);
    let b = make_int_array(&mut ctx, &[2]);
    let r = native_arrays_support_vectorized_mismatch(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(0),
            Value::Object(Some(b)),
            Value::Long(0),
            Value::Int(0),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(-1)));
}

// JIT round 17 wave 6, lane vecmis: a refused range answers `!length`
// ("nothing compared"), so the JDK caller's scalar tail decides. It used
// to answer -1, "all equal", which made the caller report equality.
#[test]
fn t2_arrays_support_mismatch_out_of_range_length_is_safe() {
    let mut ctx = mock_ctx();
    let a = make_int_array(&mut ctx, &[1, 2]);
    let b = make_int_array(&mut ctx, &[1, 2, 3]);
    // Request 5 elements; both arrays are shorter.
    let r = native_arrays_support_vectorized_mismatch(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(0),
            Value::Object(Some(b)),
            Value::Long(0),
            Value::Int(5),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(!5)));
}

fn vecmis_call(
    ctx: &mut dyn NativeContext,
    a: cratonvm_types::ObjectRef,
    a_off: i64,
    b: cratonvm_types::ObjectRef,
    b_off: i64,
    length: i32,
    log2_scale: i32,
) -> Option<Value> {
    native_arrays_support_vectorized_mismatch(
        ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(a_off),
            Value::Object(Some(b)),
            Value::Long(b_off),
            Value::Int(length),
            Value::Int(log2_scale),
        ],
    )
    .unwrap()
}

// An unknown scale, a negative offset and an offset that is not a whole
// element past the base are refused with `!length` too.
#[test]
fn r17_vecmis_refusals_answer_not_length() {
    let mut ctx = mock_ctx();
    let a = make_int_array(&mut ctx, &[1, 2, 3, 4]);
    let b = make_int_array(&mut ctx, &[1, 2, 3, 5]);
    assert_eq!(
        vecmis_call(&mut ctx, a, 16, b, 16, 4, 7),
        Some(Value::Int(!4))
    );
    assert_eq!(
        vecmis_call(&mut ctx, a, -8, b, 16, 4, 2),
        Some(Value::Int(!4))
    );
    assert_eq!(
        vecmis_call(&mut ctx, a, 18, b, 16, 3, 2),
        Some(Value::Int(!3))
    );
    // In range, aligned: still the first differing index.
    assert_eq!(
        vecmis_call(&mut ctx, a, 20, b, 20, 3, 2),
        Some(Value::Int(2))
    );
    // A zero length is "equal", as the JDK's Java body answers.
    assert_eq!(
        vecmis_call(&mut ctx, a, 16, b, 16, 0, 2),
        Some(Value::Int(-1))
    );
}

// Float/double elements compare by BITS, as HotSpot's memory compare:
// `0.0f` and `-0.0f` differ, two identical NaNs do not.
#[test]
fn r17_vecmis_float_and_double_compare_bits() {
    let mut ctx = mock_ctx();
    let fa = ctx.new_array(ArrayElementType::Float, 3);
    let fb = ctx.new_array(ArrayElementType::Float, 3);
    for (i, (x, y)) in [(1.0f32, 1.0f32), (f32::NAN, f32::NAN), (0.0, -0.0)]
        .into_iter()
        .enumerate()
    {
        ctx.set_array_element(fa, i, Value::Float(x));
        ctx.set_array_element(fb, i, Value::Float(y));
    }
    assert_eq!(
        vecmis_call(&mut ctx, fa, 16, fb, 16, 3, 2),
        Some(Value::Int(2))
    );
    assert_eq!(
        vecmis_call(&mut ctx, fa, 16, fb, 16, 2, 2),
        Some(Value::Int(-1))
    );

    let da = ctx.new_array(ArrayElementType::Double, 2);
    let db = ctx.new_array(ArrayElementType::Double, 2);
    ctx.set_array_element(da, 0, Value::Double(f64::NAN));
    ctx.set_array_element(db, 0, Value::Double(f64::NAN));
    ctx.set_array_element(da, 1, Value::Double(-0.0));
    ctx.set_array_element(db, 1, Value::Double(0.0));
    assert_eq!(
        vecmis_call(&mut ctx, da, 16, db, 16, 2, 3),
        Some(Value::Int(1))
    );
}

// BUFCMP17-2: the windowed raw-bytes compare answers what the whole-range
// compare answers, including a difference past the first window and a
// side that cannot be read.
#[test]
fn r17_vecmis_windowed_compare_matches_whole_range() {
    let mut ctx = mock_ctx();
    let bytes: Vec<u8> = [1i32, 2, 3, 4, 5, -6]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let a = make_byte_array(&mut ctx, &bytes);
    let b = make_int_array(&mut ctx, &[1, 2, 3, 4, 5, 6]);
    let same = make_int_array(&mut ctx, &[1, 2, 3, 4, 5, -6]);
    for window in [8usize, 16, 24] {
        let run = |ctx: &mut dyn NativeContext, other: cratonvm_types::ObjectRef, n: i32| {
            vectorized_mismatch_compare_windows(
                ctx,
                Some(a),
                16,
                Some(other),
                16,
                n as usize * 4,
                4,
                n,
                window,
            )
        };
        assert_eq!(run(&mut ctx, b, 6), 5, "window {window}");
        assert_eq!(run(&mut ctx, same, 6), -1, "window {window}");
        // Past the end of both arrays: nothing decided.
        assert_eq!(run(&mut ctx, same, 7), !7, "window {window}");
    }
}

// JIT round 17 wave 4, lane bufcmp: a NULL base is an off-heap address (a
// direct buffer view's `base()`), compared as raw bytes; it used to answer
// -1 ("equal") for any null base.
#[test]
fn r17_arrays_support_mismatch_null_base_reads_the_address() {
    let mut ctx = mock_ctx();
    let native: Vec<u8> = [1i32, 2, 3, -4]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let addr = native.as_ptr() as i64;
    let other = make_int_array(&mut ctx, &[1, 2, 9, -4]);
    let same = make_int_array(&mut ctx, &[1, 2, 3, -4]);
    let call = |ctx: &mut dyn NativeContext, b: cratonvm_types::ObjectRef| {
        native_arrays_support_vectorized_mismatch(
            ctx,
            &[
                Value::Object(None),
                Value::Long(addr),
                Value::Object(Some(b)),
                Value::Long(16),
                Value::Int(4),
                Value::Int(2),
            ],
        )
        .unwrap()
    };
    assert_eq!(call(&mut ctx, other), Some(Value::Int(2)));
    assert_eq!(call(&mut ctx, same), Some(Value::Int(-1)));
}

// A `byte[]` under an int-scaled compare (a native-order heap view) is
// compared as bytes, not one byte per int.
#[test]
fn r17_arrays_support_mismatch_byte_array_under_int_scale() {
    let mut ctx = mock_ctx();
    let bytes: Vec<u8> = [1i32, 2, 3, -4]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let a = make_byte_array(&mut ctx, &bytes);
    let b = make_int_array(&mut ctx, &[1, 2, 3, 5]);
    let r = native_arrays_support_vectorized_mismatch(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Long(16),
            Value::Object(Some(b)),
            Value::Long(16),
            Value::Int(4),
            Value::Int(2),
        ],
    );
    assert_eq!(r.unwrap(), Some(Value::Int(3)));
}

// -----------------------------------------------------------------------
// T2.3.2: ConcurrentHashMap.tabAt / casTabAt / setTabAt
// -----------------------------------------------------------------------

fn make_ref_array(ctx: &mut dyn NativeContext, values: &[Value]) -> cratonvm_types::ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Reference, values.len());
    for (i, v) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, *v);
    }
    arr
}

#[test]
fn t2_chm_tab_at_reads_array_slot() {
    let mut ctx = mock_ctx();
    let a = make_ref_array(&mut ctx, &[Value::Int(10), Value::Int(20), Value::Int(30)]);
    let r = native_chm_tab_at(&mut ctx, &[Value::Object(Some(a)), Value::Int(1)]).unwrap();
    assert_eq!(r, Some(Value::Int(20)));
}

#[test]
fn t2_chm_tab_at_out_of_range_returns_null() {
    let mut ctx = mock_ctx();
    let a = make_ref_array(&mut ctx, &[Value::Int(10)]);
    let r = native_chm_tab_at(&mut ctx, &[Value::Object(Some(a)), Value::Int(5)]).unwrap();
    assert_eq!(r, Some(Value::Object(None)));
}

#[test]
fn t2_chm_set_tab_at_writes_slot() {
    let mut ctx = mock_ctx();
    let a = make_ref_array(&mut ctx, &[Value::Int(1), Value::Int(2)]);
    native_chm_set_tab_at(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Int(0), Value::Int(99)],
    )
    .unwrap();
    assert_eq!(ctx.get_array_element(a, 0), Value::Int(99));
}

#[test]
fn t2_chm_cas_tab_at_swaps_on_match() {
    let mut ctx = mock_ctx();
    let a = make_ref_array(&mut ctx, &[Value::Int(7)]);
    let r = native_chm_cas_tab_at(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Int(0),
            Value::Int(7),
            Value::Int(42),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(1)));
    assert_eq!(ctx.get_array_element(a, 0), Value::Int(42));
}

#[test]
fn t2_chm_cas_tab_at_leaves_slot_on_mismatch() {
    let mut ctx = mock_ctx();
    let a = make_ref_array(&mut ctx, &[Value::Int(7)]);
    let r = native_chm_cas_tab_at(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Int(0),
            Value::Int(99),
            Value::Int(42),
        ],
    )
    .unwrap();
    assert_eq!(r, Some(Value::Int(0)));
    assert_eq!(ctx.get_array_element(a, 0), Value::Int(7));
}

// -----------------------------------------------------------------------
// T2.3.3: ArrayList.elementData(int)
// -----------------------------------------------------------------------

fn make_arraylist(ctx: &mut dyn NativeContext, values: &[Value]) -> cratonvm_types::ObjectRef {
    let list = crate::try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2).unwrap();
    let data = make_ref_array(ctx, values);
    ctx.set_field(list, 0, Value::Object(Some(data)));
    ctx.set_field(list, 1, Value::Int(values.len() as i32));
    list
}

#[test]
fn t2_arraylist_element_data_returns_slot_value() {
    let mut ctx = mock_ctx();
    let list = make_arraylist(&mut ctx, &[Value::Int(11), Value::Int(22), Value::Int(33)]);
    let r =
        native_arraylist_element_data(&mut ctx, &[Value::Object(Some(list)), Value::Int(2)])
            .unwrap();
    assert_eq!(r, Some(Value::Int(33)));
}

#[test]
fn t2_arraylist_element_data_oob_throws() {
    let mut ctx = mock_ctx();
    let list = make_arraylist(&mut ctx, &[Value::Int(1)]);
    let r =
        native_arraylist_element_data(&mut ctx, &[Value::Object(Some(list)), Value::Int(7)]);
    assert!(r.is_err());
}

#[test]
fn t2_arraylist_element_data_negative_index_throws() {
    let mut ctx = mock_ctx();
    let list = make_arraylist(&mut ctx, &[Value::Int(1)]);
    let r =
        native_arraylist_element_data(&mut ctx, &[Value::Object(Some(list)), Value::Int(-1)]);
    assert!(r.is_err());
}

// -----------------------------------------------------------------------
// T2.3.6: Arrays.parallelSort
// -----------------------------------------------------------------------

#[test]
fn t2_parallel_sort_int_full_array() {
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[5, 2, 8, 1, 9, 3]);
    native_parallel_sort_int(&mut ctx, &[Value::Object(Some(arr))]).unwrap();
    let got: Vec<i32> = (0..6)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            _ => 0,
        })
        .collect();
    assert_eq!(got, vec![1, 2, 3, 5, 8, 9]);
}

#[test]
fn t2_parallel_sort_int_range_sorts_only_subrange() {
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[5, 2, 8, 1, 9, 3]);
    native_parallel_sort_int_range(
        &mut ctx,
        &[Value::Object(Some(arr)), Value::Int(1), Value::Int(5)],
    )
    .unwrap();
    let got: Vec<i32> = (0..6)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            _ => 0,
        })
        .collect();
    assert_eq!(got, vec![5, 1, 2, 8, 9, 3]);
}

#[test]
fn t2_parallel_sort_int_range_from_greater_than_to_fails() {
    let mut ctx = mock_ctx();
    let arr = make_int_array(&mut ctx, &[1, 2, 3]);
    let r = native_parallel_sort_int_range(
        &mut ctx,
        &[Value::Object(Some(arr)), Value::Int(2), Value::Int(1)],
    );
    assert!(r.is_err());
}

#[test]
fn t2_parallel_sort_long_sorts_in_place() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(ArrayElementType::Long, 4);
    let src: [i64; 4] = [40, 10, 30, 20];
    for (i, v) in src.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(*v));
    }
    native_parallel_sort_long(&mut ctx, &[Value::Object(Some(arr))]).unwrap();
    let got: Vec<i64> = (0..4)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Long(v) => v,
            _ => 0,
        })
        .collect();
    assert_eq!(got, vec![10, 20, 30, 40]);
}

#[test]
fn t2_parallel_sort_double_total_order_sorts_nan_last() {
    let mut ctx = mock_ctx();
    let arr = ctx.new_array(ArrayElementType::Double, 4);
    let src: [f64; 4] = [f64::NAN, 2.0, 1.0, f64::INFINITY];
    for (i, v) in src.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Double(*v));
    }
    native_parallel_sort_double(&mut ctx, &[Value::Object(Some(arr))]).unwrap();
    let got: Vec<f64> = (0..4)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Double(v) => v,
            _ => 0.0,
        })
        .collect();
    assert_eq!(got[0], 1.0);
    assert_eq!(got[1], 2.0);
    assert_eq!(got[2], f64::INFINITY);
    assert!(got[3].is_nan());
}

// -----------------------------------------------------------------------
// T2.3.8: Spliterator primitive specializations
// -----------------------------------------------------------------------

fn make_int_spliterator(
    ctx: &mut dyn NativeContext,
    data: &[i32],
) -> cratonvm_types::ObjectRef {
    let arr = make_int_array(ctx, data);
    let spl =
        crate::try_alloc_concurrent_synthetic(ctx, "java/util/Spliterator$OfInt", 2).unwrap();
    ctx.set_field(spl, SPL_FIELD_DATA, Value::Object(Some(arr)));
    ctx.set_field(spl, SPL_FIELD_CURSOR, Value::Int(0));
    spl
}

#[test]
fn t2_spl_int_estimate_size_is_remaining() {
    let mut ctx = mock_ctx();
    let spl = make_int_spliterator(&mut ctx, &[1, 2, 3, 4]);
    let r = native_spl_estimate_size(&mut ctx, &[Value::Object(Some(spl))]).unwrap();
    assert_eq!(r, Some(Value::Long(4)));
}

#[test]
fn t2_spl_characteristics_bitmask() {
    let mut ctx = mock_ctx();
    let r = native_spl_characteristics(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Int(SPL_PRIM_CHARS)));
}

#[test]
fn t2_spl_has_characteristics_subset_check() {
    let mut ctx = mock_ctx();
    // ORDERED subset should yield true; a random unrelated bit should not.
    let ordered = 0x10;
    let r =
        native_spl_has_characteristics(&mut ctx, &[Value::Object(None), Value::Int(ordered)])
            .unwrap();
    assert_eq!(r, Some(Value::Int(1)));
    let not_a_char = 0x2;
    let r2 = native_spl_has_characteristics(
        &mut ctx,
        &[Value::Object(None), Value::Int(not_a_char)],
    )
    .unwrap();
    assert_eq!(r2, Some(Value::Int(0)));
}

#[test]
fn t2_spl_try_split_returns_null() {
    let mut ctx = mock_ctx();
    let r = native_spl_int_try_split(&mut ctx, &[]).unwrap();
    assert_eq!(r, Some(Value::Object(None)));
}

// -----------------------------------------------------------------------
// B1 / V1: BitSet negative / oversized index validation
// -----------------------------------------------------------------------

/// Allocate a default BitSet via the real init native.
fn make_bitset(ctx: &mut dyn NativeContext) -> cratonvm_types::ObjectRef {
    let bs = crate::try_alloc_concurrent_synthetic(ctx, "java/util/BitSet", 2).unwrap();
    native_bs_init(ctx, &[Value::Object(Some(bs))]).unwrap();
    bs
}

/// A thrown Java exception (catchable), not an internal/fatal VM error.
fn assert_thrown(r: &MethodCallResult) {
    match r {
        Err(MethodCallFailed::ExceptionThrown(_)) => {}
        other => panic!("expected ExceptionThrown, got {other:?}"),
    }
}

#[test]
fn bs_set_negative_index_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_set(&mut ctx, &[Value::Object(Some(bs)), Value::Int(-1)]);
    assert_thrown(&r);
}

#[test]
fn bs_clear_negative_index_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_clear(&mut ctx, &[Value::Object(Some(bs)), Value::Int(-5)]);
    assert_thrown(&r);
}

#[test]
fn bs_flip_negative_index_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_flip(&mut ctx, &[Value::Object(Some(bs)), Value::Int(-1)]);
    assert_thrown(&r);
}

#[test]
fn bs_get_negative_index_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_get(&mut ctx, &[Value::Object(Some(bs)), Value::Int(-1)]);
    assert_thrown(&r);
}

#[test]
fn bs_set_min_int_index_does_not_panic_and_throws() {
    // i32::MIN as usize used to wrap to ~usize::MAX and drive a gigantic
    // allocation / overflow. It must now be a clean IndexOutOfBounds throw.
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_set(&mut ctx, &[Value::Object(Some(bs)), Value::Int(i32::MIN)]);
    assert_thrown(&r);
}

#[test]
fn bs_set_range_negative_from_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_set_range(
        &mut ctx,
        &[Value::Object(Some(bs)), Value::Int(-1), Value::Int(4)],
    );
    assert_thrown(&r);
}

#[test]
fn bs_set_range_from_greater_than_to_throws() {
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    let r = native_bs_set_range(
        &mut ctx,
        &[Value::Object(Some(bs)), Value::Int(8), Value::Int(4)],
    );
    assert_thrown(&r);
}

#[test]
fn bs_set_then_get_valid_index_roundtrips() {
    // Non-negative indices still work normally after the validation.
    let mut ctx = mock_ctx();
    let bs = make_bitset(&mut ctx);
    native_bs_set(&mut ctx, &[Value::Object(Some(bs)), Value::Int(130)]).unwrap();
    let g = native_bs_get(&mut ctx, &[Value::Object(Some(bs)), Value::Int(130)]).unwrap();
    assert_eq!(g, Some(Value::Int(1)));
    let g0 = native_bs_get(&mut ctx, &[Value::Object(Some(bs)), Value::Int(129)]).unwrap();
    assert_eq!(g0, Some(Value::Int(0)));
}

#[test]
fn bs_max_words_bounds_largest_int_index() {
    // The backing-word cap must accommodate the largest legal bit index
    // (Integer.MAX_VALUE) so we never reject a spec-valid index, while
    // still bounding the allocation.
    assert_eq!(bs_word_count((i32::MAX as usize) + 1), BS_MAX_WORDS);
}

// -----------------------------------------------------------------------
// nb-core-mediums (fable-2026-06-10): MessageFormat number element (B8)
// pure-helper coverage. The Scanner UTF-8 boundary (B4) test that used to
// sit here covered `scanner_remaining`, a helper of the dead Scanner bundle
// removed from this file; the live UTF-8 handling is `native-io`'s.
// -----------------------------------------------------------------------

#[test]
fn mf_number_integer_groups_thousands() {
    assert_eq!(p52_format_number(1234567.0, Some("integer")), "1,234,567");
    assert_eq!(p52_format_number(-1234.0, Some("integer")), "-1,234");
    assert_eq!(p52_format_number(0.0, Some("integer")), "0");
}

#[test]
fn mf_number_percent_and_currency() {
    assert_eq!(p52_format_number(0.5, Some("percent")), "50%");
    assert_eq!(p52_format_number(12.5, Some("currency")), "$12.50");
}

#[test]
fn mf_number_explicit_decimal_pattern() {
    // Two fraction digits, with grouping.
    assert_eq!(p52_format_number(1234.5, Some("#,##0.00")), "1,234.50");
    // No grouping, three fraction digits.
    assert_eq!(p52_format_number(3.14159, Some("0.000")), "3.142");
}

#[test]
fn mf_number_default_style_general_form() {
    assert_eq!(p52_format_number(1000.0, None), "1,000");
    assert_eq!(p52_format_number(1234.5, None), "1,234.5");
}

#[test]
fn mf_group_digits_basic() {
    assert_eq!(p52_group_digits("1"), "1");
    assert_eq!(p52_group_digits("12"), "12");
    assert_eq!(p52_group_digits("123"), "123");
    assert_eq!(p52_group_digits("1234"), "1,234");
    assert_eq!(p52_group_digits("1234567"), "1,234,567");
}

// -----------------------------------------------------------------------
// BUGFIX [nb-phases-early (2)]: PKCS7 constant-time unpad regression tests.
// These exercise the rewritten constant-time validator (fixed 16-byte
// window, branch-once-at-end) for correctness across every valid pad_len
// and a representative set of malformed inputs. They are pure (no
// NativeContext) since pkcs7_unpad operates on &[u8].
// -----------------------------------------------------------------------

#[test]
fn pkcs7_unpad_roundtrip_all_pad_lengths() {
    // For every payload that yields pad_len 1..=16, pad then unpad must
    // recover the original bytes exactly.
    for plaintext_len in 0..16usize {
        let plaintext: Vec<u8> = (0..plaintext_len)
            .map(|i| (i as u8).wrapping_mul(7))
            .collect();
        let padded = pkcs7_pad(&plaintext);
        assert_eq!(padded.len() % 16, 0);
        let unpadded = pkcs7_unpad(&padded).expect("valid padding must unpad");
        assert_eq!(unpadded, plaintext, "len {plaintext_len}");
    }
}

#[test]
fn pkcs7_unpad_full_block_padding() {
    // A 16-byte plaintext pads to a second all-0x10 block; unpad recovers it.
    let plaintext = vec![0xAAu8; 16];
    let padded = pkcs7_pad(&plaintext);
    assert_eq!(padded.len(), 32);
    assert_eq!(&padded[16..], &[0x10u8; 16]);
    assert_eq!(pkcs7_unpad(&padded).unwrap(), plaintext);
}

#[test]
fn pkcs7_unpad_rejects_corrupt_padding_byte() {
    // pad_len = 5; corrupt a NON-last padding byte. The old early-return
    // loop and the new constant-time loop must both reject, but the new
    // one does so without leaking how deep the mismatch was.
    let mut padded = pkcs7_pad(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]); // 11 -> pad 5
    assert_eq!(*padded.last().unwrap(), 5);
    let n = padded.len();
    padded[n - 3] ^= 0xFF; // flip a padding byte that is not the trailing one
    assert!(pkcs7_unpad(&padded).is_err());
}

#[test]
fn pkcs7_unpad_rejects_zero_and_oversize_pad_byte() {
    let mut block = vec![0u8; 16];
    // pad_byte 0 is invalid.
    assert!(pkcs7_unpad(&block).is_err());
    // pad_byte > 16 is invalid.
    block[15] = 17;
    assert!(pkcs7_unpad(&block).is_err());
}

#[test]
fn pkcs7_unpad_rejects_bad_length() {
    assert!(pkcs7_unpad(&[]).is_err());
    assert!(pkcs7_unpad(&[1, 2, 3]).is_err()); // not a multiple of 16
}

// -----------------------------------------------------------------------
// KeyGenerator — the parts that are arithmetic rather than heap access.
// The registrations themselves need a VM; these do not, and they are the
// parts where being wrong is silent.
// -----------------------------------------------------------------------

fn hex24(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The exact vector `javax.crypto.spec.DESedeKeySpec.isParityAdjusted`
/// accepted, computed by running this same rule in Java on HotSpot 25 and
/// on this VM and feeding the result to the JDK's own checker. If this
/// assertion fails, the parity rule has drifted from the one the JDK
/// validates against — which is the failure that would otherwise surface as
/// `InvalidKeyException` deep inside somebody's DESede cipher.
#[test]
fn des_parity_matches_the_jdk_checker() {
    let mut key: Vec<u8> = (0..24u32).map(|i| (i * 7) as u8).collect();
    assert_eq!(
        hex24(&key),
        "00070e151c232a31383f464d545b626970777e858c939aa1"
    );
    des_set_odd_parity(&mut key, 0);
    des_set_odd_parity(&mut key, 8);
    des_set_odd_parity(&mut key, 16);
    assert_eq!(
        hex24(&key),
        "01070e151c232a31383e464c545b626870767f858c929ba1"
    );
    // …and the property the checker actually tests, stated independently of
    // the vector: every byte has odd population count.
    for (i, b) in key.iter().enumerate() {
        assert_eq!(b.count_ones() % 2, 1, "byte {i} is not odd parity");
    }
}

/// A DES key is 8 bytes of odd parity. Same construction, same checker.
#[test]
fn des_parity_single_block() {
    let mut key: Vec<u8> = (0..8u32).map(|i| (0x10 + i * 11) as u8).collect();
    assert_eq!(hex24(&key), "101b26313c47525d");
    des_set_odd_parity(&mut key, 0);
    assert_eq!(hex24(&key), "101a26313d46525d");
}

/// Parity must be idempotent: conditioning an already-conditioned key must
/// not move it. A rule that flipped bit 0 unconditionally would pass the
/// vector test above and fail this one.
#[test]
fn des_parity_is_idempotent() {
    let mut key: Vec<u8> = (0..24u32).map(|i| (i * 37 + 5) as u8).collect();
    des_set_odd_parity(&mut key, 0);
    des_set_odd_parity(&mut key, 8);
    des_set_odd_parity(&mut key, 16);
    let once = key.clone();
    des_set_odd_parity(&mut key, 0);
    des_set_odd_parity(&mut key, 8);
    des_set_odd_parity(&mut key, 16);
    assert_eq!(once, key);
}

/// 2-key Triple DES: 24 bytes with K3 == K1 and K2 != K1 — the shape
/// HotSpot 25 produced on every `init(112)` run.
#[test]
fn desede_112_folds_k3_onto_k1() {
    let mut key: Vec<u8> = (0..24u32).map(|i| (i * 13 + 1) as u8).collect();
    keygen_condition_key("DESede", &mut key);
    keygen_fold_two_key_desede(&mut key);
    assert_eq!(key.len(), 24);
    assert_eq!(&key[0..8], &key[16..24], "K3 must equal K1");
    assert_ne!(&key[0..8], &key[8..16], "K2 must not equal K1");
    // The fold must not smuggle in a byte that lost its parity.
    for b in key.iter() {
        assert_eq!(b.count_ones() % 2, 1);
    }
}

/// `key_size / 8` is the wrong length for exactly one family, and this is
/// the assertion that says so: 168 bits of DESede is 24 bytes, not 21.
#[test]
fn desede_key_length_carries_its_parity_bits() {
    assert_eq!(keygen_byte_len("DESede", 168), 24);
    assert_eq!(keygen_byte_len("DESede", 112), 24);
    assert_eq!(keygen_byte_len("DES", 56), 8);
    // Everything else really is bits/8.
    assert_eq!(keygen_byte_len("AES", 256), 32);
    assert_eq!(keygen_byte_len("HmacSHA256", 256), 32);
}

/// Every default measured on HotSpot 25. The AES row is the one that was
/// wrong in the field: 128 where HotSpot gives 256.
#[test]
fn keygen_defaults_match_hotspot() {
    assert_eq!(keygen_default_bits("AES"), Some(256));
    assert_eq!(keygen_default_bits("aes"), Some(256));
    assert_eq!(keygen_default_bits("DESede"), Some(168));
    assert_eq!(keygen_default_bits("DES"), Some(56));
    assert_eq!(keygen_default_bits("HmacSHA1"), Some(512));
    assert_eq!(keygen_default_bits("HmacSHA224"), Some(224));
    assert_eq!(keygen_default_bits("HmacSHA256"), Some(256));
    assert_eq!(keygen_default_bits("HmacSHA384"), Some(384));
    assert_eq!(keygen_default_bits("HmacSHA512"), Some(512));
    assert_eq!(keygen_default_bits("Blowfish"), Some(128));
    assert_eq!(keygen_default_bits("ChaCha20"), Some(256));
    // No default arm: an algorithm this VM cannot generate for is refused,
    // not served with somebody else's default.
    assert_eq!(keygen_default_bits("CRATONVM-NO-SUCH-KEYGEN"), None);
    assert_eq!(keygen_default_bits(""), None);
}

/// The sizes HotSpot refuses, and the ones it accepts.
#[test]
fn keygen_size_admission_matches_hotspot() {
    let (aes, _) = keygen_allowed_bits("AES").expect("AES has a fixed set");
    assert_eq!(aes, &[128, 192, 256][..]);
    assert!(!aes.contains(&129));
    let (desede, _) = keygen_allowed_bits("DESede").expect("DESede has a fixed set");
    assert_eq!(desede, &[112, 168][..]);
    assert!(!desede.contains(&128));
    // The HMAC family takes any positive multiple of 8 — HotSpot really
    // does hand back an 8-byte key for `HmacSHA256` `init(64)`.
    assert!(keygen_allowed_bits("HmacSHA256").is_none());
}

/// `Formatter.formatMessage`'s step-4 guard, against the HotSpot answers
/// measured on JDK 25.0.3+9 for the same strings.
///
/// Every case here can fail: the guard is the place this defect was most
/// likely to live, and three of these rows (`{x}` before a real `{0}`, a
/// lone trailing `{`, and `{` before a non-digit) are exactly the ones a
/// "does it contain a brace" shortcut gets wrong in one direction or the
/// other.
#[test]
fn jul_format_guard_matches_the_jdk_indexof_scan() {
    // A `{<digit>` anywhere makes it a pattern.
    assert!(jul_message_is_java_text_format("one={0} two={1}"));
    assert!(jul_message_is_java_text_format("{0}"));
    assert!(jul_message_is_java_text_format("trailing {9}"));
    // `{10}` is `{` followed by `1` — a pattern by this rule, even though
    // MessageFormat may then leave it alone for want of an 11th argument.
    assert!(jul_message_is_java_text_format("ten={10}"));
    // A `{` whose next char is not a digit is not enough on its own...
    assert!(!jul_message_is_java_text_format("set={x}"));
    assert!(!jul_message_is_java_text_format("no placeholder here"));
    assert!(!jul_message_is_java_text_format(""));
    // ...but the scan must keep going and find the later one. HotSpot
    // formats this string (and MessageFormat then throws on `{x}`, which
    // the swallow turns back into the raw message).
    assert!(jul_message_is_java_text_format("set={x} v={0}"));
    // The JDK's `index >= fence` break: a `{` in the LAST position has no
    // next char and is never a pattern.
    assert!(!jul_message_is_java_text_format("trail{"));
    assert!(!jul_message_is_java_text_format("{"));
    // The byte scan must not mistake a multi-byte char's continuation byte
    // for a digit, and must not miss a pattern that follows one.
    assert!(!jul_message_is_java_text_format("héllo {x}"));
    assert!(jul_message_is_java_text_format("héllo {0}"));
}

// =======================================================================
// G21 — the JUL null axis on the rows THIS file owns.
//
// Every expectation below is transcribed from a HotSpot 25.0.3+9-LTS run
// (`scratchpad/g21/G21Probe.java`, 2026-08-17, 34 rows), not read off the
// JDK source and not carried over from another record.
//
// The tests come in PAIRS on purpose, and the second half is the half that
// matters. `Handler.setLevel(null)` throws while `Handler.setFilter(null)`,
// `Logger.setLevel(null)` and NINE `LogRecord` setters return normally — a
// blanket "JUL rejects null" rule passes every throws-test here and breaks
// 22 measured rows. HANDOFF-20260814 §5 records that generalisation as
// having already cost this family working paths once. If a later change
// adds a null guard to `lr_set`, or to the `Handler` setters this file does
// not register, `jul_log_record_reference_setters_accept_null_and_must_not_
// throw` and `jul_this_file_registers_no_other_handler_setter` are what go
// red.
// =======================================================================

/// A registry with only this file's JUL rows in it.
fn jul_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_phase54_logging_extras(&mut r);
    r
}

/// The registry that owns `Handler.setLevel` / `getLevel` in COMPATIBLE mode.
///
/// This file registered both until 2026-09-12 and its registrations were
/// dead: `reflect_annotations::register_annotation_overrides` runs later and
/// overwrote them in every feature arm. They were deleted because, being
/// `Intrinsic`, they SURVIVED the `java/util/logging/` retirement's refusal
/// under `--real-jdk` and kept the real bytecode from running.
///
/// The contract did not move with them, so neither do the two tests below:
/// they now score the body that actually dispatches, which carries the same
/// 2026-08-13 HotSpot measurement in its own comment.
fn jul_owner_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    crate::reflect_annotations::register_annotation_overrides(&mut r);
    r
}

/// The owning body reads and writes `logLevel` BY NAME, where the deleted one
/// chose by layout and fell back to slot 0. So the mock has to declare the
/// field, or `set_field_by_name` silently no-ops and the assertion below
/// would be reading a slot nothing ever wrote.
fn jul_declare_log_level(ctx: &MockNativeContext, class_id: ClassId) {
    ctx.set_declared_fields(
        class_id,
        vec![cratonvm_native_api::FieldMetadata {
            name: "logLevel".to_string(),
            descriptor: "Ljava/util/logging/Level;".to_string(),
            access_flags: 0,
            slot_index: 0,
            declaring_class_id: class_id,
            is_static: false,
        }],
    );
}

fn jul_find(
    r: &NativeMethodRegistry,
    class: &str,
    name: &str,
    desc: &str,
) -> cratonvm_native_api::NativeCallback {
    r.find(class, name, desc)
        .unwrap_or_else(|| panic!("{class}.{name}{desc} is not registered by this file"))
}

fn jul_runtime_error(failed: MethodCallFailed) -> RuntimeError {
    match failed {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(e)) => e,
        other => panic!("expected a RuntimeError, got {other:?}"),
    }
}

/// Assert an NPE whose message is EXACTLY `expected` — `None` for the bare
/// NPE HotSpot throws from `Handler.setLevel`. The message is asserted, not
/// just the class: `Level.parse(null)` threw the right class with an
/// invented `"Name cannot be null"` for months (G15-1 §4), so a
/// class-only assertion is not enough to pin one of these rows.
fn jul_assert_npe(result: MethodCallResult, expected: Option<&str>, what: &str) {
    let err = result
        .err()
        .unwrap_or_else(|| panic!("{what}: expected a throw, got a return"));
    match jul_runtime_error(err) {
        RuntimeError::NullPointerException { message } => assert_eq!(
            message.as_deref(),
            expected,
            "{what}: NPE message must be HotSpot's text verbatim"
        ),
        other => panic!("{what}: expected NullPointerException, got {other:?}"),
    }
}

const JUL_SET_LEVEL_DESC: &str = "(Ljava/util/logging/Level;)V";
const JUL_GET_LEVEL_DESC: &str = "()Ljava/util/logging/Level;";

/// MEASURED: `Handler.setLevel(null)` throws a BARE `NullPointerException`
/// (`getMessage()` is null), and — the half a "store, then throw" fix would
/// still get wrong — `getLevel()` afterwards answers the level that was
/// already there.
#[test]
fn jul_handler_set_level_null_is_refused_before_the_store() {
    let r = jul_owner_registry();
    let set_level = jul_find(
        &r,
        "java/util/logging/Handler",
        "setLevel",
        JUL_SET_LEVEL_DESC,
    );
    let get_level = jul_find(
        &r,
        "java/util/logging/Handler",
        "getLevel",
        JUL_GET_LEVEL_DESC,
    );

    let mut ctx = mock_ctx();
    let handler = ctx.alloc_object(ClassId::new(0), 1);
    jul_declare_log_level(&ctx, ClassId::new(0));
    let level = ctx.alloc_object(ClassId::new(1), 0);
    set_level(
        &mut ctx,
        &[Value::Object(Some(handler)), Value::Object(Some(level))],
    )
    .expect("a non-null level must be stored");

    jul_assert_npe(
        set_level(
            &mut ctx,
            &[Value::Object(Some(handler)), Value::Object(None)],
        ),
        None,
        "Handler.setLevel(null)",
    );

    // The store must NOT have happened. This is the second `RJdkIntrinsics3`
    // assertion: `logrec:Handler.getLevel() unchanged by the failed set`.
    assert_eq!(
        get_level(&mut ctx, &[Value::Object(Some(handler))]).unwrap(),
        Some(Value::Object(Some(level))),
        "a refused setLevel(null) must leave the previous level in place"
    );
}

/// The happy path still stores, so the guard cannot be a blanket refusal.
#[test]
fn jul_handler_set_level_still_stores_a_real_level() {
    let r = jul_owner_registry();
    let set_level = jul_find(
        &r,
        "java/util/logging/Handler",
        "setLevel",
        JUL_SET_LEVEL_DESC,
    );
    let get_level = jul_find(
        &r,
        "java/util/logging/Handler",
        "getLevel",
        JUL_GET_LEVEL_DESC,
    );

    let mut ctx = mock_ctx();
    let handler = ctx.alloc_object(ClassId::new(0), 1);
    jul_declare_log_level(&ctx, ClassId::new(0));
    let warning = ctx.alloc_object(ClassId::new(1), 0);
    let all = ctx.alloc_object(ClassId::new(1), 0);

    set_level(
        &mut ctx,
        &[Value::Object(Some(handler)), Value::Object(Some(warning))],
    )
    .unwrap();
    assert_eq!(
        get_level(&mut ctx, &[Value::Object(Some(handler))]).unwrap(),
        Some(Value::Object(Some(warning)))
    );
    set_level(
        &mut ctx,
        &[Value::Object(Some(handler)), Value::Object(Some(all))],
    )
    .unwrap();
    assert_eq!(
        get_level(&mut ctx, &[Value::Object(Some(handler))]).unwrap(),
        Some(Value::Object(Some(all))),
        "setLevel must still overwrite an existing level"
    );
}

/// **The anti-generalisation half.** Nine `LogRecord` setters take null and
/// RETURN on HotSpot. Six of them are reference-typed rows this test can
/// reach through the shared `lr_set` store-then-return body; the
/// `setSourceClassName` / `setSourceMethodName` pair is left out because it
/// is `Bridge`-tagged and retired under `--real-jdk`, and
/// `setLongThreadID` is not reference-typed. MEASURED rows 23-30 of the G21
/// probe: `setLoggerName`, `setMessage`, `setParameters`, `setThrown`,
/// `setResourceBundle`, `setResourceBundleName` all RETURNED, on HotSpot
/// and on CratonVM alike.
///
/// This test fails the moment someone "fixes the JUL null contract" by
/// putting a guard in `lr_set`.
#[test]
fn jul_log_record_reference_setters_accept_null_and_must_not_throw() {
    let r = jul_registry();
    let lr = "java/util/logging/LogRecord";
    let legal: [(&str, &str); 6] = [
        ("setLoggerName", "(Ljava/lang/String;)V"),
        ("setMessage", "(Ljava/lang/String;)V"),
        ("setParameters", "([Ljava/lang/Object;)V"),
        ("setThrown", "(Ljava/lang/Throwable;)V"),
        ("setResourceBundle", "(Ljava/util/ResourceBundle;)V"),
        ("setResourceBundleName", "(Ljava/lang/String;)V"),
    ];
    let mut ctx = mock_ctx();
    let record = ctx.alloc_object(ClassId::new(0), 12);
    for (name, desc) in legal {
        let cb = jul_find(&r, lr, name, desc);
        let result = cb(
            &mut ctx,
            &[Value::Object(Some(record)), Value::Object(None)],
        );
        assert!(
            result.is_ok(),
            "LogRecord.{name}{desc} with null is LEGAL on HotSpot and must not throw"
        );
    }
}

/// `LogRecord.setLevel` and `setInstant` DO throw on HotSpot — and this
/// file must not be the reason. Neither is registered here, so the real
/// bytecode runs and throws on its own; MEASURED rows 21 and 31 already
/// match on `--real-jdk` with no native involved.
///
/// Pinning the absence keeps a later lane from "completing the set" by
/// adding a raw-slot `setLevel` here, which is exactly how `Handler`'s
/// unguarded body got written.
#[test]
fn jul_log_record_set_level_and_set_instant_are_left_to_real_bytecode() {
    let r = jul_registry();
    let lr = "java/util/logging/LogRecord";
    assert!(
        r.find(lr, "setLevel", JUL_SET_LEVEL_DESC).is_none(),
        "LogRecord.setLevel must stay unregistered: the real bytecode already throws"
    );
    assert!(
        r.find(lr, "setInstant", "(Ljava/time/Instant;)V").is_none(),
        "LogRecord.setInstant must stay unregistered: the real bytecode already throws"
    );
}

/// **The other anti-generalisation half.** `Handler.setFilter(null)`,
/// `setEncoding(null)` RETURN on HotSpot while `setFormatter(null)` and
/// `setErrorManager(null)` throw — four siblings, two verdicts. This file
/// registers NONE of them, so under `--real-jdk` the real bytecode gives
/// all four the right answer (MEASURED rows 3-10, all matching).
///
/// Registering any of them here would put the verdict back in Rust's hands
/// for no measured gain, and the two that must RETURN are the ones a
/// copy of the `setLevel` guard would break.
#[test]
fn jul_this_file_registers_no_other_handler_setter() {
    let r = jul_registry();
    let handler = "java/util/logging/Handler";
    let untouched: [(&str, &str); 4] = [
        ("setFilter", "(Ljava/util/logging/Filter;)V"),
        ("setEncoding", "(Ljava/lang/String;)V"),
        ("setFormatter", "(Ljava/util/logging/Formatter;)V"),
        ("setErrorManager", "(Ljava/util/logging/ErrorManager;)V"),
    ];
    for (name, desc) in untouched {
        assert!(
            r.find(handler, name, desc).is_none(),
            "Handler.{name}{desc} is served correctly by real bytecode; do not register it here"
        );
    }
}

/// MEASURED: `Formatter.formatMessage(null)` throws a HELPFUL NPE naming
/// the `record` parameter — a different shape from `Handler.setLevel`'s
/// bare one, in the same family, which is why both are asserted verbatim.
/// This row was wrong in BOTH modes (it answered `""`).
#[test]
fn jul_formatter_format_message_null_record_throws_the_measured_npe() {
    let r = jul_registry();
    let cb = jul_find(
        &r,
        "java/util/logging/Formatter",
        "formatMessage",
        "(Ljava/util/logging/LogRecord;)Ljava/lang/String;",
    );
    let mut ctx = mock_ctx();
    let formatter = ctx.alloc_object(ClassId::new(0), 0);
    jul_assert_npe(
        cb(
            &mut ctx,
            &[Value::Object(Some(formatter)), Value::Object(None)],
        ),
        Some(JUL_NPE_NULL_FORMAT_RECORD),
        "Formatter.formatMessage(null)",
    );
}

/// The one NPE message this file hard-codes, transcribed from the oracle
/// run. `Handler.setLevel`'s NPE has no message at all and is pinned as
/// `None` above; if HotSpot's text ever changes, these are the two cells to
/// re-measure.
#[test]
fn jul_the_null_messages_are_the_measured_hotspot_text() {
    assert_eq!(
        JUL_NPE_NULL_FORMAT_RECORD,
        "Cannot invoke \"java.util.logging.LogRecord.getMessage()\" because \"record\" is null"
    );
}

// -------------------------------------------------------------------------
// G32: InetSocketAddress's two guards, and the ORDER each caller runs them in
// -------------------------------------------------------------------------

/// Pull the class and message out of whatever a guard returned.
fn refusal(e: MethodCallFailed) -> String {
    match e {
        MethodCallFailed::InternalError(cratonvm_types::error::VmError::Runtime(r)) => {
            format!("{r}")
        }
        other => format!("UNEXPECTED {other:?}"),
    }
}

/// TRANSCRIBED from HotSpot 25.0.3+9, not paraphrased: an apostrophe, no
/// article, no trailing period. This body used to raise
/// `NullPointerException: null object argument` instead, because it read the
/// host with `obj_arg`.
#[test]
fn g32_isa_null_host_is_illegal_argument_with_the_measured_text() {
    let e = p52_isa_check_host(Value::Object(None)).unwrap_err();
    assert_eq!(
        refusal(e),
        "IllegalArgumentException: hostname can't be null"
    );
}

/// A non-null host passes, and so does a host the VM cannot read as text —
/// the guard is about NULLNESS only, and must not start decoding.
#[test]
fn g32_isa_non_null_host_passes_the_guard() {
    let mut ctx = mock_ctx();
    let s = ctx.create_string("example.invalid");
    assert!(p52_isa_check_host(Value::Object(Some(s))).is_ok());
}

/// `port out of range:-1` — NO SPACE after the colon. A `{port}` written as
/// `: {port}` reads identically in a review and differs on the wire.
#[test]
fn g32_isa_port_message_has_no_space_after_the_colon() {
    let e = p52_isa_check_port(-1).unwrap_err();
    assert_eq!(refusal(e), "IllegalArgumentException: port out of range:-1");
    let e = p52_isa_check_port(65536).unwrap_err();
    assert_eq!(
        refusal(e),
        "IllegalArgumentException: port out of range:65536"
    );
}

/// 0 and 65535 are both LEGAL — 65535 is the last legal port, and an
/// exclusive upper bound is the obvious off-by-one here.
#[test]
fn g32_isa_port_bounds_are_inclusive_at_both_ends() {
    assert_eq!(p52_isa_check_port(0).unwrap(), 0);
    assert_eq!(p52_isa_check_port(65535).unwrap(), 65535);
}

/// **The row that cannot be derived, pinned in both directions.**
///
/// When host AND port are both invalid, the two entry points disagree about
/// which one to report, because the JDK writes them differently:
///
/// ```java
/// createUnresolved(host, port) -> new InetSocketAddress(checkPort(port), checkHost(host));
/// InetSocketAddress(hostname, port) { checkHost(hostname); ... checkPort(port) ... }
/// ```
///
/// `checkPort` is `createUnresolved`'s first *argument*; `checkHost` is the
/// constructor's first *statement*. MEASURED on HotSpot 25.0.3+9:
/// `createUnresolved(null, -1)` answers `port out of range:-1` and
/// `new InetSocketAddress((String) null, -1)` answers
/// `hostname can't be null`.
///
/// This test reproduces the two call sites' orderings rather than calling
/// the registered natives (which need a live heap), so it fails the moment
/// someone "simplifies" the two into one shared guard — which is exactly the
/// change that looks obviously correct and is not.
#[test]
fn g32_isa_the_two_entry_points_check_in_opposite_orders() {
    // createUnresolved: PORT first.
    let created = p52_isa_check_port(-1)
        .map(|_| ())
        .and_then(|_| p52_isa_check_host(Value::Object(None)));
    assert_eq!(
        refusal(created.unwrap_err()),
        "IllegalArgumentException: port out of range:-1",
        "createUnresolved(null, -1) must report the PORT"
    );

    // <init>(String, int): HOST first.
    let constructed = p52_isa_check_host(Value::Object(None))
        .and_then(|_| p52_isa_check_port(-1).map(|_| ()));
    assert_eq!(
        refusal(constructed.unwrap_err()),
        "IllegalArgumentException: hostname can't be null",
        "new InetSocketAddress((String) null, -1) must report the HOST"
    );
}

// -------------------------------------------------------------------------
// G32: EnumMap(null)
// -------------------------------------------------------------------------

/// `EnumMap(Class)` reaches `keyType.getEnumConstantsShared()` unguarded, so
/// a null key type is a `NullPointerException` before anything is stored.
/// This body used to build a zero-length universe and RETURN, leaving a live
/// map with a null `keyType`.
#[test]
fn g32_enum_map_null_key_type_throws_npe() {
    let mut ctx = mock_ctx();
    let this = crate::try_alloc_concurrent_synthetic(&mut ctx, "java/util/EnumMap", 3).unwrap();
    let e = native_em_init(&mut ctx, &[Value::Object(Some(this)), Value::Object(None)])
        .unwrap_err();
    assert!(
        refusal(e).starts_with("NullPointerException"),
        "a null key type must be an NPE"
    );
}

/// A MISSING argument is the same case as an explicit null — a native must
/// not read a shorter `args` slice as "no key type was requested".
#[test]
fn g32_enum_map_missing_key_type_arg_throws_npe() {
    let mut ctx = mock_ctx();
    let this = crate::try_alloc_concurrent_synthetic(&mut ctx, "java/util/EnumMap", 3).unwrap();
    let e = native_em_init(&mut ctx, &[Value::Object(Some(this))]).unwrap_err();
    assert!(refusal(e).starts_with("NullPointerException"));
}

// -----------------------------------------------------------------
// G43-1 — `InheritableThreadLocal`: `childValue` and transitivity.
//
// Each body runs on its OWN `std::thread`. `TL_MAP` and the
// `TL_INHERITED_DRAINED` one-shot are thread-locals, so a fresh thread is
// the only way to be sure the drain under test has not already been run by
// a sibling — which is exactly the state that would make these tests pass
// vacuously under `--test-threads=1`.
// -----------------------------------------------------------------

/// Register `obj` as an `InheritableThreadLocal` through the real `<init>`
/// native and return the ThreadLocal the snapshot's row will name
/// ([`snap_value`]).
fn itl_register(ctx: &mut MockNativeContext, obj: ObjectRef) -> ObjectRef {
    native_itl_init(ctx, &[Value::Object(Some(obj))]).expect("ITL <init> must not fail");
    obj
}

/// The value `snap` carries for the ThreadLocal `tl`. The mock has no weak
/// globals, so a row's owner is its `owner_fallback`.
fn snap_value(snap: &InheritedTlBucket, tl: ObjectRef) -> ThreadLocalValue {
    snap.iter()
        .find(|(_, row)| row.owner == 0 && row.owner_fallback == tl)
        .map(|(_, row)| row.value)
        .expect("the ITL entry")
}

/// gc-common w29-a: the inheritable table is keyed by weak lock keys,
/// which are per `(VM, address)`, and mock addresses repeat across
/// contexts -- so each test below owns a private VM identity, and this
/// guard forgets only that VM's keys and rows (lesson pp).
struct W29aTlVm(usize);
impl Drop for W29aTlVm {
    fn drop(&mut self) {
        crate::forget_vm_lock_keys(self.0);
        forget_vm_thread_local_side_tables(self.0);
    }
}

fn w29a_private_vm(ctx: &MockNativeContext, vm: usize) -> W29aTlVm {
    ctx.set_vm_identity(vm);
    W29aTlVm(vm)
}

#[test]
fn g43_1_snapshot_applies_child_value_to_each_inherited_entry() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        let _vm = w29a_private_vm(&ctx, 0x29a0_4301);
        let itl = ctx.alloc_object(ClassId::new(0), 0);
        let key = itl_register(&mut ctx, itl);

        let parent = ctx.alloc_object(ClassId::new(0), 0);
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(itl)), Value::Object(Some(parent))],
        )
        .expect("set must not fail");

        // The one scripted `invoke_virtual` stands in for an override of
        // `childValue`. HotSpot stores `key.childValue(value)`; CratonVM
        // stored `value` verbatim, which is oracle row 16.
        let child = ctx.alloc_object(ClassId::new(0), 0);
        ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(child)))));

        let snap = snapshot_inheritable_tl_entries(&mut ctx).expect("one inheritable entry");
        let stored = snap_value(&snap, key);
        assert_eq!(
            tl_value_to_java(&ctx, stored),
            Value::Object(Some(child)),
            "the child must receive childValue(parent), not the parent value verbatim"
        );
        assert_ne!(
            tl_value_to_java(&ctx, stored),
            Value::Object(Some(parent)),
            "returning the parent value here is precisely the divergence"
        );

        tl_inheritable_remove(&ctx, key);
    })
    .join()
    .expect("test thread must not panic");
}

/// gc-common w3-b: a dying thread's `ThreadLocal` values are JNI global
/// roots until something removes them, and the OS thread's `thread_local!`
/// destructor does not. `release_current_thread_locals` must drop every
/// handle (object values only — a primitive/null value owns none), leave
/// the map empty, and be idempotent.
#[test]
fn release_current_thread_locals_drops_every_value_root() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        let tl_a = ctx.alloc_object(ClassId::new(0), 0);
        let tl_b = ctx.alloc_object(ClassId::new(0), 0);
        let tl_c = ctx.alloc_object(ClassId::new(0), 0);
        let holder_a = ctx.alloc_object(ClassId::new(0), 0);
        let holder_b = ctx.alloc_object(ClassId::new(0), 0);
        let before = ctx.global_root_count();
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(tl_a)), Value::Object(Some(holder_a))],
        )
        .expect("set must not fail");
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(tl_b)), Value::Object(Some(holder_b))],
        )
        .expect("set must not fail");
        native_tl_set(&mut ctx, &[Value::Object(Some(tl_c)), Value::Int(7)])
            .expect("set must not fail");
        assert_eq!(
            ctx.global_root_count(),
            before + 2,
            "each object value is held through one JNI global root (non-vacuity)"
        );

        let released = release_current_thread_locals(&mut ctx);
        assert_eq!(released, 3, "all three entries are released");
        assert_eq!(
            ctx.global_root_count(),
            before,
            "no ThreadLocal value of a dead thread may stay a GC root"
        );
        assert!(TL_MAP.with(|m| m.borrow().is_empty()));
        assert_eq!(release_current_thread_locals(&mut ctx), 0, "idempotent");
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}

/// zt t20/tg9 (Defect D1, `CRATONVM_TL_RELEASE_MINTS_NO_THREAD`): the
/// thread-end release asks for the thread's EXISTING mirror only, so a VM
/// service attachment that never had a `java.lang.Thread` (the ZGC
/// director tick's) gets none built at its detach -- no
/// `ClassLoaders.<clinit>` on it during boot. The kill arm keeps the
/// minting lookup. Source pin: the mock answers `current_thread_object_if_any`
/// with a minted object, so the "no mirror" case cannot be built here.
#[test]
fn zt20tg9_the_thread_end_release_never_mints_a_mirror() {
    let src = concat!(include_str!("../phases_early/mod.rs"), "\n", include_str!("../phases_early/sis_read_deadline.rs"), "\n", include_str!("../phases_early/take_inherited_for_thread_object.rs"), "\n", include_str!("../phases_early/register_qname_natives.rs"), "\n", include_str!("../phases_early/initialize_real_thread_pool_executor.rs"), "\n", include_str!("../phases_early/register_phase52_byte_order.rs"), "\n", include_str!("../phases_early/register_synthetic_socket_stubs.rs"), "\n", include_str!("../phases_early/p54_huc_do_request_body.rs"));
    let body_of = |head: &str| -> String {
        let at = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
        let rest = &src[at..];
        let end = ["\nfn ", "\npub fn ", "\n#["]
            .iter()
            .filter_map(|n| rest[1..].find(n))
            .min()
            .unwrap_or(rest.len() - 1);
        rest[..=end].to_string()
    };
    let release = body_of(concat!("pub fn release_current_", "thread_locals("));
    assert!(
        release.contains(concat!("take_inherited_for_", "ending_thread(ctx)")),
        "the release must take the bucket through the non-minting lookup"
    );
    assert!(
        !release.contains(concat!("take_inherited_for_", "current_thread(ctx)")),
        "the release must not call the minting lookup directly"
    );
    let ending = body_of(concat!("fn take_inherited_for_", "ending_thread("));
    let if_any = ending
        .find(concat!("current_thread_object", "_if_any()"))
        .expect("the switch-on arm reads the existing mirror only");
    let gate = ending
        .find(concat!("tl_release_mints_no_", "thread_enabled()"))
        .expect("the arm is gated by its switch");
    assert!(gate < if_any, "the gate comes first");
    assert_eq!(
        ending.matches(concat!("current_thread", "_object()")).count(),
        0,
        "only the kill arm (through take_inherited_for_current_thread) may mint"
    );
}

/// gc-common w4-b: two virtual threads mounted on ONE carrier (one OS
/// thread here) must not see each other's `ThreadLocal` values, a value
/// must survive an unmount/remount, and the dying one's values must be
/// released on the carrier without touching the other's.
/// `docs/feature-designs/native-dispatch-architecture.md`.
#[test]
fn virtual_threads_on_one_carrier_keep_separate_thread_locals() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        let tl = ctx.alloc_object(ClassId::new(0), 0);
        let key = ctx.identity_hash_code(tl);
        let value_a = ctx.alloc_object(ClassId::new(0), 0);
        let before = ctx.global_root_count();

        // Mount A (first mount: a fresh state), set, unmount.
        let carrier = swap_current_thread_locals(DetachedThreadLocals::default());
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(tl)), Value::Object(Some(value_a))],
        )
        .expect("set must not fail");
        let saved_a = swap_current_thread_locals(carrier);
        assert_eq!(saved_a.len(), 1, "A's value travels with A (non-vacuity)");
        assert!(
            TL_MAP.with(|m| m.borrow().is_empty()),
            "the carrier must not keep A's value once A unmounts"
        );

        // Mount B on the same carrier: B must NOT see A's value.
        let carrier = swap_current_thread_locals(DetachedThreadLocals::default());
        assert!(
            TL_MAP.with(|m| m.borrow().get(&key).is_none()),
            "virtual thread B read virtual thread A's ThreadLocal -- HotSpot gives null"
        );
        // B dies on the carrier: its (empty) state is released, not A's.
        release_current_thread_locals(&mut ctx);
        let _ = swap_current_thread_locals(carrier);
        assert_eq!(
            ctx.global_root_count(),
            before + 1,
            "A's root must survive B's death"
        );

        // Remount A: its value is back.
        let carrier = swap_current_thread_locals(saved_a);
        match TL_MAP.with(|m| {
            m.borrow()
                .get(&key)
                .and_then(|bucket| bucket.iter().find(|row| row.owner_fallback == tl))
                .map(|row| row.value)
        }) {
            Some(stored) => assert_eq!(
                tl_value_to_java(&ctx, stored),
                Value::Object(Some(value_a)),
                "A must read its own value after a remount"
            ),
            None => panic!("A lost its ThreadLocal value across an unmount"),
        }
        // A dies: its value's global root is released.
        assert_eq!(release_current_thread_locals(&mut ctx), 1);
        let dead_a = swap_current_thread_locals(carrier);
        assert!(dead_a.is_empty(), "the release drained A's installed map");
        assert_eq!(
            ctx.global_root_count(),
            before,
            "a dead virtual thread's ThreadLocal value must not stay a GC root"
        );
    })
    .join()
    .expect("test thread must not panic");
}

/// One inherited bucket holding one object value (one global root), filed
/// under the bucket hash `tl_key` for a fresh mock ThreadLocal (no weak
/// owner handle: the mock has none).
fn bucket_with_one_root(ctx: &mut MockNativeContext, tl_key: i32) -> InheritedTlBucket {
    let tl = ctx.alloc_object(ClassId::new(0), 0);
    let value = ctx.alloc_object(ClassId::new(0), 0);
    let row = TlRow {
        owner: 0,
        owner_fallback: tl,
        value: tl_value_from_java(ctx, Value::Object(Some(value))),
    };
    vec![(tl_key, row)]
}


/// gch h3/jx (`CRATONVM_TL_INHERITED_PENDING_EPHEMERON`): a bucket whose
/// value is held PLAIN by the pending table owns no global root; the
/// `inherited-tl-pending` scan pins the value to its child `Thread` on a
/// licensed cycle and roots it otherwise (and never for another VM); a move
/// re-addresses it; and a take turns it back into a root at its current
/// address. The positive control is the root count before the conversion.
#[test]
fn gch_jx_plain_pending_values_are_pinned_to_their_child_and_rooted_again_on_take() {
    std::thread::spawn(|| {
        const VM: usize = 0x6a_7e01;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(VM);
        let before = ctx.global_root_count();
        let child = ctx.alloc_object(ClassId::new(0), 0);
        let mut b = bucket_with_one_root(&mut ctx, 1);
        let value = match b[0].1.value {
            ThreadLocalValue::Root { handle, .. } => {
                ctx.resolve_global_root(handle).expect("a live root")
            }
            ThreadLocalValue::Plain(_) => panic!("an object value is a root"),
        };
        assert_eq!(ctx.global_root_count(), before + 1, "positive control");

        let handles = tl_bucket_values_to_plain(&mut ctx, &mut b);
        assert_eq!(handles.len(), 1);
        assert!(matches!(
            b[0].1.value,
            ThreadLocalValue::Plain(Value::Object(Some(v))) if addr(v) == addr(value)
        ));
        tl_inherited_pending().lock().insert((VM, addr(child)), b);
        ANY_INHERITED_TL_QUEUED.store(true, std::sync::atomic::Ordering::Release);
        for h in handles {
            assert!(ctx.remove_global_root(h));
        }
        assert_eq!(
            ctx.global_root_count(),
            before,
            "the table holds it, no handle"
        );

        let mut out = Vec::new();
        gc_scan_inherited_tl_pending_values(VM, &mut out, &|_| false);
        assert_eq!(
            out.iter().map(|&o| addr(o)).collect::<Vec<_>>(),
            vec![addr(value)]
        );
        let mut foreign = Vec::new();
        gc_scan_inherited_tl_pending_values(VM + 1, &mut foreign, &|_| true);
        assert!(foreign.is_empty(), "another VM's scan reports nothing");
        let mut out = Vec::new();
        gc_scan_inherited_tl_pending_values(VM, &mut out, &|_| true);
        assert!(out.is_empty(), "a licensed cycle roots nothing");
        let pinned =
            cratonvm_types::metadata_pin::roots_for_loader(addr(child)).unwrap_or_default();
        assert!(pinned.contains(&addr(value)), "pinned to its child Thread");
        cratonvm_types::metadata_pin::forget_vm_metadata_pins(VM);

        let target = ctx.alloc_object(ClassId::new(0), 0);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(addr(value), addr(target));
        gc_update_inherited_tl_pending_values(VM, &map);
        let mut out = Vec::new();
        gc_scan_inherited_tl_pending_values(VM, &mut out, &|_| false);
        assert_eq!(
            out.iter().map(|&o| addr(o)).collect::<Vec<_>>(),
            vec![addr(target)]
        );

        let mut bucket = tl_inherited_pending()
            .lock()
            .remove(&(VM, addr(child)))
            .expect("still queued");
        tl_bucket_values_to_roots(&mut ctx, &mut bucket);
        assert_eq!(ctx.global_root_count(), before + 1, "a root again");
        match bucket[0].1.value {
            ThreadLocalValue::Root { handle, .. } => assert_eq!(
                ctx.resolve_global_root(handle).map(addr),
                Some(addr(target)),
                "at its current address"
            ),
            ThreadLocalValue::Plain(_) => panic!("rooted on take"),
        }
        for (_, row) in bucket {
            tl_release_row(&mut ctx, row);            }
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}


/// gc wave 9 lane F: the G1 remark's ephemeron step names exactly the
/// unmarked plain values of LIVE children of this VM -- not a dead child's,
/// not a marked value, not another VM's. The positive control is the live
/// child's unmarked value.
#[test]
fn gcf_w9_ephemeron_step_names_only_unmarked_values_of_live_children() {
    std::thread::spawn(|| {
        const VM: usize = 0x6a_7e09;
        let addr = |o: ObjectRef| o.as_ptr() as usize;
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(VM);
        let before = ctx.global_root_count();
        let live_child = ctx.alloc_object(ClassId::new(0), 0);
        let dead_child = ctx.alloc_object(ClassId::new(0), 0);
        let marked_child = ctx.alloc_object(ClassId::new(0), 0);
        let mut values = Vec::new();
        for child in [live_child, dead_child, marked_child] {
            let mut b = bucket_with_one_root(&mut ctx, 1);
            let handles = tl_bucket_values_to_plain(&mut ctx, &mut b);
            if let ThreadLocalValue::Plain(Value::Object(Some(v))) = b[0].1.value {
                values.push(addr(v));
            }
            tl_inherited_pending().lock().insert((VM, addr(child)), b);
            ANY_INHERITED_TL_QUEUED.store(true, std::sync::atomic::Ordering::Release);
            for h in handles {
                assert!(ctx.remove_global_root(h));
            }
        }
        assert_eq!(values.len(), 3);
        let (live_v, dead_v, marked_v) = (values[0], values[1], values[2]);
        let dead = addr(dead_child);
        let is_live = |a: usize| a != dead && a != live_v && a != dead_v;
        let mut out = Vec::new();
        gc_inherited_tl_pending_values_of_live_children(VM, &is_live, &mut out);
        assert_eq!(out, vec![live_v], "only the live child's unmarked value");
        assert!(!out.contains(&marked_v));
        let mut foreign = Vec::new();
        gc_inherited_tl_pending_values_of_live_children(VM + 1, &is_live, &mut foreign);
        assert!(foreign.is_empty(), "another VM's step names nothing");

        for child in [live_child, dead_child, marked_child] {
            let bucket = tl_inherited_pending().lock().remove(&(VM, addr(child)));
            for (_, row) in bucket.expect("still queued") {
                tl_release_row(&mut ctx, row);                }            }
        assert_eq!(ctx.global_root_count(), before);
    })
    .join()
    .expect("test thread must not panic");
}

/// **A RELEASE RE-ARMS THE INHERITED DRAIN** (gc-common w5-b, applying
/// `handoff-w4g-reattach-resets-inherited-drain.md`).
///
/// A JNI host thread that detaches (`release_current_thread_locals`) and
/// re-attaches runs a DIFFERENT `java.lang.Thread` on the same OS thread.
/// Its inherited bucket must be drained by its first `ThreadLocal` access;
/// with the one-shot left `true` it never was, and its values stayed GC
/// roots for the life of the VM.
#[test]
fn a_release_re_arms_the_inherited_drain_for_the_next_attachment() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        ctx.set_vm_identity(0x5b_7e04);
        let before = ctx.global_root_count();
        // First attachment touches a ThreadLocal, which runs its drain.
        let first_tl = ctx.alloc_object(ClassId::new(0), 0);
        native_tl_set(&mut ctx, &[Value::Object(Some(first_tl)), Value::Int(1)])
            .expect("set must not fail");
        assert!(TL_INHERITED_DRAINED.with(|f| f.get()), "non-vacuity");
        release_current_thread_locals(&mut ctx);
        assert!(
            !TL_INHERITED_DRAINED.with(|f| f.get()),
            "the next Java thread on this OS thread must drain its own bucket"
        );

        // Second attachment: a different Thread with a queued bucket. The
        // mock mints the current-thread object one pointer step past the
        // probe (see `g43_1_snapshot_drains_this_threads_pending_inheritance_first`).
        let tl_key = 0x5b7e;
        let bucket = bucket_with_one_root(&mut ctx, tl_key);
        let probe = ctx.current_thread_object();
        // SAFETY: never dereferenced -- only its address keys the queue.
        let second = unsafe { ObjectRef::from_raw((probe.as_ptr() as usize + 8) as *mut u8) };
        queue_inherited_tl_for_child(&mut ctx, second, bucket);
        drain_inherited_for_current_thread(&mut ctx);
        assert!(
            TL_MAP.with(|m| m.borrow().contains_key(&tl_key)),
            "the re-attached thread must see its inherited value"
        );
        assert_eq!(release_current_thread_locals(&mut ctx), 1);
        assert_eq!(
            ctx.global_root_count(),
            before,
            "every inherited value's root is released with the second attachment"
        );
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn g43_1_child_value_falls_back_to_the_parent_value_with_no_body_or_a_throw() {
    std::thread::spawn(|| {
        // No override and no scripted result: `invoke_virtual` answers
        // `Ok(None)`, the shape a synthetic-JDK image that does not declare
        // `childValue` produces. The base implementation returns its
        // argument, so the parent value is the correct fallback.
        let mut ctx = mock_ctx();
        let _vm = w29a_private_vm(&ctx, 0x29a0_4302);
        let itl = ctx.alloc_object(ClassId::new(0), 0);
        let key = itl_register(&mut ctx, itl);
        let parent = ctx.alloc_object(ClassId::new(0), 0);
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(itl)), Value::Object(Some(parent))],
        )
        .expect("set must not fail");

        let snap = snapshot_inheritable_tl_entries(&mut ctx).expect("one entry");
        assert_eq!(
            tl_value_to_java(&ctx, snap_value(&snap, key)),
            Value::Object(Some(parent)),
            "Ok(None) must fall back to the parent value"
        );

        // A THROWING override takes the same arm — the bounded deviation
        // recorded at the call site: this function returns `Option`, not
        // `Result`, so the throw cannot propagate out of `Thread.<init>`
        // the way it does on HotSpot.
        let exc = ctx.alloc_object(ClassId::new(0), 0);
        ctx.set_invoke_virtual_result(Err(MethodCallFailed::ExceptionThrown(exc)));
        let snap = snapshot_inheritable_tl_entries(&mut ctx).expect("one entry");
        assert_eq!(
            tl_value_to_java(&ctx, snap_value(&snap, key)),
            Value::Object(Some(parent)),
            "a throwing childValue must degrade to the base implementation, not lose the entry"
        );

        tl_inheritable_remove(&ctx, key);
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn g43_1_child_value_runs_for_an_explicit_null_but_not_after_remove() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        let _vm = w29a_private_vm(&ctx, 0x29a0_4303);
        let itl = ctx.alloc_object(ClassId::new(0), 0);
        let key = itl_register(&mut ctx, itl);

        // MEASURED on HotSpot (`G36Cv` row 4): an explicitly stored `null`
        // IS an entry, so `childValue(null)` runs and its result is what
        // the child gets.
        native_tl_set(&mut ctx, &[Value::Object(Some(itl)), Value::Object(None)])
            .expect("set(null) must not fail");
        let child = ctx.alloc_object(ClassId::new(0), 0);
        ctx.set_invoke_virtual_result(Ok(Some(Value::Object(Some(child)))));
        let snap = snapshot_inheritable_tl_entries(&mut ctx)
            .expect("an explicit null is still an inherited entry");
        assert_eq!(
            tl_value_to_java(&ctx, snap_value(&snap, key)),
            Value::Object(Some(child)),
            "childValue must be applied to a stored null too"
        );

        // MEASURED (`G36Cv` row 5): after `remove()` there is no entry, so
        // nothing is called and nothing is inherited.
        native_tl_remove(&mut ctx, &[Value::Object(Some(itl))]).expect("remove");
        assert!(
            snapshot_inheritable_tl_entries(&mut ctx).is_none(),
            "a removed ThreadLocal must not be inherited"
        );

        tl_inheritable_remove(&ctx, key);
    })
    .join()
    .expect("test thread must not panic");
}

#[test]
fn g43_1_snapshot_drains_this_threads_pending_inheritance_first() {
    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        // The private VM identity (see below) is set BEFORE the ITL is
        // registered: the inheritable table is per VM since gc-common
        // w10-a, so an ITL registered under another VM is invisible to
        // this VM's snapshot.
        let _vm = w29a_private_vm(&ctx, 0x5b_7e01);
        let itl = ctx.alloc_object(ClassId::new(0), 0);
        let key = itl_register(&mut ctx, itl);

        // The state a child thread is in before its FIRST ThreadLocal
        // access: its inherited entries are queued, not yet in `TL_MAP`.
        // Constructing a grandchild from here used to snapshot an empty
        // map, which is oracle row 24 (`gp-init` on HotSpot, `null` here).
        let gp_value = ctx.alloc_object(ClassId::new(0), 0);
        let rooted = tl_value_from_java(&mut ctx, Value::Object(Some(gp_value)));
        let pending: InheritedTlBucket = vec![(
            ctx.identity_hash_code(itl),
            TlRow {
                owner: 0,
                owner_fallback: itl,
                value: rooted,
            },
        )];

        // The mock mints a fresh "current thread" object on every call and
        // steps its pointer by 8 (`test_utils::alloc_entry`) -- so the
        // object the drain is about to allocate is one step past this
        // probe. Nothing between here and the call may allocate. The queue
        // is keyed by `(vm_identity, child address)` (gc-common w5-b), and
        // mock addresses repeat across contexts, so this test owns a
        // private VM identity.
        ctx.set_vm_identity(0x5b_7e01);
        let probe = ctx.current_thread_object();
        // SAFETY: never dereferenced -- only its address keys the queue.
        let drain_thread =
            unsafe { ObjectRef::from_raw((probe.as_ptr() as usize + 8) as *mut u8) };
        queue_inherited_tl_for_child(&mut ctx, drain_thread, pending);

        let snap = snapshot_inheritable_tl_entries(&mut ctx)
            .expect("pending inheritance must be drained before the snapshot is taken");
        assert_eq!(
            tl_value_to_java(&ctx, snap_value(&snap, key)),
            Value::Object(Some(gp_value)),
            "inheritance must be transitive even when this thread never read the value"
        );

        tl_inheritable_remove(&ctx, key);
    })
    .join()
    .expect("test thread must not panic");
}

/// The hazard that stopped the previous lane, pinned as a test: `childValue`
/// is application bytecode, and the old body called into Java while holding
/// the inheritable table's lock AND `TL_MAP.borrow()`. An override that
/// constructs an `InheritableThreadLocal` re-enters `native_itl_init` (the
/// same non-reentrant `parking_lot::Mutex` — a deadlock on the
/// thread-construction path, i.e. a hang in every executor), and one that
/// reads a `ThreadLocal` re-enters `TL_MAP.borrow_mut()` (a `RefCell`
/// double-borrow panic). This hook does BOTH from inside the callback.
/// The test hanging or panicking is the regression.
#[test]
fn g43_1_child_value_may_reenter_the_threadlocal_machinery() {
    fn reentrant_child_value(
        ctx: &mut MockNativeContext,
        _receiver: ObjectRef,
        method: &str,
        _descriptor: &str,
        args: &[Value],
    ) -> Option<MethodCallResult> {
        if method != "childValue" {
            return None;
        }
        // Re-enter the global mutex...
        let nested = ctx.alloc_object(ClassId::new(0), 0);
        native_itl_init(ctx, &[Value::Object(Some(nested))]).expect("nested ITL <init>");
        tl_inheritable_remove(&*ctx, nested);
        // ...and the RefCell, through a real ThreadLocal write.
        let other = ctx.alloc_object(ClassId::new(0), 0);
        native_tl_set(ctx, &[Value::Object(Some(other)), Value::Object(None)])
            .expect("nested set");
        // Hand the parent value straight back.
        let parent = args.first().copied().unwrap_or(Value::Object(None));
        Some(Ok(Some(parent)))
    }

    std::thread::spawn(|| {
        let mut ctx = mock_ctx();
        let _vm = w29a_private_vm(&ctx, 0x29a0_4304);
        ctx.set_invoke_virtual_hook(reentrant_child_value);
        let itl = ctx.alloc_object(ClassId::new(0), 0);
        let key = itl_register(&mut ctx, itl);
        let parent = ctx.alloc_object(ClassId::new(0), 0);
        native_tl_set(
            &mut ctx,
            &[Value::Object(Some(itl)), Value::Object(Some(parent))],
        )
        .expect("set must not fail");

        let snap = snapshot_inheritable_tl_entries(&mut ctx).expect("one entry");
        assert_eq!(
            tl_value_to_java(&ctx, snap_value(&snap, key)),
            Value::Object(Some(parent))
        );

        tl_inheritable_remove(&ctx, key);
    })
    .join()
    .expect("test thread must not deadlock or panic");
}
