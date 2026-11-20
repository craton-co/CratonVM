use super::*;
use crate::class::{Class, ClassId, ClassLoaderId, ClassState, ClassStore};
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

fn empty_cp() -> ConstantPool {
    ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
}

fn make_class(
    store: &mut ClassStore,
    name: &str,
    superclass: Option<ClassId>,
    flags: ClassAccessFlags,
) -> ClassId {
    let id = store.next_id();
    store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_cp(),
        access_flags: flags,
        superclass,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: crate::class_origin::ClassOrigin::VmInternal,
        signature: None,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    });
    id
}

// --- The four JVMS §5.4.4 protected disjuncts (defect 1b) ---

/// Build the cross-package `protected` fixture used by the four-disjunct
/// tests.
///
/// ```text
///   package p:  C  <-  Mid          Other
///                       ^             ^
///   package q:          D             |
///                       ^             |
///                      Sub        (extends C, unrelated to D)
/// ```
///
/// `C` declares the `protected` member; `D` (a different runtime package)
/// is the accessor. Returns `(C, Mid, D, Sub, Other)`.
fn protected_fixture(store: &mut ClassStore) -> (ClassId, ClassId, ClassId, ClassId, ClassId) {
    let c = make_class(store, "p/C", None, ClassAccessFlags::PUBLIC);
    let mid = make_class(store, "p/Mid", Some(c), ClassAccessFlags::PUBLIC);
    let d = make_class(store, "q/D", Some(mid), ClassAccessFlags::PUBLIC);
    let sub = make_class(store, "q/Sub", Some(d), ClassAccessFlags::PUBLIC);
    let other = make_class(store, "p/Other", Some(c), ClassAccessFlags::PUBLIC);
    (c, mid, d, sub, other)
}

