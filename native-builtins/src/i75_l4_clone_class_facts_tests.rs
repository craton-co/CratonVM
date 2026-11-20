use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::flags::with_thread_overrides;



/// `Object` <- `LinkedHashMap` <- `p/Sub` (declares `Cloneable`),
/// `Object` <- `Hashtable` <- `Properties`, `Object` <- `p/Plain`.
fn hierarchy(ctx: &mut MockNativeContext) -> [ClassId; 4] {
    let object = ctx
        .ensure_class_initialized("java/lang/Object")
        .expect("Object");
    let cloneable = ctx
        .ensure_class_initialized("java/lang/Cloneable")
        .expect("Cloneable");
    let lhm = ctx
        .ensure_class_initialized("java/util/LinkedHashMap")
        .expect("LHM");
    let sub = ctx.ensure_class_initialized("p/Sub").expect("Sub");
    let table = ctx
        .ensure_class_initialized("java/util/Hashtable")
        .expect("Hashtable");
    let props = ctx
        .ensure_class_initialized("java/util/Properties")
        .expect("Properties");
    let plain = ctx.ensure_class_initialized("p/Plain").expect("Plain");
    ctx.set_superclass(lhm, object);
    ctx.set_superclass(sub, lhm);
    ctx.set_interfaces(sub, vec![cloneable]);
    ctx.set_superclass(table, object);
    ctx.set_interfaces(table, vec![cloneable]);
    ctx.set_superclass(props, table);
    ctx.set_superclass(plain, object);
    [sub, props, plain, object]
}

#[test]
fn the_facts_are_the_walks_answers_and_are_filed_per_vm_and_epoch() {
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x7501);
    let [sub, props, plain, _] = hierarchy(&mut ctx);
    let epoch = ctx.gc_collection_count();
    for (class, cloneable, overlay) in [
        (sub, true, CloneOverlay::LinkedHashMap),
        (props, true, CloneOverlay::Properties),
        (plain, false, CloneOverlay::None),
    ] {
        let want = CloneClassFacts { cloneable, overlay };
        assert_eq!(object_class_is_cloneable(&ctx, class), cloneable);
        assert_eq!(object_clone_overlay_of(&ctx, class), overlay);
        assert_eq!(clone_class_facts_get(0x7501, epoch, class), None, "cold");
        assert_eq!(object_clone_class_facts(&ctx, class), want, "miss");
        assert_eq!(
            clone_class_facts_get(0x7501, epoch, class),
            Some(want),
            "filed"
        );
        assert_eq!(object_clone_class_facts(&ctx, class), want, "hit");
        assert_eq!(
            clone_class_facts_get(0x7502, epoch, class),
            None,
            "another VM"
        );
        assert_eq!(
            clone_class_facts_get(0x7501, epoch + 1, class),
            None,
            "another epoch"
        );
    }
}

#[test]
fn vm_identity_zero_and_an_image_without_cloneable_file_nothing() {
    let mut ctx = MockNativeContext::new();
    let [sub, ..] = hierarchy(&mut ctx);
    let epoch = ctx.gc_collection_count();
    let _ = object_clone_class_facts(&ctx, sub);
    assert_eq!(clone_class_facts_get(0, epoch, sub), None);

    let mut bare = MockNativeContext::new();
    bare.set_vm_identity(0x7503);
    let c = bare
        .ensure_class_initialized("p/NoCloneable")
        .expect("class");
    let facts = object_clone_class_facts(&bare, c);
    assert!(
        facts.cloneable,
        "the permissive answer of an image without Cloneable"
    );
    assert_eq!(
        clone_class_facts_get(0x7503, bare.gc_collection_count(), c),
        None
    );
}

#[test]
fn every_slot_is_in_range() {
    for id in [0u32, 1, 63, 64, 1000, u32::MAX] {
        assert!(clone_class_facts_slot(ClassId::new(id)) < CLONE_CLASS_FACTS_SLOTS);
    }
}
