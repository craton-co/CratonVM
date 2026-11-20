use super::*;
use crate::classloading::{ClassLoaderId, ClassState};
use cratonvm_classloading::ClassOrigin;
use cratonvm_reader::class_access_flags::ClassAccessFlags;
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use std::sync::Arc;

/// Interpreter round i7 wave 136, lane L6: HotSpot's external name of an
/// array class keeps a hidden element's `/0x<hex>` suffix.
#[test]
fn i7w136_external_array_descriptor_keeps_a_hidden_elements_suffix() {
    assert_eq!(external_array_descriptor("[I"), "[I");
    assert_eq!(external_array_descriptor("[Ljava/lang/String;"), "[Ljava.lang.String;");
    assert_eq!(
        external_array_descriptor("[[Lp/Foo/0x00000000a1b2c3d4;"),
        "[[Lp.Foo/0x00000000a1b2c3d4;"
    );
    assert_eq!(external_array_descriptor("[LFoo/0x0123;"), "[LFoo/0x0123;");
    // Not a hex suffix: every slash is a dot.
    assert_eq!(external_array_descriptor("[La/0xfeed/B;"), "[La.0xfeed.B;");
    assert_eq!(external_array_descriptor("[La/0xzz;"), "[La.0xzz;");
}

fn method(name: &str, flags: MethodAccessFlags) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: flags,
        name: Arc::from(name),
        descriptor: Arc::from("()V"),
        attributes: vec![],
    }
}

fn real() -> ClassOrigin {
    ClassOrigin::ApplicationClassPath {
        source: Arc::from("test"),
    }
}

fn add(
    store: &mut ClassStore,
    name: &str,
    flags: ClassAccessFlags,
    superclass: Option<ClassId>,
    interfaces: Vec<ClassId>,
    methods: Vec<ClassFileMethod>,
    origin: ClassOrigin,
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
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: flags,
        superclass,
        interfaces,
        fields: vec![],
        methods,
        first_field_index: 0,
        num_total_fields: 0,
        bootstrap_methods: vec![],
        signature: None,
        annotations: Vec::new(),
        nest_host: None,
        nest_members: Vec::new(),
        record_components: Vec::new(),
        permitted_subclasses: Vec::new(),
        inner_classes: Vec::new(),
        enclosing_method: None,
        hidden: false,
        module_name: None,
        origin,
        has_finalizer: false,
        code_source: None,
        array_info: None,
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
        init_state: Arc::new(std::sync::atomic::AtomicU8::new(0)),
    })
}

const PUB: MethodAccessFlags = MethodAccessFlags::PUBLIC;
const CLS: ClassAccessFlags = ClassAccessFlags::PUBLIC;

fn iface_flags() -> ClassAccessFlags {
    ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT
}

fn object(store: &mut ClassStore) -> ClassId {
    add(store, "java/lang/Object", CLS, None, vec![], vec![], real())
}

/// Divergence 1: a private (or static) method in a subclass does not hide
/// the inherited instance method.
#[test]
fn private_or_static_subclass_method_is_not_an_override() {
    for flags in [MethodAccessFlags::PRIVATE, PUB | MethodAccessFlags::STATIC] {
        let mut s = ClassStore::new();
        let obj = object(&mut s);
        let a = add(
            &mut s,
            "p/A",
            CLS,
            Some(obj),
            vec![],
            vec![method("m", PUB)],
            real(),
        );
        let b = add(
            &mut s,
            "p/B",
            CLS,
            Some(a),
            vec![],
            vec![method("m", flags)],
            real(),
        );
        assert_eq!(
            select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Selected(a)
        );
    }
}

