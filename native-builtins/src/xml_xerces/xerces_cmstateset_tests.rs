use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ArrayElementType;

fn new_cmstateset(
    ctx: &mut MockNativeContext,
    bit_count: i32,
    byte_count: i32,
    bits1: i32,
    bits2: i32,
    bytes: &[u8],
) -> ObjectRef {
    let class_id = ctx
        .ensure_class_initialized(XERCES_CMSTATESET)
        .expect("CMStateSet class id");
    let obj = ctx.alloc_object(class_id, 5);
    ctx.set_field_by_name(obj, "fBitCount", Value::Int(bit_count));
    ctx.set_field_by_name(obj, "fByteCount", Value::Int(byte_count));
    ctx.set_field_by_name(obj, "fBits1", Value::Int(bits1));
    ctx.set_field_by_name(obj, "fBits2", Value::Int(bits2));
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (idx, byte) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, idx, Value::Int((*byte as i8) as i32));
    }
    ctx.set_field_by_name(obj, "fByteArray", Value::Object(Some(arr)));
    obj
}

fn java_cmstateset_byte_hash(bytes: &[u8]) -> i32 {
    let mut hash = 0i32;
    for byte in bytes.iter().rev() {
        hash = (*byte as i8 as i32).wrapping_add(hash.wrapping_mul(31));
    }
    hash
}

#[test]
fn essential_natives_include_xerces_cmstateset_intrinsics() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    for (name, descriptor) in [
        ("hashCode", "()I"),
        ("equals", "(Ljava/lang/Object;)Z"),
        (
            "isSameSet",
            "(Lcom/sun/org/apache/xerces/internal/impl/dtd/models/CMStateSet;)Z",
        ),
    ] {
        assert!(
            registry.find(XERCES_CMSTATESET, name, descriptor).is_some(),
            "CMStateSet.{name}{descriptor} must be in the real-JDK essential registry"
        );
    }
}

#[test]
fn xerces_cmstateset_hash_code_matches_jdk_small_and_byte_array_paths() {
    let mut ctx = MockNativeContext::new();
    let small = new_cmstateset(&mut ctx, 64, 0, 17, -9, &[]);
    let small_result =
        native_xerces_cmstateset_hash_code(&mut ctx, &[Value::Object(Some(small))])
            .expect("small hash result")
            .expect("small hash value");
    assert_eq!(
        small_result,
        Value::Int(17i32.wrapping_add((-9i32).wrapping_mul(31)))
    );

    let bytes = [0x7f, 0x80, 0xff, 0x01];
    let large = new_cmstateset(&mut ctx, 65, bytes.len() as i32, 0, 0, &bytes);
    let large_result =
        native_xerces_cmstateset_hash_code(&mut ctx, &[Value::Object(Some(large))])
            .expect("large hash result")
            .expect("large hash value");
    assert_eq!(large_result, Value::Int(java_cmstateset_byte_hash(&bytes)));
}

#[test]
fn xerces_cmstateset_equals_and_is_same_set_compare_real_fields() {
    let mut ctx = MockNativeContext::new();
    let a = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x00, 0x7f]);
    let b = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x00, 0x7f]);
    let c = new_cmstateset(&mut ctx, 65, 3, 0, 0, &[0xaa, 0x01, 0x7f]);

    let eq_ab = native_xerces_cmstateset_equals(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Object(Some(b))],
    )
    .expect("equals result")
    .expect("equals value");
    assert_eq!(eq_ab, Value::Int(1));

    let same_ac = native_xerces_cmstateset_is_same_set(
        &mut ctx,
        &[Value::Object(Some(a)), Value::Object(Some(c))],
    )
    .expect("isSameSet result")
    .expect("isSameSet value");
    assert_eq!(same_ac, Value::Int(0));
}
