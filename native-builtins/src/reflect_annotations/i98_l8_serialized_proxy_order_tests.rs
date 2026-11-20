use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_types::ClassId;

/// Interpreter round i4 wave 98, lane L8. Failed before: the read side
/// matched the interface SET, so a stream naming `[A, B]` could resolve
/// to the generated `[B, A]` class (whichever row the map iterated first).
#[test]
fn a_serialized_proxy_resolves_to_the_class_of_its_ordered_interfaces() {
    const VM: usize = 0x198_1808;
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(VM);
    let mut ctx = ctx;
    let a = ClassId::new(0x7A_0001);
    let b = ClassId::new(0x7A_0002);
    let (ab_class, ba_class) = (ClassId::new(0x7A_1001), ClassId::new(0x7A_1002));
    {
        let mut guard = PROXY_CLASS_CACHE.write();
        let map = guard.get_or_insert_with(rustc_hash::FxHashMap::default);
        map.insert((VM, 0, vec![b, a]), ba_class);
        map.insert((VM, 0, vec![a, b]), ab_class);
    }
    let got_ab = resolve_serialized_proxy_class_ids(&mut ctx, &[a, b]);
    let got_ba = resolve_serialized_proxy_class_ids(&mut ctx, &[b, a]);
    crate::forget_vm_proxy_classes(VM);
    assert_eq!(got_ab, Some(ab_class));
    assert_eq!(got_ba, Some(ba_class));
}