/// The per-thread `select` memo is keyed by the store's identity, not its
/// address: two stores built one after the other in the same stack slot,
/// numbering the same class names from 0, must each get their own answer.
/// It also answers repeat questions exactly as the walk does.
#[test]
fn select_memo_keeps_same_shaped_stores_apart_and_agrees_with_the_walk() {
    for (flags, overrides) in [
        (PUB, true),
        (MethodAccessFlags::PRIVATE, false),
        (PUB, true),
    ] {
        let mut s = ClassStore::new();
        let obj = object(&mut s);
        let a = add(
            &mut s,
            "p/A",
            CLS,
            Some(obj),
            vec![],
            vec![method("m", PUB)],
            real(),
        );
        let b = add(
            &mut s,
            "p/B",
            CLS,
            Some(a),
            vec![],
            vec![method("m", flags)],
            real(),
        );
        let want = Selection::Selected(if overrides { b } else { a });
        for _ in 0..2 {
            assert_eq!(select(&s, b, ResolvedRef::Declared(a), "m", "()V"), want);
            assert_eq!(
                select_uncached(&s, b, ResolvedRef::Declared(a), "m", "()V"),
                want
            );
        }
        // A different resolved reference is a different key.
        assert_eq!(
            select(&s, a, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Selected(a)
        );
    }
}

/// `refine_lenient_selection` (the `invoke_on_class_shared` retarget):
/// a strict `Selected` replaces the lenient walk's private hit; an
/// `AbstractMethod` answer keeps the lenient one.
#[test]
fn refine_lenient_selection_takes_only_strict_selected_answers() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let abs = ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT;
    let a = add(
        &mut s,
        "p/A",
        abs,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", MethodAccessFlags::PRIVATE)],
        real(),
    );
    let lenient = crate::classloading::find_method_recursive(b, "m", "()V", &s);
    assert_eq!(
        lenient.map(|(_, d)| d),
        Some(b),
        "the walk stops at the private m"
    );
    let refined = refine_lenient_selection(&s, b, a, "m", "()V", lenient);
    assert_eq!(refined.map(|(_, d)| d), Some(a));

    // Abstract re-declaration: §5.4.6 says AME, the door keeps its walk.
    let c = add(
        &mut s,
        "p/C",
        abs,
        Some(a),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let d = add(&mut s, "p/D", CLS, Some(c), vec![], vec![], real());
    let lenient = crate::classloading::find_method_recursive(d, "m", "()V", &s);
    let refined = refine_lenient_selection(&s, d, a, "m", "()V", lenient);
    assert_eq!(refined.map(|(_, d)| d), lenient.map(|(_, d)| d));
}

/// `refine_lenient_selection_for`, the form `execute_invoke_kind`'s
/// receiver-class fallback uses (no retarget: the door is handed the
/// receiver's own class and the resolved reference): a private
/// same-signature method in the receiver no longer hides the resolved
/// public one, and a walk that lands on the resolved declaration itself is
/// accepted as is.
#[test]
fn refine_for_a_receiver_class_dispatch_skips_a_private_hider() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", MethodAccessFlags::PRIVATE)],
        real(),
    );
    let resolved = ResolvedRef::Declared(a);
    let lenient = crate::classloading::find_method_recursive(b, "m", "()V", &s);
    assert_eq!(lenient.map(|(_, d)| d), Some(b));
    let refined = refine_lenient_selection_for(&s, b, resolved, "m", "()V", lenient);
    assert_eq!(refined.map(|(_, d)| d), Some(a));

    let c = add(&mut s, "p/C", CLS, Some(a), vec![], vec![], real());
    let lenient = crate::classloading::find_method_recursive(c, "m", "()V", &s);
    assert_eq!(lenient.map(|(_, d)| d), Some(a));
    let refined = refine_lenient_selection_for(&s, c, resolved, "m", "()V", lenient);
    assert_eq!(refined.map(|(_, d)| d), Some(a));
}

/// The JIT call-site form (wave 8): `walk_selection_moves_to` names the
/// selected class only where the receiver walk would run another body —
/// a private hider, a cross-package package-private "override" — and
/// `select_for_receiver_dispatch` answers with that class's method.
#[test]
fn jit_receiver_dispatch_moves_only_where_the_walk_is_not_the_selection() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let hider = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", MethodAccessFlags::PRIVATE)],
        real(),
    );
    let overrider = add(
        &mut s,
        "p/E",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let inheritor = add(&mut s, "p/F", CLS, Some(a), vec![], vec![], real());
    assert_eq!(
        receiver_dispatch_moves_to(&s, hider, a, "m", "()V"),
        Some(a)
    );
    assert_eq!(
        select_for_receiver_dispatch(&s, hider, Some(a), "m", "()V").map(|(_, d)| d),
        Some(a)
    );
    // No owner: the walk, unchanged.
    assert_eq!(
        select_for_receiver_dispatch(&s, hider, None, "m", "()V").map(|(_, d)| d),
        Some(hider)
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, overrider, a, "m", "()V"),
        None
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, inheritor, a, "m", "()V"),
        None
    );
    assert_eq!(receiver_dispatch_moves_to(&s, a, a, "m", "()V"), None);

    // A package-private resolved method is not overridden from another
    // package, whatever the overrider's own access.
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p1/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", MethodAccessFlags::empty())],
        real(),
    );
    let elsewhere = add(
        &mut s,
        "p2/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let same_package = add(
        &mut s,
        "p1/C",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, elsewhere, a, "m", "()V"),
        Some(a)
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, same_package, a, "m", "()V"),
        None
    );

    // A compatibility class in the hierarchy keeps the walk.
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let stub_hider = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", MethodAccessFlags::PRIVATE)],
        ClassOrigin::compatibility_stub("test"),
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, stub_hider, a, "m", "()V"),
        None
    );
}

