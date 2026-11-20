use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Counter so tests generate unique JNDI sub-roots (they also share the
/// process-wide `ServiceName` intern table).
static SEQ: AtomicU32 = AtomicU32::new(1);

/// A private VM identity per test (gc-common w10-d). The store and the
/// MSC container are per VM, so tests no longer share a map and need no
/// module lock; before w10-d a sibling test's reset of the one
/// process-global map could wipe a test's bindings mid-assertion.
/// Dropping the guard forgets the VM's bindings and container, even when
/// an assertion fails.
struct TestVm(usize);

impl TestVm {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0x10D0_1000);
        TestVm(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl Drop for TestVm {
    fn drop(&mut self) {
        forget_vm_jndi_bindings(self.0);
        crate::jboss_msc::forget_vm_msc_container(self.0);
    }
}

fn unique_subroot(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    format!("java:jboss/t19_2_b_{tag}_{n}")
}

/// Synthesize a dummy `ObjectRef` for tests that only care about
/// round-tripping the binding.  We pick a distinctive pointer value
/// so two fake refs don't collide.
fn fake_object_ref(id: usize) -> ObjectRef {
    // SAFETY: this pointer is never dereferenced; it's used purely
    // as an identity key by the binding store's HashMap.  The tests
    // do not hand the ref to NativeContext / the heap, so there's
    // no VM invariant that it points at a live allocation.
    unsafe { ObjectRef::from_raw((id * 16 + 8) as *mut _) }
}

// ---------------------------------------------------------------
// 1. InitialContext constructor
// ---------------------------------------------------------------
#[test]
fn t19_2_b_initial_context_constructor_succeeds() {
    // Constructor logic is pure: we just need the allowlist to hold.
    assert!(validate_jndi_name("java:comp/env").is_ok());
    assert!(validate_jndi_name("java:").is_ok());
}

// ---------------------------------------------------------------
// 2. bind / lookup round-trip
// ---------------------------------------------------------------
#[test]
fn t19_2_b_bind_lookup_round_trip() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("roundtrip");
    let full = format!("{name}/leaf");
    let obj = fake_object_ref(1);
    bind_value(vm, &full, "com/example/Thing", obj).expect("bind");
    let round = lookup_value(vm, &full).expect("lookup");
    assert_eq!(round.as_ptr() as usize, obj.as_ptr() as usize);
}

// ---------------------------------------------------------------
// 3. rebind replaces previous
// ---------------------------------------------------------------
#[test]
fn t19_2_b_rebind_replaces_previous() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("rebind");
    let full = format!("{name}/thing");
    let a = fake_object_ref(11);
    let b = fake_object_ref(22);
    bind_value(vm, &full, "X", a).expect("bind");
    rebind_value(vm, &full, "Y", b).expect("rebind");
    let got = lookup_value(vm, &full).expect("lookup");
    assert_eq!(got.as_ptr() as usize, b.as_ptr() as usize);
}

// ---------------------------------------------------------------
// 4. unbind removes entry
// ---------------------------------------------------------------
#[test]
fn t19_2_b_unbind_removes_entry() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("unbind");
    let full = format!("{name}/leaf");
    bind_value(vm, &full, "X", fake_object_ref(33)).unwrap();
    assert!(lookup_value(vm, &full).is_ok());
    unbind_value(vm, &full).unwrap();
    let err = lookup_value(vm, &full).unwrap_err();
    assert!(
        err.contains("NameNotFoundException"),
        "expected NameNotFoundException, got {err}"
    );
}

// ---------------------------------------------------------------
// 5. lookup missing → NameNotFoundException
// ---------------------------------------------------------------
#[test]
fn t19_2_b_lookup_missing_name_throws_name_not_found() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("missing");
    let full = format!("{name}/nope");
    let err = lookup_value(vm, &full).unwrap_err();
    assert!(err.contains("NameNotFoundException"), "got: {err}");
}

// ---------------------------------------------------------------
// 6. hierarchical path walk
// ---------------------------------------------------------------
#[test]
fn t19_2_b_hierarchical_path_walk() {
    let t = TestVm::new();
    let vm = t.0;
    let root = unique_subroot("hier");
    let full = format!("{root}/ds/KeycloakDS");
    let obj = fake_object_ref(44);
    bind_value(vm, &full, "javax/sql/DataSource", obj).unwrap();

    // The bind_info_for parse must produce a 4-or-more-segment
    // ServiceName plus the stripped binding path.
    let info = context_names_bind_info_for(&full).unwrap();
    assert!(info
        .binder_service_name
        .canonical()
        .starts_with("java.jboss.t19_2_b_hier"));
    assert!(info
        .binder_service_name
        .canonical()
        .ends_with(".ds.KeycloakDS"));
    assert!(info.binding_name.contains("/ds/KeycloakDS"));

    // Walk the hierarchy: lookup the full name should succeed.
    let resolved = lookup_value(vm, &full).unwrap();
    assert_eq!(resolved.as_ptr() as usize, obj.as_ptr() as usize);

    // A lookup of an intermediate that was never bound must fail
    // (`java:jboss/<root>/ds` is a subcontext, not a value).
    let mid = format!("{root}/ds");
    let err = lookup_value(vm, &mid).unwrap_err();
    assert!(err.contains("NameNotFoundException"));
}

