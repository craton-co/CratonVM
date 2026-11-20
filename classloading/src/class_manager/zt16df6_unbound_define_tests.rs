use super::*;

/// `public class <this_name> extends java/lang/Object` with no members
/// (the `tests` module's `w6f_class_bytes`).
fn class_bytes(this_name: &str) -> Vec<u8> {
    let super_name = "java/lang/Object";
    let mut b: Vec<u8> = Vec::new();
    b.extend_from_slice(&[0xCA, 0xFE, 0xBA, 0xBE]);
    b.extend_from_slice(&[0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x34]);
    b.extend_from_slice(&[0x00, 0x05]);
    b.push(7);
    b.extend_from_slice(&[0x00, 0x02]);
    b.push(1);
    b.extend_from_slice(&(this_name.len() as u16).to_be_bytes());
    b.extend_from_slice(this_name.as_bytes());
    b.push(7);
    b.extend_from_slice(&[0x00, 0x04]);
    b.push(1);
    b.extend_from_slice(&(super_name.len() as u16).to_be_bytes());
    b.extend_from_slice(super_name.as_bytes());
    b.extend_from_slice(&[0x00, 0x21]);
    b.extend_from_slice(&[0x00, 0x01]);
    b.extend_from_slice(&[0x00, 0x03]);
    b.extend_from_slice(&[0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x00]);
    b.extend_from_slice(&[0x00, 0x00]);
    b
}
/// zt t16/df6 (`docs/feature-designs/zgc-architecture.md`
/// §1, corner 2 and D6): a define with `bind_name_after_add_class` into a
/// user namespace is stored but answered by no name index until it is
/// bound, while it still holds the name against a second define
/// (HotSpot's placeholder); a withdrawn one frees the name for a fresh
/// define and stays stored, nameless; a bound one is answered everywhere.
/// A built-in namespace, and a define without the request, bind at once.
#[test]
fn zt16df6_an_unbound_define_is_named_only_once_bound_and_a_withdrawn_one_frees_the_name() {
    let mut cm = ClassManager::new(&[], &[], &[]);
    let user = ClassLoaderId::UserDefined(4646);
    let late = || DefineClassOptions {
        skip_verification: true,
        bind_name_after_add_class: true,
        ..Default::default()
    };
    let plain = || DefineClassOptions {
        skip_verification: true,
        ..Default::default()
    };
    let bytes = class_bytes("zt16df6/Late");

    // Unbound: stored, in no name index, but holding the name.
    let first = cm
        .define_class_with_options("zt16df6/Late", &bytes, user, late())
        .expect("define");
    assert!(cm.get_class(first).is_some(), "stored");
    assert_eq!(cm.class_defined_by_loader_exact("zt16df6/Late", user), None);
    assert_eq!(cm.find_class_by_name_in_loader("zt16df6/Late", user), None);
    assert!(!cm.class_store.has_live_name("zt16df6/Late"));
    assert_eq!(cm.unbound_define_of("zt16df6/Late", user), Some(first));
    assert_eq!(
        cm.unbound_define_of("", user),
        Some(first),
        "any of the loader's"
    );
    assert_eq!(
        cm.unbound_define_of("", ClassLoaderId::UserDefined(4747)),
        None
    );
    assert!(cm.is_unbound_define(first));
    let dup = cm.define_class_with_options("zt16df6/Late", &bytes, user, plain());
    assert!(
        matches!(
            dup,
            Err(VmError::Linkage(
                LinkageError::DuplicateClassDefinition { .. }
            ))
        ),
        "the unbound define holds the name: {dup:?}"
    );

    // Withdrawn: the name is free; the class stays stored, nameless.
    assert!(cm.withdraw_unbound_define(first));
    assert!(!cm.withdraw_unbound_define(first), "once");
    assert_eq!(cm.bind_unbound_define(first), None, "nothing left to bind");
    assert_eq!(cm.unbound_define_of("zt16df6/Late", user), None);
    assert!(cm.get_class(first).is_some(), "stored, nameless");
    assert_eq!(cm.class_defined_by_loader_exact("zt16df6/Late", user), None);
    let retry = cm
        .define_class_with_options("zt16df6/Late", &bytes, user, late())
        .expect("the retry defines afresh");
    assert_ne!(retry, first);

    // Bound: every name index answers it, once.
    assert_eq!(cm.bind_unbound_define(retry), Some(true));
    assert_eq!(cm.bind_unbound_define(retry), None);
    assert_eq!(
        cm.class_defined_by_loader_exact("zt16df6/Late", user),
        Some(retry)
    );
    assert_eq!(
        cm.find_class_by_name_in_loader("zt16df6/Late", user),
        Some(retry)
    );
    assert!(cm.class_store.has_live_name("zt16df6/Late"));
    assert_eq!(cm.unbound_define_of("zt16df6/Late", user), None);
    assert!(
        cm.define_class_with_options("zt16df6/Late", &bytes, user, plain())
            .is_err(),
        "bound: a duplicate"
    );

    // Only a user namespace defers.
    let app = class_bytes("zt16df6/App");
    let app_id = cm
        .define_class_with_options("zt16df6/App", &app, ClassLoaderId::Application, late())
        .expect("application define");
    assert_eq!(cm.bind_unbound_define(app_id), None, "bound at the define");
    assert_eq!(
        cm.class_defined_by_loader_exact("zt16df6/App", ClassLoaderId::Application),
        Some(app_id)
    );
    // Without the request, a user define binds at once.
    let now = class_bytes("zt16df6/Now");
    let now_id = cm
        .define_class_with_options("zt16df6/Now", &now, user, plain())
        .expect("plain define");
    assert_eq!(
        cm.class_defined_by_loader_exact("zt16df6/Now", user),
        Some(now_id)
    );
    assert_eq!(cm.bind_unbound_define(now_id), None);
}

