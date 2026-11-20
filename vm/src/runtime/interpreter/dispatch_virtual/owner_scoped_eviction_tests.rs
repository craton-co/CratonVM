use super::virtual_native_owner_redefined;
use crate::vm::SharedVm;
use cratonvm_classloading::{ClassLoaderId, DefineClassOptions, RedefineOptions};
use cratonvm_native_api::{NativeContext, NativeKind, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ClassId, Value};

fn dummy_native(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// A version-52 class `name extends super_name` whose one method
/// `static value()I` is `ldc #8; ireturn`, `#8` being `CONSTANT_Integer
/// constant`.
fn class_bytes(name: &str, super_name: &str, constant: i32) -> Vec<u8> {
    class_file(name, super_name, constant, false)
}

/// [`class_bytes`], or with `native` the one method `public static native
/// value()I` and no `Code`.
fn class_file(name: &str, super_name: &str, constant: i32, native: bool) -> Vec<u8> {
    fn utf8(out: &mut Vec<u8>, s: &str) {
        out.push(1);
        out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&9u16.to_be_bytes()); // constant_pool_count
    utf8(&mut b, name); // #1
    b.extend_from_slice(&[7, 0, 1]); // #2 Class #1
    utf8(&mut b, super_name); // #3
    b.extend_from_slice(&[7, 0, 3]); // #4 Class #3
    utf8(&mut b, "value"); // #5
    utf8(&mut b, "()I"); // #6
    utf8(&mut b, "Code"); // #7
    b.push(3); // #8 Integer
    b.extend_from_slice(&constant.to_be_bytes());
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0]); // interfaces, fields
    b.extend_from_slice(&[0, 1]); // methods_count
    if native {
        // public static native value()I, no attributes
        b.extend_from_slice(&[0x01, 0x09, 0, 5, 0, 6, 0, 0]);
        b.extend_from_slice(&[0, 0]); // no class attributes
        return b;
    }
    b.extend_from_slice(&[0x00, 0x09, 0, 5, 0, 6, 0, 1]); // public static value()I
    let code = [0x12, 8, 0xac]; // ldc #8; ireturn
    b.extend_from_slice(&[0, 7]); // "Code"
    b.extend_from_slice(&(12 + code.len() as u32).to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]); // max_stack 1, max_locals 0
    b.extend_from_slice(&(code.len() as u32).to_be_bytes());
    b.extend_from_slice(&code);
    b.extend_from_slice(&[0, 0, 0, 0]); // no handlers, no attributes
    b.extend_from_slice(&[0, 0]); // no class attributes
    b
}

/// A version-52 class `name extends super_name` that declares NO method:
/// it inherits `value()I` from `super_name`.
fn inheriting_class_bytes(name: &str, super_name: &str) -> Vec<u8> {
    let mut b = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 52];
    b.extend_from_slice(&5u16.to_be_bytes()); // constant_pool_count
    for (s, index) in [(name, 1u8), (super_name, 3)] {
        b.push(1); // #index Utf8
        b.extend_from_slice(&u16::try_from(s.len()).unwrap_or(0).to_be_bytes());
        b.extend_from_slice(s.as_bytes());
        b.extend_from_slice(&[7, 0, index]); // #index+1 Class #index
    }
    b.extend_from_slice(&[0x00, 0x21]); // ACC_PUBLIC | ACC_SUPER
    b.extend_from_slice(&[0, 2, 0, 4]); // this_class, super_class
    b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // interfaces, fields, methods, attributes
    b
}

fn define(shared: &SharedVm, name: &str, super_name: &str) -> Option<ClassId> {
    define_bytes(shared, name, &class_bytes(name, super_name, 1))
}

fn define_bytes(shared: &SharedVm, name: &str, bytes: &[u8]) -> Option<ClassId> {
    shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            name,
            bytes,
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
        .ok()
}

fn redefine(shared: &SharedVm, cid: ClassId, name: &str, super_name: &str) -> bool {
    shared
        .classes
        .class_manager
        .write()
        .redefine_class(
            cid,
            class_bytes(name, super_name, 2),
            RedefineOptions::default(),
        )
        .is_ok()
}

/// The owner is the nearest class of the registration's name on the
/// receiver's chain; only ITS redefinition evicts, and a native whose
/// owner is not on the chain (an interface-name registration) is left to
/// the legacy tests.
#[test]
fn only_the_owners_redefinition_evicts_an_inherited_native() {
    const PARENT: &str = "r15w4walk2/OwnerParent";
    const CHILD: &str = "r15w4walk2/OwnerChild";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(parent) = define(&shared, PARENT, "java/lang/Object") else {
        // The stripped test VM could not define it: nothing to run.
        return;
    };
    let Some(child) = define(&shared, CHILD, PARENT) else {
        return;
    };
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register(PARENT, "value", "()I", dummy_native, NativeKind::Bridge);
        r.register("r15w4walk2/OwnerIface", "value", "()I", dummy_native, NativeKind::Bridge);
     }
    let (Some(on_parent), Some(on_iface)) = (
        registry.resolve_id(PARENT, "value", "()I"),
        registry.resolve_id("r15w4walk2/OwnerIface", "value", "()I"),
    ) else {
        panic!("both rows were registered");
    };
    let verdict = |id| {
        virtual_native_owner_redefined(
            &shared.classes.class_manager.read(),
            &registry,
            child,
            id,
        )
    };
    assert_eq!(verdict(on_parent), Some(false), "nothing redefined yet");
    assert_eq!(
        verdict(on_iface),
        None,
        "an owner off the chain is not named"
    );
    // The receiver's own redefinition does not touch the ancestor's native.
    if !redefine(&shared, child, CHILD, PARENT) {
        return;
    }
    assert_eq!(verdict(on_parent), Some(false));
    // The owner's does: `value()I` of a user class is in no immunity set.
    if !redefine(&shared, parent, PARENT, "java/lang/Object") {
        return;
    }
    assert_eq!(verdict(on_parent), Some(true));
    assert_eq!(verdict(on_iface), None);
}

