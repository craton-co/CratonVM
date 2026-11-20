//! gc-common w21-b (`common-w20v-zgc-mark-skips-wild-child-pointers-under-
//! nio-selector-stress`): `build_set` allocates the `HashSet` (and runs
//! its `<init>`) before it adds the keys, so the keys must be rooted from
//! BEFORE that allocation. The mock makes the set's `new_object` a moving
//! collection; every `Set.add` must then carry a key's moved address.
use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn w21b_build_set_adds_the_keys_where_the_sets_allocation_moved_them() {
    let mut ctx = MockNativeContext::new();
    let k0 = ctx.alloc_object(1);
    let k1 = ctx.alloc_object(1);
    // A by-name tag follows each key through the relocation, so an `add`
    // argument can be traced back to the key it must be.
    ctx.set_field_by_name(k0, "w21b_tag", Value::Int(10));
    ctx.set_field_by_name(k1, "w21b_tag", Value::Int(11));
    ctx.w21b_relocate_on_new_object("java/util/HashSet");

    let set = build_set(&mut ctx, &[k0, k1]);

    assert_eq!(
        ctx.relocation_count(),
        1,
        "the set's allocation moved every object"
    );
    assert_eq!(ctx.pinned_count(), 0, "every pin is released");
    let added: Vec<Value> = ctx
        .recorded_calls()
        .iter()
        .filter(|c| c.method_name == "add")
        .map(|c| c.args[0])
        .collect();
    assert_eq!(added.len(), 2, "one Set.add per key");
    let mut tags: Vec<i32> = added
        .iter()
        .map(|v| match v {
            Value::Object(Some(k)) => {
                assert_ne!(k.as_ptr(), k0.as_ptr(), "a key's pre-collection address");
                assert_ne!(k.as_ptr(), k1.as_ptr(), "a key's pre-collection address");
                match ctx.get_field_by_name(*k, "w21b_tag") {
                    Value::Int(t) => t,
                    other => panic!("Set.add got an address that names no key: {other:?}"),
                }
            }
            other => panic!("Set.add got {other:?}"),
        })
        .collect();
    tags.sort_unstable();
    assert_eq!(tags, vec![10, 11], "each key is added at its moved address");
    // The set itself was allocated after the move: a live mock object.
    let _ = ctx.object_num_fields(set);
}