/// `strict_selection_error` (the `--real-jdk` half of the
/// `invoke_on_class_shared` retarget): the abstract re-declaration the
/// lenient walk steps past is an `AbstractMethodError`, two concrete
/// maximally-specific defaults are an ICCE, and an ordinary selection —
/// or a walk that found nothing — raises nothing.
#[test]
fn strict_selection_error_raises_only_what_selection_raises() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let abs = ClassAccessFlags::PUBLIC | ClassAccessFlags::ABSTRACT;
    let a = add(
        &mut s,
        "p/A",
        abs,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let c = add(
        &mut s,
        "p/C",
        abs,
        Some(a),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let d = add(&mut s, "p/D", CLS, Some(c), vec![], vec![], real());
    let lenient = crate::classloading::find_method_recursive(d, "m", "()V", &s);
    assert_eq!(
        lenient.map(|(_, id)| id),
        Some(a),
        "the walk steps past C.m"
    );
    let error = strict_selection_error(&s, d, a, "m", "()V", lenient);
    assert!(
        matches!(
            &error,
            Some(SelectionError::AbstractMethod(message))
                if message.starts_with("Receiver class p.D ")
        ),
        "expected an AbstractMethodError, got {error:?}"
    );
    // Nothing found: the door's own not-found handling keeps the case.
    assert_eq!(strict_selection_error(&s, d, a, "m", "()V", None), None);
    // An ordinary override selects: nothing to raise.
    let e = add(
        &mut s,
        "p/E",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let lenient = crate::classloading::find_method_recursive(e, "m", "()V", &s);
    assert_eq!(strict_selection_error(&s, e, a, "m", "()V", lenient), None);

    // Two unrelated concrete defaults.
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let j = add(
        &mut s,
        "p/J",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let k = add(&mut s, "p/K", CLS, Some(obj), vec![i, j], vec![], real());
    let lenient = crate::classloading::find_method_recursive(k, "m", "()V", &s);
    assert!(lenient.is_some(), "the walk picks one default");
    assert_eq!(
        strict_selection_error(&s, k, i, "m", "()V", lenient),
        Some(SelectionError::ConflictingDefaults(
            "Conflicting default methods: p/I.m p/J.m".to_string()
        ))
    );
}

/// Divergence 2: a package-private method is not overridden from another
/// package (§5.4.5), and is from the same package.
#[test]
fn package_private_is_overridden_only_within_its_package() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p1/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", MethodAccessFlags::empty())],
        real(),
    );
    let b = add(
        &mut s,
        "p2/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let c = add(
        &mut s,
        "p1/C",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    assert_eq!(
        select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
        Selection::Selected(a)
    );
    assert_eq!(
        select(&s, c, ResolvedRef::Declared(a), "m", "()V"),
        Selection::Selected(c)
    );
    // A public mR is overridden from anywhere.
    let mut s2 = ClassStore::new();
    let obj2 = object(&mut s2);
    let a2 = add(
        &mut s2,
        "p1/A",
        CLS,
        Some(obj2),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b2 = add(
        &mut s2,
        "p2/B",
        CLS,
        Some(a2),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    assert_eq!(
        select(&s2, b2, ResolvedRef::Declared(a2), "m", "()V"),
        Selection::Selected(b2)
    );
}

/// Round 12 wave 6 (lane override), the `R12Hunt3PkgPrivate` shape: an
/// application `A` with a package-private `m`, a user loader's copy of
/// `Sub` (same package NAME, public `m`) directly under `A`, and a user
/// loader's `SubSub` under the application `Sub`. The `--compatible`
/// policy (a fresh store) selects the copy's `m` by package name; the
/// strict policy selects `A.m` (JVMS 5.4.5: another runtime package), and
/// flipping the policy retires the memoized answer. The transitive clause
/// keeps `SubSub.m` an override under both.
#[test]
fn package_private_override_across_loaders_follows_the_store_policy() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let pkg_private = || vec![method("m", MethodAccessFlags::empty())];
    let public = || vec![method("m", PUB)];
    let a = add(&mut s, "A", CLS, Some(obj), vec![], pkg_private(), real());
    let sub = add(&mut s, "Sub", CLS, Some(a), vec![], public(), real());
    let child_sub = add(&mut s, "Sub", CLS, Some(a), vec![], public(), real());
    let child_subsub = add(&mut s, "SubSub", CLS, Some(sub), vec![], public(), real());
    if let Some(c) = s.get_mut(child_sub) {
        c.loader_id = ClassLoaderId::UserDefined(7);
    }
    if let Some(c) = s.get_mut(child_subsub) {
        c.loader_id = ClassLoaderId::UserDefined(8);
    }
    let resolved = ResolvedRef::Declared(a);
    assert!(!s.override_packages_by_loader());
    assert_eq!(
        select(&s, child_sub, resolved, "m", "()V"),
        Selection::Selected(child_sub)
    );
    s.set_override_packages_by_loader(true);
    assert_eq!(
        select(&s, child_sub, resolved, "m", "()V"),
        Selection::Selected(a)
    );
    assert_eq!(
        select(&s, sub, resolved, "m", "()V"),
        Selection::Selected(sub)
    );
    assert_eq!(
        select(&s, child_subsub, resolved, "m", "()V"),
        Selection::Selected(child_subsub)
    );
    // The JIT's refinement door agrees: the lenient walk's `Sub.m` copy
    // moves to `A`.
    assert_eq!(
        receiver_dispatch_moves_to(&s, child_sub, a, "m", "()V"),
        Some(a)
    );
    s.set_override_packages_by_loader(false);
    assert_eq!(
        select(&s, child_sub, resolved, "m", "()V"),
        Selection::Selected(child_sub)
    );
}

/// Round 13 wave 1 (lane mhffm), `R13MhffmMhReflOverride` kind 5, the
/// shape `invoke_virtual_declared`'s selection (`declared_dispatch_selection`
/// in `vm_exec.rs`) serves: `B` overrides the package-private `A.m` in A's
/// runtime package, a user loader's `C` under `B` redeclares `m`
/// package-private. Against `A.m` the walk's `C.m` moves to `B`, not to
/// the resolved `A`; against `B.m` it moves to `B` as well.
#[test]
fn a_user_loader_redeclaration_under_an_override_moves_to_the_override() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let pkg_private = || vec![method("m", MethodAccessFlags::empty())];
    let a = add(&mut s, "A", CLS, Some(obj), vec![], pkg_private(), real());
    let b = add(&mut s, "B", CLS, Some(a), vec![], pkg_private(), real());
    let child_c = add(&mut s, "C", CLS, Some(b), vec![], pkg_private(), real());
    if let Some(c) = s.get_mut(child_c) {
        c.loader_id = ClassLoaderId::UserDefined(9);
    }
    s.set_override_packages_by_loader(true);
    assert_eq!(
        receiver_dispatch_moves_to(&s, child_c, a, "m", "()V"),
        Some(b)
    );
    assert_eq!(
        receiver_dispatch_moves_to(&s, child_c, b, "m", "()V"),
        Some(b)
    );
    // The app receivers keep the walk.
    assert_eq!(receiver_dispatch_moves_to(&s, b, a, "m", "()V"), None);
}

/// JVMS 5.4.6: a resolved private method is itself the selected method.
/// The lambda proxies reach `Function.lambda$andThen$0`-style synthetic
/// bodies (private interface instance methods) through the strict door
/// with the receiver's own class, where selection by name finds nothing.
#[test]
fn strict_selection_error_leaves_a_private_interface_body_alone() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let p = add(
        &mut s,
        "p/P",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("lam", MethodAccessFlags::PRIVATE)],
        real(),
    );
    let q = add(&mut s, "p/Q", CLS, Some(obj), vec![p], vec![], real());
    let lenient = crate::classloading::find_method_recursive(q, "lam", "()V", &s);
    assert!(lenient.is_some(), "the lenient walk finds the private body");
    assert_eq!(
        strict_selection_error(&s, q, p, "lam", "()V", lenient),
        None,
        "a private resolved method is its own selection"
    );
}