// ---------------------------------------------------------------
// 7. ContextNames.bindInfoFor parses absolute name
// ---------------------------------------------------------------
#[test]
fn t19_2_b_context_names_bind_info_parses_absolute_name() {
    let info = context_names_bind_info_for("java:jboss/datasources/KeycloakDS").unwrap();
    assert_eq!(
        info.binder_service_name.canonical(),
        "java.jboss.datasources.KeycloakDS"
    );
    assert_eq!(info.parent_context_service_name.canonical(), "java.jboss");
    assert_eq!(&*info.binding_name, "datasources/KeycloakDS");
    assert_eq!(&*info.absolute_name, "java:jboss/datasources/KeycloakDS");

    let exported = context_names_bind_info_for("ExampleDS").unwrap();
    assert_eq!(
        exported.parent_context_service_name.canonical(),
        "java.jboss.exported"
    );
    assert_eq!(&*exported.binding_name, "ExampleDS");

    let explicit_exported = context_names_bind_info_for("java:jboss/exported/Foo").unwrap();
    assert_eq!(
        explicit_exported.parent_context_service_name.canonical(),
        "java.jboss.exported"
    );
    assert_eq!(&*explicit_exported.binding_name, "Foo");

    // And ensure the injection-guard rejects hostile URL schemes.
    assert!(context_names_bind_info_for("ldap://evil.example/a").is_err());
    assert!(context_names_bind_info_for("rmi://evil.example/a").is_err());
    assert!(context_names_bind_info_for("dns:evil.example").is_err());
}

// ---------------------------------------------------------------
// 8. ServiceBasedNamingStore registers an MSC service
// ---------------------------------------------------------------
#[test]
fn t19_2_b_service_based_naming_store_registers_msc_service() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("mscreg");
    let full = format!("{name}/SvcThing");
    let obj = fake_object_ref(55);
    bind_value(vm, &full, "java/lang/Object", obj).unwrap();

    // T19.1 container must now see the binder service.
    let info = context_names_bind_info_for(&full).unwrap();
    let state = crate::jboss_msc::container_for(vm).get_state(&info.binder_service_name);
    assert!(
        state.is_some(),
        "MSC container should see the binder service {:?}",
        info.binder_service_name.canonical()
    );
}

// ---------------------------------------------------------------
// 9. listBindings returns all direct children
// ---------------------------------------------------------------
#[test]
fn t19_2_b_list_bindings_returns_all_direct_children() {
    let t = TestVm::new();
    let vm = t.0;
    let root = unique_subroot("listbind");
    bind_value(
        vm,
        &format!("{root}/child1"),
        "java/lang/Object",
        fake_object_ref(61),
    )
    .unwrap();
    bind_value(
        vm,
        &format!("{root}/child2"),
        "java/lang/Object",
        fake_object_ref(62),
    )
    .unwrap();
    bind_value(
        vm,
        &format!("{root}/sub/grandchild"),
        "java/lang/Object",
        fake_object_ref(63),
    )
    .unwrap();

    let kids = list_bindings(vm, &root).unwrap();
    // Only the two direct children; grandchild must not leak into
    // the direct-children list.
    assert_eq!(
        kids.len(),
        2,
        "expected exactly 2 direct children, got {:?}",
        kids.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>()
    );
    let names: Vec<String> = kids.iter().map(|(k, _)| k.to_string()).collect();
    assert!(names.iter().any(|n| n.ends_with("/child1")));
    assert!(names.iter().any(|n| n.ends_with("/child2")));
    assert!(!names.iter().any(|n| n.contains("grandchild")));
}