/// A redefined owner that declares the method with NO body keeps its
/// native, at the cached hit and in the one redefine rule every door asks:
/// no agent can weave a native method.
#[test]
fn a_bodyless_owner_keeps_its_native_after_a_redefinition() {
    use super::{redefined_ancestor_guard, AncestorWalkDoor};
    use crate::runtime::interpreter::AncestorGuard;
    const PARENT: &str = "r15w4walk2/BodylessParent";
    const CHILD: &str = "r15w4walk2/BodylessChild";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let parent_bytes = |constant| class_file(PARENT, "java/lang/Object", constant, true);
    let Ok(parent) = shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            PARENT,
            &parent_bytes(1),
            ClassLoaderId::Application,
            DefineClassOptions::default(),
        )
    else {
        return;
    };
    let Some(child) = define(&shared, CHILD, PARENT) else {
        return;
    };
    let mut registry = NativeMethodRegistry::new();
    { let r = &mut registry; 
        r.register(PARENT, "value", "()I", dummy_native, NativeKind::Bridge);
     }
    let Some(id) = registry.resolve_id(PARENT, "value", "()I") else {
        panic!("the row was registered");
    };
    if shared
        .classes
        .class_manager
        .write()
        .redefine_class(parent, parent_bytes(2), RedefineOptions::default())
        .is_err()
    {
        return;
    }
    let cm = shared.classes.class_manager.read();
    assert!(cm.class_redefine_generation(parent) > 0);
    if registry.ancestor_redefine_bodyless_keep() {
        assert_eq!(
            virtual_native_owner_redefined(&cm, &registry, child, id),
            Some(false),
            "a native declaration cannot have been woven"
        );
    }
    let vm_registry = &shared.natives.native_methods;
    if vm_registry.ancestor_redefine_rule() && vm_registry.ancestor_redefine_bodyless_keep() {
        assert!(matches!(
            redefined_ancestor_guard(
                &shared,
                &cm,
                parent,
                PARENT,
                "value",
                "()I",
                AncestorWalkDoor::JitSiteCache,
                None,
            ),
            AncestorGuard::Look
        ));
    }
}

/// j27 wave 4 (lane REDEFOWNER; RD16-1): a cached `Native` / `Intrinsic`
/// hit is judged by the OWNER its fill recorded, not by the call site's
/// constant-pool class name. Residue (b): `invokestatic Child.value()I`
/// filled with `Parent`'s native (`Parent` declares it); a weave of
/// `Parent` must evict it, though the name `Child` names no redefined
/// class. Before, the hit asked `native_shadow_suppressed_by_redefine`
/// for `Child` and kept running the pre-weave native.
#[test]
fn a_cached_shadow_is_evicted_by_its_owners_weave_not_the_site_name() {
    use super::cached_shadow_owner_redefined;
    const PARENT: &str = "j27redefowner/Parent";
    const CHILD: &str = "j27redefowner/Child";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(parent) = define(&shared, PARENT, "java/lang/Object") else {
        return;
    };
    let child_bytes = inheriting_class_bytes(CHILD, PARENT);
    let Some(child) = define_bytes(&shared, CHILD, &child_bytes) else {
        return;
    };
    let evicts = |owner| cached_shadow_owner_redefined(&shared, owner, "value", "()I");
    assert!(!evicts(parent), "nothing redefined yet");
    // A redefinition of the site's class cannot weave `Parent.value`
    // (it adds no method): the entry stays. The name reading evicted it.
    if shared
        .classes
        .class_manager
        .write()
        .redefine_class(child, child_bytes, RedefineOptions::default())
        .is_err()
    {
        return;
    }
    assert!(!evicts(parent), "the child's weave leaves the parent's native");
    if !redefine(&shared, parent, PARENT, "java/lang/Object") {
        return;
    }
    assert!(evicts(parent), "the owner's weave evicts the shadow");
}


/// RD16-1 residue (a): a same-named class of ANOTHER loader being woven
/// does not evict an entry whose owner is this loader's copy (the name
/// reading resolved the name through the flat store and could pick either).
#[test]
fn a_same_named_class_of_another_loader_does_not_evict() {
    use super::cached_shadow_owner_redefined;
    const NAME: &str = "j27redefowner/Twin";
    let shared = SharedVm::new(crate::config::VmConfig::default());
    let Some(app) = define(&shared, NAME, "java/lang/Object") else {
        return;
    };
    let Ok(fork) = shared
        .classes
        .class_manager
        .write()
        .define_class_with_options(
            NAME,
            &class_bytes(NAME, "java/lang/Object", 1),
            ClassLoaderId::UserDefined(4_327),
            DefineClassOptions::default(),
        )
    else {
        return;
    };
    if fork == app || !redefine(&shared, fork, NAME, "java/lang/Object") {
        return;
    }
    assert!(cached_shadow_owner_redefined(&shared, fork, "value", "()I"));
    assert!(
        !cached_shadow_owner_redefined(&shared, app, "value", "()I"),
        "the other loader's weave is not this owner's"
    );
}
