use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};

fn field(name: &str, descriptor: &str, slot: usize, owner: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: owner,
        is_static: false,
    }
}

/// A `MutableCallSite` subclass declaring `MethodHandle target` (slot 2,
/// after `CallSite`'s `type` and `target`) reads and writes slot 1, the
/// real field; a plain subclass and `CallSite` itself resolve to slot 1.
#[test]
fn a_subclass_look_alike_target_does_not_hide_the_call_sites_field() {
    let mut ctx = MockNativeContext::new();
    let cs = ctx
        .ensure_class_initialized("java/lang/invoke/CallSite")
        .expect("mock class");
    let shadow = ctx
        .ensure_class_initialized("p/Indy21cShadowSite")
        .expect("mock class");
    let plain = ctx
        .ensure_class_initialized("p/Indy21cPlainSite")
        .expect("mock class");
    let mh = "Ljava/lang/invoke/MethodHandle;";
    ctx.set_declared_fields(
        cs,
        vec![
            field("type", "Ljava/lang/invoke/MethodType;", 0, cs),
            field("target", mh, 1, cs),
        ],
    );
    ctx.set_declared_fields(shadow, vec![field("target", mh, 2, shadow)]);
    ctx.set_declared_fields(plain, vec![field("other", mh, 2, plain)]);

    let shadow_site = ctx.alloc_object(shadow, 3);
    let plain_site = ctx.alloc_object(plain, 3);
    let cs_site = ctx.alloc_object(cs, 2);
    assert_eq!(
        cs_target_slot(&ctx, shadow_site),
        1,
        "the look-alike is slot 2"
    );
    assert_eq!(cs_target_slot(&ctx, cs_site), 1);
    // `plain` declares no `target`: whatever the mock answers for it, it is
    // not past `CallSite`'s own slot.
    assert!(cs_target_slot(&ctx, plain_site) <= 1);

    let (old, new) = (ctx.alloc_object(cs, 0), ctx.alloc_object(cs, 0));
    ctx.set_field(shadow_site, 1, Value::Object(Some(old)));
    cs_write_target(&mut ctx, shadow_site, Value::Object(Some(new)));
    assert_eq!(ctx.get_field(shadow_site, 1), Value::Object(Some(new)));
    assert_eq!(
        ctx.get_field(shadow_site, 2),
        Value::Int(0),
        "the look-alike is untouched"
    );
    assert_eq!(
        cs_read_target(&mut ctx, shadow_site),
        Value::Object(Some(new))
    );
}

/// No `CallSite` class known (the synthetic model's world): the derived
/// answer, exactly as before.
#[test]
fn without_a_call_site_class_the_derived_slot_stands() {
    let mut ctx = MockNativeContext::new();
    let only = ctx
        .ensure_class_initialized("p/Indy21cOnlySite")
        .expect("mock class");
    ctx.set_declared_fields(
        only,
        vec![field("target", "Ljava/lang/invoke/MethodHandle;", 2, only)],
    );
    let site = ctx.alloc_object(only, 3);
    assert_eq!(cs_target_slot(&ctx, site), 2);
}