/// Divergence 3: an abstract re-declaration is selected (AME), not walked
/// past to the concrete ancestor.
#[test]
fn abstract_redeclaration_selects_the_abstract_method() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b = add(
        &mut s,
        "p/B",
        CLS | ClassAccessFlags::ABSTRACT,
        Some(a),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let c = add(&mut s, "p/C", CLS, Some(b), vec![], vec![], real());
    assert_eq!(
        select(&s, c, ResolvedRef::Declared(a), "m", "()V"),
        Selection::AbstractMethod(Some(b))
    );
    assert!(select_or_lenient(&s, c, Some(ResolvedRef::Declared(a)), "m", "()V").is_none());
    // HotSpot names the selected method when it is not the resolved one.
    assert_eq!(
        abstract_method_message(&s, c, ResolvedRef::Declared(a), Some(b), "m", "()V"),
        "Receiver class p.C does not define or inherit an implementation of the resolved \
         method 'void m()' of class p.A. Selected method is 'abstract void p.B.m()'."
    );
    assert_eq!(
        abstract_method_message(&s, c, ResolvedRef::Declared(b), Some(b), "m", "()V"),
        "Receiver class p.C does not define or inherit an implementation of the resolved \
         method 'abstract void m()' of abstract class p.B."
    );
    // The lenient walk is what the old path used, and it walks past B.
    assert_eq!(
        crate::classloading::find_method_recursive(c, "m", "()V", &s).map(|(_, d)| d),
        Some(a)
    );
    // Interpreter round i7 wave 142, lane L2: at a LINKED entry HotSpot's
    // vtable entry names the abstract method in the slot (B's), whichever
    // the site resolved (L2W142LinkedAmeText `virtual-reabstracted*`).
    for resolved in [a, b] {
        assert_eq!(
            linked_virtual_abstract_message(
                &s,
                c,
                ResolvedRef::Declared(resolved),
                Some(b),
                "m",
                "()V"
            )
            .as_deref(),
            Some("Missing implementation of resolved method 'abstract void m()' of abstract class p.B.")
        );
    }
    // `invokeinterface` keeps the resolution text.
    assert_eq!(
        linked_virtual_abstract_message(&s, c, ResolvedRef::Interface(None), Some(b), "m", "()V"),
        None
    );
}

/// Interpreter round i7 wave 142, lane L2: a miranda slot (an abstract
/// class implementing an interface without declaring the method) names the
/// interface method at a linked `invokevirtual`; with a default method in
/// the closure (default-method processing runs) the overpass text stays.
#[test]
fn linked_virtual_text_names_the_miranda_interface_and_keeps_the_overpass() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let ab = add(
        &mut s,
        "p/AB",
        CLS | ClassAccessFlags::ABSTRACT,
        Some(obj),
        vec![i],
        vec![],
        real(),
    );
    let v = add(&mut s, "p/V", CLS, Some(ab), vec![], vec![], real());
    assert_eq!(
        select(&s, v, ResolvedRef::Declared(i), "m", "()V"),
        Selection::AbstractMethod(None)
    );
    assert_eq!(
        linked_virtual_abstract_message(&s, v, ResolvedRef::Declared(i), None, "m", "()V")
            .as_deref(),
        Some("Missing implementation of resolved method 'abstract void m()' of interface p.I.")
    );
    // J extends I with a default `d`: default-method processing runs for U.
    let j = add(
        &mut s,
        "p/J",
        iface_flags(),
        Some(obj),
        vec![i],
        vec![method("d", PUB)],
        real(),
    );
    let aj = add(
        &mut s,
        "p/AJ",
        CLS | ClassAccessFlags::ABSTRACT,
        Some(obj),
        vec![j],
        vec![],
        real(),
    );
    let u = add(&mut s, "p/U", CLS, Some(aj), vec![], vec![], real());
    assert_eq!(
        linked_virtual_abstract_message(&s, u, ResolvedRef::Declared(i), None, "m", "()V"),
        None
    );
}

