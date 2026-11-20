use super::{aastore_element_assignable, recorded_proxy_interface_set};
use cratonvm_classloading::ClassOrigin;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef};
use std::sync::Arc;

/// One VM with four fabricated interfaces and two proxy classes.
///
/// * `G12Marker`, `G12Other` — unrelated marker interfaces.
/// * `G12Sub extends G12Super` — the transitive leg (oracle:
///   `SuperI[] <- proxy(SubI)` is OK, `SubI[] <- proxy(SuperI)` throws).
/// * `$Proxy9` — origin `GeneratedProxy { interfaces: [G12Marker] }`, the
///   shape `define_or_get_proxy_class` produces from real `proxy_gen` bytes.
/// * `$Proxy8` — origin `GeneratedProxy { interfaces: [] }`, the shape
///   `ensure_synthetic_class` produces from a proxy NAME with no interface
///   data. This is the population the old blanket existed for and it must
///   keep failing open.
///
/// Neither proxy class is given a real `interfaces` vector or a proxy
/// superclass, deliberately: `is_subclass_of` and `is_assignable_to_name`
/// therefore both decline for both of them, so every admission below has to
/// come from the arm under test rather than from the hierarchy walk above
/// it. That is what makes the legal rows evidence.
struct Fixture {
    shared: Arc<crate::vm::SharedVm>,
    marker_arr: ObjectRef,
    other_arr: ObjectRef,
    sub_arr: ObjectRef,
    super_arr: ObjectRef,
    serializable_arr: ObjectRef,
    recorded_proxy: ObjectRef,
    unrecorded_proxy: ObjectRef,
    sub_proxy: ObjectRef,
    super_proxy: ObjectRef,
}

/// Fabricate an INTERFACE. Separate `fn`s rather than closures throughout
/// this module, and every class-manager guard dropped before the next
/// statement: `aastore_element_assignable` takes the read lock itself, and a
/// write guard kept alive as a statement temporary would deadlock the test
/// rather than fail it.
fn iface(shared: &crate::vm::SharedVm, name: &str) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    cm.class_store
        .get_mut(id)
        .expect("just fabricated")
        .access_flags |= ClassAccessFlags::INTERFACE;
    id
}

/// Fabricate a `$ProxyN` and stamp the interface set the VM is to have
/// "recorded" for it. An empty `ifaces` leaves the origin as
/// `ensure_synthetic_class` already set it — `GeneratedProxy` with an empty
/// list — which is the no-record shape.
fn proxy_class(shared: &crate::vm::SharedVm, name: &str, ifaces: &[ClassId]) -> ClassId {
    let mut cm = shared.classes.class_manager.write();
    let id = cm
        .try_ensure_synthetic_class(name, 0)
        .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
    if !ifaces.is_empty() {
        cm.class_store
            .get_mut(id)
            .expect("just fabricated")
            .set_origin(ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(ifaces.to_vec()),
            });
    }
    id
}

fn ref_array(shared: &crate::vm::SharedVm, component: ClassId) -> ObjectRef {
    // A reference array's own class id IS its component class id
    // (JVMS §4.4.1) — that is how `array_descriptor_of` recovers the
    // component name.
    shared
        .mem
        .heap
        .alloc_array(component, ArrayElementType::Reference, 1)
}

fn fixture() -> Fixture {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let marker = iface(&shared, "cratonvm/test/G12Marker");
    let other = iface(&shared, "cratonvm/test/G12Other");
    let sup = iface(&shared, "cratonvm/test/G12Super");
    let sub = iface(&shared, "cratonvm/test/G12Sub");
    {
        let mut cm = shared.classes.class_manager.write();
        // Through `set_interfaces`: a raw push leaves the store's
        // implementor index and subtype display blind to the edge.
        let mut sub_ifaces = cm
            .class_store
            .get(sub)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default();
        sub_ifaces.push(sup);
        cm.set_interfaces(sub, sub_ifaces);
    }
    let serializable = {
        let mut cm = shared.classes.class_manager_write();
        cm.load_class("java/io/Serializable")
            .expect("Compatible mode fabricates")
    };

    // A digit-suffixed `$ProxyN` simple name is what
    // `is_generated_proxy_name` matches, so all four of these are already
    // `GeneratedProxy` before we touch them — the unrecorded one with an
    // EMPTY list, which is precisely the fabricated-proxy shape.
    let recorded = proxy_class(&shared, "jdk/proxy9/$Proxy9", &[marker]);
    let unrecorded = proxy_class(&shared, "jdk/proxy9/$Proxy8", &[]);
    let super_only = proxy_class(&shared, "jdk/proxy9/$Proxy7", &[sup]);
    let sub_only = proxy_class(&shared, "jdk/proxy9/$Proxy6", &[sub]);

    Fixture {
        marker_arr: ref_array(&shared, marker),
        other_arr: ref_array(&shared, other),
        sub_arr: ref_array(&shared, sub),
        super_arr: ref_array(&shared, sup),
        serializable_arr: ref_array(&shared, serializable),
        recorded_proxy: shared.mem.heap.alloc_object(recorded, 0),
        unrecorded_proxy: shared.mem.heap.alloc_object(unrecorded, 0),
        sub_proxy: shared.mem.heap.alloc_object(sub_only, 0),
        super_proxy: shared.mem.heap.alloc_object(super_only, 0),
        shared,
    }
}

