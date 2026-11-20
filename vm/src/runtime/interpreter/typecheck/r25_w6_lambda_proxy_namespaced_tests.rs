use super::*;
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};

fn site(proxy: ClassId, iface_id: Option<ClassId>) -> Arc<LambdaCallSite> {
    Arc::new(LambdaCallSite {
        functional_interface: Arc::from("java/lang/Runnable"),
        functional_interface_id: iface_id,
        sam_method_name: Arc::from("run"),
        sam_descriptor: Arc::from("()V"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeStatic,
            class_name: Arc::from("cratonvm/test/W6RHost"),
            member_name: Arc::from("lambda$0"),
            descriptor: Arc::from("()V"),
        },
        instantiated_descriptor: Arc::from("()V"),
        capture_types: vec![],
        proxy_class_id: proxy,
        serializable_flag: false,
    })
}

#[test]
fn a_resolved_interface_decides_by_class_not_by_name() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let loaded = {
        let mut cm = shared.classes.class_manager.write();
        (
            cm.load_class("java/lang/Object"),
            cm.load_class("java/lang/Runnable"),
        )
    };
    let (Ok(object), Ok(runnable)) = loaded else {
        return; // Stripped VM (no class library on the test classpath).
    };
    assert!(
        shared
            .classes
            .aligned(cratonvm_types::HotSpotAlignment::NamespacedClassTypes),
        "the row is on by default"
    );
    let other_copy = shared.alloc_lambda_proxy_id();
    let own = shared.alloc_lambda_proxy_id();
    {
        let mut proxies = shared.classes.lambda_proxies.write();
        proxies.insert(other_copy, site(other_copy, Some(object)));
        proxies.insert(own, site(own, Some(runnable)));
    }
    assert!(
        !lambda_proxy_satisfies(&shared, other_copy, runnable),
        "a same-NAMED interface of another class is not the proxy's"
    );
    assert!(
        lambda_proxy_satisfies(&shared, other_copy, object),
        "control: the resolved class itself"
    );
    assert!(
        lambda_proxy_satisfies(&shared, own, runnable),
        "control: the proxy's own interface"
    );
}
