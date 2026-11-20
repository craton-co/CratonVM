use super::{
    interface_special_receiver_check_enabled, interface_special_receiver_refusal_for_class,
    ClassId,
};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use crate::vm::SharedVm;
use crate::VmConfig;
use std::sync::Arc;

/// The switch is the VM's configuration field, not an environment read:
/// two VMs with different configurations answer differently in one
/// process.
#[test]
fn the_check_switch_is_read_from_the_vm_configuration() {
    let on = SharedVm::new(VmConfig {
        interface_special_receiver_check: true,
        ..VmConfig::default()
    });
    let off = SharedVm::new(VmConfig {
        interface_special_receiver_check: false,
        ..VmConfig::default()
    });
    assert!(interface_special_receiver_check_enabled(&on));
    assert!(!interface_special_receiver_check_enabled(&off));
}

/// A VM-minted lambda proxy with a sender the class store does not know
/// is not refused: the proxy arm reads the sender first and answers
/// `None` before the satisfies question (wave 138, lane L6, narrowed the
/// arm from "never refused" to the `checkcast` answer; the refusal itself
/// is shown by `L6W138LambdaProxyTraitReceiver.java`).
#[test]
fn a_lambda_proxy_receiver_with_an_unknown_sender_is_not_refused() {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let proxy = shared.alloc_lambda_proxy_id();
    shared.classes.lambda_proxies.write().insert(
        proxy,
        Arc::new(LambdaCallSite {
            functional_interface_id: None,
            functional_interface: "java/lang/Runnable".into(),
            sam_method_name: "run".into(),
            sam_descriptor: "()V".into(),
            impl_handle: MethodHandle {
                kind: MethodHandleKind::InvokeStatic,
                class_name: "test/A".into(),
                member_name: "lambda$0".into(),
                descriptor: "()V".into(),
            },
            instantiated_descriptor: "()V".into(),
            capture_types: vec![],
            proxy_class_id: proxy,
            serializable_flag: false,
        }),
    );
    assert!(shared.classes.is_lambda_proxy_class(proxy));
    for sender in [ClassId::new(0x7fff_0001), ClassId::new(0x7fff_0042)] {
        assert!(
            shared.classes.class_manager.read().get_class(sender).is_none(),
            "sender {sender:?} must be unknown for this test"
        );
        assert_eq!(
            interface_special_receiver_refusal_for_class(&shared, sender, proxy),
            None,
            "sender {sender:?}"
        );
    }
}