/// Disjunct `T == C`: the receiver's static type is the **declaring**
/// class. This is what javac emits for `this.inheritedProtectedField` in a
/// cross-package subclass: `getfield p/C.f` from inside `q/D`, so
/// `T = p/C`. FAILS BEFORE THE FIX — the old single-clause implementation
/// asked only `T <: D`, and `p/C` is a *super*class of `q/D`.
#[test]
fn protected_disjunct_t_equals_declaring_class() {
    let mut store = ClassStore::new();
    let (c, _mid, d, _sub, _other) = protected_fixture(&mut store);
    let accessor = store.get(d).unwrap();
    let declaring = store.get(c).unwrap();
    let receiver = store.get(c).unwrap();
    assert!(
        check_field_access(
            accessor,
            declaring,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok(),
        "T == C: javac's `this.inheritedProtected` shape must be permitted"
    );
    assert!(
        check_method_access(
            accessor,
            declaring,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok(),
        "T == C: the method half must agree with the field half"
    );
}

/// Disjunct `T == D`: the receiver's static type is the accessor itself.
#[test]
fn protected_disjunct_t_equals_accessor() {
    let mut store = ClassStore::new();
    let (c, _mid, d, _sub, _other) = protected_fixture(&mut store);
    let accessor = store.get(d).unwrap();
    let declaring = store.get(c).unwrap();
    let receiver = store.get(d).unwrap();
    assert!(check_field_access(
        accessor,
        declaring,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_ok());
    assert!(check_method_access(
        accessor,
        declaring,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_ok());
}

/// Disjunct `D <: T`: the receiver is typed as a *superclass* of the
/// accessor that is still at or below the declaring class (`p/Mid`).
/// FAILS BEFORE THE FIX for the same reason as `T == C`.
#[test]
fn protected_disjunct_accessor_subclass_of_receiver_type() {
    let mut store = ClassStore::new();
    let (c, mid, d, _sub, _other) = protected_fixture(&mut store);
    let accessor = store.get(d).unwrap();
    let declaring = store.get(c).unwrap();
    let receiver = store.get(mid).unwrap();
    assert!(
        check_field_access(
            accessor,
            declaring,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok(),
        "D <: T: a receiver typed as an intermediate superclass is legal"
    );
    assert!(check_method_access(
        accessor,
        declaring,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_ok());
}

/// Disjunct `T <: D`: the receiver is typed as a subclass of the accessor.
/// This is the one clause the pre-fix implementation had.
#[test]
fn protected_disjunct_receiver_type_subclass_of_accessor() {
    let mut store = ClassStore::new();
    let (c, _mid, d, sub, _other) = protected_fixture(&mut store);
    let accessor = store.get(d).unwrap();
    let declaring = store.get(c).unwrap();
    let receiver = store.get(sub).unwrap();
    assert!(check_field_access(
        accessor,
        declaring,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_ok());
    assert!(check_method_access(
        accessor,
        declaring,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_ok());
}

/// CONTROL: widening to four disjuncts must not neuter the clause. A
/// *sibling* receiver — `p/Other extends p/C`, unrelated to `q/D` in both
/// directions — satisfies none of the four and is still denied. This is
/// the case the clause exists for: it stops a subclass from using its
/// inherited access to reach a sibling's protected state.
#[test]
fn protected_sibling_receiver_still_denied_after_widening() {
    let mut store = ClassStore::new();
    let (c, _mid, d, _sub, other) = protected_fixture(&mut store);
    let accessor = store.get(d).unwrap();
    let declaring = store.get(c).unwrap();
    let receiver = store.get(other).unwrap();
    assert!(check_field_access(
        accessor,
        declaring,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_err());
    assert!(check_method_access(
        accessor,
        declaring,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(receiver)
    )
    .is_err());
}

/// CONTROL: the receiver clause is only reached once clause 1 (the
/// accessor is a subclass of the declaring class) holds. A cross-package
/// non-subclass is denied no matter how the receiver is typed.
#[test]
fn protected_non_subclass_accessor_denied_regardless_of_receiver() {
    let mut store = ClassStore::new();
    let (c, _mid, _d, _sub, other) = protected_fixture(&mut store);
    let stranger = make_class(&mut store, "r/Stranger", None, ClassAccessFlags::PUBLIC);
    let accessor = store.get(stranger).unwrap();
    let declaring = store.get(c).unwrap();
    for recv in [
        None,
        Some(store.get(c).unwrap()),
        Some(store.get(other).unwrap()),
    ] {
        assert!(
            check_field_access(
                accessor,
                declaring,
                FieldAccessFlags::PROTECTED,
                &store,
                recv
            )
            .is_err(),
            "clause 1 gates the receiver clause, not the other way round"
        );
    }
}

// --- The protected receiver-subtype trap (audit 2026-07-26) ---

/// PINS A TRAP, does not assert desired behaviour.
///
/// The JVMS 5.4.4 cross-package protected rule has two clauses: the
/// accessor must be a subclass of the declaring class, AND the receiver's
/// static type must be the accessor or a subclass of it. The second clause
/// is implemented, but `receiver: None` satisfies it **vacuously** -- so a
/// caller that has not plumbed the static receiver type through gets clause
/// 1 only, and cross-package protected access that JVMS forbids is allowed.
///
/// Today nothing calls `check_field_access` at all (see the module STATUS
/// section), so this is latent rather than live. It becomes live the moment
/// someone wires the check up at `getfield`/`invokevirtual` and passes
/// `None` because the receiver type is inconvenient to thread through. This
/// test exists so that reviewer sees the divergence spelled out.
#[test]
fn protected_receiver_none_is_vacuous_not_a_check() {
    let mut store = ClassStore::new();
    // p/Declaring  <- q/Accessor (subclass, different package)
    //                 p/Sibling  (unrelated to Accessor)
    let declaring = make_class(&mut store, "p/Declaring", None, ClassAccessFlags::PUBLIC);
    let accessor = make_class(
        &mut store,
        "q/Accessor",
        Some(declaring),
        ClassAccessFlags::PUBLIC,
    );
    let sibling = make_class(
        &mut store,
        "p/Sibling",
        Some(declaring),
        ClassAccessFlags::PUBLIC,
    );

    let acc = store.get(accessor).unwrap();
    let dec = store.get(declaring).unwrap();
    let sib = store.get(sibling).unwrap();

    // With the receiver supplied, clause 2 does its job: `p/Sibling` is not
    // a subtype of `q/Accessor`, so cross-package protected access through
    // it is denied.
    assert!(
        check_field_access(acc, dec, FieldAccessFlags::PROTECTED, &store, Some(sib)).is_err(),
        "clause 2 must reject a sibling receiver"
    );

    // With `None`, the SAME access is allowed. This is the trap.
    assert!(
        check_field_access(acc, dec, FieldAccessFlags::PROTECTED, &store, None).is_ok(),
        "an omitted receiver satisfies clause 2 vacuously - a wirer that \
         passes None gets clause 1 only"
    );
}

// --- package_of ---

#[test]
fn package_of_standard_class() {
    assert_eq!(package_of("java/lang/Object"), "java/lang");
}

#[test]
fn package_of_nested() {
    assert_eq!(package_of("com/example/foo/Bar"), "com/example/foo");
}

#[test]
fn package_of_default_package() {
    assert_eq!(package_of("Foo"), "");
}

// --- same_package_name (loader-unaware string comparison) ---

#[test]
fn same_package_java_lang() {
    assert!(same_package_name("java/lang/Object", "java/lang/String"));
}

#[test]
fn different_packages() {
    assert!(!same_package_name("java/lang/Object", "java/util/List"));
}

#[test]
fn same_default_package() {
    assert!(same_package_name("Foo", "Bar"));
}

#[test]
fn default_vs_named_package() {
    assert!(!same_package_name("Foo", "com/example/Bar"));
}

// --- same_runtime_package (loader-aware, JVMS §5.3) ---

/// Build a class with an explicit defining loader for the
/// loader-aware runtime-package tests.
fn make_class_with_loader(
    store: &mut ClassStore,
    name: &str,
    loader_id: ClassLoaderId,
) -> ClassId {
    let id = store.next_id();
    store.add(Class {
        id,
        loader_id,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_cp(),
        access_flags: ClassAccessFlags::SUPER, // package-private
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: crate::class_origin::ClassOrigin::VmInternal,
        signature: None,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    });
    id
}

#[test]
fn same_loader_same_package_is_same_runtime_package() {
    let mut store = ClassStore::new();
    let a = make_class_with_loader(&mut store, "java/lang/A", ClassLoaderId::Bootstrap);
    let b = make_class_with_loader(&mut store, "java/lang/B", ClassLoaderId::Bootstrap);
    assert!(same_runtime_package(
        store.get(a).unwrap(),
        store.get(b).unwrap()
    ));
}

#[test]
fn different_loader_same_package_name_is_distinct_runtime_package() {
    // H5: a user-defined loader's `java/lang/Evil` must NOT be in the
    // same runtime package as a bootstrap-defined `java/lang/Object`.
    let mut store = ClassStore::new();
    let boot = make_class_with_loader(&mut store, "java/lang/Object", ClassLoaderId::Bootstrap);
    let evil =
        make_class_with_loader(&mut store, "java/lang/Evil", ClassLoaderId::UserDefined(7));
    assert!(!same_runtime_package(
        store.get(boot).unwrap(),
        store.get(evil).unwrap()
    ));
}

#[test]
fn spoofed_java_lang_class_cannot_reach_package_private_member() {
    // End-to-end: a user-defined-loader class named `java/lang/Evil`
    // is denied package-private field/method access to a
    // bootstrap-defined `java/lang` class.
    let mut store = ClassStore::new();
    let victim =
        make_class_with_loader(&mut store, "java/lang/Object", ClassLoaderId::Bootstrap);
    let evil =
        make_class_with_loader(&mut store, "java/lang/Evil", ClassLoaderId::UserDefined(7));
    let victim_c = store.get(victim).unwrap();
    let evil_c = store.get(evil).unwrap();

    // Package-private (no modifier) and protected non-subclass both denied.
    assert!(
        check_field_access(evil_c, victim_c, FieldAccessFlags::empty(), &store, None).is_err()
    );
    assert!(
        check_method_access(evil_c, victim_c, MethodAccessFlags::empty(), &store, None)
            .is_err()
    );
    assert!(check_class_access(evil_c, victim_c).is_err());
}

// --- check_class_access ---

#[test]
fn public_class_always_accessible() {
    let mut store = ClassStore::new();
    let accessor_id = make_class(
        &mut store,
        "com/foo/Accessor",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let target_id = make_class(
        &mut store,
        "com/bar/Target",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let accessor = store.get(accessor_id).unwrap();
    let target = store.get(target_id).unwrap();
    assert!(check_class_access(accessor, target).is_ok());
}

#[test]
fn package_private_class_same_package() {
    let mut store = ClassStore::new();
    let accessor_id = make_class(
        &mut store,
        "com/foo/Accessor",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let target_id = make_class(
        &mut store,
        "com/foo/Target",
        None,
        ClassAccessFlags::SUPER, // no PUBLIC
    );

    let accessor = store.get(accessor_id).unwrap();
    let target = store.get(target_id).unwrap();
    assert!(check_class_access(accessor, target).is_ok());
}

#[test]
fn package_private_class_different_package() {
    let mut store = ClassStore::new();
    let accessor_id = make_class(
        &mut store,
        "com/foo/Accessor",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let target_id = make_class(
        &mut store,
        "com/bar/Target",
        None,
        ClassAccessFlags::SUPER, // no PUBLIC
    );

    let accessor = store.get(accessor_id).unwrap();
    let target = store.get(target_id).unwrap();
    assert!(check_class_access(accessor, target).is_err());
}

/// i9-L2: a hidden class's `/0x<hex>` tail is not a package segment. A
/// lambda proxy stored as `p/Host$$Lambda/0x1f` is in its host's runtime
/// package `p`, so it may access `p`'s package-private classes; a
/// non-hidden class whose name merely looks like that is not rewritten.
#[test]
fn a_hidden_class_is_in_its_class_file_names_runtime_package() {
    let mut store = ClassStore::new();
    let proxy = make_class_with_loader(
        &mut store,
        "p/Host$$Lambda/0x1f",
        ClassLoaderId::Application,
    );
    let target = make_class_with_loader(&mut store, "p/PkgPrivate", ClassLoaderId::Application);
    assert!(
        check_class_access(store.get(proxy).unwrap(), store.get(target).unwrap()).is_err(),
        "control: a non-hidden `p/Host$$Lambda/0x1f` is in package `p/Host$$Lambda`"
    );
    store.get_mut(proxy).unwrap().hidden = true;
    let (proxy_c, target_c) = (store.get(proxy).unwrap(), store.get(target).unwrap());
    assert!(same_runtime_package(proxy_c, target_c));
    assert!(check_class_access(proxy_c, target_c).is_ok());
    // Another loader's `p` is still another runtime package.
    let foreign = make_class_with_loader(&mut store, "p/Other", ClassLoaderId::UserDefined(9));
    let (proxy_c, foreign_c) = (store.get(proxy).unwrap(), store.get(foreign).unwrap());
    assert!(check_class_access(proxy_c, foreign_c).is_err());
}

/// i9-L2: the denial carries HotSpot 25's `check_klass_accessibility`
/// text, split and joint forms, and leaves the loader clause out for a
/// user-defined loader rather than invent an identity hash.
#[test]
fn a_class_access_denial_carries_hotspots_message() {
    let mut store = ClassStore::new();
    let target = make_class_with_loader(
        &mut store,
        "java/util/ImmutableCollections$ListN",
        ClassLoaderId::Bootstrap,
    );
    store.get_mut(target).unwrap().module_name = Some("java.base".to_string());
    let app = make_class_with_loader(&mut store, "q/X", ClassLoaderId::Application);
    let err = check_class_access(store.get(app).unwrap(), store.get(target).unwrap())
        .expect_err("package-private, other package");
    let LinkageError::IllegalAccessError { message } = err else {
        panic!("expected IllegalAccessError, got {err:?}");
    };
    assert_eq!(
        message,
        "failed to access class java.util.ImmutableCollections$ListN from class q.X \
         (java.util.ImmutableCollections$ListN is in module java.base of loader \
         'bootstrap'; q.X is in unnamed module of loader 'app')"
    );

    let same = make_class_with_loader(&mut store, "p/Hidden", ClassLoaderId::Application);
    assert_eq!(
        class_access_denied_message(store.get(app).unwrap(), store.get(same).unwrap(), None),
        "failed to access class p.Hidden from class q.X \
         (p.Hidden and q.X are in unnamed module of loader 'app')"
    );

    let custom = make_class_with_loader(&mut store, "r/Custom", ClassLoaderId::UserDefined(3));
    assert_eq!(
        class_access_denied_message(store.get(custom).unwrap(), store.get(same).unwrap(), None),
        "failed to access class p.Hidden from class r.Custom"
    );
}

/// JIT round 17 wave 5 (lane mh5): with a reader for user-defined
/// loaders, the clause names them as HotSpot does (`<loader class> @<hash>`),
/// split form for two loaders' unnamed modules; a reader that cannot name
/// one keeps the bare text.
#[test]
fn a_class_access_denial_names_user_defined_loaders_through_the_reader() {
    let mut store = ClassStore::new();
    let target = make_class_with_loader(&mut store, "Box", ClassLoaderId::UserDefined(3));
    let caller = make_class_with_loader(&mut store, "Caller", ClassLoaderId::UserDefined(4));
    let app = make_class_with_loader(&mut store, "q/X", ClassLoaderId::Application);
    let reader = |c: &Class| match c.loader_id {
        ClassLoaderId::UserDefined(3) => Some("L$ChildFirst @1dbd16a6".to_string()),
        ClassLoaderId::UserDefined(4) => Some("'r' @7a81197d".to_string()),
        _ => None,
    };
    let (t, a) = (store.get(target).unwrap(), store.get(caller).unwrap());
    assert_eq!(
        class_access_denied_message_naming(a, t, None, &reader),
        "failed to access class Box from class Caller (Box is in unnamed module of loader \
         L$ChildFirst @1dbd16a6; Caller is in unnamed module of loader 'r' @7a81197d)"
    );
    assert_eq!(
        class_access_denied_message_naming(store.get(app).unwrap(), t, None, &reader),
        "failed to access class Box from class q.X (Box is in unnamed module of loader \
         L$ChildFirst @1dbd16a6; q.X is in unnamed module of loader 'app')"
    );
    assert_eq!(
        class_access_denied_message_naming(a, t, None, &|_| None),
        "failed to access class Box from class Caller"
    );
}

/// Interpreter round i1 wave 26 (lane L5): a refused MEMBER carries HotSpot
/// 25's `check_field_accessability` / `check_method_accessability` text —
/// the referencing class first in the joint module clause — and a
/// user-defined loader's clause is left out (probe
/// `tools/probes/interp/L5/L5W25MemberAccessProbe.java` printed the full
/// messages on HotSpot).
#[test]
fn a_member_access_denial_carries_hotspots_message() {
    let mut store = ClassStore::new();
    let accessor = make_class_with_loader(&mut store, "q/Peek", ClassLoaderId::Application);
    let holder = make_class_with_loader(&mut store, "p/Secret", ClassLoaderId::Application);
    let (a, h) = (store.get(accessor).unwrap(), store.get(holder).unwrap());
    assert_eq!(
        field_access_denied_message(
            a,
            h,
            FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC,
            "P",
            None
        ),
        "class q.Peek tried to access private field p.Secret.P \
         (q.Peek and p.Secret are in unnamed module of loader 'app')"
    );
    assert_eq!(
        field_access_denied_message(a, h, FieldAccessFlags::STATIC, "Q", None),
        "class q.Peek tried to access field p.Secret.Q \
         (q.Peek and p.Secret are in unnamed module of loader 'app')"
    );
    assert_eq!(
        method_access_denied_message(
            a,
            h,
            MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC,
            "int p.Secret.p()",
            None
        ),
        "class q.Peek tried to access private method 'int p.Secret.p()' \
         (q.Peek and p.Secret are in unnamed module of loader 'app')"
    );
    assert_eq!(
        method_access_denied_message(
            a,
            h,
            MethodAccessFlags::PROTECTED | MethodAccessFlags::ABSTRACT,
            "void p.Secret.m(int, java.lang.String)",
            None
        ),
        "class q.Peek tried to access abstract protected method \
         'void p.Secret.m(int, java.lang.String)' \
         (q.Peek and p.Secret are in unnamed module of loader 'app')"
    );

    let boot = make_class_with_loader(&mut store, "java/lang/Thing", ClassLoaderId::Bootstrap);
    store.get_mut(boot).unwrap().module_name = Some("java.base".to_string());
    assert_eq!(
        field_access_denied_message(
            store.get(accessor).unwrap(),
            store.get(boot).unwrap(),
            FieldAccessFlags::PROTECTED,
            "x",
            None
        ),
        "class q.Peek tried to access protected field java.lang.Thing.x \
         (q.Peek is in unnamed module of loader 'app'; java.lang.Thing is in \
         module java.base of loader 'bootstrap')"
    );

    let custom = make_class_with_loader(&mut store, "r/Custom", ClassLoaderId::UserDefined(3));
    assert_eq!(
        field_access_denied_message(
            store.get(custom).unwrap(),
            store.get(holder).unwrap(),
            FieldAccessFlags::PRIVATE,
            "f",
            None
        ),
        "class r.Custom tried to access private field p.Secret.f"
    );
}

/// JIT round 20 wave 15, lane huntLink: with a reader for user-defined
/// loaders, the MEMBER texts name the loader as HotSpot does, like the
/// class text above (`R20HuntLinkAccess`).
#[test]
fn a_member_access_denial_names_user_defined_loaders_through_the_reader() {
    let mut store = ClassStore::new();
    let accessor = make_class_with_loader(&mut store, "q/S", ClassLoaderId::UserDefined(3));
    let holder = make_class_with_loader(&mut store, "p/H", ClassLoaderId::UserDefined(3));
    let reader = |c: &Class| -> Option<String> {
        matches!(c.loader_id, ClassLoaderId::UserDefined(3)).then(|| "L @1dbd16a6".to_string())
    };
    let (a, h) = (store.get(accessor).unwrap(), store.get(holder).unwrap());
    assert_eq!(
        method_access_denied_message_naming(
            a,
            h,
            MethodAccessFlags::PRIVATE,
            "int p.H.m()",
            None,
            &reader
        ),
        "class q.S tried to access private method 'int p.H.m()' \
         (q.S and p.H are in unnamed module of loader L @1dbd16a6)"
    );
    assert_eq!(
        field_access_denied_message_naming(a, h, FieldAccessFlags::empty(), "f", None, &reader),
        "class q.S tried to access field p.H.f \
         (q.S and p.H are in unnamed module of loader L @1dbd16a6)"
    );
}

/// JIT round 20 wave 15, lane huntLink: HotSpot's nest-host failure texts
/// (`InstanceKlass::nest_host`) and the suffix a refused private access
/// carries (`R20HuntLinkNest`).
#[test]
fn a_failed_nest_host_claim_has_hotspots_text() {
    let mut store = ClassStore::new();
    let host = make_class_with_loader(&mut store, "n/Host", ClassLoaderId::Application);
    let listed = make_class_with_loader(&mut store, "n/Mem", ClassLoaderId::Application);
    let unlisted = make_class_with_loader(&mut store, "n/Un", ClassLoaderId::Application);
    let other = make_class_with_loader(&mut store, "m/Other", ClassLoaderId::Application);
    store.get_mut(host).unwrap().nest_members =
        vec!["n/Mem".to_string(), "m/Other".to_string()];
    for id in [listed, unlisted, other] {
        store.get_mut(id).unwrap().nest_host = Some("n/Host".to_string());
    }
    let c = |id| store.get(id).unwrap();
    let none = |_: &Class| -> Option<String> { None };
    assert_eq!(nest_host_error_text(c(listed), c(host), None, &none), None);
    assert_eq!(nest_host_error_text(c(host), c(host), None, &none), None);
    assert_eq!(
        nest_host_error_text(c(unlisted), c(host), None, &none).as_deref(),
        Some(
            "Type n.Un (loader: 'app') is not a nest member of type n.Host (loader: 'app'): \
             current type is not listed as a nest member"
        )
    );
    assert_eq!(
        nest_host_error_text(c(other), c(host), None, &none).as_deref(),
        Some(
            "Type m.Other (loader: 'app') is not a nest member of type n.Host (loader: \
             'app'): types are in different packages"
        )
    );
    assert_eq!(nest_host_error_suffix(None, None), "");
    assert_eq!(nest_host_error_suffix(Some("a"), None), ", (a)");
    assert_eq!(nest_host_error_suffix(None, Some("b")), ", (b)");
    assert_eq!(nest_host_error_suffix(Some("a"), Some("b")), ", (a, b)");
}

/// JIT round 20 wave 15, lane huntLink: an interface never reaches a
/// protected member through the subclass clause (HotSpot's
/// `Reflection::verify_member_access`), while a class in the same package
/// does (`R20HuntLinkAccess`, `q.ICImpl`).
#[test]
fn an_interface_never_uses_the_protected_clause() {
    let mut store = ClassStore::new();
    let object = make_class(
        &mut store,
        "java/lang/Object",
        None,
        ClassAccessFlags::PUBLIC,
    );
    let iface = make_class(
        &mut store,
        "q/IC",
        Some(object),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT,
    );
    let class = make_class(&mut store, "q/C", Some(object), ClassAccessFlags::PUBLIC);
    let c = |id| store.get(id).unwrap();
    let protected = MethodAccessFlags::PROTECTED;
    assert!(
        check_method_access(c(class), c(object), protected, &store, Some(c(object))).is_ok()
    );
    assert!(
        check_method_access(c(iface), c(object), protected, &store, Some(c(object))).is_err()
    );
    assert!(check_field_access(
        c(iface),
        c(object),
        FieldAccessFlags::PROTECTED,
        &store,
        None
    )
    .is_err());
}

/// Interpreter round i1 wave 48, lane L5: a boot-namespace class of a
/// module the JDK's table defines to the platform loader is named
/// `'platform'`, as HotSpot's platform loader defines it; `java.base` and
/// a boot module (`java.desktop`) stay `'bootstrap'`, and without the
/// table the text is unchanged.
#[test]
fn a_platform_module_class_is_named_loader_platform() {
    let mut store = ClassStore::new();
    let app = make_class_with_loader(&mut store, "q/X", ClassLoaderId::Application);
    let sql = make_class_with_loader(&mut store, "java/sql/Hidden", ClassLoaderId::Bootstrap);
    store.get_mut(sql).unwrap().module_name = Some("java.sql".to_string());
    let awt = make_class_with_loader(&mut store, "java/awt/Hidden", ClassLoaderId::Bootstrap);
    store.get_mut(awt).unwrap().module_name = Some("java.desktop".to_string());
    let mut registry = ModuleRegistry::new();
    let (a, s, w) = (
        store.get(app).unwrap(),
        store.get(sql).unwrap(),
        store.get(awt).unwrap(),
    );
    assert_eq!(
        class_access_denied_message(a, s, Some(&registry)),
        "failed to access class java.sql.Hidden from class q.X (java.sql.Hidden is in \
         module java.sql of loader 'bootstrap'; q.X is in unnamed module of loader 'app')"
    );
    registry.set_platform_loader_modules(["java.sql".to_string()]);
    assert_eq!(
        class_access_denied_message(a, s, Some(&registry)),
        "failed to access class java.sql.Hidden from class q.X (java.sql.Hidden is in \
         module java.sql of loader 'platform'; q.X is in unnamed module of loader 'app')"
    );
    let Err(LinkageError::IllegalAccessError { message }) =
        check_class_access_in(a, s, Some(&registry))
    else {
        panic!("expected IllegalAccessError");
    };
    assert_eq!(message, class_access_denied_message(a, s, Some(&registry)));
    assert_eq!(
        field_access_denied_message(a, w, FieldAccessFlags::PRIVATE, "f", Some(&registry)),
        "class q.X tried to access private field java.awt.Hidden.f (q.X is in unnamed \
         module of loader 'app'; java.awt.Hidden is in module java.desktop of loader \
         'bootstrap')"
    );
    assert_eq!(message_loader_of(s, None), ClassLoaderId::Bootstrap);
    assert_eq!(
        message_loader_of(s, Some(&registry)),
        ClassLoaderId::Extension
    );
}

// --- check_field_access ---

#[test]
fn public_field_always_accessible() {
    let mut store = ClassStore::new();
    let accessor_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let declaring_id = make_class(
        &mut store,
        "com/bar/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let accessor = store.get(accessor_id).unwrap();
    let declaring = store.get(declaring_id).unwrap();
    assert!(
        check_field_access(accessor, declaring, FieldAccessFlags::PUBLIC, &store, None).is_ok()
    );
}

#[test]
fn private_field_same_class() {
    let mut store = ClassStore::new();
    let class_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let class = store.get(class_id).unwrap();
    assert!(check_field_access(class, class, FieldAccessFlags::PRIVATE, &store, None).is_ok());
}

#[test]
fn private_field_different_class() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/foo/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_err());
}

#[test]
fn protected_field_subclass() {
    let mut store = ClassStore::new();
    let parent_id = make_class(
        &mut store,
        "com/foo/Parent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let child_id = make_class(
        &mut store,
        "com/bar/Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let child = store.get(child_id).unwrap();
    let parent = store.get(parent_id).unwrap();
    // Cross-package protected access through a receiver of the accessor's
    // own type (`child`) is permitted (JVMS §5.4.4 receiver-subtype clause
    // satisfied). `None` (untracked receiver) is likewise permitted.
    assert!(check_field_access(
        child,
        parent,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(child)
    )
    .is_ok());
    assert!(
        check_field_access(child, parent, FieldAccessFlags::PROTECTED, &store, None).is_ok()
    );
}

#[test]
fn protected_field_non_subclass_different_package() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/bar/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_field_access(a, b, FieldAccessFlags::PROTECTED, &store, None).is_err());
}

#[test]
fn package_private_field_same_package() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/foo/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store, None).is_ok());
}

#[test]
fn package_private_field_different_package() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/bar/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store, None).is_err());
}

// --- check_method_access ---

#[test]
fn public_method_always_accessible() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/bar/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_method_access(a, b, MethodAccessFlags::PUBLIC, &store, None).is_ok());
}

#[test]
fn private_method_same_class() {
    let mut store = ClassStore::new();
    let class_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let class = store.get(class_id).unwrap();
    assert!(
        check_method_access(class, class, MethodAccessFlags::PRIVATE, &store, None).is_ok()
    );
}

#[test]
fn private_method_different_class() {
    let mut store = ClassStore::new();
    let a_id = make_class(
        &mut store,
        "com/foo/A",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let b_id = make_class(
        &mut store,
        "com/foo/B",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store, None).is_err());
}

#[test]
fn protected_method_subclass_different_package() {
    let mut store = ClassStore::new();
    let parent_id = make_class(
        &mut store,
        "com/foo/Parent",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let child_id = make_class(
        &mut store,
        "com/bar/Child",
        Some(parent_id),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );

    let child = store.get(child_id).unwrap();
    let parent = store.get(parent_id).unwrap();
    // Receiver of the accessor's own type satisfies the §5.4.4 clause;
    // `None` (untracked) is also permitted.
    assert!(check_method_access(
        child,
        parent,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(child)
    )
    .is_ok());
    assert!(
        check_method_access(child, parent, MethodAccessFlags::PROTECTED, &store, None).is_ok()
    );
}

// --- JVMS §5.4.4 cross-package protected receiver-subtype clause ---

/// Builds the classic §5.4.4 / JLS §6.6.2 shape:
/// `p/Base` (declares the protected member) and two subclasses in a
/// *different* package `q`: `q/Sub` (the accessor C) and `q/Sibling`
/// (an unrelated subtype of Base that is NOT a subtype of Sub).
/// Returns `(store, base_id, sub_id, sibling_id, grandsub_id)` where
/// `q/GrandSub extends q/Sub`.
fn build_protected_hierarchy() -> (ClassStore, ClassId, ClassId, ClassId, ClassId) {
    let mut store = ClassStore::new();
    let base = make_class(
        &mut store,
        "p/Base",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let sub = make_class(
        &mut store,
        "q/Sub",
        Some(base),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let sibling = make_class(
        &mut store,
        "q/Sibling",
        Some(base),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let grandsub = make_class(
        &mut store,
        "q/GrandSub",
        Some(sub),
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    (store, base, sub, sibling, grandsub)
}

#[test]
fn protected_cross_pkg_receiver_self_type_ok() {
    // Receiver static type == accessor C → allowed.
    let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
    let base_c = store.get(base).unwrap();
    let sub_c = store.get(sub).unwrap();
    assert!(check_field_access(
        sub_c,
        base_c,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(sub_c)
    )
    .is_ok());
    assert!(check_method_access(
        sub_c,
        base_c,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(sub_c)
    )
    .is_ok());
}

#[test]
fn protected_cross_pkg_receiver_subtype_of_c_ok() {
    // Receiver static type is a subclass of accessor C → allowed.
    let (store, base, sub, _sibling, grand) = build_protected_hierarchy();
    let base_c = store.get(base).unwrap();
    let sub_c = store.get(sub).unwrap();
    let grand_c = store.get(grand).unwrap();
    assert!(check_field_access(
        sub_c,
        base_c,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(grand_c)
    )
    .is_ok());
}

#[test]
fn protected_cross_pkg_receiver_sibling_denied() {
    // Receiver static type is a sibling (subtype of D=Base, but NOT of
    // C=Sub) → DENIED per §5.4.4 receiver-subtype clause. This is the
    // bug being fixed: previously the subclass check alone admitted it.
    let (store, base, sub, sibling, _grand) = build_protected_hierarchy();
    let base_c = store.get(base).unwrap();
    let sub_c = store.get(sub).unwrap();
    let sibling_c = store.get(sibling).unwrap();
    assert!(check_field_access(
        sub_c,
        base_c,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(sibling_c)
    )
    .is_err());
    assert!(check_method_access(
        sub_c,
        base_c,
        MethodAccessFlags::PROTECTED,
        &store,
        Some(sibling_c)
    )
    .is_err());
}

#[test]
fn protected_cross_pkg_receiver_declaring_type_permitted_by_5_4_4() {
    // Receiver static type T is the declaring class C itself (`p/Base`),
    // accessed from D=`q/Sub` in another package. §5.4.4 PERMITS this.
    //
    // It is tempting to read the protected rule as "a subclass may not
    // reach its other-package superclass's protected member through a bare
    // superclass-typed receiver" and deny it. That conflates two different
    // checks:
    //
    //   * §5.4.4 (this function, resolution time) constrains T, the class
    //     named in the *symbolic reference* — "T is either a subclass of D,
    //     a superclass of D, or D itself". Reaching this branch already
    //     established D <: C, so T == C makes T a superclass of D and the
    //     clause is satisfied. Denying it would reject javac's ordinary
    //     `this.inheritedProtectedField`, which compiles to `getfield C.f`.
    //
    //   * §4.10.1.8 (the type checker, verification time) constrains the
    //     actual *operand-stack* type, which must be assignable to D. That
    //     is the check that stops a superclass-typed value from reaching a
    //     protected member, and it is strictly stronger. It is NOT
    //     implemented here and cannot be — this function never sees the
    //     stack. See `arch-2026-07-26/`.
    //
    // HotSpot draws the same line: `Reflection::verify_field_access` lists
    // `field_class == resolved_class` as an explicit disjunct, while
    // `ClassVerifier::verify_field_instructions` separately requires the
    // stack type to be assignable to the current class.
    let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
    let base_c = store.get(base).unwrap();
    let sub_c = store.get(sub).unwrap();
    assert!(
        check_field_access(
            sub_c,
            base_c,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(base_c)
        )
        .is_ok(),
        "T == C is a §5.4.4 disjunct (T is a superclass of D); the strict \
         receiver rule is §4.10.1.8's and operates on the operand stack"
    );
}

#[test]
fn protected_cross_pkg_untracked_receiver_allowed() {
    // No receiver tracked (None) → receiver clause N/A; the subclass
    // check governs. Preserves behavior for callers that don't model the
    // receiver type and for static members.
    let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
    let base_c = store.get(base).unwrap();
    let sub_c = store.get(sub).unwrap();
    assert!(
        check_field_access(sub_c, base_c, FieldAccessFlags::PROTECTED, &store, None).is_ok()
    );
}

#[test]
fn protected_same_pkg_ignores_receiver() {
    // Same-package protected access is granted regardless of receiver
    // type (the §5.4.4 receiver clause only applies cross-package).
    let mut store = ClassStore::new();
    let base = make_class(
        &mut store,
        "p/Base",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    // Accessor in the SAME package as the declaring class, not a subclass.
    let peer = make_class(
        &mut store,
        "p/Peer",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let unrelated = make_class(
        &mut store,
        "p/Unrelated",
        None,
        ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
    );
    let base_c = store.get(base).unwrap();
    let peer_c = store.get(peer).unwrap();
    let unrelated_c = store.get(unrelated).unwrap();
    // Even with an unrelated receiver, same-package access is allowed.
    assert!(check_field_access(
        peer_c,
        base_c,
        FieldAccessFlags::PROTECTED,
        &store,
        Some(unrelated_c)
    )
    .is_ok());
}

// --- Nest-based access control (JEP 181) ---

fn make_nest_class(
    store: &mut ClassStore,
    name: &str,
    nest_host: Option<&str>,
    nest_members: &[&str],
) -> ClassId {
    let id = store.next_id();
    store.add(Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: Arc::from(name),
        source_file: None,
        version: ClassFileVersion::JAVA_11,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_cp(),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass: None,
        interfaces: vec![],
        fields: vec![],
        methods: vec![],
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        annotations: Vec::new(),
        nest_host: nest_host.map(|s| s.to_string()),
        nest_members: nest_members.iter().map(|s| s.to_string()).collect(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin: crate::class_origin::ClassOrigin::VmInternal,
        signature: None,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    });
    id
}

#[test]
fn nestmates_same_host_can_access_private() {
    let mut store = ClassStore::new();
    // Outer is the nest host, Inner has NestHost pointing to Outer
    let _outer_id =
        make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$Inner"]);
    let inner_id = make_nest_class(
        &mut store,
        "com/foo/Outer$Inner",
        Some("com/foo/Outer"),
        &[],
    );

    let outer = store.get(_outer_id).unwrap();
    let inner = store.get(inner_id).unwrap();

    // Inner can access Outer's private fields
    assert!(check_field_access(inner, outer, FieldAccessFlags::PRIVATE, &store, None).is_ok());
    // Outer can access Inner's private methods
    assert!(
        check_method_access(outer, inner, MethodAccessFlags::PRIVATE, &store, None).is_ok()
    );
}

#[test]
fn nestmates_both_inner_classes() {
    let mut store = ClassStore::new();
    // The nest host must exist and must list both members for the
    // bidirectional confirmation (JVMS §5.4.4) to succeed.
    let _outer_id = make_nest_class(
        &mut store,
        "com/foo/Outer",
        None,
        &["com/foo/Outer$A", "com/foo/Outer$B"],
    );
    // Two inner classes with the same nest host
    let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
    let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();

    // A and B are nestmates, can access each other's private members
    assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_ok());
    assert!(check_field_access(b, a, FieldAccessFlags::PRIVATE, &store, None).is_ok());
}

#[test]
fn non_nestmates_cannot_access_private() {
    let mut store = ClassStore::new();
    let a_id = make_nest_class(
        &mut store,
        "com/foo/A",
        None, // its own host
        &[],
    );
    let b_id = make_nest_class(
        &mut store,
        "com/foo/B",
        None, // its own host (different nest)
        &[],
    );

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();

    assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_err());
    assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store, None).is_err());
}

#[test]
fn are_nestmates_same_class() {
    let mut store = ClassStore::new();
    let id = make_nest_class(&mut store, "com/foo/A", None, &[]);
    let class = store.get(id).unwrap();
    assert!(are_nestmates(class, class, &store));
}

#[test]
fn spoofed_nest_host_is_rejected() {
    let mut store = ClassStore::new();
    // Victim nest: Host explicitly lists only its legitimate member.
    let _host_id = make_nest_class(
        &mut store,
        "com/victim/Host",
        None,
        &["com/victim/Host$Member"],
    );
    let member_id = make_nest_class(
        &mut store,
        "com/victim/Host$Member",
        Some("com/victim/Host"),
        &[],
    );
    // Attacker self-declares the victim's host as its NestHost but is
    // NOT listed in Host's NestMembers.
    let attacker_id = make_nest_class(
        &mut store,
        "com/attacker/Evil",
        Some("com/victim/Host"),
        &[],
    );

    let member = store.get(member_id).unwrap();
    let attacker = store.get(attacker_id).unwrap();

    // Spoof must fail: attacker is not a confirmed nestmate of member.
    assert!(!are_nestmates(attacker, member, &store));
    assert!(
        check_field_access(attacker, member, FieldAccessFlags::PRIVATE, &store, None).is_err()
    );
    assert!(
        check_method_access(attacker, member, MethodAccessFlags::PRIVATE, &store, None)
            .is_err()
    );
}

#[test]
fn unconfirmed_nest_host_when_host_missing() {
    let mut store = ClassStore::new();
    // Two classes claim the same host, but the host class is not loaded.
    let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
    let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);
    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    // Host unresolvable → claim unconfirmed → not nestmates.
    assert!(!are_nestmates(a, b, &store));
}

// --- Hidden classes are nestmates of their declared host (defect 1a) ---

/// A JEP 371 hidden class, minted the way
/// `native-builtins/src/lookup_define.rs` mints one: a mangled
/// `"{host}/0x{n:x}"` name that no class file's `NestMembers` could ever
/// spell, plus a `nest_host` supplied by the defining `Lookup`.
fn make_hidden_nest_class(store: &mut ClassStore, name: &str, nest_host: &str) -> ClassId {
    let id = make_nest_class(store, name, Some(nest_host), &[]);
    store.get_mut(id).unwrap().hidden = true;
    id
}

/// THE LAMBDA SHAPE. FAILS BEFORE THE FIX.
///
/// `LambdaMetafactory` spins the lambda body's implementation class as a
/// hidden class in the capturing class's nest; its bytecode then does
/// `invokestatic com/foo/Host.lambda$run$0`, which is `private static`.
/// Because the host's `NestMembers` is a compile-time attribute it can
/// never list the runtime-generated hidden class, so the bidirectional
/// confirmation was unsatisfiable and this access — present in essentially
/// every modern Java program — would have thrown `IllegalAccessError` the
/// moment `check_method_access` was wired into the interpreter.
#[test]
fn hidden_class_is_nestmate_of_its_lambda_host() {
    let mut store = ClassStore::new();
    // A top-level class with a lambda has NO NestMembers attribute at all
    // (there are no compile-time nest members to list) — which is exactly
    // why the confirmation could never succeed.
    let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
    let hidden_id = make_hidden_nest_class(&mut store, "com/foo/Host/0x2a", "com/foo/Host");

    let host = store.get(host_id).unwrap();
    let hidden = store.get(hidden_id).unwrap();

    assert!(
        are_nestmates(hidden, host, &store),
        "a NESTMATE hidden class must be a nestmate of its declared host"
    );
    assert!(
        are_nestmates(host, hidden, &store),
        "nest membership is symmetric"
    );
    assert!(
        check_method_access(
            hidden,
            host,
            MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC,
            &store,
            None,
        )
        .is_ok(),
        "the lambda body's `invokestatic host.lambda$run$0` must be permitted"
    );
    assert!(
        check_field_access(hidden, host, FieldAccessFlags::PRIVATE, &store, None).is_ok(),
        "a captured private field read from the lambda body must be permitted"
    );
}

/// A hidden class defined into a *nested* host's nest (the `Lookup` was on
/// `Outer$Inner`, so `resolve_lookup_nest_host` hands back `Outer`) must be
/// a nestmate of every other member of that nest, not just of the lookup
/// class.
#[test]
fn hidden_class_joins_the_whole_nest_not_just_the_lookup_class() {
    let mut store = ClassStore::new();
    let _outer = make_nest_class(
        &mut store,
        "com/foo/Outer",
        None,
        &["com/foo/Outer$A", "com/foo/Outer$B"],
    );
    let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
    let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);
    let hidden_id = make_hidden_nest_class(&mut store, "com/foo/Outer/0x7", "com/foo/Outer");

    let a = store.get(a_id).unwrap();
    let b = store.get(b_id).unwrap();
    let hidden = store.get(hidden_id).unwrap();

    assert!(are_nestmates(hidden, a, &store));
    assert!(are_nestmates(hidden, b, &store));
    assert!(
        check_field_access(hidden, b, FieldAccessFlags::PRIVATE, &store, None).is_ok(),
        "a lambda captured in Outer$A may still touch Outer$B's private state"
    );
}

/// Interpreter round i1 wave 27 (lane L5): a nest lies within one loader.
/// A user loader's `Outer$In` that names `Outer` as its host, while the
/// `Outer` that lists it was defined by ANOTHER loader, is its own nest
/// host (HotSpot: the host resolved through the member's loader is in a
/// different run-time package), whether the pair is asked directly or
/// through the store walk (probe `L5W27NestmateEdges`).
#[test]
fn a_nest_claim_on_another_loaders_host_is_not_confirmed() {
    let mut store = ClassStore::new();
    let outer_id = make_nest_class(&mut store, "l5n2/Outer", None, &["l5n2/Outer$In"]);
    let in_id = make_nest_class(&mut store, "l5n2/Outer$In", Some("l5n2/Outer"), &[]);
    let sib_id = make_nest_class(&mut store, "l5n2/Outer$Sib", Some("l5n2/Outer"), &[]);
    store.get_mut(outer_id).unwrap().loader_id = ClassLoaderId::UserDefined(2);
    store.get_mut(in_id).unwrap().loader_id = ClassLoaderId::UserDefined(1);
    store.get_mut(sib_id).unwrap().loader_id = ClassLoaderId::UserDefined(1);
    let outer = store.get(outer_id).unwrap();
    let inner = store.get(in_id).unwrap();
    let sib = store.get(sib_id).unwrap();
    assert!(!are_nestmates(inner, outer, &store));
    assert!(!are_nestmates(outer, inner, &store));
    assert!(check_field_access(inner, outer, FieldAccessFlags::PRIVATE, &store, None).is_err());
    // Two members of loader 1 claiming a host only loader 2 defined are
    // each their own host, not nestmates through loader 2's list.
    assert!(!are_nestmates(inner, sib, &store));
}

/// Wave 27 (lane L5), the fast path's two halves: a nested class and the
/// top-level host that lists it are nestmates either way round, and a
/// host of another package is not the member's host even when it lists it.
#[test]
fn a_member_and_its_listing_host_are_nestmates_only_in_one_package() {
    let mut store = ClassStore::new();
    let outer_id = make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$In"]);
    let in_id = make_nest_class(&mut store, "com/foo/Outer$In", Some("com/foo/Outer"), &[]);
    let far_id = make_nest_class(&mut store, "com/bar/Far", None, &["com/foo/Stray"]);
    let stray_id = make_nest_class(&mut store, "com/foo/Stray", Some("com/bar/Far"), &[]);
    let outer = store.get(outer_id).unwrap();
    let inner = store.get(in_id).unwrap();
    assert!(are_nestmates(inner, outer, &store));
    assert!(are_nestmates(outer, inner, &store));
    let far = store.get(far_id).unwrap();
    let stray = store.get(stray_id).unwrap();
    assert!(!are_nestmates(stray, far, &store));
    assert!(!are_nestmates(far, stray, &store));
}

/// Wave 27 (lane L5): a NESTMATE hidden class whose defining Lookup was on
/// a NESTED class records that class; JEP 371 puts it in that class's nest,
/// hosted by the outer class, so it is a nestmate of the outer class and
/// of the lookup class alike.
#[test]
fn a_hidden_class_of_a_nested_lookup_joins_the_outer_nest() {
    let mut store = ClassStore::new();
    let outer_id = make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$In"]);
    let in_id = make_nest_class(&mut store, "com/foo/Outer$In", Some("com/foo/Outer"), &[]);
    let hidden_id =
        make_hidden_nest_class(&mut store, "com/foo/Outer$In$H/0x9", "com/foo/Outer$In");
    let outer = store.get(outer_id).unwrap();
    let inner = store.get(in_id).unwrap();
    let hidden = store.get(hidden_id).unwrap();
    assert!(are_nestmates(hidden, inner, &store));
    assert!(are_nestmates(hidden, outer, &store));
    assert!(are_nestmates(outer, hidden, &store));
}

/// CONTROL: the exemption is keyed on `Class::hidden`, nothing else. An
/// ordinary class file that claims `NestHost com/foo/Host` while the host
/// does not list it is still an unconfirmed (spoofed) claim and is still
/// denied — the identical shape to the test above minus the hidden flag.
#[test]
fn non_hidden_class_with_same_shape_is_still_denied() {
    let mut store = ClassStore::new();
    let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
    let spoof_id = make_nest_class(
        &mut store,
        "com/evil/Spoof",
        Some("com/foo/Host"),
        &[], // host does not list it back
    );
    let host = store.get(host_id).unwrap();
    let spoof = store.get(spoof_id).unwrap();
    assert!(
        !are_nestmates(spoof, host, &store),
        "only `hidden` classes are exempt from bidirectional confirmation"
    );
    assert!(
        check_method_access(spoof, host, MethodAccessFlags::PRIVATE, &store, None).is_err()
    );
}

/// CONTROL: a hidden class with no `NestHost` (a non-NESTMATE
/// `defineHiddenClass`) is its own nest host and gets no private access to
/// anyone.
#[test]
fn hidden_class_without_nest_host_is_its_own_nest() {
    let mut store = ClassStore::new();
    let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
    let lone_id = make_nest_class(&mut store, "com/foo/Host/0x3", None, &[]);
    store.get_mut(lone_id).unwrap().hidden = true;
    let host = store.get(host_id).unwrap();
    let lone = store.get(lone_id).unwrap();
    assert!(!are_nestmates(lone, host, &store));
    assert!(check_field_access(lone, host, FieldAccessFlags::PRIVATE, &store, None).is_err());
}

// --- JPMS module access: array / empty-package targets ---

use crate::module::ModuleDescriptor;

fn make_class_in_module(store: &mut ClassStore, name: &str, module: Option<&str>) -> ClassId {
    let id = make_class(store, name, None, ClassAccessFlags::PUBLIC);
    store.get_mut(id).unwrap().module_name = module.map(|m| m.to_string());
    id
}

/// A non-empty registry where `java.xml` reads `java.base` but `java.base`
/// exports nothing — so a *real* cross-module access would be denied. This
/// lets the array / empty-package exemptions be tested against a registry
/// that would otherwise reject.
fn strict_registry() -> ModuleRegistry {
    let desc = |name: &str| ModuleDescriptor {
        name: name.to_string(),
        version: None,
        is_open: false,
        requires: vec![],
        exports: vec![],
        opens: vec![],
        uses: vec![],
        provides: vec![],
        automatic: false,
        main_class: None,
    };
    let mut reg = ModuleRegistry::new();
    reg.register(desc("java.base"), vec![]);
    reg.register(desc("java.xml"), vec![]);
    reg.build_readability_graph();
    reg.add_reads("java.xml", "java.base");
    reg
}

#[test]
fn module_access_array_target_is_exempt() {
    // Regression: java.xml's `XMLSecurityManager` references `int[]`, which
    // CratonVM synthesises as `[I` under module `java.base` with an empty
    // package. The JPMS check must not deny access to an array class.
    let mut store = ClassStore::new();
    let accessor = make_class_in_module(
        &mut store,
        "jdk/xml/internal/XMLSecurityManager",
        Some("java.xml"),
    );
    let prim_arr = make_class_in_module(&mut store, "[I", Some("java.base"));
    let ref_arr = make_class_in_module(&mut store, "[Ljava/lang/Object;", Some("java.base"));
    let reg = strict_registry();
    assert!(check_module_access(
        store.get(accessor).unwrap(),
        store.get(prim_arr).unwrap(),
        &reg
    )
    .is_ok());
    assert!(check_module_access(
        store.get(accessor).unwrap(),
        store.get(ref_arr).unwrap(),
        &reg
    )
    .is_ok());
}

#[test]
fn module_access_empty_package_named_module_is_exempt() {
    // A default-(empty-)package class labelled into a named module is a
    // CratonVM artifact (e.g. a synthetic/hidden class), not a real export
    // boundary — don't deny on it.
    let mut store = ClassStore::new();
    let accessor = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    let target = make_class_in_module(&mut store, "DefaultPkgClass", Some("java.base"));
    let reg = strict_registry();
    assert!(check_module_access(
        store.get(accessor).unwrap(),
        store.get(target).unwrap(),
        &reg
    )
    .is_ok());
}

#[test]
fn module_access_real_unexported_package_still_denied() {
    // Control: a genuine cross-module access to an un-exported package of a
    // named module is still denied — the exemptions above don't neuter the
    // check for real classes.
    let mut store = ClassStore::new();
    let accessor = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    let target = make_class_in_module(&mut store, "java/lang/invoke/Hidden", Some("java.base"));
    let reg = strict_registry();
    assert!(check_module_access(
        store.get(accessor).unwrap(),
        store.get(target).unwrap(),
        &reg
    )
    .is_err());
}

// --- i11-L2: the JPMS export clause of a CONSTANT_Class resolution ---

use crate::module::{ModuleExportsEntry, ALL_UNNAMED_TARGET};

/// `java.base` exporting `java/lang` unqualified and `jdk/internal/access`
/// to `java.xml` only; `java.xml` exporting nothing; `app.cp`, a module
/// known only from a class-path jar; `java.open`, an open module.
fn export_registry() -> ModuleRegistry {
    let desc =
        |name: &str, exports: Vec<ModuleExportsEntry>, is_open: bool, automatic: bool| {
            ModuleDescriptor {
                name: name.to_string(),
                version: None,
                is_open,
                requires: vec![],
                exports,
                opens: vec![],
                uses: vec![],
                provides: vec![],
                automatic,
                main_class: None,
            }
        };
    let export = |pkg: &str, to: &[&str]| ModuleExportsEntry {
        package_name: pkg.to_string(),
        to_modules: to.iter().map(|m| m.to_string()).collect(),
        ..Default::default()
    };
    let mut reg = ModuleRegistry::new();
    reg.register(
        desc(
            "java.base",
            vec![
                export("java/lang", &[]),
                export("jdk/internal/access", &["java.xml"]),
            ],
            false,
            false,
        ),
        vec![],
    );
    reg.register(desc("java.xml", vec![], false, false), vec![]);
    reg.register(desc("app.cp", vec![], false, true), vec![]);
    reg.register(desc("java.open", vec![], true, false), vec![]);
    reg.build_readability_graph();
    reg
}

#[test]
fn the_export_clause_refuses_only_an_unexported_package_of_a_named_module() {
    let mut store = ClassStore::new();
    let unnamed = make_class_in_module(&mut store, "p/App", None);
    let class_path = make_class_in_module(&mut store, "q/Jar", Some("app.cp"));
    let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let access = make_class_in_module(
        &mut store,
        "jdk/internal/access/SharedSecrets",
        Some("java.base"),
    );
    let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
    let opened = make_class_in_module(&mut store, "o/Inside", Some("java.open"));
    let arr =
        make_class_in_module(&mut store, "[Ljdk/internal/misc/Unsafe;", Some("java.base"));
    let reg = export_registry();
    let c = |id| store.get(id).unwrap();
    let denied = |a, t| class_export_denial(c(a), c(t), &reg);

    // Unqualified export: everyone.
    for accessor in [unnamed, class_path, xml] {
        assert_eq!(denied(accessor, string), None);
    }
    // Not exported: an unnamed accessor (and a class-path-only module,
    // which HotSpot treats as unnamed) is refused but NOT named...
    for accessor in [unnamed, class_path] {
        let d = denied(accessor, unsafe_).expect("jdk.internal.misc is not exported");
        assert!(!d.accessor_named);
        assert!(
            d.message.ends_with(
                "because module java.base does not export jdk.internal.misc to unnamed module"
            ),
            "{}",
            d.message
        );
    }
    // ...and a named accessor is refused and named, with HotSpot's text.
    let d = denied(xml, unsafe_).expect("not exported to java.xml either");
    assert!(d.accessor_named);
    assert_eq!(
        d.message,
        "class jdk.xml.internal.Foo (in module java.xml) cannot access class \
         jdk.internal.misc.Unsafe (in module java.base) because module java.base \
         does not export jdk.internal.misc to module java.xml"
    );
    // A qualified export reaches its target module only.
    assert_eq!(denied(xml, access), None);
    assert!(denied(unnamed, access).is_some());
    // Same module, an open module, the unnamed module and a
    // class-path-only module as targets, and an array name: nothing to
    // refuse.
    assert_eq!(denied(string, unsafe_), None);
    assert_eq!(denied(xml, opened), None);
    assert_eq!(denied(xml, unnamed), None);
    assert_eq!(denied(xml, class_path), None);
    assert_eq!(denied(unnamed, arr), None);
    // An empty registry decides nothing.
    assert_eq!(
        class_export_denial(c(xml), c(unsafe_), &ModuleRegistry::new()),
        None
    );
}

/// Interpreter round i1 wave 26, lane L5b: a class a USER-DEFINED loader
/// defined in a JDK package is labelled with the platform module by
/// package name, but it is a member of its loader's unnamed module, so no
/// export clause applies to it — as target or as accessor. The bootstrap
/// class of the same shape is still refused (control).
#[test]
fn a_user_loaders_class_in_a_jdk_package_is_not_a_platform_module_member() {
    let mut store = ClassStore::new();
    let unnamed = make_class_in_module(&mut store, "p/App", None);
    store.get_mut(unnamed).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let spoof =
        make_class_in_module(&mut store, "jdk/internal/misc/L5Helper", Some("java.base"));
    store.get_mut(spoof).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let boot = make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    store.get_mut(boot).unwrap().loader_id = ClassLoaderId::Bootstrap;
    let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    store.get_mut(xml).unwrap().loader_id = ClassLoaderId::Bootstrap;
    let reg = export_registry();
    let c = |id| store.get(id).unwrap();
    assert_eq!(class_export_denial(c(unnamed), c(spoof), &reg), None);
    assert!(
        class_export_denial(c(unnamed), c(boot), &reg).is_some(),
        "control: the bootstrap class is java.base's"
    );
    assert!(check_module_access(c(spoof), c(xml), &reg).is_ok());
    assert!(check_module_access(c(xml), c(spoof), &reg).is_ok());
}

#[test]
fn a_dynamic_export_admits_its_target() {
    let mut store = ClassStore::new();
    let unnamed = make_class_in_module(&mut store, "p/App", None);
    let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let mut reg = export_registry();
    // `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED`: the
    // unnamed module only, not every named module.
    reg.add_exports("java.base", "jdk/internal/misc", ALL_UNNAMED_TARGET);
    let c = |id| store.get(id).unwrap();
    assert_eq!(class_export_denial(c(unnamed), c(unsafe_), &reg), None);
    assert!(class_export_denial(c(xml), c(unsafe_), &reg).is_some());
    // `Module.addExports(pn, java.xml)`.
    reg.add_exports("java.base", "jdk/internal/misc", "java.xml");
    assert_eq!(class_export_denial(c(xml), c(unsafe_), &reg), None);
}

#[test]
fn a_layer_modules_exports_are_judged_by_its_identity() {
    use crate::module::LayerExportTarget;
    let mut store = ClassStore::new();
    // Two layers, loaders 7 and 8, each with a module `m` holding `p`
    // (exported) and `q`; only loader 7's `m` exports `q`.
    let mut reg = export_registry();
    let pkgs = vec!["p".to_string(), "q".to_string()];
    reg.define_layer_module(7, "m", false, &pkgs);
    reg.define_layer_module(8, "m", false, &pkgs);
    for ns in [7, 8] {
        reg.add_layer_export(ns, "m", "p", LayerExportTarget::Everyone);
    }
    reg.add_layer_export(7, "m", "q", LayerExportTarget::Everyone);
    // `exports r to n` within loader 7's layer.
    reg.define_layer_module(7, "n", false, &["s".to_string()]);
    reg.define_layer_module(7, "k", false, &["r".to_string()]);
    reg.add_layer_export(
        7,
        "k",
        "r",
        LayerExportTarget::Layer {
            loader_ns: 7,
            name: "n".to_string(),
        },
    );
    let mk = |store: &mut ClassStore, name: &str, loader: u32| {
        let id = make_class_in_module(store, name, None);
        store.get_mut(id).unwrap().loader_id = ClassLoaderId::UserDefined(loader);
        id
    };
    let q7 = mk(&mut store, "q/Other", 7);
    let q8 = mk(&mut store, "q/Other", 8);
    let p8 = mk(&mut store, "p/Hello", 8);
    let r7 = mk(&mut store, "r/Other", 7);
    let s7 = mk(&mut store, "s/Acc", 7);
    let child = mk(&mut store, "acc/Acc", 9);
    let c = |id| store.get(id).unwrap();
    assert_eq!(class_export_denial(c(child), c(q7), &reg), None);
    let denial =
        class_export_denial(c(child), c(q8), &reg).expect("q of loader 8 is not exported");
    assert_eq!(
        denial.message,
        "class acc.Acc (in unnamed module) cannot access class q.Other (in module m) \
         because module m does not export q to unnamed module"
    );
    assert_eq!(class_export_denial(c(child), c(p8), &reg), None);
    // The module's own class and a qualified target.
    assert_eq!(class_export_denial(c(p8), c(q8), &reg), None);
    assert_eq!(class_export_denial(c(s7), c(r7), &reg), None);
    let named = class_export_denial(c(q7), c(r7), &reg).expect("r is exported to n only");
    assert!(named.accessor_named);
    assert!(class_export_denial(c(child), c(r7), &reg).is_some());
    // A loader's unnamed module as the target.
    reg.add_layer_export(7, "k", "r", LayerExportTarget::Unnamed { loader_ns: 9 });
    assert_eq!(class_export_denial(c(child), c(r7), &reg), None);
    // An open module exports every package.
    reg.define_layer_module(8, "m", true, &pkgs);
    assert_eq!(class_export_denial(c(child), c(q8), &reg), None);
}

/// Interpreter round i1 wave 45, lane L5: a layer module's class as the
/// ACCESSOR of a name-map module is in a named module, so an export to
/// `ALL-UNNAMED` does not reach it; an unqualified export and a run-time
/// export to the layer module's name do.
#[test]
fn a_layer_class_is_a_named_accessor_of_a_name_map_module() {
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["p".to_string()]);
    reg.add_exports("java.base", "jdk/internal/misc", "ALL-UNNAMED");
    let layer = make_class_in_module(&mut store, "p/Acc", None);
    store.get_mut(layer).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    // The same loader's class outside the module's packages is unnamed.
    let outside = make_class_in_module(&mut store, "o/Acc", None);
    store.get_mut(outside).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
    let access = make_class_in_module(
        &mut store,
        "jdk/internal/access/SharedSecrets",
        Some("java.base"),
    );
    let c = |id| store.get(id).unwrap();
    assert_eq!(class_export_denial(c(outside), c(unsafe_), &reg), None);
    let d = class_export_denial(c(layer), c(unsafe_), &reg).expect("ALL-UNNAMED is not m");
    assert!(d.accessor_named);
    assert_eq!(
        d.message,
        "class p.Acc (in module m) cannot access class jdk.internal.misc.Unsafe \
         (in module java.base) because module java.base does not export \
         jdk.internal.misc to module m"
    );
    assert_eq!(class_export_denial(c(layer), c(string), &reg), None);
    // A descriptor's qualified export names a module of its own
    // configuration, never a layer module.
    assert!(class_export_denial(c(layer), c(access), &reg).is_some());
    // A run-time export to the layer module's identity reaches it.
    reg.add_exports_to_layer_module("java.base", "jdk/internal/misc", "m", 7);
    assert_eq!(class_export_denial(c(layer), c(unsafe_), &reg), None);
}

/// Interpreter round i1 wave 45, lane L5: a layer module's class must
/// read the target's module (HotSpot's `MODULE_NOT_READABLE`), from the
/// reads `addReads0` recorded; no recorded read decides nothing.
#[test]
fn a_layer_class_must_read_the_module_it_names() {
    use crate::module::LayerExportTarget;
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["p".to_string()]);
    reg.define_layer_module(7, "n", false, &["q".to_string()]);
    reg.add_layer_export(7, "n", "q", LayerExportTarget::Everyone);
    let acc = make_class_in_module(&mut store, "p/Acc", None);
    store.get_mut(acc).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let other = make_class_in_module(&mut store, "q/Other", None);
    store.get_mut(other).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let xml = make_class_in_module(&mut store, "javax/xml/X", Some("java.xml"));
    let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
    reg.add_exports("java.xml", "javax/xml", "");
    let c = |id| store.get(id).unwrap();
    // Nothing recorded: permissive.
    assert_eq!(class_export_denial(c(acc), c(xml), &reg), None);
    assert_eq!(class_export_denial(c(acc), c(other), &reg), None);
    reg.add_layer_read(7, "m", LayerExportTarget::Named("java.base".to_string()));
    assert_eq!(class_export_denial(c(acc), c(string), &reg), None);
    let d = class_export_denial(c(acc), c(xml), &reg).expect("m does not read java.xml");
    assert_eq!(
        d.message,
        "class p.Acc (in module m) cannot access class javax.xml.X (in module java.xml) \
         because module m does not read module java.xml"
    );
    let d = class_export_denial(c(acc), c(other), &reg).expect("m does not read n");
    assert!(
        d.message
            .ends_with("because module m does not read module n"),
        "{}",
        d.message
    );
    reg.add_layer_read(7, "m", LayerExportTarget::Named("java.xml".to_string()));
    reg.add_layer_read(
        7,
        "m",
        LayerExportTarget::Layer {
            loader_ns: 7,
            name: "n".to_string(),
        },
    );
    assert_eq!(class_export_denial(c(acc), c(xml), &reg), None);
    assert_eq!(class_export_denial(c(acc), c(other), &reg), None);
}

/// Interpreter round i1 wave 46, lane L5: the reflective gates' one
/// module question answers a layer accessor by its module's identity
/// and everyone else from the name map.
#[test]
fn the_reflective_module_question_is_asked_by_identity() {
    use super::ReflectiveModuleQuestion::{Export, Open};
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["p".to_string()]);
    reg.add_exports("java.base", "jdk/internal/misc", "ALL-UNNAMED");
    let layer = make_class_in_module(&mut store, "p/Acc", None);
    store.get_mut(layer).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let outside = make_class_in_module(&mut store, "o/Acc", None);
    store.get_mut(outside).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let plain = make_class_in_module(&mut store, "q/Plain", None);
    let c = |id| store.get(id).unwrap();
    assert_eq!(
        reflective_module_access(c(outside), c(unsafe_), &reg, Export),
        Ok(())
    );
    assert_eq!(
        reflective_module_access(c(layer), c(unsafe_), &reg, Export),
        Err("module java.base does not \"exports jdk.internal.misc\" to module m".to_string())
    );
    assert_eq!(
        reflective_module_access(c(layer), c(unsafe_), &reg, Open),
        Err("module java.base does not \"opens jdk.internal.misc\" to module m".to_string())
    );
    // An unnamed target is open to a layer accessor.
    assert_eq!(
        reflective_module_access(c(layer), c(plain), &reg, Open),
        Ok(())
    );
    // No module registered: class-path-only mode admits everything.
    let empty = ModuleRegistry::new();
    assert_eq!(
        reflective_module_access(c(layer), c(unsafe_), &empty, Open),
        Ok(())
    );
}

/// Interpreter round i3 wave 74, lane L3: the "every accessor" screen
/// agrees with `reflective_module_access`'s `Export` answer -- an unnamed
/// target outside any layer module is reached by a name-map accessor, a
/// layer accessor and an unnamed one; a named or a layer-module target is
/// left to the full question.
#[test]
fn the_every_accessor_export_screen_agrees_with_the_full_question() {
    use super::ReflectiveModuleQuestion::Export;
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["p".to_string()]);
    let layer = make_class_in_module(&mut store, "p/Acc", None);
    store.get_mut(layer).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let plain = make_class_in_module(&mut store, "q/Plain", None);
    let named = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let c = |id| store.get(id).unwrap();
    assert!(reflective_export_reaches_every_accessor(c(plain), &reg));
    for accessor in [layer, named, unsafe_, plain] {
        assert_eq!(
            reflective_module_access(c(accessor), c(plain), &reg, Export),
            Ok(())
        );
    }
    assert!(!reflective_export_reaches_every_accessor(c(named), &reg));
    assert!(!reflective_export_reaches_every_accessor(c(unsafe_), &reg));
    // A class of a layer module is that module's, not unnamed.
    assert!(!reflective_export_reaches_every_accessor(c(layer), &reg));
    // Class-path-only mode: everything.
    let empty = ModuleRegistry::new();
    assert!(reflective_export_reaches_every_accessor(c(unsafe_), &empty));
}

