use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::FieldMetadata;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

const ACC_PUBLIC: u16 = 0x0001;

fn fm(name: &str, desc: &str, slot: usize, declaring: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: ACC_PUBLIC,
        slot_index: slot,
        declaring_class_id: declaring,
        is_static: false,
    }
}

/// Build a context with:
///   Object (non-Serializable)
///   Serializable (marker)
///   Point implements Serializable { int x; int y; }
///   Holder implements Serializable { int n; String s; Point p; }
fn setup() -> (MockNativeContext, ClassId, ClassId) {
    let mut ctx = MockNativeContext::new();
    let _object = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let serializable = ctx
        .ensure_class_initialized("java/io/Serializable")
        .unwrap();
    let _string = ctx.ensure_class_initialized("java/lang/String").unwrap();
    let point = ctx.ensure_class_initialized("Point").unwrap();
    let holder = ctx.ensure_class_initialized("Holder").unwrap();

    ctx.set_interfaces(point, vec![serializable]);
    ctx.set_interfaces(holder, vec![serializable]);
    ctx.set_declared_fields(point, vec![fm("x", "I", 0, point), fm("y", "I", 1, point)]);
    ctx.set_declared_fields(
        holder,
        vec![
            fm("n", "I", 0, holder),
            fm("s", "Ljava/lang/String;", 1, holder),
            fm("p", "LPoint;", 2, holder),
        ],
    );
    (ctx, point, holder)
}

#[test]
fn primitive_and_string_and_nested_object_roundtrip() {
    let _serial_guard = super::serialization_test_guard();
    let (mut ctx, point, holder) = setup();

    // Build a Point(3, 4) and a Holder(7, "hi", point).
    let p = ctx.alloc_object(point, 2);
    ctx.set_field(p, 0, Value::Int(3));
    ctx.set_field(p, 1, Value::Int(4));
    let s = ctx.create_string("hi");
    let h = ctx.alloc_object(holder, 3);
    ctx.set_field(h, 0, Value::Int(7));
    ctx.set_field(h, 1, Value::Object(Some(s)));
    ctx.set_field(h, 2, Value::Object(Some(p)));

    let addr = 0x9000_0001_usize;
    reset_serialization_state_for(addr);
    oos_buf_reset(addr);
    handle_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, HandleState::new());

    oos_write_value(&mut ctx, addr, &Value::Object(Some(h))).expect("write must succeed");

    // Round-trip: load the written bytes back as a reader on the same addr.
    let bytes = oos_buf_snapshot(addr);
    ois_clear_handles(&mut ctx, addr);
    ois_buf_load(addr, bytes);

    let val = ois_read_value(&mut ctx, addr);
    let read_h = match val {
        Value::Object(Some(o)) => o,
        other => panic!("expected Holder object, got {:?}", other),
    };
    assert_eq!(ctx.get_field(read_h, 0), Value::Int(7), "int field n");
    // String field round-trips to a String with the same content.
    let read_s = match ctx.get_field(read_h, 1) {
        Value::Object(Some(o)) => ctx.read_string(o).unwrap_or_default(),
        other => panic!("expected String field, got {:?}", other),
    };
    assert_eq!(read_s, "hi");
    // Nested Point object round-trips its primitive fields.
    let read_p = match ctx.get_field(read_h, 2) {
        Value::Object(Some(o)) => o,
        other => panic!("expected nested Point, got {:?}", other),
    };
    assert_eq!(ctx.get_field(read_p, 0), Value::Int(3), "Point.x");
    assert_eq!(ctx.get_field(read_p, 1), Value::Int(4), "Point.y");
}

#[test]
fn non_serializable_object_is_rejected() {
    let _serial_guard = super::serialization_test_guard();
    let mut ctx = MockNativeContext::new();
    let _object = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let _serializable = ctx
        .ensure_class_initialized("java/io/Serializable")
        .unwrap();
    // Plain (non-Serializable) class.
    let plain = ctx.ensure_class_initialized("Plain").unwrap();
    let obj = ctx.alloc_object(plain, 1);

    let addr = 0x9000_0002_usize;
    reset_serialization_state_for(addr);
    oos_buf_reset(addr);

    let err = oos_write_value(&mut ctx, addr, &Value::Object(Some(obj)))
        .expect_err("non-Serializable must raise NotSerializableException");
    // The rejection is a THROWN `java.io.NotSerializableException` object
    // now, not an `IOException` naming the class in its text, so assert on
    // the class of what was thrown. `{:?}` on `ExceptionThrown` prints
    // `ExceptionThrown(ObjectRef { ptr: 0x… })` — an address with no class
    // — so a `msg.contains("NotSerializableException")` check would pass
    // only for the fallback shape and silently accept a wrong class.
    let thrown_class = match &err {
        MethodCallFailed::ExceptionThrown(exc) => ctx
            .class_name_of_id(ctx.class_id_of_object(*exc))
            .unwrap_or_default(),
        other => format!("{other:?}"),
    };
    assert!(
        thrown_class.contains("NotSerializableException"),
        "error should be NotSerializableException, got: {thrown_class}"
    );
}

