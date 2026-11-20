use super::invokestatic_init_class;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, MethodAccessFlags};
use cratonvm_reader::method::ClassFileMethod;
use std::sync::Arc;

fn method(name: &str, descriptor: &str, flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        name: Arc::from(name),
        descriptor: Arc::from(descriptor),
        access_flags: flags,
        attributes: vec![],
    }
}

/// `C.m()` for a static `m` declared in `P` must initialize `P`, not `C`
/// (JVMS §5.5, JLS §12.4.1). Every shape that is not that one keeps the
/// symbolic owner, so the change cannot widen what `invokestatic` skips.
#[test]
fn an_inherited_static_initializes_its_declaring_superclass_only() {
    let shared = Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
    let (parent, child) = {
        let mut cm = shared.classes.class_manager.write();
        let parent = cm
            .try_ensure_synthetic_class("cratonvm/test/InitDeclParent", 0)
            .expect("Compatible mode fabricates");
        let child = cm
            .try_ensure_synthetic_class("cratonvm/test/InitDeclChild", 0)
            .expect("Compatible mode fabricates");
        let iface = cm
            .try_ensure_synthetic_class("cratonvm/test/InitDeclIface", 0)
            .expect("Compatible mode fabricates");
        {
            let p = cm.class_store.get_mut(parent).expect("fabricated");
            p.methods.push(method(
                "inherited",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            ));
            p.methods
                .push(method("instance", "()V", MethodAccessFlags::PUBLIC));
        }
        {
            let i = cm.class_store.get_mut(iface).expect("fabricated");
            i.access_flags |= ClassAccessFlags::INTERFACE;
            i.methods.push(method(
                "ifaceStatic",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            ));
        }
        cm.set_superclass(child, Some(parent));
        // Through `set_interfaces`, never a raw push: the store's
        // implementor index and subtype display mirror the field.
        let mut child_ifaces = cm
            .class_store
            .get(child)
            .map(|c| c.interfaces.clone())
            .unwrap_or_default();
        child_ifaces.push(iface);
        cm.set_interfaces(child, child_ifaces);
        {
            let c = cm.class_store.get_mut(child).expect("fabricated");
            c.methods.push(method(
                "own",
                "()V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC,
            ));
        }
        (parent, child)
    };

    assert_eq!(
        invokestatic_init_class(&shared, child, "inherited", "()V"),
        parent,
        "an inherited static initializes the class that declares it"
    );
    assert_eq!(
        invokestatic_init_class(&shared, child, "own", "()V"),
        child,
        "a static the symbolic owner declares itself initializes that owner"
    );
    assert_eq!(
        invokestatic_init_class(&shared, child, "instance", "()V"),
        child,
        "a non-static match is not an invokestatic target; keep the owner"
    );
    assert_eq!(
        invokestatic_init_class(&shared, child, "ifaceStatic", "()V"),
        child,
        "static interface methods are not inherited; keep the owner"
    );
    assert_eq!(
        invokestatic_init_class(&shared, child, "absent", "()V"),
        child,
        "an unresolvable method keeps the owner, and resolution reports it"
    );
}