/// Interpreter round i1 wave 47, lane L6: the one module question
/// (`module_access`) by identity -- `privateLookupIn`'s three.
#[test]
fn the_one_module_question_is_asked_by_identity() {
    use super::ModuleQuestion::{Export, Open, Read, Same};
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["lp".to_string()]);
    let mk = |store: &mut ClassStore, name: &str, module: Option<&str>, loader| {
        let id = make_class_in_module(store, name, module);
        store.get_mut(id).unwrap().loader_id = loader;
        id
    };
    let app = mk(&mut store, "p/App", None, ClassLoaderId::Application);
    let mate = mk(&mut store, "p/Mate", None, ClassLoaderId::Application);
    let user = mk(&mut store, "u/Other", None, ClassLoaderId::UserDefined(9));
    let string = mk(
        &mut store,
        "java/lang/String",
        Some("java.base"),
        ClassLoaderId::Bootstrap,
    );
    let unsafe_ = mk(
        &mut store,
        "jdk/internal/misc/Unsafe",
        Some("java.base"),
        ClassLoaderId::Bootstrap,
    );
    let opened = mk(
        &mut store,
        "o/Inside",
        Some("java.open"),
        ClassLoaderId::Application,
    );
    let layer = mk(&mut store, "lp/L", None, ClassLoaderId::UserDefined(7));
    let c = |id| store.get(id).unwrap();
    let ask = |reg: &ModuleRegistry, a, t, q| module_access(c(a), c(t), reg, q);

    assert_eq!(ask(&reg, app, mate, Same), Some(true));
    assert_eq!(ask(&reg, app, user, Same), Some(false));
    assert_eq!(ask(&reg, app, string, Read), Some(true));
    assert_eq!(ask(&reg, app, string, Export), Some(true));
    assert_eq!(ask(&reg, app, string, Open), Some(false));
    assert_eq!(ask(&reg, app, user, Open), Some(true));
    assert_eq!(ask(&reg, app, opened, Open), Some(true));
    // A layer module's opens are the JDK's: undecided; its exports are
    // the record's (none here).
    assert_eq!(ask(&reg, app, layer, Open), None);
    assert_eq!(ask(&reg, app, layer, Export), Some(false));
    // An opening to the application loader's own unnamed module
    // (`Instrumentation.redefineModule`) reaches its classes only.
    reg.add_opens(
        "java.base",
        "java/lang",
        &crate::module::unnamed_module_of_loader_target(
            ClassLoaderId::Application.to_native_id(),
        ),
    );
    assert_eq!(ask(&reg, app, string, Open), Some(true));
    assert_eq!(ask(&reg, user, string, Open), Some(false));
    // `ALL-UNNAMED` reaches every unnamed module.
    reg.add_opens("java.base", "jdk/internal/misc", "ALL-UNNAMED");
    assert_eq!(ask(&reg, user, unsafe_, Open), Some(true));
    // No module registered: nothing to answer from.
    let empty = ModuleRegistry::new();
    assert_eq!(ask(&empty, app, string, Open), None);
}