/// zt t18/df7 (x13 review item 21a): an unbound define and a withdrawn
/// one are nameless to the whole-store enumerations
/// (`ClassStore::iter_named`: JVMTI `GetLoadedClasses`,
/// `getAllLoadedClasses`, JDWP, the heap dumper) and to the loader-blind
/// `ClassStore::find_by_name`, as HotSpot's `InstanceKlass` that is in no
/// dictionary is never listed; a bound one is listed again. `iter` (the
/// walks that must see every stored class) still yields both.
#[test]
fn zt18df7_unbound_and_withdrawn_defines_are_not_listed() {
    let mut cm = ClassManager::new(&[], &[], &[]);
    let user = ClassLoaderId::UserDefined(4848);
    let late = || DefineClassOptions {
        skip_verification: true,
        bind_name_after_add_class: true,
        ..Default::default()
    };
    let listed =
        |cm: &ClassManager, id: ClassId| cm.class_store.iter_named().any(|c| c.id == id);
    let stored = |cm: &ClassManager, id: ClassId| cm.class_store.iter().any(|c| c.id == id);

    let withdrawn = cm
        .define_class_with_options("zt18df7/Gone", &class_bytes("zt18df7/Gone"), user, late())
        .expect("define");
    assert!(cm.class_store.is_nameless(withdrawn), "unbound");
    assert!(!listed(&cm, withdrawn) && stored(&cm, withdrawn));
    assert!(cm.class_store.find_by_name("zt18df7/Gone").is_none());
    assert!(cm.withdraw_unbound_define(withdrawn));
    assert!(
        cm.class_store.is_nameless(withdrawn),
        "withdrawn: nameless for good"
    );
    assert!(!listed(&cm, withdrawn) && stored(&cm, withdrawn));
    assert!(cm.class_store.find_by_name("zt18df7/Gone").is_none());

    let bound = cm
        .define_class_with_options("zt18df7/Kept", &class_bytes("zt18df7/Kept"), user, late())
        .expect("define");
    assert!(!listed(&cm, bound));
    assert_eq!(cm.bind_unbound_define(bound), Some(true));
    assert!(!cm.class_store.is_nameless(bound));
    assert!(listed(&cm, bound), "bound: listed");
    assert_eq!(
        cm.class_store.find_by_name("zt18df7/Kept").map(|c| c.id),
        Some(bound)
    );
    // The withdrawn class is still out; nothing else changed.
    assert!(!listed(&cm, withdrawn));
    assert_eq!(
        cm.class_store.iter_named().count() + 1,
        cm.class_store.iter().count(),
        "exactly the withdrawn class is unlisted"
    );
    // Unloading the withdrawn class clears its mark with it.
    assert!(cm.class_store.remove(withdrawn).is_some());
    assert!(!cm.class_store.is_nameless(withdrawn));
}

/// zt t21/df9 (define page §9): a pending JVMS §5.3.4 constraint on the
/// name keeps the pin a withdrawn define set (HotSpot's `check_or_update`
/// ran before the failed `addClass` upcall), so the retry by the same
/// loader, and a define by the other loader of the constraint, are
/// refused with HotSpot's text for a pin to a class that never became
/// `loaded`: the new class's module clause alone, no "A different class".
/// Without a constraint the retry defines afresh (the t16 test above).
#[test]
fn zt21df9_a_withdrawn_define_keeps_its_constraint_pin() {
    let mut cm = ClassManager::new(&[], &[], &[]);
    let a = ClassLoaderId::UserDefined(4949);
    let b = ClassLoaderId::UserDefined(4950);
    let late = || DefineClassOptions {
        skip_verification: true,
        bind_name_after_add_class: true,
        ..Default::default()
    };
    let name = "zt21df9/Pend";
    let bytes = class_bytes(name);
    assert!(cm
        .loader_constraints
        .get_mut()
        .impose(name, a.to_native_id(), b.to_native_id())
        .is_none());
    let first = cm
        .define_class_with_options(name, &bytes, a, late())
        .expect("define");
    assert_eq!(
        cm.loader_constraints
            .get_mut()
            .pinned_for(name, a.to_native_id()),
        None,
        "unbound: not pinned yet"
    );
    assert!(cm.withdraw_unbound_define(first));
    assert_eq!(
        cm.loader_constraints
            .get_mut()
            .pinned_for(name, b.to_native_id()),
        Some(first.as_u32()),
        "the withdrawn define keeps the pin, for the whole group"
    );
    for (loader, label) in [(a, "same loader"), (b, "other loader")] {
        match cm.define_class_with_options(name, &bytes, loader, late()) {
            Err(VmError::Linkage(LinkageError::LoaderConstraintViolation { message })) => {
                assert!(
                    message.starts_with(&format!(
                        "loader constraint violation: loader {loader} wants to load class zt21df9.Pend. (zt21df9.Pend is in unnamed module of loader {loader}"
                    )),
                    "{label}: {message}"
                );
                assert!(!message.contains("A different"), "{label}: {message}");
            }
            other => panic!("{label}: the pin refuses the define: {other:?}"),
        }
    }
    // An unconstrained name is untouched by the withdraw.
    let free = cm
        .define_class_with_options("zt21df9/Free", &class_bytes("zt21df9/Free"), a, late())
        .expect("define");
    assert!(cm.withdraw_unbound_define(free));
    assert!(cm
        .define_class_with_options("zt21df9/Free", &class_bytes("zt21df9/Free"), a, late())
        .is_ok());
}
