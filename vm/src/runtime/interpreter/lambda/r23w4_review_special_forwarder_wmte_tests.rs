use super::{lambda_special_forwarder_type_refusal, ClassId, JvmThread, SharedVm};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

fn site(proxy: u32) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface: Arc::from("java/util/function/Function"),
        functional_interface_id: None,
        sam_method_name: Arc::from("apply"),
        sam_descriptor: Arc::from("(Ljava/lang/Object;)Ljava/lang/Object;"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeSpecial,
            class_name: Arc::from("q/Base"),
            member_name: Arc::from("m"),
            descriptor: Arc::from("()Ljava/lang/String;"),
        },
        instantiated_descriptor: Arc::from("(Lp/Host;)Ljava/lang/String;"),
        capture_types: vec![],
        proxy_class_id: ClassId::new(proxy),
        serializable_flag: false,
    }
}

