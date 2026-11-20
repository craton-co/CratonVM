//! gc-common w20-f: the `Arena.global()` / `MemorySegment.NULL` memo cells
//! are per VM, and the arena, segment and descriptor mints release every
//! pin they take.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// Private VM identities: no other test publishes under them.
const W20F_VM_A: usize = 0x0F20_A008;
const W20F_VM_B: usize = 0x0F20_B008;

#[test]
fn w20f_each_vm_publishes_its_own_ffm_singletons_and_forgets_them() {
    assert_eq!(claim_global_arena(W20F_VM_A, 11), None);
    assert_eq!(global_arena_handle(W20F_VM_A), Some(11));
    assert_eq!(
        global_arena_handle(W20F_VM_B),
        None,
        "a handle indexes its own VM's global-ref table; another VM must not see it"
    );
    assert_eq!(
        claim_global_arena(W20F_VM_B, 22),
        None,
        "VM B claims its own"
    );
    assert_eq!(
        claim_global_arena(W20F_VM_A, 33),
        Some(11),
        "a second claim loses"
    );
    forget_vm_ffm_singletons(W20F_VM_A);
    assert_eq!(global_arena_handle(W20F_VM_A), None);
    assert_eq!(
        global_arena_handle(W20F_VM_B),
        Some(22),
        "forgetting A keeps B"
    );

    forget_vm_ffm_singletons(W20F_VM_B);
    forget_vm_ffm_singletons(W20F_VM_B);
    assert_eq!(global_arena_handle(W20F_VM_B), None, "idempotent");
}

#[test]
fn w20f_arena_mint_and_allocate_store_the_arena_and_release_their_pins() {
    let mut ctx = mock_ctx();
    let pins_before = ctx.native_pin_count_for_test();
    let arena = pe_new_arena_carrier(&mut ctx, ffi::ARENA_CONFINED).unwrap();
    assert!(matches!(ctx.get_field(arena, 0), Value::Int(k) if k == ffi::ARENA_CONFINED));
    assert!(
        matches!(ctx.get_field(arena, 1), Value::Object(Some(_))),
        "id array stored"
    );
    assert!(matches!(ctx.get_field(arena, 2), Value::Int(0)));
    assert!(matches!(ctx.get_field(arena, 3), Value::Int(0)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "the mint releases its pin"
    );

    let seg = match pe_arena_allocate_impl(&mut ctx, arena, 16, 8) {
        Ok(Some(Value::Object(Some(seg)))) => seg,
        other => panic!("allocate gave {other:?}"),
    };
    assert_eq!(ctx.get_field(seg, 2), Value::Object(Some(arena)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "allocate releases its pin"
    );
    let _ = pe_arena_close(&mut ctx, &[Value::Object(Some(arena))]);
}

#[test]
fn w20f_function_descriptor_mint_stores_both_layouts_and_releases_its_pins() {
    let mut ctx = mock_ctx();
    let ret = ctx.fresh_object_ref();
    let params = ctx.fresh_object_ref();
    let pins_before = ctx.native_pin_count_for_test();
    let desc = match pe_new_function_descriptor(
        &mut ctx,
        Value::Object(Some(ret)),
        Value::Object(Some(params)),
    ) {
        Ok(Some(Value::Object(Some(d)))) => d,
        other => panic!("FunctionDescriptor.of gave {other:?}"),
    };
    assert_eq!(ctx.get_field(desc, 0), Value::Object(Some(ret)));
    assert_eq!(ctx.get_field(desc, 1), Value::Object(Some(params)));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);

    let void_desc =
        match pe_new_function_descriptor(&mut ctx, Value::Object(None), Value::Object(None)) {
            Ok(Some(Value::Object(Some(d)))) => d,
            other => panic!("FunctionDescriptor.ofVoid gave {other:?}"),
        };
    assert_eq!(ctx.get_field(void_desc, 0), Value::Object(None));
    assert_eq!(ctx.native_pin_count_for_test(), pins_before);
}
