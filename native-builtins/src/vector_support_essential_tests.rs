use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn register_essential_includes_jdk25_vector_support_natives() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let vector_support = "jdk/internal/vm/vector/VectorSupport";

    assert!(registry
        .find(vector_support, "registerNatives", "()I")
        .is_some());
    assert!(registry
        .find(vector_support, "getCPUFeatures", "()Ljava/lang/String;")
        .is_some());
    assert!(registry
        .find(vector_support, "getMaxLaneCount", "(Ljava/lang/Class;)I")
        .is_some());
}

#[test]
fn class_component_type_alias_is_registered() {
    for name in ["getComponentType", "componentType"] {
        let mut registry = NativeMethodRegistry::new();
        register_essential_natives(&mut registry);
        assert!(
            registry
                .find("java/lang/Class", name, "()Ljava/lang/Class;")
                .is_some(),
            "Class.{name}() must use the native array-component mirror path"
        );

        let mut final_registry = NativeMethodRegistry::new();
        register_enterprise_final_natives(&mut final_registry);
        assert!(
            final_registry
                .find("java/lang/Class", name, "()Ljava/lang/Class;")
                .is_some(),
            "Class.{name}() must also be registered in the final native set"
        );
    }
}

#[test]
fn linked_blocking_queue_clear_preserves_real_jdk_count_field() {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let clear = registry
        .find("java/util/concurrent/LinkedBlockingQueue", "clear", "()V")
        .expect("essential LinkedBlockingQueue.clear native");

    let mut ctx = MockNativeContext::new();
    let queue_class = ctx
        .ensure_class_initialized("java/util/concurrent/LinkedBlockingQueue")
        .expect("mock queue class");
    let queue = ctx.alloc_object(queue_class, 3);
    let count_class = ctx
        .ensure_class_initialized("java/util/concurrent/atomic/AtomicInteger")
        .expect("mock AtomicInteger class");
    let count = ctx.alloc_object(count_class, 1);
    ctx.set_field(queue, 1, Value::Object(Some(count)));

    clear(&mut ctx, &[Value::Object(Some(queue))]).expect("clear succeeds");

    assert_eq!(
        ctx.get_field(queue, 1),
        Value::Object(Some(count)),
        "real-JDK LinkedBlockingQueue.count must not be stomped as synthetic size"
    );
    assert_eq!(
        ctx.get_field_by_name(queue, "count"),
        Value::Object(Some(count))
    );
}