/// Divergence 4: two unrelated concrete defaults conflict; an abstract
/// re-declaration in a sub-interface masks its super-interface default.
#[test]
fn maximally_specific_defaults_conflict_and_abstract_masks() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let j = add(
        &mut s,
        "p/J",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let c = add(&mut s, "p/C", CLS, Some(obj), vec![i, j], vec![], real());
    // Declaration order, as HotSpot's hierarchy walk visits them.
    assert_eq!(
        select(&s, c, ResolvedRef::Interface(Some(i)), "m", "()V"),
        Selection::ConflictingDefaults(i, j)
    );
    assert_eq!(
        conflicting_defaults_message(&s, c, i, j, "m", "()V"),
        "Conflicting default methods: p/I.m p/J.m"
    );
    let resolved = Some(ResolvedRef::Interface(Some(i)));
    assert!(select_or_lenient(&s, c, resolved, "m", "()V").is_none());

    let k = add(
        &mut s,
        "p/K",
        iface_flags(),
        Some(obj),
        vec![i],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let d = add(&mut s, "p/D", CLS, Some(obj), vec![k], vec![], real());
    assert_eq!(
        select(&s, d, ResolvedRef::Interface(Some(i)), "m", "()V"),
        Selection::AbstractMethod(None)
    );
    // HotSpot's overpass for the masked default (the hierarchy has a
    // default method, so default-method processing ran).
    assert_eq!(
        abstract_method_message(&s, d, ResolvedRef::Interface(Some(i)), None, "m", "()V"),
        "Method p/D.m()V is abstract"
    );
    // A sub-interface default beats its super-interface default.
    let l = add(
        &mut s,
        "p/L",
        iface_flags(),
        Some(obj),
        vec![i],
        vec![method("m", PUB)],
        real(),
    );
    let e = add(&mut s, "p/E", CLS, Some(obj), vec![i, l], vec![], real());
    assert_eq!(
        select(&s, e, ResolvedRef::Interface(Some(i)), "m", "()V"),
        Selection::Selected(l)
    );
}

/// Divergence 5: a private interface method is not a default candidate.
#[test]
fn private_interface_method_is_not_a_default() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let j = add(
        &mut s,
        "p/J",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", MethodAccessFlags::PRIVATE)],
        real(),
    );
    let c = add(&mut s, "p/C", CLS, Some(obj), vec![i, j], vec![], real());
    assert_eq!(
        select(&s, c, ResolvedRef::Interface(Some(i)), "m", "()V"),
        Selection::AbstractMethod(None)
    );
}

/// The superinterface closure is HotSpot's hierarchy-walk order: the
/// superclass's interfaces before the class's own, declaration order, and
/// a superinterface right after its first sub-interface.
#[test]
fn superinterface_closure_follows_hotspot_walk_order() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let base = add(
        &mut s,
        "p/Base",
        iface_flags(),
        Some(obj),
        vec![],
        vec![],
        real(),
    );
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![base],
        vec![],
        real(),
    );
    let j = add(
        &mut s,
        "p/J",
        iface_flags(),
        Some(obj),
        vec![],
        vec![],
        real(),
    );
    let k = add(
        &mut s,
        "p/K",
        iface_flags(),
        Some(obj),
        vec![],
        vec![],
        real(),
    );
    let b = add(&mut s, "p/B", CLS, Some(obj), vec![k], vec![], real());
    let c = add(&mut s, "p/C", CLS, Some(b), vec![i, j], vec![], real());
    assert_eq!(superinterface_closure(&s, c), Some(vec![k, i, base, j]));
    assert_eq!(superinterface_closure(&s, i), Some(vec![base]));
}

/// Without any default method in the hierarchy HotSpot leaves the abstract
/// interface method in the slot, and words the error the receiver way.
#[test]
fn abstract_interface_method_without_defaults_uses_the_receiver_wording() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let i = add(
        &mut s,
        "p/I",
        iface_flags(),
        Some(obj),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    let c = add(&mut s, "p/C", CLS, Some(obj), vec![i], vec![], real());
    assert_eq!(
        select(&s, c, ResolvedRef::Interface(Some(i)), "m", "()V"),
        Selection::AbstractMethod(None)
    );
    assert_eq!(
        abstract_method_message(&s, c, ResolvedRef::Interface(Some(i)), None, "m", "()V"),
        "Receiver class p.C does not define or inherit an implementation of the resolved \
         method 'abstract void m()' of interface p.I."
    );
}

/// The compatibility guard: any stub in the hierarchy, or an abstract /
/// interface receiver class, keeps the lenient answer.
#[test]
fn compatibility_shapes_stay_lenient() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        ClassOrigin::compatibility_stub("test"),
    );
    let b = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    assert_eq!(
        select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
        Selection::Lenient
    );
    let abs = add(
        &mut s,
        "p/Abs",
        CLS | ClassAccessFlags::ABSTRACT,
        Some(obj),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::ABSTRACT)],
        real(),
    );
    assert_eq!(
        select(&s, abs, ResolvedRef::Declared(abs), "m", "()V"),
        Selection::Lenient
    );
    assert_eq!(
        select_or_lenient(&s, abs, Some(ResolvedRef::Declared(abs)), "m", "()V")
            .map(|(_, d)| d),
        Some(abs)
    );
}

