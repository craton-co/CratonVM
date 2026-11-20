//! JIT round j27 wave 4, lane MHROWS2 (MHEDITOR17-1).
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

#[test]
fn a_jdk_shaped_handle_is_real_and_a_kinded_carrier_is_not() {
    let mut ctx = MockNativeContext::new();
    let class = ClassId::new(0x7281);
    // A JDK `DirectMethodHandle`'s width: far below the carrier window.
    let jdk = ctx.alloc_object(class, 6);
    assert!(mh_is_real_handle(&ctx, jdk));
    assert!(mh_is_real_jdk_handle(&ctx, jdk));
    let carrier = ctx.alloc_object(class, MH_BOUND + 1);
    ctx.set_field(carrier, MH_KIND, Value::Int(MH_KIND_STATIC));
    assert!(!mh_is_real_handle(&ctx, carrier));
    assert!(!mh_is_real_jdk_handle(&ctx, carrier));
    // A wide object whose `MH_KIND` slot holds no kind is no carrier. The
    // mock fills fresh slots with `Int(0)` -- which IS a kind
    // (`MH_KIND_STATIC`) -- so the slot is set to the VM's reference
    // default, `null`, explicitly.
    let wide = ctx.alloc_object(class, MH_BOUND + 1);
    ctx.set_field(wide, MH_KIND, Value::Object(None));
    assert!(mh_is_real_jdk_handle(&ctx, wide));
}