/// `recorded_proxy_interface_set` is the whole discriminator, and the
/// emptiness rule is the half that is easy to delete by accident: an
/// `ensure_synthetic_class`-fabricated `$ProxyN` carries
/// `GeneratedProxy { interfaces: [] }`, so treating "is a GeneratedProxy" as
/// the test would refuse every one of them.
#[test]
fn an_empty_recorded_interface_list_is_no_record_at_all() {
    assert!(
        recorded_proxy_interface_set(&ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(vec![ClassId::new(7)]),
        })
        .is_some(),
        "a generated proxy WITH interfaces is a positive record"
    );
    assert!(
        recorded_proxy_interface_set(&ClassOrigin::GeneratedProxy {
            interfaces: Arc::from(Vec::new()),
        })
        .is_none(),
        "an EMPTY list is `no interface data recorded`, not `implements \
         nothing` — a fabricated $ProxyN has exactly this origin and must \
         keep failing open"
    );
    assert!(
        recorded_proxy_interface_set(&ClassOrigin::VmInternal).is_none(),
        "`java/lang/reflect/Proxy$Instance` is VmInternal, not a generated \
         proxy — the shim's interface set lives on the heap object"
    );
    assert!(
        recorded_proxy_interface_set(&ClassOrigin::CompatibilityStub {
            reason: Arc::from("probe"),
        })
        .is_none(),
    );
}

/// The measured row. `Runnable[] <- Proxy(Marker)`, in the shape this VM
/// can fabricate.
#[test]
fn a_recorded_proxy_is_refused_by_an_interface_it_does_not_implement() {
    let f = fixture();
    assert!(
        !aastore_element_assignable(&f.shared, f.other_arr, f.recorded_proxy),
        "a proxy generated for G12Marker is not a G12Other; HotSpot 25.0.3 \
         throws ArrayStoreException for `Runnable[] <- Proxy(Marker)` \
         (measured, RArrayStoreInterfaces s12) and the `$Proxy` name \
         blanket admitted it without looking at the interface set"
    );
}

/// The control, and the row that says the refusal above is a decision and
/// not a new blanket in the other direction. `s25` on the fixture.
///
/// Note the fixture gives this proxy class NO `interfaces` vector, so
/// `is_subclass_of` and `is_assignable_to_name` both declined before the arm
/// under test ran: this admission comes from the recorded origin alone.
#[test]
fn a_recorded_proxy_is_admitted_by_an_interface_it_does_implement() {
    let f = fixture();
    assert!(
        aastore_element_assignable(&f.shared, f.marker_arr, f.recorded_proxy),
        "a proxy generated for G12Marker must still store into a \
         G12Marker[] — RArrayStoreInterfaces s25, green today, and a fix \
         that turns it red has replaced one wrong answer with another"
    );
}

/// Transitively, through super-interfaces, in both directions.
/// MEASURED on HotSpot: `SuperI[] <- proxy(SubI)` is OK and
/// `SubI[] <- proxy(SuperI)` throws.
#[test]
fn the_recorded_walk_is_transitive_and_directional() {
    let f = fixture();
    assert!(
        aastore_element_assignable(&f.shared, f.super_arr, f.sub_proxy),
        "a proxy over G12Sub is a G12Super — the recorded walk must go \
         through super-interfaces, not just compare names"
    );
    assert!(
        !aastore_element_assignable(&f.shared, f.sub_arr, f.super_proxy),
        "a proxy over G12Super is NOT a G12Sub; a walk that answered `true` \
         here would be matching the pair rather than the direction"
    );
}

/// The population the blanket was written for, unchanged. This is the
/// assertion that fails if the fix is phrased as "refuse every proxy".
///
/// `vm/src/runtime/interpreter/tests.rs`'s `jdk/proxy3/$Proxy27` fixture is
/// this exact shape, and so is the `AotIntegrationTests` /
/// `TypeMappedAnnotation.adapt` store it stands for.
#[test]
fn a_proxy_with_no_recorded_interface_set_still_fails_open() {
    let f = fixture();
    assert!(
        aastore_element_assignable(&f.shared, f.other_arr, f.unrecorded_proxy),
        "a $ProxyN whose interface set this VM never recorded is exactly \
         the imprecise-type-information case the predicate is contractually \
         required to fail OPEN on; only a RECORDED set makes a refusal \
         provable"
    );
}

/// `Serializable` is universal for a proxy and it is not a proxy rule:
/// `java.lang.reflect.Proxy.class.getInterfaces()` is `[java.io.Serializable]`
/// (MEASURED, `scratchpad/g12/ProxyStoreProbe.java`), so every proxy
/// inherits it through its superclass — which the fixture's proxy classes
/// deliberately do not have, so this arm has to state it.
///
/// `Cloneable` is the control on the oracle side (`Cloneable[] <- proxy(A)`
/// throws) and is not asserted here, because in a bare test VM the outcome
/// would turn on whether `java/lang/Cloneable` resolves rather than on this
/// arm — a row that can pass vacuously is not evidence.
#[test]
fn every_proxy_is_serializable() {
    let f = fixture();
    assert!(
        aastore_element_assignable(&f.shared, f.serializable_arr, f.recorded_proxy),
        "Serializable[] <- proxy is OK on HotSpot for every proxy, by \
         inheritance from java.lang.reflect.Proxy"
    );
}