/// Interpreter round i1 wave 32: `invokespecial` selection as HotSpot's
/// `runtime_resolve_special_method` (the four rows of item 1 of
/// `i29-L4-invokespecial-and-invokeinterface-selection-diverge-from-hotspot`),
/// and item 5's `Object` member through an interface.
#[test]
fn special_selection_follows_runtime_resolve_special_method() {
    let mut store = ClassStore::new();
    let obj = object(&mut store);
    let abs = MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT;
    let st = MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC;
    let abs_cls = CLS | ClassAccessFlags::ABSTRACT;
    let m = |flags| vec![method("m", flags)];

    // super.m() over an abstract re-declaration of a concrete ancestor.
    let sa_a = add(&mut store, "SaA", CLS, Some(obj), vec![], m(PUB), real());
    let sa_b = add(
        &mut store,
        "SaB",
        abs_cls,
        Some(sa_a),
        vec![],
        m(abs),
        real(),
    );
    let sa_c = add(&mut store, "SaC", CLS, Some(sa_b), vec![], m(PUB), real());
    match select_special(&store, sa_c, sa_b, false, "m", "()V") {
        SpecialSelection::AbstractMethod(msg) => assert_eq!(msg, "'void SaB.m()'"),
        _ => panic!("super.m() over an abstract re-declaration is an AbstractMethodError"),
    }

    // A static declaration between is skipped: SsA.m runs.
    let ss_a = add(&mut store, "SsA", CLS, Some(obj), vec![], m(PUB), real());
    let ss_b = add(&mut store, "SsB", CLS, Some(ss_a), vec![], m(st), real());
    let ss_c = add(&mut store, "SsC", CLS, Some(ss_b), vec![], vec![], real());
    assert!(matches!(
        select_special(&store, ss_c, ss_a, false, "m", "()V"),
        SpecialSelection::Owner(id) if id == ss_a
    ));

    // Conflicting defaults inherited by the superclass.
    let k1 = add(
        &mut store,
        "ScK1",
        iface_flags(),
        Some(obj),
        vec![],
        m(PUB),
        real(),
    );
    let k2 = add(
        &mut store,
        "ScK2",
        iface_flags(),
        Some(obj),
        vec![],
        m(PUB),
        real(),
    );
    let sc_b = add(
        &mut store,
        "ScB",
        abs_cls,
        Some(obj),
        vec![k1, k2],
        vec![],
        real(),
    );
    let sc_c = add(&mut store, "ScC", CLS, Some(sc_b), vec![], vec![], real());
    match select_special(&store, sc_c, sc_b, false, "m", "()V") {
        SpecialSelection::IncompatibleClassChange(msg) => {
            assert_eq!(msg, "Conflicting default methods: ScK1.m ScK2.m")
        }
        _ => panic!("conflicting defaults are an IncompatibleClassChangeError"),
    }

    // I.super.m() where I re-declares the default abstract.
    let si_j = add(
        &mut store,
        "SiJ",
        iface_flags(),
        Some(obj),
        vec![],
        m(PUB),
        real(),
    );
    let si_i = add(
        &mut store,
        "SiI",
        iface_flags(),
        Some(obj),
        vec![si_j],
        m(abs),
        real(),
    );
    let si_c = add(
        &mut store,
        "SiC",
        CLS,
        Some(obj),
        vec![si_i],
        m(PUB),
        real(),
    );
    match select_special(&store, si_c, si_i, true, "m", "()V") {
        SpecialSelection::AbstractMethod(msg) => assert_eq!(msg, "'void SiI.m()'"),
        _ => panic!("I.super.m() of an abstract re-declaration is an AbstractMethodError"),
    }

    // The ordinary shapes: the nearest override, and no answer at all for
    // a constructor or a call in the declaring class itself.
    let ok_a = add(&mut store, "OkA", CLS, Some(obj), vec![], m(PUB), real());
    let ok_b = add(&mut store, "OkB", CLS, Some(ok_a), vec![], m(PUB), real());
    let ok_c = add(&mut store, "OkC", CLS, Some(ok_b), vec![], vec![], real());
    assert!(matches!(
        select_special(&store, ok_c, ok_a, false, "m", "()V"),
        SpecialSelection::Owner(id) if id == ok_b
    ));
    assert!(matches!(
        select_special(&store, ok_c, ok_a, false, "<init>", "()V"),
        SpecialSelection::Unchanged
    ));
    assert!(matches!(
        select_special(&store, ok_b, ok_b, false, "m", "()V"),
        SpecialSelection::Unchanged
    ));

    // Item 5: `Object.clone` is protected, so an interface reference to
    // `clone()` does not resolve; `hashCode` (public) does.
    let mut store = ClassStore::new();
    let object_methods = vec![
        method("clone", MethodAccessFlags::PROTECTED),
        method("hashCode", PUB),
        method(
            "registerNatives",
            MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC,
        ),
    ];
    let obj = add(
        &mut store,
        "java/lang/Object",
        CLS,
        None,
        vec![],
        object_methods,
        real(),
    );
    let clone_abs = vec![method("clone", abs)];
    let io = add(
        &mut store,
        "IoI",
        iface_flags(),
        Some(obj),
        vec![],
        vec![],
        real(),
    );
    let io2 = add(
        &mut store,
        "IoJ",
        iface_flags(),
        Some(obj),
        vec![],
        clone_abs,
        real(),
    );
    let hidden = |owner, name| {
        interface_ref_names_a_non_public_object_method(&store, owner, name, "()V")
    };
    assert!(hidden(io, "clone"));
    assert!(hidden(io, "registerNatives"));
    assert!(!hidden(io, "hashCode"));
    assert!(!hidden(io, "absent"));
    assert!(!hidden(io2, "clone"));
    assert!(!hidden(obj, "clone"));
}

