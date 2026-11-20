use super::*;

// Each test owns its own pair of VM identities (gc-common w15-e). The two
// tests below used to share one pair, and they run in parallel: the
// `Teardown` of `each_vm_gets_its_own_container` (and its mid-test
// `forget_vm_msc_container(VM_A)`) dropped VM_A's `SERVICE_ROOTS` row
// while `service_roots_are_per_vm` was between two scans, so its
// "B's map must not rewrite A's row" assertion saw an empty row (`[]`).
const VM_A: usize = 0x10D0_0601;
const VM_B: usize = 0x10D0_0602;
const CONTAINER_VM_A: usize = 0x10D0_0603;
const CONTAINER_VM_B: usize = 0x10D0_0604;
// Controller ids nothing else mints (the real counter starts at 1).
const ID: u64 = u64::MAX - 0x10D0;

fn at(addr: usize) -> ObjectRef {
    // SAFETY: never dereferenced; the scan copies it, the remap reads its address.
    unsafe { ObjectRef::from_raw(addr as *mut u8) }
}

fn scan(vm: usize) -> Vec<usize> {
    let mut out = Vec::new();
    gc_scan_msc_service_roots(vm, &mut out);
    out.iter().map(|r| r.as_ptr() as usize).collect()
}

/// Drops the named VMs' rows and containers even when an assertion fails.
/// It names only the calling test's own identities.
struct Teardown([usize; 2]);
impl Drop for Teardown {
    fn drop(&mut self) {
        for vm in self.0 {
            forget_vm_msc_service_roots(vm);
            forget_vm_msc_container(vm);
        }
    }
}

#[test]
fn service_roots_are_per_vm() {
    let _t = Teardown([VM_A, VM_B]);
    // The same controller id in two VMs names two independent rows.
    roots_row(VM_A, ID, |r| r.service = Some(at(0x10D0_1000)));
    roots_row(VM_B, ID, |r| r.listeners.push(at(0x10D0_3000)));

    assert_eq!(scan(VM_A), vec![0x10D0_1000], "A roots only what A wrote");
    assert_eq!(scan(VM_B), vec![0x10D0_3000], "B roots only what B wrote");
    assert_eq!(roots_get(VM_B, ID, |r| r.service).flatten(), None);

    let mut moved = cratonvm_types::PointerMap::default();
    moved.insert(0x10D0_1000, 0x10D0_4000);
    gc_update_msc_service_refs(VM_B, &moved);
    assert_eq!(
        scan(VM_A),
        vec![0x10D0_1000],
        "B's map must not rewrite A's row"
    );
    gc_update_msc_service_refs(VM_A, &moved);
    assert_eq!(scan(VM_A), vec![0x10D0_4000], "A's map rewrites A's row");

    forget_vm_msc_service_roots(VM_A);
    assert!(scan(VM_A).is_empty(), "teardown drops A's rows");
    assert_eq!(scan(VM_B), vec![0x10D0_3000], "and leaves B's");
}

#[test]
fn each_vm_gets_its_own_container() {
    let _t = Teardown([CONTAINER_VM_A, CONTAINER_VM_B]);
    let a = container_for(CONTAINER_VM_A);
    let b = container_for(CONTAINER_VM_B);
    assert!(!Arc::ptr_eq(&a, &b), "two VMs must not share a container");
    assert!(
        Arc::ptr_eq(&a, &container_for(CONTAINER_VM_A)),
        "one container per VM"
    );

    // Both VMs install the same service name: two controllers, and
    // neither VM resolves the name to the other VM's controller.
    let n = ServiceName::parse("w10d.msc.same.name");
    let id_a = a.add_service(n.clone(), vec![], Mode::Active, 0).unwrap();
    let only_a = ServiceName::parse("w10d.msc.only.in.a");
    a.add_service(only_a.clone(), vec![], Mode::Active, 0)
        .unwrap();
    let id_b = b.add_service(n.clone(), vec![], Mode::Active, 0).unwrap();
    assert_ne!(id_a, id_b, "controller ids stay unique in the process");
    assert_eq!(a.get_id(&n), Some(id_a));
    assert_eq!(b.get_id(&n), Some(id_b));
    assert_eq!(b.get_id(&only_a), None, "B must not see A's services");

    // Starting A's service leaves B's untouched.
    a.start_without_callback(id_a);
    assert_eq!(a.get_state(&n), Some(ServiceState::Up));
    if msc_real_start_enabled() {
        // (With real start off, B's own workers may start it.)
        assert_eq!(b.get_state(&n), Some(ServiceState::Down));
    }

    // Teardown shuts A's container down and a later use gets a new one.
    forget_vm_msc_container(CONTAINER_VM_A);
    assert!(a.is_shutdown());
    assert!(!b.is_shutdown(), "A's teardown must not shut B's container");
    assert!(!Arc::ptr_eq(&a, &container_for(CONTAINER_VM_A)));
}

/// `start_without_callback` brings up exactly one controller. The JNDI
/// binder path used `drain_tasks_locally`, which under the real-start
/// default also marks every other start-ready service `Up` without
/// running its `start()`.
#[test]
fn start_without_callback_starts_only_its_own_controller() {
    let c = ServiceContainer::new();
    let binder = ServiceName::parse("w10d.msc.binder");
    let other = ServiceName::parse("w10d.msc.real.service");
    let other_id = c
        .add_service(other.clone(), vec![], Mode::Active, 0)
        .unwrap();
    let binder_id = c
        .add_service(binder.clone(), vec![], Mode::Active, 0)
        .unwrap();
    c.start_without_callback(binder_id);
    assert_eq!(c.get_state(&binder), Some(ServiceState::Up));
    if msc_real_start_enabled() {
        assert_eq!(
            c.get_state(&other),
            Some(ServiceState::Down),
            "a service with a real start() must wait for drive_starts"
        );
    }
    let _ = other_id;
}
