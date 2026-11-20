use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

const INIT: VhStaticInit = VhStaticInit {
    mode: "getAndAdd",
    variable: b'I',
};

/// A row-less static carrier handle naming `p/J27S.f` (slot 0), in a
/// context with its own VM identity.
pub(super) fn setup(vm: usize) -> (MockNativeContext, ClassId, ObjectRef) {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    let s = ctx
        .ensure_class_initialized("p/J27S")
        .unwrap_or(ClassId::new(0));
    ctx.set_declared_fields(
        s,
        vec![FieldMetadata {
            name: "f".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0x0008,
            slot_index: 0,
            declaring_class_id: s,
            is_static: true,
        }],
    );
    let vh = ctx.alloc_object(ClassId::new(0), VH_FIELD_COUNT);
    let class = ctx.create_string("p/J27S");
    let field = ctx.create_string("f");
    ctx.set_field(vh, VH_KIND, Value::Int(VH_KIND_STATIC));
    ctx.set_field(vh, VH_CLASS, Value::Object(Some(class)));
    ctx.set_field(vh, VH_FIELD, Value::Object(Some(field)));
    (ctx, s, vh)
}

/// The second access is served from the memo (it no longer reads the
/// handle's names), and a generation move discards the line.
#[test]
fn a_resolved_static_slot_is_memoised_until_the_generation_moves() {
    let (mut ctx, s, vh) = setup(0x27F1_0001);
    let name = ctx.create_string("f");
    // Another test thread may move the shared generation between the two
    // calls (a legal miss), so a few rounds are allowed for one hit.
    let mut served = false;
    for _ in 0..64 {
        ctx.set_field(vh, VH_FIELD, Value::Object(Some(name)));
        let first = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(matches!(first, Ok(Some((c, 0))) if c == s), "{first:?}");
        // Unname the field: only a memo hit can still answer slot 0.
        ctx.set_field(vh, VH_FIELD, Value::Object(None));
        let hit = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
        assert!(
            matches!(hit, Ok(Some((c, 0))) if c == s) || matches!(hit, Ok(None)),
            "{hit:?}"
        );
        if matches!(hit, Ok(Some(_))) {
            served = true;
            break;
        }
    }
    assert!(served, "the memo never served the resolved slot");
    vh_meta_bump_generation();
    let miss = vh_static_slot_of(&mut ctx, vh, None, &mut [], INIT);
    assert!(matches!(miss, Ok(None)), "the stale line was served: {miss:?}");
}