/// Interpreter round i1 wave 49, lane L6: the unqualified questions
/// (`Module.isExported(pn)` / `isOpen(pn)`) about ONE class's module, by
/// identity -- not answered `true` by the same-module shortcut, and a
/// layer module's qualified-only export is not "exported".
#[test]
fn the_unqualified_export_question_is_asked_by_identity() {
    use super::ModuleQuestion::{ExportAll, OpenAll};
    use crate::module::LayerExportTarget;
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["lp".to_string(), "lq".to_string()]);
    reg.add_layer_export(7, "m", "lp", LayerExportTarget::Everyone);
    reg.add_layer_export(7, "m", "lq", LayerExportTarget::Named("friend".to_string()));
    let mk = |store: &mut ClassStore, name: &str, module: Option<&str>, loader| {
        let id = make_class_in_module(store, name, module);
        store.get_mut(id).unwrap().loader_id = loader;
        id
    };
    let app = mk(&mut store, "p/App", None, ClassLoaderId::Application);
    let string = mk(
        &mut store,
        "java/lang/String",
        Some("java.base"),
        ClassLoaderId::Bootstrap,
    );
    let unsafe_ = mk(
        &mut store,
        "jdk/internal/misc/Unsafe",
        Some("java.base"),
        ClassLoaderId::Bootstrap,
    );
    let exported = mk(&mut store, "lp/L", None, ClassLoaderId::UserDefined(7));
    let qualified = mk(&mut store, "lq/Q", None, ClassLoaderId::UserDefined(7));
    let c = |id| store.get(id).unwrap();
    let ask = |reg: &ModuleRegistry, t, q| module_access(c(t), c(t), reg, q);

    assert_eq!(ask(&reg, app, ExportAll), Some(true));
    assert_eq!(ask(&reg, app, OpenAll), Some(true));
    assert_eq!(ask(&reg, string, ExportAll), Some(true));
    assert_eq!(ask(&reg, string, OpenAll), Some(false));
    assert_eq!(ask(&reg, unsafe_, ExportAll), Some(false));
    assert_eq!(ask(&reg, exported, ExportAll), Some(true));
    assert_eq!(ask(&reg, qualified, ExportAll), Some(false));
    // A layer module's opens are the JDK's.
    assert_eq!(ask(&reg, qualified, OpenAll), None);
    // The accessor is ignored.
    assert_eq!(
        module_access(c(app), c(qualified), &reg, ExportAll),
        Some(false)
    );
    let empty = ModuleRegistry::new();
    assert_eq!(ask(&empty, string, ExportAll), None);
}

