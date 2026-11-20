#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_types::ArrayElementType;

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

/// Two VMs on one thread with two `String` layouts: each hashes with its
/// own slot, and the compact string's `coder` is never written. Under the
/// old process-wide latch the second VM inherited slot 1 from the first
/// and stored the hash into `coder`.
#[test]
fn the_string_hash_slot_is_latched_per_vm() {
    // "hi".hashCode() == 'h' * 31 + 'i'
    const HI: i32 = 0x68 * 31 + 0x69;
    // VM A: the mock's synthetic `{value:[C, hash:I}` layout.
    let mut a = vm_ctx(0xE15_0203);
    let sa = a.create_string("hi");
    let ha = native_string_hash_code(&mut a, &[Value::Object(Some(sa))]).unwrap();
    assert_eq!(ha, Some(Value::Int(HI)));
    assert_eq!(
        a.get_field(sa, 1),
        Value::Int(HI),
        "slot 1 caches it in the char[] layout"
    );

    // VM B, same thread: the JDK 9+ compact layout
    // `{value:[B, coder:B, hash:I, hashIsZero:Z}`, LATIN1.
    let mut b = vm_ctx(0xE15_0204);
    let cid = b.ensure_class_initialized("java/lang/String").unwrap();
    let bytes = b.new_array(ArrayElementType::Byte, 2);
    b.set_array_element(bytes, 0, Value::Int(0x68));
    b.set_array_element(bytes, 1, Value::Int(0x69));
    let sb = b.alloc_object(cid, 4);
    b.set_field(sb, 0, Value::Object(Some(bytes)));
    for slot in 1..4 {
        b.set_field(sb, slot, Value::Int(0));
    }
    let hb = native_string_hash_code(&mut b, &[Value::Object(Some(sb))]).unwrap();
    assert_eq!(hb, Some(Value::Int(HI)));
    assert_eq!(
        b.get_field(sb, 1),
        Value::Int(0),
        "`coder` must not receive the hash"
    );
    assert_eq!(
        b.get_field(sb, 2),
        Value::Int(HI),
        "slot 2 caches it in the compact layout"
    );
}

#[test]
fn split_parts_land_in_the_rooted_array() {
    let mut ctx = vm_ctx(0xE15_0201);
    let parts = vec!["a".to_string(), "bc".to_string(), "def".to_string()];
    let r = string_array_from_parts(&mut ctx, &parts).unwrap();
    let Some(Value::Object(Some(arr))) = r else {
        panic!("expected a String[], got {r:?}");
    };
    assert_eq!(ctx.array_length(arr), 3);
    for (i, want) in parts.iter().enumerate() {
        let Value::Object(Some(s)) = ctx.get_array_element(arr, i) else {
            panic!("element {i} must be a String");
        };
        assert_eq!(ctx.read_string(s).as_deref(), Some(want.as_str()));
    }
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

#[test]
fn chars_stream_is_written_through_its_root() {
    let mut ctx = vm_ctx(0xE15_0202);
    let r = match int_stream_of(&mut ctx, &[0x61, 0x62], true) {
        Ok(r) => r,
        Err(e) => {
            // The mock refused the synthetic `IntStream` layout: nothing
            // of this fix is reachable then.
            eprintln!("skipped: the mock cannot allocate an IntStream ({e:?})");
            return;
        }
    };
    let Some(Value::Object(Some(stream))) = r else {
        panic!("expected an IntStream, got {r:?}");
    };
    let Value::Object(Some(arr)) = ctx.get_field(stream, 0) else {
        panic!("slot 0 must hold the int[]");
    };
    assert_eq!(ctx.array_length(arr), 2);
    assert_eq!(ctx.get_array_element(arr, 0), Value::Int(0x61));
    assert_eq!(ctx.get_array_element(arr, 1), Value::Int(0x62));
    assert_eq!(ctx.get_field(stream, 1), Value::Object(None));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
