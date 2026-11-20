use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

/// Both setters store into the slot `getTarget` reads (`cs_target_slot`),
/// and leave the old target replaced -- the retarget a Groovy call site
/// makes on every cache miss.
#[test]
fn set_call_site_target_normal_and_volatile_store_the_new_target() {
    let mut ctx = MockNativeContext::new();
    let site = ctx.alloc_object(ClassId::new(0), 1);
    let old = ctx.alloc_object(ClassId::new(0), 1);
    let new = ctx.alloc_object(ClassId::new(0), 1);
    ctx.set_field(site, 0, Value::Object(Some(old)));

    let args = [Value::Object(Some(site)), Value::Object(Some(new))];
    assert!(matches!(
        native_mhn_set_call_site_target_normal(&mut ctx, &args),
        Ok(None)
    ));
    assert_eq!(cs_read_target(&mut ctx, site), Value::Object(Some(new)));

    let args = [Value::Object(Some(site)), Value::Object(Some(old))];
    assert!(matches!(
        native_mhn_set_call_site_target_volatile(&mut ctx, &args),
        Ok(None)
    ));
    assert_eq!(cs_read_target(&mut ctx, site), Value::Object(Some(old)));
}

/// A call site argument that is not an object is the caller's bug, not a
/// silent no-op.
#[test]
fn set_call_site_target_refuses_a_non_object_site() {
    let mut ctx = MockNativeContext::new();
    let args = [Value::Int(0), Value::Object(None)];
    assert!(native_mhn_set_call_site_target_normal(&mut ctx, &args).is_err());
}