#[test]
fn resolution_returns_the_first_declaration_whatever_its_flags() {
    let mut s = ClassStore::new();
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b = add(
        &mut s,
        "p/B",
        CLS,
        Some(a),
        vec![],
        vec![method("m", PUB | MethodAccessFlags::STATIC)],
        real(),
    );
    let c = add(&mut s, "p/C", CLS, Some(b), vec![], vec![], real());
    assert_eq!(resolve_declaring(&s, c, "m", "()V"), Some(b));
    assert_eq!(
        static_flag_mismatch(&s, c, "m", "()V", false).as_deref(),
        Some("Expecting non-static method 'void p.B.m()'")
    );
    assert_eq!(static_flag_mismatch(&s, c, "m", "()V", true), None);
    assert_eq!(
        static_flag_mismatch(&s, a, "m", "()V", true).as_deref(),
        Some("Expected static method 'void p.A.m()'")
    );
    assert_eq!(static_flag_mismatch(&s, a, "<init>", "()V", true), None);
}

#[test]
fn external_method_names_match_hotspot_spelling() {
    assert_eq!(
        external_method_name(
            "p/Foo",
            "bar",
            "(Ljava/lang/String;I[[J)[Ljava/lang/Object;"
        ),
        "java.lang.Object[] p.Foo.bar(java.lang.String, int, long[][])"
    );
    assert_eq!(external_method_name("Foo", "m", "()V"), "void Foo.m()");
}

