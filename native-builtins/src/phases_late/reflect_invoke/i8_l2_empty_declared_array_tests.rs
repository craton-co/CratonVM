use super::empty_declared_array;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::Value;

/// The synthetic empty reflection answers carry their declared component
/// (`Annotation[0]`, `TypeVariable[0]`), not `Object` (i8-L2).
#[test]
fn i8_l2_empty_reflection_answers_are_typed() {
    let mut ctx = mock_ctx();
    for name in [
        "java/lang/annotation/Annotation",
        "java/lang/reflect/TypeVariable",
    ] {
        let component = ctx
            .ensure_class_initialized(name)
            .expect("mock registers the class");
        let arr = match empty_declared_array(&mut ctx, name) {
            Ok(Some(Value::Object(Some(arr)))) => arr,
            other => panic!("expected an array, got {other:?}"),
        };
        assert_eq!(ctx.ref_array_component(arr), Some(component), "{name}");
        assert_eq!(ctx.array_length(arr), 0);
    }
}
