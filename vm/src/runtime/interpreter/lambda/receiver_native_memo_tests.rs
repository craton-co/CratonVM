use super::{
    forget_unloaded_private_impl_memo, forget_vm_private_impl_memo, lambda_receiver_native_id,
    receiver_native_memo, ClassId, SharedVm,
};

/// Lambda dispatch plan stage 3: a row stamped with the current native
/// registry generation is served as is; a row stamped with another one is
/// re-derived (here for a receiver class the VM cannot name, which has no
/// receiver-keyed native) and re-stamped. Rows go with an unloaded
/// receiver class and with their VM.
#[test]
fn the_receiver_native_id_is_memoised_per_registry_generation() {
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let vm = shared.vm_identity;
    let generation = shared.natives.native_methods.generation();
    let proxy = ClassId::new(0x7FF0_0201);
    let (served, stale) = (ClassId::new(0x7FF0_0202), ClassId::new(0x7FF0_0203));
    let planted = cratonvm_native_api::NativeMethodId::from_u32(7);
    {
        let mut memo = receiver_native_memo().write();
        memo.insert((vm, proxy, served), (generation, Some(planted)));
        memo.insert(
            (vm, proxy, stale),
            (generation.wrapping_add(1), Some(planted)),
        );
    }
    assert_eq!(
        lambda_receiver_native_id(&shared, proxy, served, &|| std::sync::Arc::from("x/Y"), "m", "()V"),
        Some(planted)
    );
    assert_eq!(
        lambda_receiver_native_id(&shared, proxy, stale, &|| std::sync::Arc::from("x/Y"), "m", "()V"),
        None
    );
    assert_eq!(
        receiver_native_memo()
            .read()
            .get(&(vm, proxy, stale))
            .copied(),
        Some((generation, None))
    );
    let unloaded: rustc_hash::FxHashSet<ClassId> = [stale].into_iter().collect();
    forget_unloaded_private_impl_memo(vm, &unloaded);
    assert!(receiver_native_memo()
        .read()
        .get(&(vm, proxy, stale))
        .is_none());
    assert!(receiver_native_memo()
        .read()
        .get(&(vm, proxy, served))
        .is_some());
    forget_vm_private_impl_memo(vm);
    assert!(!receiver_native_memo()
        .read()
        .keys()
        .any(|&(vid, _, _)| vid == vm));
}

/// Wave 8: the rows KEYED by a proxy whose host unloaded go from both
/// memos (`forget_unloaded_lambda_proxy_rows`); another proxy's rows and
/// another VM's rows stay.
#[test]
fn rows_keyed_by_a_dead_proxy_are_forgotten() {
    let (vm, other_vm) = (usize::MAX - 21, usize::MAX - 22);
    let (dead, live) = (ClassId::new(0x7FF0_0301), ClassId::new(0x7FF0_0302));
    let receiver = ClassId::new(0x7FF0_0303);
    {
        let mut memo = super::private_impl_memo().write();
        memo.insert((vm, dead), None);
        memo.insert((vm, live), None);
        memo.insert((other_vm, dead), None);
        let mut natives = receiver_native_memo().write();
        natives.insert((vm, dead, receiver), (0, None));
        natives.insert((vm, live, receiver), (0, None));
        natives.insert((other_vm, dead, receiver), (0, None));
    }
    let dead_proxies: rustc_hash::FxHashSet<ClassId> = [dead].into_iter().collect();
    super::forget_dead_lambda_proxy_memo_rows(vm, &dead_proxies);
    {
        let memo = super::private_impl_memo().read();
        assert!(!memo.contains_key(&(vm, dead)));
        assert!(memo.contains_key(&(vm, live)));
        assert!(memo.contains_key(&(other_vm, dead)));
        let natives = receiver_native_memo().read();
        assert!(!natives.contains_key(&(vm, dead, receiver)));
        assert!(natives.contains_key(&(vm, live, receiver)));
        assert!(natives.contains_key(&(other_vm, dead, receiver)));
    }
    forget_vm_private_impl_memo(vm);
    forget_vm_private_impl_memo(other_vm);
}
