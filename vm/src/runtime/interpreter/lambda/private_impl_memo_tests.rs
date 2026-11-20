use super::{
    forget_unloaded_private_impl_memo, forget_vm_private_impl_memo,
    lambda_private_impl_dispatch_class, private_impl_memo, ClassId, SharedVm,
};
use crate::classloading::resolution::{LambdaCallSite, MethodHandle, MethodHandleKind};
use std::sync::Arc;

fn site(proxy: u32, owner: &str, name: &str, desc: &str) -> LambdaCallSite {
    LambdaCallSite {
        functional_interface: Arc::from("java/util/function/ToIntFunction"),
        functional_interface_id: None,
        sam_method_name: Arc::from("applyAsInt"),
        sam_descriptor: Arc::from("(Ljava/lang/Object;)I"),
        impl_handle: MethodHandle {
            kind: MethodHandleKind::InvokeVirtual,
            class_name: Arc::from(owner),
            member_name: Arc::from(name),
            descriptor: Arc::from(desc),
        },
        instantiated_descriptor: Arc::from("(Ljava/lang/Object;)I"),
        capture_types: vec![],
        proxy_class_id: ClassId::new(proxy),
        serializable_flag: false,
    }
}

/// A public impl method has no private declaring class; with no
/// user-defined loader in the process the answer is memoised for the
/// proxy, and the row goes with its VM.
#[test]
fn a_public_impl_answer_is_memoised_and_dropped_with_its_vm() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    if shared
        .classes
        .class_manager
        .write()
        .load_class("java/lang/Object")
        .is_err()
    {
        return; // Stripped VM (no class library on the test classpath).
    }
    let cs = site(0x7FF0_0001, "java/lang/Object", "hashCode", "()I");
    assert_eq!(lambda_private_impl_dispatch_class(&shared, &cs), None);
    let key = (shared.vm_identity, cs.proxy_class_id);
    // The flag only ever goes false -> true, so `false` now means it was
    // `false` when the memo rule was evaluated.
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
        assert_eq!(private_impl_memo().read().get(&key).copied(), Some(None));
    }
    // Served from the memo: same answer.
    assert_eq!(lambda_private_impl_dispatch_class(&shared, &cs), None);
    // W3A item 3: and from this thread's front, which the VM's disposal
    // empties with the shared memo.
    let in_front = || super::OWNER_FRONT.with(|f| f.borrow().private.contains_key(&key));
    if !cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity) {
        assert!(in_front());
    }
    forget_vm_private_impl_memo(shared.vm_identity);
    assert!(private_impl_memo().read().get(&key).is_none());
    assert!(!in_front());
}

/// The front is bounded: a full table starts over rather than growing.
#[test]
fn the_owner_front_stays_within_its_cap() {
    let mut front = super::OwnerFront::default();
    for i in 0..=super::OWNER_FRONT_CAP as u32 {
        front.put_private((1, ClassId::new(i)), None);
        front.put_selection((1, ClassId::new(i), ClassId::new(0)), None);
    }
    assert!(front.private.len() <= super::OWNER_FRONT_CAP);
    assert!(front.selection.len() <= super::OWNER_FRONT_CAP);
    assert_eq!(front.private.len(), 1, "the overflowing row restarted the table");
}

/// A row whose answer names an unloaded class is dropped; a row of
/// another VM is not.
#[test]
fn a_row_naming_an_unloaded_class_is_forgotten() {
    // gcg g2/k: identities from the real counter (were `usize::MAX - 11/12`).
    let (vm, other_vm) = (
        crate::vm::vm_init::test_vm_identity(),
        crate::vm::vm_init::test_vm_identity(),
    );
    let dead = ClassId::new(0x7FF0_0100);
    let live = ClassId::new(0x7FF0_0101);
    {
        let mut memo = private_impl_memo().write();
        memo.insert((vm, ClassId::new(0x7FF0_0002)), Some(dead));
        memo.insert((vm, ClassId::new(0x7FF0_0003)), Some(live));
        memo.insert((other_vm, ClassId::new(0x7FF0_0002)), Some(dead));
    }
    let unloaded: rustc_hash::FxHashSet<ClassId> = [dead].into_iter().collect();
    forget_unloaded_private_impl_memo(vm, &unloaded);
    {
        let memo = private_impl_memo().read();
        assert!(memo.get(&(vm, ClassId::new(0x7FF0_0002))).is_none());
        assert_eq!(
            memo.get(&(vm, ClassId::new(0x7FF0_0003))).copied(),
            Some(Some(live))
        );
        assert!(memo.get(&(other_vm, ClassId::new(0x7FF0_0002))).is_some());
    }
    forget_vm_private_impl_memo(vm);
    forget_vm_private_impl_memo(other_vm);
}
