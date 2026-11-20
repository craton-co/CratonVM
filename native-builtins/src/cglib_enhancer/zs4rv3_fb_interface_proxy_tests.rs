use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess,
};

/// zs s4/rv3: under the strict moving mock (every allocation relocates
/// the heap) the interface-proxy wrapper's handler holds the raw factory
/// and the bean factory at their CURRENT addresses. Before the fix the
/// two parameters were pinned after the mirror mint (a GC point), so the
/// handler store wrote a retired address and the mock panics.
#[test]
fn the_interface_proxy_handler_holds_current_factories_across_the_mirror_mint() {
    if !crate::reflect_annotations::real_proxy_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let iface = cratonvm_types::ClassId::new(0x7e51);
    let proxy = cratonvm_types::ClassId::new(0x7e52);
    // A cache hit: the define is not this test's GC point, the mirror
    // mint is. The mock's interface is non-public, so the key carries
    // its own loader.
    let loader = ctx.loader_id_of_class(iface) as u32; // Cast: a small loader id
    {
        let mut guard = crate::PROXY_CLASS_CACHE.write();
        let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
        for l in [0, loader] {
            map.insert((ctx.vm_identity(), l, vec![iface]), proxy);
        }
    }
    let raw = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let bean_factory = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let mine = ctx.pin_native_root(raw);
    ctx.pin_native_root(bean_factory);
    ctx.set_moving_heap(true);
    let real =
        build_or_get_factory_bean_interface_proxy(&mut ctx, iface, raw, bean_factory, "bean")
            .expect("a wrapper over the cached proxy class");
    let raw_now = ctx.read_native_pin(mine, raw);
    let bean_now = ctx.read_native_pin(mine + 1, bean_factory);
    assert!(
        ctx.relocations_for_test() > 0,
        "positive control: the heap moved"
    );
    let Value::Object(Some(handler)) = ctx.get_field(real, 0) else {
        panic!("the proxy's handler field");
    };
    assert_eq!(
        ctx.get_field(handler, FB_HANDLER_FIELD_RAW_FACTORY),
        Value::Object(Some(raw_now))
    );
    assert_eq!(
        ctx.get_field(handler, FB_HANDLER_FIELD_BEAN_FACTORY),
        Value::Object(Some(bean_now))
    );
    ctx.unpin_native_roots(mine);
    assert_eq!(ctx.native_pin_count_for_test(), 0, "no pin leaked");
    let mut guard = crate::PROXY_CLASS_CACHE.write();
    if let Some(map) = guard.as_mut() {
        map.retain(|(vm, _, _), _| *vm != ctx.vm_identity());
    }
}
