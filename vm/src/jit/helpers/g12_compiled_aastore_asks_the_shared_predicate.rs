use super::*;
use crate::config::VmConfig;
use cratonvm_classloading::ClassOrigin;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use std::sync::Arc;

/// Fabricate an INTERFACE.
///
/// A plain `fn`, not a closure: the guard must be dropped before the next
/// statement (`aastore_element_assignable` takes the read lock itself, and a
/// live write guard would deadlock the test rather than fail it), and the
/// `Arc<SharedVm>` is moved into the returned tuple below, which a closure
/// borrowing it would complicate for no gain.
pub fn mk_iface(shared: &SharedVm, name: &str) -> ClassId {
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

/// One VM, two unrelated fabricated interfaces, and a `$ProxyN` whose
/// RECORDED interface set is `[G12JitMarker]` — the shape
/// `define_or_get_proxy_class` produces from real `proxy_gen` bytes.
///
/// The proxy class is deliberately given no `interfaces` vector and no
/// proxy superclass, so the hierarchy walk inside the predicate declines for
/// it: whatever these assertions observe comes from the recorded interface
/// set, not from the class graph.
///
/// Returns `(vm, G12JitMarker[], G12JitOther[], the proxy)`.
pub fn vm_with_a_recorded_proxy() -> (Arc<SharedVm>, ObjectRef, ObjectRef, ObjectRef) {
    let shared: Arc<SharedVm> = Arc::new(SharedVm::new(VmConfig::default()));
    let marker = mk_iface(&shared, "cratonvm/test/G12JitMarker");
    let other = mk_iface(&shared, "cratonvm/test/G12JitOther");
    let proxy_cid = {
        let mut cm = shared.classes.class_manager.write();
        let id = cm
            .try_ensure_synthetic_class("jdk/proxy12/$Proxy0", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --real-jdk");
        cm.class_store
            .get_mut(id)
            .expect("just fabricated")
            .set_origin(ClassOrigin::GeneratedProxy {
                interfaces: Arc::from(vec![marker]),
            });
        id
    };
    // A reference array's own class id IS its component class id
    // (JVMS §4.4.1).
    let marker_arr = shared
        .mem
        .heap
        .alloc_array(marker, ArrayElementType::Reference, 1);
    let other_arr = shared
        .mem
        .heap
        .alloc_array(other, ArrayElementType::Reference, 1);
    let proxy = shared.mem.heap.alloc_object(proxy_cid, 0);
    (shared, marker_arr, other_arr, proxy)
}

/// The LEGAL direction, end to end through the extern entry point compiled
/// code actually calls. `s25` on the fixture, and the row that goes red if
/// the fix were phrased as "refuse every proxy" — which is how this vector
/// goes from 2 fails to 20.
///
/// It needs no exception machinery at all (a `0` is the allow path), which
/// is why the legal direction is the one asserted here and the refusal is
/// asserted at the predicate below: reaching `i64::MIN` requires an
/// installed JIT thread AND a throwable built through
/// `throw_runtime_error`, i.e. two mechanisms that are `RExceptions`'
/// subject and not this one's.
#[test]
pub fn the_compiled_entry_point_admits_a_proxy_into_its_own_interfaces_array() {
    let (shared, marker_arr, _other_arr, proxy) = vm_with_a_recorded_proxy();
    let vm_ptr = Arc::as_ptr(&shared) as i64;
    // SAFETY: `vm_ptr` names the live `SharedVm` above and both refs are
    // live heap objects from its own heap — the contract compiled code
    // satisfies.
    unsafe {
        assert_eq!(
            jit_aastore_type_check(vm_ptr, marker_arr.as_ptr() as i64, proxy.as_ptr() as i64),
            0,
            "a proxy generated for G12JitMarker must still store into a \
             G12JitMarker[] in COMPILED code"
        );
        assert_eq!(
            jit_aastore_type_check(vm_ptr, marker_arr.as_ptr() as i64, 0),
            0,
            "a null element is always storable and must never reach the \
             predicate at all"
        );
    }
}

/// The REFUSED direction, at the predicate `aastore_store_is_refused`
/// consults two lines in. Everything after that call in this file is
/// message construction and throwable delivery — the *decision* is here,
/// and it is the same `false` the interpreter's `aastore` arm reads.
#[test]
pub fn the_predicate_the_compiled_path_consults_refuses_the_measured_row() {
    let (shared, _marker_arr, other_arr, proxy) = vm_with_a_recorded_proxy();
    assert!(
        !crate::runtime::interpreter::aastore_element_assignable(&shared, other_arr, proxy),
        "`Runnable[] <- Proxy(Marker)` is ArrayStoreException on HotSpot \
         25.0.3 (MEASURED, RArrayStoreInterfaces s12) in both tiers; the \
         compiled tier reads this predicate and nothing else"
    );
}