// ---------------------------------------------------------------
// 10. concurrent bind + lookup — no panic, no data race
// ---------------------------------------------------------------
#[test]
fn t19_2_b_concurrent_bind_lookup_no_data_race() {
    let t = TestVm::new();
    let vm = t.0;
    let root = unique_subroot("race");
    let handles: Vec<_> = (0..4)
        .map(|tid| {
            let root = root.clone();
            std::thread::spawn(move || {
                for i in 0..32 {
                    let full = format!("{root}/t{tid}_k{i}");
                    bind_value(
                        vm,
                        &full,
                        "java/lang/Object",
                        fake_object_ref(tid * 100 + i),
                    )
                    .expect("thread bind");
                    // Every writer also reads back its own key —
                    // exercises the RwLock read path concurrently
                    // with writers on other threads.
                    let got = lookup_value(vm, &full).expect("thread lookup");
                    assert_eq!(got.as_ptr() as usize % 2, 0);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("thread panicked");
    }
    // All four threads, 32 bindings each, still visible.
    let prefix = format!("{root}/");
    let survivors = BINDINGS
        .peek(vm, |store| {
            store.keys().filter(|k| k.starts_with(&prefix)).count()
        })
        .unwrap_or(0);
    assert_eq!(survivors, 4 * 32);
}

// ---------------------------------------------------------------
// Extra coverage: the injection guard is the load-bearing piece.
// ---------------------------------------------------------------
#[test]
fn t19_2_b_injection_guard_rejects_remote_schemes() {
    for bad in [
        "ldap://evil/a",
        "ldaps://evil/a",
        "rmi://evil/a",
        "dns:evil",
        "iiop://evil/a",
        "corbaname:evil",
        "http://evil/a",
        "file:/etc/passwd",
    ] {
        assert!(
            validate_jndi_name(bad).is_err(),
            "injection guard must reject {bad}"
        );
    }
}

#[test]
fn t19_2_b_injection_guard_accepts_known_roots() {
    for good in [
        "java:",
        "java:jboss/datasources/KeycloakDS",
        "java:comp/env/thing",
        "java:global/app/mod/bean",
        "java:app/whatever",
        "java:module/thing",
    ] {
        assert!(
            validate_jndi_name(good).is_ok(),
            "valid name unexpectedly rejected: {good}"
        );
    }
}

#[test]
fn t19_2_b_url_reference_lookup_refuses_to_connect() {
    let t = TestVm::new();
    let vm = t.0;
    let name = unique_subroot("urlref");
    let full = format!("{name}/thing");
    // Manually install a URL-ref entry to simulate what a
    // spec-compliant JDK would open as a network connection.
    {
        let info = context_names_bind_info_for(&full).unwrap();
        BINDINGS.with(vm, |store| {
            store.insert(
                info.absolute_name.clone(),
                BindingEntry {
                    service_name: info.binder_service_name,
                    class_name: Arc::<str>::from("javax/naming/Reference"),
                    value: None,
                    failed: false,
                    url_reference: Some(Arc::<str>::from("ldap://attacker/")),
                },
            );
        });
    }
    let err = lookup_value(vm, &full).unwrap_err();
    assert!(
        err.contains("NameNotFoundException"),
        "URL-ref lookup must refuse with NameNotFoundException, got {err}"
    );
    assert!(err.contains("refused by JNDI injection guard"));
}

/// `common-w9b-jndi-bindings-store-holds-unrooted-object-refs`: two VMs
/// bind the same name. Each VM's scan holds only its own value, a remap
/// with one VM's pointer map moves only that VM's entry, each VM's lookup
/// answers its own object, and teardown drops one VM's bindings only.
#[test]
fn w10d_bindings_are_per_vm_rooted_and_remapped() {
    let (ta, tb) = (TestVm::new(), TestVm::new());
    let (vm_a, vm_b) = (ta.0, tb.0);
    let name = format!("{}/cfg", unique_subroot("w10d_two_vms"));
    let (obj_a, obj_b) = (fake_object_ref(0x10D1), fake_object_ref(0x10D2));
    bind_value(vm_a, &name, "java/util/HashMap", obj_a).unwrap();
    bind_value(vm_b, &name, "java/util/HashMap", obj_b).unwrap();

    let scan = |vm: usize| {
        let mut out = Vec::new();
        gc_scan_jndi_binding_roots(vm, &mut out);
        out
    };
    assert_eq!(scan(vm_a), vec![obj_a], "A's scan roots A's value only");
    assert_eq!(scan(vm_b), vec![obj_b], "B's scan roots B's value only");

    // A moving collection of A relocates A's value.
    let moved_a = fake_object_ref(0x10D3);
    let mut map = cratonvm_types::PointerMap::default();
    map.insert(obj_a.as_ptr() as usize, moved_a.as_ptr() as usize);
    gc_update_jndi_binding_refs(vm_b, &map);
    assert_eq!(
        lookup_value(vm_a, &name).unwrap(),
        obj_a,
        "B's map must not move A's entry"
    );
    gc_update_jndi_binding_refs(vm_a, &map);
    assert_eq!(
        lookup_value(vm_a, &name).unwrap(),
        moved_a,
        "A's lookup sees the move"
    );
    assert_eq!(
        lookup_value(vm_b, &name).unwrap(),
        obj_b,
        "B keeps its own object"
    );

    // Each VM's binder service lives in that VM's MSC container only.
    let info = context_names_bind_info_for(&name).unwrap();
    assert!(crate::jboss_msc::container_for(vm_a)
        .get_state(&info.binder_service_name)
        .is_some());

    forget_vm_jndi_bindings(vm_a);
    assert!(
        lookup_value(vm_a, &name).is_err(),
        "teardown drops A's bindings"
    );
    assert!(scan(vm_a).is_empty());
    assert_eq!(lookup_value(vm_b, &name).unwrap(), obj_b, "and leaves B's");
}