/// Interpreter round i1 wave 21 (lane L5,
/// `interpreter-L2-class-epoch-readers-outside-the-site-caches-stay-process-wide-FIXED-20260925.md`):
/// the memo is stamped with its store's own definition epoch, so an edge
/// change in ANOTHER store keeps this store's entry, and one in this store
/// retires it. A hit is told from a re-walk by planting a wrong answer
/// in the filled entry.
#[test]
fn select_memo_is_retired_only_by_its_own_stores_edge_changes() {
    let build = || {
        let mut s = ClassStore::new();
        let obj = object(&mut s);
        let a = add(
            &mut s,
            "p/A",
            CLS,
            Some(obj),
            vec![],
            vec![method("m", PUB)],
            real(),
        );
        let b = add(&mut s, "p/B", CLS, Some(a), vec![], vec![], real());
        (s, obj, a, b)
    };
    let (mut other, other_obj, other_a, other_b) = build();
    let (mut mine, mine_obj, a, b) = build();
    let (other_slot, mine_slot) = (
        cratonvm_classloading::store_epoch_slot(other.layout_domain()),
        cratonvm_classloading::store_epoch_slot(mine.layout_domain()),
    );
    if other_slot == mine_slot {
        // The two stores share an epoch slot (domains 64 apart): the
        // documented conservative case, nothing to assert.
        return;
    }
    let key = store_identity(&mine);
    let plant = || {
        SELECT_MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            let idx = select_memo_slot(key, b.as_u32(), "m", "()V");
            if let Some(Some(entry)) = memo.get_mut(idx) {
                entry.answer = Selection::Lenient;
            }
        })
    };
    // The other two stamps are process-wide, and other tests move them.
    // Read through the memo's own stamp helpers, so the test holds with
    // `CRATONVM_INTERP_SELECT_MEMO_EDGE_EPOCH` either way (wave 94).
    let _ = mine_slot;
    let stamps = |s: &ClassStore| {
        (
            select_memo_hierarchy_stamp(s),
            select_memo_origin_stamp(s),
            cratonvm_classloading::class_redefinition_count(),
        )
    };
    let mut checked = false;
    for round in 0..50 {
        let before = stamps(&mine);
        assert_eq!(
            select(&mine, b, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Selected(a)
        );
        plant();
        // Another store's edge change (alternating, so it always moves).
        let to = if round % 2 == 0 { other_obj } else { other_a };
        other.set_superclass(other_b, Some(to));
        if stamps(&mine) != before {
            // Another test moved a process stamp, or its store shares
            // `mine`'s slot and moved that.
            continue;
        }
        assert_eq!(
            select(&mine, b, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Lenient,
            "another store's edge change must not retire this store's entry"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for this store's epoch slot");
    // This store's own edge changes (away and back) retire the planted
    // entry: the walk answers again.
    mine.set_superclass(b, Some(mine_obj));
    mine.set_superclass(b, Some(a));
    assert_eq!(
        select(&mine, b, ResolvedRef::Declared(a), "m", "()V"),
        Selection::Selected(a)
    );
}

/// The TDigest `(int) -> double` lambda peephole (`vm/src/jit/helpers.rs`)
/// asks [`receiver_dispatch_moves_to`] before it compiles, calls, or
/// by-class dispatches the `get(I)D` the receiver walk found (interpreter
/// round i1 wave 9). A text pin: the helper cannot be driven without
/// compiled code.
#[test]
fn the_lambda_get_peephole_asks_selection_before_any_direct_route() {
    let src = crate::jit::helpers::HELPERS_SRC.replace("\r\n", "\n");
    let start = src
        .find("\npub unsafe fn jit_lambda_int_to_double_body(")
        .expect("the peephole body exists");
    let rest = &src[start + 1..];
    let body = &rest[..rest.find("\n\x7d\n").map_or(rest.len(), |e| e + 3)];
    // Round 12 wave 1 moved the selection question and the compile arm into
    // `resolve_lambda_get_route`, which the body calls before either direct
    // route: read the resolver first, then the body.
    let route_at = src
        .find("\npub unsafe fn resolve_lambda_get_route(")
        .expect("the route resolver exists");
    let route_rest = &src[route_at + 1..];
    let route = &route_rest[..route_rest
        .find("\n\x7d\n")
        .map_or(route_rest.len(), |e| e + 3)];
    assert!(
        body.contains("resolve_lambda_get_route("),
        "the body resolves its route"
    );
    let body = format!("{route}{body}");
    let body = body.as_str();
    let ask = body
        .find("receiver_dispatch_moves_to(")
        .expect("the peephole asks the selection rule");
    for later in [
        "try_jit_compile_callee_for_class(",
        "try_call_compiled_entry_reentrant(",
        "invoke_on_class_shared(",
    ] {
        let at = body.find(later).expect("the peephole still has this arm");
        assert!(ask < at, "selection is asked before {later}");
    }
}

/// Interpreter round i4 wave 94, lane L7
/// (`i92-L7-selection-and-callback-memos-need-a-hierarchy-edge-epoch`):
/// under the edge stamp a definition of an unrelated class keeps a
/// memoised selection, and an edge change in the store retires it.
#[test]
fn edge_stamped_select_memo_survives_definitions_and_retires_on_edges() {
    let mut s = ClassStore::new();
    s.set_select_memo_edge_epoch(true);
    let obj = object(&mut s);
    let a = add(
        &mut s,
        "p/A",
        CLS,
        Some(obj),
        vec![],
        vec![method("m", PUB)],
        real(),
    );
    let b = add(&mut s, "p/B", CLS, Some(a), vec![], vec![], real());
    let key = store_identity(&s);
    let plant = || {
        SELECT_MEMO.with(|memo| {
            let mut memo = memo.borrow_mut();
            let idx = select_memo_slot(key, b.as_u32(), "m", "()V");
            if let Some(Some(entry)) = memo.get_mut(idx) {
                entry.answer = Selection::Lenient;
            }
        })
    };
    let stamps = |s: &ClassStore| (select_memo_hierarchy_stamp(s), select_memo_origin_stamp(s));
    let mut checked = false;
    for round in 0..50 {
        let before = stamps(&s);
        assert_eq!(
            select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Selected(a)
        );
        plant();
        // A brand-new class that nothing stored names.
        let unrelated = format!("q/Unrelated{round}");
        let _ = add(&mut s, &unrelated, CLS, Some(obj), vec![], vec![], real());
        if stamps(&s) != before {
            // Another test's in-place `set_origin` (process-wide).
            continue;
        }
        assert_eq!(
            select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
            Selection::Lenient,
            "a definition of an unrelated class must not retire the entry"
        );
        checked = true;
        break;
    }
    assert!(checked, "no quiet window for the in-place provenance epoch");
    // An edge change in this store (away and back) retires it.
    s.set_superclass(b, Some(obj));
    s.set_superclass(b, Some(a));
    assert_eq!(
        select(&s, b, ResolvedRef::Declared(a), "m", "()V"),
        Selection::Selected(a)
    );
}

/// j27 wave 5 (lane INVHUNT): a spliced `super.m()` may run the lenient
/// walk's body only where `invokespecial` selection runs it too.
#[test]
fn super_special_body_agrees_only_with_the_invokespecial_selection() {
    let mut store = ClassStore::new();
    let obj = object(&mut store);
    let abs = MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT;
    let abs_cls = CLS | ClassAccessFlags::ABSTRACT;
    let m = |flags| vec![method("m", flags)];
    // G.m concrete; S re-declares it abstract; C's `super.m()` names S.
    let g = add(&mut store, "JgG", CLS, Some(obj), vec![], m(PUB), real());
    let s = add(&mut store, "JgS", abs_cls, Some(g), vec![], m(abs), real());
    let c = add(&mut store, "JgC", CLS, Some(s), vec![], m(PUB), real());
    let lenient = crate::classloading::find_method_recursive(s, "m", "()V", &store)
        .map(|(_, id)| id);
    assert_eq!(lenient, Some(g), "the lenient walk steps past S's abstract m");
    assert!(
        !super_special_body_agrees(&store, c, s, "m", "()V", g),
        "HotSpot throws AbstractMethodError; G.m must not be spliced"
    );
    // S inherits G.m: the walk is the selection.
    let s2 = add(&mut store, "JgS2", abs_cls, Some(g), vec![], vec![], real());
    let c2 = add(&mut store, "JgC2", CLS, Some(s2), vec![], m(PUB), real());
    assert!(super_special_body_agrees(&store, c2, s2, "m", "()V", g));
}
