use super::lambda_proxy_new_instance_refusal;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{LambdaSerialMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::ClassId;

/// Failed before K4: the lambda arm answered `None` (construct) for every
/// caller. A non-capturing proxy's private constructor is reachable from
/// its host and refused to a class of another nest.
#[test]
fn only_the_hosts_nest_may_construct_a_lambda_proxy() {
    let mut ctx = mock_ctx();
    let host = ctx
        .ensure_class_initialized("p/Host")
        .unwrap_or(ClassId::new(0));
    let outsider = ctx
        .ensure_class_initialized("q/Outsider")
        .unwrap_or(ClassId::new(0));
    assert_ne!(host, outsider);
    let proxy = ClassId::new(0x8000_0c61);
    ctx.set_lambda_proxy_host(proxy, "p/Host");
    ctx.set_lambda_serial_metadata(
        proxy,
        LambdaSerialMetadata {
            functional_interface: "java/lang/Runnable".to_string(),
            sam_method_name: "run".to_string(),
            sam_descriptor: "()V".to_string(),
            impl_class: "p/Host".to_string(),
            impl_member: "lambda$main$0".to_string(),
            impl_descriptor: "()V".to_string(),
            impl_ref_kind: 6,
            instantiated_descriptor: "()V".to_string(),
            capture_types: String::new(),
            marker_interfaces: Vec::new(),
            bridges: Vec::new(),
        },
    );
    let mirror = ctx.alloc_object(ClassId::new(0), 0);

    ctx.set_frame_class_ids(vec![host]);
    assert!(lambda_proxy_new_instance_refusal(&mut ctx, mirror, proxy).is_none());

    ctx.set_frame_class_ids(vec![outsider]);
    match lambda_proxy_new_instance_refusal(&mut ctx, mirror, proxy) {
        Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalAccessException { message },
        ))) => assert_eq!(
            message,
            "class q.Outsider cannot access a member of class \
             p.Host$$Lambda/0x0000000080000c61 with modifiers \"private\""
        ),
        other => panic!("expected IllegalAccessException, got {other:?}"),
    }
}