#[test]
fn cycle_writes_back_reference_not_infinite() {
    let _serial_guard = super::serialization_test_guard();
    // Holder.p points at a Point, and we make a self-cycle:
    // h.p = h (re-using the same slot just to force a back-ref).
    let (mut ctx, _point, holder) = setup();
    let h = ctx.alloc_object(holder, 3);
    ctx.set_field(h, 0, Value::Int(1));
    ctx.set_field(h, 1, Value::Object(None));
    ctx.set_field(h, 2, Value::Object(Some(h))); // self-reference

    let addr = 0x9000_0003_usize;
    reset_serialization_state_for(addr);
    oos_buf_reset(addr);
    handle_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, HandleState::new());

    // Must terminate (no infinite recursion) and emit a TC_REFERENCE.
    oos_write_value(&mut ctx, addr, &Value::Object(Some(h)))
        .expect("self-referential write must terminate");
    let bytes = oos_buf_snapshot(addr);
    assert!(
        bytes.contains(&TC_REFERENCE),
        "a self-referential graph must emit TC_REFERENCE"
    );

    // And it must decode back to the same instance for the self field.
    ois_clear_handles(&mut ctx, addr);
    ois_buf_load(addr, bytes);
    let val = ois_read_value(&mut ctx, addr);
    let read_h = match val {
        Value::Object(Some(o)) => o,
        other => panic!("expected Holder, got {:?}", other),
    };
    assert_eq!(
        ctx.get_field(read_h, 2),
        Value::Object(Some(read_h)),
        "self-reference field must resolve to the same decoded instance"
    );
}

#[test]
fn int_array_field_roundtrip() {
    let _serial_guard = super::serialization_test_guard();
    let mut ctx = MockNativeContext::new();
    let _object = ctx.ensure_class_initialized("java/lang/Object").unwrap();
    let serializable = ctx
        .ensure_class_initialized("java/io/Serializable")
        .unwrap();
    let arrh = ctx.ensure_class_initialized("ArrHolder").unwrap();
    ctx.set_interfaces(arrh, vec![serializable]);
    ctx.set_declared_fields(arrh, vec![fm("data", "[I", 0, arrh)]);

    let arr = ctx.new_array(ArrayElementType::Int, 3);
    ctx.set_array_element(arr, 0, Value::Int(10));
    ctx.set_array_element(arr, 1, Value::Int(20));
    ctx.set_array_element(arr, 2, Value::Int(30));
    let h = ctx.alloc_object(arrh, 1);
    ctx.set_field(h, 0, Value::Object(Some(arr)));

    let addr = 0x9000_0004_usize;
    reset_serialization_state_for(addr);
    oos_buf_reset(addr);
    handle_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, HandleState::new());

    oos_write_value(&mut ctx, addr, &Value::Object(Some(h))).unwrap();
    let bytes = oos_buf_snapshot(addr);
    ois_clear_handles(&mut ctx, addr);
    ois_buf_load(addr, bytes);

    let val = ois_read_value(&mut ctx, addr);
    let read_h = match val {
        Value::Object(Some(o)) => o,
        other => panic!("expected ArrHolder, got {:?}", other),
    };
    let read_arr = match ctx.get_field(read_h, 0) {
        Value::Object(Some(a)) => a,
        other => panic!("expected int[] field, got {:?}", other),
    };
    assert_eq!(ctx.array_length(read_arr), 3);
    assert_eq!(ctx.get_array_element(read_arr, 0), Value::Int(10));
    assert_eq!(ctx.get_array_element(read_arr, 1), Value::Int(20));
    assert_eq!(ctx.get_array_element(read_arr, 2), Value::Int(30));
}

/// gc-common w21-c: an array takes a wire handle on the writer side as it
/// always did on the reader side, so a back-reference written after an
/// array names the right object. `Pair { int[] a; Point p; Point q; }`
/// with `p == q`: `q` used to come back as the array.
#[test]
fn back_reference_after_an_array_names_the_right_object() {
    let _serial_guard = super::serialization_test_guard();
    let (mut ctx, point, _holder) = setup();
    let serializable = ctx
        .ensure_class_initialized("java/io/Serializable")
        .unwrap();
    let pair = ctx.ensure_class_initialized("Pair").unwrap();
    ctx.set_interfaces(pair, vec![serializable]);
    ctx.set_declared_fields(
        pair,
        vec![
            fm("a", "[I", 0, pair),
            fm("p", "LPoint;", 1, pair),
            fm("q", "LPoint;", 2, pair),
        ],
    );
    let arr = ctx.new_array(ArrayElementType::Int, 1);
    ctx.set_array_element(arr, 0, Value::Int(5));
    let p = ctx.alloc_object(point, 2);
    ctx.set_field(p, 0, Value::Int(1));
    ctx.set_field(p, 1, Value::Int(2));
    let h = ctx.alloc_object(pair, 3);
    ctx.set_field(h, 0, Value::Object(Some(arr)));
    ctx.set_field(h, 1, Value::Object(Some(p)));
    ctx.set_field(h, 2, Value::Object(Some(p)));

    let addr = 0x9000_0005_usize;
    reset_serialization_state_for(addr);
    oos_buf_reset(addr);
    handle_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(addr, HandleState::new());
    oos_write_value(&mut ctx, addr, &Value::Object(Some(h))).unwrap();
    let bytes = oos_buf_snapshot(addr);
    ois_clear_handles(&mut ctx, addr);
    ois_buf_load(addr, bytes);

    let read_h = match ois_read_value(&mut ctx, addr) {
        Value::Object(Some(o)) => o,
        other => panic!("expected Pair, got {:?}", other),
    };
    let read_p = ctx.get_field(read_h, 1);
    assert!(matches!(read_p, Value::Object(Some(_))), "p: {read_p:?}");
    assert_eq!(
        ctx.get_field(read_h, 2),
        read_p,
        "q is a back-reference to p, not to the array before it"
    );
    ois_clear_handles(&mut ctx, addr);
}

#[test]
fn field_type_code_maps_descriptors() {
    assert_eq!(field_type_code("I"), 'I');
    assert_eq!(field_type_code("J"), 'J');
    assert_eq!(field_type_code("Ljava/lang/String;"), 'L');
    assert_eq!(field_type_code("[I"), '[');
    assert_eq!(field_type_code("[Ljava/lang/Object;"), '[');
    assert_eq!(field_type_code("Z"), 'Z');
}