/// Interpreter round i45x, lane L5: the truth table of
/// [`module_access`] -- readability x exports x opens, unqualified and
/// qualified, for unnamed, name-map and layer modules on either side --
/// pinned in one place, with the answers the JDK's `Module.canRead` /
/// `implIsExportedOrOpen` give (an `opens` is an export too; an open
/// module opens everything; a layer module's `opens` the registry holds
/// no record of answer `None`).
#[test]
fn i45x_module_access_truth_table() {
    use super::ModuleQuestion::{Export, Open, Read, Same};
    use crate::module::LayerExportTarget;
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    // java.base: `opens java.util to java.xml`, `opens java.io`.
    reg.add_opens("java.base", "java/util", "java.xml");
    reg.add_opens("java.base", "java/io", "");
    let pkgs = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    // Layer 7: m {lp: to all, lq: to `friend`, lr: to n}, n {ns}, and the
    // open module ol {op}.
    reg.define_layer_module(7, "m", false, &pkgs(&["lp", "lq", "lr"]));
    reg.define_layer_module(7, "n", false, &pkgs(&["ns"]));
    reg.define_layer_module(7, "ol", true, &pkgs(&["op"]));
    reg.add_layer_export(7, "m", "lp", LayerExportTarget::Everyone);
    reg.add_layer_export(7, "m", "lq", LayerExportTarget::Named("friend".to_string()));
    reg.add_layer_export(
        7,
        "m",
        "lr",
        LayerExportTarget::Layer {
            loader_ns: 7,
            name: "n".to_string(),
        },
    );
    let mk = |store: &mut ClassStore, name: &str, module: Option<&str>, loader| {
        let id = make_class_in_module(store, name, module);
        store.get_mut(id).unwrap().loader_id = loader;
        id
    };
    let boot = ClassLoaderId::Bootstrap;
    let app = mk(&mut store, "p/App", None, ClassLoaderId::Application);
    let app2 = mk(&mut store, "p/App2", None, ClassLoaderId::Application);
    let user = mk(&mut store, "u/Other", None, ClassLoaderId::UserDefined(9));
    let xml = mk(&mut store, "jdk/xml/internal/Foo", Some("java.xml"), boot);
    let lang = mk(&mut store, "java/lang/String", Some("java.base"), boot);
    let access = mk(
        &mut store,
        "jdk/internal/access/SharedSecrets",
        Some("java.base"),
        boot,
    );
    let util = mk(&mut store, "java/util/List", Some("java.base"), boot);
    let io = mk(&mut store, "java/io/File", Some("java.base"), boot);
    let misc = mk(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"), boot);
    let jopen = mk(
        &mut store,
        "o/Inside",
        Some("java.open"),
        ClassLoaderId::Application,
    );
    let user_loader = ClassLoaderId::UserDefined(7);
    let lp = mk(&mut store, "lp/L", None, user_loader);
    let lq = mk(&mut store, "lq/Q", None, user_loader);
    let lr = mk(&mut store, "lr/R", None, user_loader);
    let ns = mk(&mut store, "ns/N", None, user_loader);
    let op = mk(&mut store, "op/O", None, user_loader);
    let c = |id| store.get(id).unwrap();

    // (label, accessor, target, question, answer)
    type Row = (&'static str, ClassId, ClassId, super::ModuleQuestion, Option<bool>);
    let table: Vec<Row> = vec![
        // --- Same
        ("same unnamed", app, app2, Same, Some(true)),
        ("same, other loader", app, user, Same, Some(false)),
        ("same named", lang, io, Same, Some(true)),
        ("same, named vs unnamed", lang, app, Same, Some(false)),
        ("same layer module", lp, lq, Same, Some(true)),
        ("same, two layer modules", lp, ns, Same, Some(false)),
        // --- Read
        ("unnamed reads named", app, xml, Read, Some(true)),
        ("unnamed reads unnamed", app, user, Read, Some(true)),
        ("named reads java.base", xml, lang, Read, Some(true)),
        ("named does not read another named", xml, jopen, Read, Some(false)),
        ("named does not read unnamed", xml, app, Read, Some(false)),
        ("name-map reader, layer provider", xml, lp, Read, None),
        ("layer, nothing recorded", lp, xml, Read, Some(true)),
        // --- Export: unnamed accessor
        ("unnamed -> unqualified export", app, lang, Export, Some(true)),
        ("unnamed -> qualified export", app, access, Export, Some(false)),
        ("unnamed -> unexported", app, misc, Export, Some(false)),
        ("unnamed -> qualified open", app, util, Export, Some(false)),
        ("unnamed -> unqualified open", app, io, Export, Some(true)),
        // --- Export: name-map accessor
        ("named -> qualified export to it", xml, access, Export, Some(true)),
        ("named -> qualified open to it", xml, util, Export, Some(true)),
        ("named -> unexported", xml, misc, Export, Some(false)),
        // --- Open
        ("open: unqualified export only", app, lang, Open, Some(false)),
        ("open: qualified open, other module", app, util, Open, Some(false)),
        ("open: unqualified open", app, io, Open, Some(true)),
        ("open: qualified open to it", xml, util, Open, Some(true)),
        ("open: qualified export only", xml, access, Open, Some(false)),
        ("open: unnamed target", lang, app, Open, Some(true)),
        ("export: unnamed target", lang, app, Export, Some(true)),
        // --- Layer target
        ("layer: unqualified export", app, lp, Export, Some(true)),
        ("layer: export to a name-map `friend`", xml, lq, Export, Some(false)),
        ("layer: export to layer n, asked by n", ns, lr, Export, Some(true)),
        ("layer: export to layer n, asked by unnamed", app, lr, Export, Some(false)),
        ("layer: opens are the JDK's", app, lp, Open, None),
        ("layer: own module", lq, lp, Export, Some(true)),
        ("layer: open module exports", app, op, Export, Some(true)),
        ("layer: open module opens", app, op, Open, Some(true)),
        ("layer accessor -> unnamed target", lp, app, Open, Some(true)),
        ("layer accessor -> unqualified open", lp, io, Open, Some(true)),
        ("layer accessor -> qualified open (to java.xml)", lp, util, Open, Some(false)),
        ("layer accessor -> unexported", lp, misc, Export, Some(false)),
    ];
    let mut wrong = Vec::new();
    for (label, a, t, q, want) in &table {
        let got = module_access(c(*a), c(*t), &reg, *q);
        if got != *want {
            wrong.push(format!("{label}: {q:?} want {want:?} got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Interpreter round i45x, lane L5: the three message sources spell
/// HotSpot's and the JDK's texts, whichever module kind is named.
#[test]
fn i45x_module_messages_have_one_source() {
    use super::ReflectiveModuleQuestion::{Export, Open};
    let mut store = ClassStore::new();
    let app = make_class_in_module(&mut store, "p/App", None);
    let unsafe_ =
        make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
    let c = |id| store.get(id).unwrap();
    assert_eq!(
        type_not_exported_message(
            c(app),
            ModuleIdent::Unnamed { loader_ns: 0 },
            c(unsafe_),
            "java.base",
            "jdk/internal/misc"
        ),
        "class p.App (in unnamed module) cannot access class jdk.internal.misc.Unsafe \
         (in module java.base) because module java.base does not export \
         jdk.internal.misc to unnamed module"
    );
    assert_eq!(
        type_not_exported_message(
            c(app),
            ModuleIdent::Named("m"),
            c(unsafe_),
            "java.base",
            "jdk/internal/misc"
        ),
        "class p.App (in module m) cannot access class jdk.internal.misc.Unsafe \
         (in module java.base) because module java.base does not export \
         jdk.internal.misc to module m"
    );
    assert_eq!(
        module_not_readable_message(c(app), "m", c(unsafe_), "module java.xml"),
        "class p.App (in module m) cannot access class jdk.internal.misc.Unsafe \
         (in module java.xml) because module m does not read module java.xml"
    );
    assert_eq!(
        module_not_readable_message(c(app), "m", c(unsafe_), "unnamed module"),
        "class p.App (in module m) cannot access class jdk.internal.misc.Unsafe \
         (in unnamed module) because module m does not read unnamed module"
    );
    assert_eq!(
        reflective_refusal_message(Open, "java.base", "java/lang", "unnamed module"),
        "module java.base does not \"opens java.lang\" to unnamed module"
    );
    assert_eq!(
        reflective_refusal_message(Export, "java.base", "jdk/internal/misc", "module m"),
        "module java.base does not \"exports jdk.internal.misc\" to module m"
    );
    // The name-map reflective gate (no layer module involved) speaks the
    // same words, for a named accessor as for an unnamed one.
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["lp".to_string()]);
    let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
    let c = |id| store.get(id).unwrap();
    assert_eq!(
        reflective_module_access(c(xml), c(unsafe_), &reg, Export),
        Err("module java.base does not \"exports jdk.internal.misc\" to module java.xml"
            .to_string())
    );
    assert_eq!(
        reflective_module_access(c(xml), c(unsafe_), &reg, Open),
        Err("module java.base does not \"opens jdk.internal.misc\" to module java.xml"
            .to_string())
    );
    assert_eq!(
        reflective_module_access(c(app), c(unsafe_), &reg, Open),
        Err("module java.base does not \"opens jdk.internal.misc\" to unnamed module"
            .to_string())
    );
}

/// Interpreter round i1 wave 46, lane L5: a layer module's class reads
/// an UNNAMED module only through an `ALL-UNNAMED` edge or that loader's
/// unnamed module; a JDK name in an unnamed module is not asked.
#[test]
fn a_layer_class_must_read_the_unnamed_module_it_names() {
    use crate::module::LayerExportTarget;
    let mut store = ClassStore::new();
    let mut reg = export_registry();
    reg.define_layer_module(7, "m", false, &["p".to_string()]);
    let acc = make_class_in_module(&mut store, "p/Acc", None);
    store.get_mut(acc).unwrap().loader_id = ClassLoaderId::UserDefined(7);
    let app = make_class_in_module(&mut store, "Target", None);
    store.get_mut(app).unwrap().loader_id = ClassLoaderId::Application;
    let user = make_class_in_module(&mut store, "u/Other", None);
    store.get_mut(user).unwrap().loader_id = ClassLoaderId::UserDefined(9);
    let jdk = make_class_in_module(&mut store, "jdk/internal/Gen", None);
    store.get_mut(jdk).unwrap().loader_id = ClassLoaderId::Application;
    let c = |id| store.get(id).unwrap();
    // Nothing recorded: permissive.
    assert_eq!(class_export_denial(c(acc), c(app), &reg), None);
    reg.add_layer_read(7, "m", LayerExportTarget::Named("java.base".to_string()));
    let d = class_export_denial(c(acc), c(app), &reg).expect("m does not read unnamed");
    assert_eq!(
        d.message,
        "class p.Acc (in module m) cannot access class Target (in unnamed module) \
         because module m does not read unnamed module"
    );
    assert!(class_export_denial(c(acc), c(user), &reg).is_some());
    assert_eq!(class_export_denial(c(acc), c(jdk), &reg), None);
    // One loader's unnamed module.
    reg.add_layer_read(7, "m", LayerExportTarget::Unnamed { loader_ns: 2 });
    assert_eq!(class_export_denial(c(acc), c(app), &reg), None);
    assert!(class_export_denial(c(acc), c(user), &reg).is_some());
    // Every unnamed module.
    reg.add_layer_read(7, "m", LayerExportTarget::AllUnnamed);
    assert_eq!(class_export_denial(c(acc), c(user), &reg), None);
}
