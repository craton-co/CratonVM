use super::*;
use cratonvm_reader::class_access_flags::{
    ClassAccessFlags, FieldAccessFlags, MethodAccessFlags,
};
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

/// JIT round 23 wave 2 (proxy-define page): `remove_sorted_id` has
/// `retain`'s effect on an ascending list (an absent id, the ends, a run
/// of duplicates), and on an unordered one for an id it does not hold.
#[test]
fn remove_sorted_id_matches_retain() {
    let cases: &[(&[u32], u32)] = &[
        (&[], 4),
        (&[4], 4),
        (&[1, 2, 3], 1),
        (&[1, 2, 3], 3),
        (&[1, 2, 3], 2),
        (&[1, 2, 3], 9),
        (&[1, 2, 2, 2, 3], 2),
        (&[5, 1, 7], 9),
    ];
    for (input, id) in cases {
        let mut expected = input.to_vec();
        expected.retain(|&k| k != *id);
        let mut got = input.to_vec();
        remove_sorted_id(&mut got, *id);
        assert_eq!(got, expected, "{input:?} without {id}");
    }
}


/// Create a minimal constant pool (just a tombstone).
fn empty_constant_pool() -> ConstantPool {
    ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
}

/// Helper to create a minimal class.
#[allow(clippy::too_many_arguments)]
fn make_class(
    id: ClassId,
    name: &str,
    superclass: Option<ClassId>,
    interfaces: Vec<ClassId>,
    fields: Vec<ClassFileField>,
    methods: Vec<ClassFileMethod>,
    first_field_index: usize,
    num_total_fields: usize,
) -> Class {
    Class {
        id,
        loader_id: ClassLoaderId::Application,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: empty_constant_pool(),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass,
        interfaces,
        fields,
        methods,
        first_field_index,
        num_total_fields,
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
        origin: ClassOrigin::default(),
        has_finalizer: false,
        code_source: None,
        array_info: None,
        init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
        record_object_methods: std::sync::atomic::AtomicU8::new(0),
    }
}

fn make_field(name: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::empty(),
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc("I"),
        attributes: vec![],
    }
}

fn make_field_desc(name: &str, descriptor: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::empty(),
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    }
}

fn make_static_field(name: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::STATIC,
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc("I"),
        attributes: vec![],
    }
}

fn make_method(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::empty(),
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    }
}

/// Interpreter round i1 wave 35: `final C extends abstract B { abstract
/// m } extends A { m }` is not devirtualised to `A.m`, since selection
/// finds B's abstract `m`; without the re-declaration it binds `A.m`.
#[test]
fn a_final_class_over_an_abstract_redeclaration_is_not_devirtualised() {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let mut store = ClassStore::new();
    let a = store.next_id();
    let mut concrete = make_method("m", "()V");
    concrete.access_flags = MethodAccessFlags::PUBLIC;
    store.add(make_class(
        a,
        "A",
        None,
        vec![],
        vec![],
        vec![concrete],
        0,
        0,
    ));
    let b = store.next_id();
    let mut abstract_m = make_method("m", "()V");
    abstract_m.access_flags = MethodAccessFlags::PUBLIC | MethodAccessFlags::ABSTRACT;
    let mut b_class = make_class(b, "B", Some(a), vec![], vec![], vec![abstract_m], 0, 0);
    b_class.access_flags |= ClassAccessFlags::ABSTRACT;
    store.add(b_class);
    let c = store.next_id();
    let mut c_class = make_class(c, "C", Some(b), vec![], vec![], vec![], 0, 0);
    c_class.access_flags |= ClassAccessFlags::FINAL;
    store.add(c_class);
    assert_eq!(
        invokevirtual_final_declaring_class(c, "m", "()V", &store),
        None
    );

    let d = store.next_id();
    let mut d_class = make_class(d, "D", Some(a), vec![], vec![], vec![], 0, 0);
    d_class.access_flags |= ClassAccessFlags::FINAL;
    store.add(d_class);
    assert_eq!(
        invokevirtual_final_declaring_class(d, "m", "()V", &store),
        Some(a)
    );
}

/// Tier-1 round, lane jitic: a `final` class that inherits `m` only from
/// interfaces is devirtualised to the default selection picks, and NOT
/// when an abstract re-declaration in a subinterface masks the default
/// (`AbstractMethodError`) or two unrelated defaults conflict
/// (`IncompatibleClassChangeError`). An unrelated abstract declaration
/// beside one default does not stop the bind (JVMS §5.4.6 step 3).
#[test]
fn final_devirt_of_an_interface_default_follows_selection() {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let iface = |store: &mut ClassStore, name: &str, supers: Vec<ClassId>, m: Option<bool>| {
        let id = store.next_id();
        let methods = match m {
            Some(is_abstract) => {
                let mut method = make_method("m", "()V");
                method.access_flags = MethodAccessFlags::PUBLIC;
                if is_abstract {
                    method.access_flags |= MethodAccessFlags::ABSTRACT;
                }
                vec![method]
            }
            None => vec![],
        };
        let mut class = make_class(id, name, None, supers, vec![], methods, 0, 0);
        class.access_flags =
            ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
        store.add(class);
        id
    };
    let final_class = |store: &mut ClassStore, name: &str, ifaces: Vec<ClassId>| {
        let id = store.next_id();
        let mut class = make_class(id, name, None, ifaces, vec![], vec![], 0, 0);
        class.access_flags |= ClassAccessFlags::FINAL;
        store.add(class);
        id
    };
    let mut store = ClassStore::new();
    let i = iface(&mut store, "I", vec![], Some(false));
    let j_masks = iface(&mut store, "J", vec![i], Some(true));
    let k_overrides = iface(&mut store, "K", vec![i], Some(false));
    let other = iface(&mut store, "O", vec![], Some(false));
    let abstract_beside = iface(&mut store, "A", vec![], Some(true));
    let plain = iface(&mut store, "P", vec![i], None);

    let through_plain = final_class(&mut store, "F1", vec![plain]);
    assert_eq!(invokevirtual_final_declaring_class(through_plain, "m", "()V", &store), Some(i));
    let masked = final_class(&mut store, "F2", vec![j_masks]);
    assert_eq!(invokevirtual_final_declaring_class(masked, "m", "()V", &store), None);
    let overridden = final_class(&mut store, "F3", vec![plain, k_overrides]);
    assert_eq!(
        invokevirtual_final_declaring_class(overridden, "m", "()V", &store),
        Some(k_overrides)
    );
    let conflicting = final_class(&mut store, "F4", vec![i, other]);
    assert_eq!(invokevirtual_final_declaring_class(conflicting, "m", "()V", &store), None);
    let beside = final_class(&mut store, "F5", vec![abstract_beside, i]);
    assert_eq!(invokevirtual_final_declaring_class(beside, "m", "()V", &store), Some(i));

    // Not final: never devirtualised through an interface.
    let open = store.next_id();
    store.add(make_class(open, "Open", None, vec![i], vec![], vec![], 0, 0));
    assert_eq!(invokevirtual_final_declaring_class(open, "m", "()V", &store), None);
}

/// JIT round 23 wave 6 review (`r23-w4b-review-cha-watermark-blind-to-in-place-reparent`):
/// the CHA watermark ceiling stops below the first compatibility stub ever
/// added, so a stub upgraded and re-parented INTO an analysed subtree
/// under its old id can never pass a watermark test.
#[test]
fn cha_watermark_ceiling_stops_below_the_first_stub() {
    let mut store = ClassStore::new();
    let a = store.next_id();
    store.add(make_class(a, "A", None, vec![], vec![], vec![], 0, 0));
    assert_eq!(store.cha_watermark_ceiling(), store.next_id().as_u32());
    let s = store.next_id();
    let mut stub = make_class(s, "S", Some(a), vec![], vec![], vec![], 0, 0);
    stub.origin = ClassOrigin::compatibility_stub("test");
    store.add(stub);
    let b = store.next_id();
    store.add(make_class(b, "B", Some(a), vec![], vec![], vec![], 0, 0));
    assert_eq!(store.cha_watermark_ceiling(), s.as_u32(), "capped at the stub's id");
    // An upgrade does not raise it again.
    if let Some(class) = store.get_mut(s) {
        class.origin = ClassOrigin::default();
    }
    assert_eq!(store.cha_watermark_ceiling(), s.as_u32(), "monotone");
}

/// An in-place edge edit of a class below an issued watermark breaks that
/// answer's premise, and the store then refuses every later answer; one at
/// or above it (a class the answer's guard already misses) does not.
#[test]
fn an_in_place_edit_below_an_issued_watermark_breaks_the_premise() {
    let mut store = ClassStore::new();
    let a = store.next_id();
    store.add(make_class(a, "A", None, vec![], vec![], vec![], 0, 0));
    let x = store.next_id();
    store.add(make_class(x, "X", None, vec![], vec![], vec![], 0, 0));
    let watermark = store.cha_watermark_ceiling();
    store.note_cha_watermark_issued(watermark);
    let later = store.next_id();
    store.add(make_class(later, "L", None, vec![], vec![], vec![], 0, 0));
    assert!(later.as_u32() >= watermark);
    store.set_superclass(later, Some(a));
    assert!(
        !store.cha_watermark_premise_broken(),
        "a class above the watermark misses the guard anyway"
    );
    store.set_superclass(x, Some(a));
    assert!(
        store.cha_watermark_premise_broken(),
        "x is below the watermark and just moved under A"
    );
}

#[test]
fn class_id_display() {
    let id = ClassId::new(42);
    assert_eq!(format!("{id}"), "42");
    assert_eq!(id.as_u32(), 42);
}

// --- Compact layout: width-descending field assignment -------------------

fn typed_field(name: &str, descriptor: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::empty(),
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    }
}

/// `java.lang.String`'s exact field set and declaration order.
fn string_shaped_fields() -> Vec<ClassFileField> {
    vec![
        typed_field("value", "[B"),
        typed_field("coder", "B"),
        typed_field("hash", "I"),
        typed_field("hashIsZero", "Z"),
    ]
}

/// The whole point of the reorder: `String` is 14 bytes of field data, and
/// declaration order spends 24 on it (`coder` at 8 forces `hash` to skip to
/// 12, and `hashIsZero` lands at 16, rounding the body to 24). Widest-first
/// packs the same four fields into 14, which rounds to 16.
///
/// Asserted as *both* arms in one test, against the same class, so a
/// regression that quietly stops reordering cannot read as a pass.
#[test]
fn width_packing_saves_eight_bytes_on_a_string_shaped_class() {
    let mut store = ClassStore::new();
    let id = store.next_id();
    store.add(make_class(
        id,
        "java/lang/String",
        None,
        vec![],
        string_shaped_fields(),
        vec![],
        0,
        4,
    ));

    let packed = store.build_compact_layout_ordered(id, true).unwrap();
    assert_eq!(
        packed.total_size,
        8 + 16,
        "widest-first must pack String's 14 bytes of fields into 16"
    );
    // value(8) @0, hash(4) @8, coder(1) @12, hashIsZero(1) @13.
    assert_eq!(packed.field_disps, vec![8, 20, 16, 21]);
    assert_eq!(packed.ref_disps, vec![8]);

    let declared = store.build_compact_layout_ordered(id, false).unwrap();
    assert_eq!(
        declared.total_size,
        8 + 24,
        "declaration order is the control: it must still cost 24"
    );
    assert_eq!(declared.field_disps, vec![8, 16, 20, 24]);
}

/// Characterises the split-loader shape these two predicates disagree on,
/// using the `AotIntegrationTests` case as the concrete example: a proxy
/// implementing an interface whose `ClassId` is the child loader's copy,
/// tested against the parent loader's copy of the same name.
///
/// `is_subclass_of` must answer `false` — the ids genuinely differ, and
/// that is the honest answer to the question it was asked.
/// `is_assignable_to_name` must answer `true`, because it is asked the
/// question a flat class store can actually answer. Both arms are asserted
/// against the same pair so a regression that collapses one into the other
/// cannot read as a pass, and a negative pins that the name walk has not
/// rotted into "everything is assignable".
///
/// The interface leg is the part `is_subclass_of_by_name` cannot do: an
/// annotation type is an interface, so a proxy reaches it through
/// `interfaces`, never through `superclass`. Five callers depend on this
/// distinction — exception `catch_type` matching, JIT
/// `checkcast`/`instanceof`, the recovered-mirror receiver check,
/// `VarHandle` return coercion and the `Serializable` probe — and none of
/// them had a test that built the two-copy hierarchy explicitly.
#[test]
fn a_proxy_is_assignable_to_the_other_loaders_copy_of_its_interface_by_name() {
    let mut store = ClassStore::new();

    // The parent loader's copy — the one the `ContextConfiguration[]` array
    // was created with.
    let parent_iface = store.next_id();
    store.add(make_class(
        parent_iface,
        "org/springframework/test/context/ContextConfiguration",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // The forked loader's copy: same name, different id. This is the whole
    // defect in one line.
    let forked_iface = store.next_id();
    store.add(make_class(
        forked_iface,
        "org/springframework/test/context/ContextConfiguration",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    assert_ne!(parent_iface, forked_iface);

    // `jdk/proxy3/$Proxy27`, synthesized against the FORKED copy.
    let proxy = store.next_id();
    store.add(make_class(
        proxy,
        "jdk/proxy3/$Proxy27",
        None,
        vec![forked_iface],
        vec![],
        vec![],
        0,
        0,
    ));

    let proxy_class = store.get(proxy).expect("proxy present");

    // Exact-identity: correctly false against the parent's copy, true
    // against its own. Neither is a bug; the bug is stopping there.
    assert!(
        !proxy_class.is_subclass_of(parent_iface, &store),
        "the two copies have different ids, so identity must not match — if \
         this starts passing, the loader split itself was fixed and the \
         fallback below is no longer the thing under test",
    );
    assert!(proxy_class.is_subclass_of(forked_iface, &store));

    // Name-based: reaches the interface through the DAG and answers the
    // question HotSpot would have, where there is only one copy.
    assert!(
        proxy_class.is_assignable_to_name(
            "org/springframework/test/context/ContextConfiguration",
            &store,
        ),
        "the name walk must cross the loader split — a proxy created FROM \
         an annotation type must not read as unrelated to it just because \
         a forked loader owns the copy it was built against",
    );

    // ...and it is still a real check: an unrelated name is refused, so
    // `ArrayStoreException` fidelity survives for the case the check exists
    // for (a String into a Runnable[]).
    assert!(
        !proxy_class.is_assignable_to_name("java/lang/Runnable", &store),
        "the fallback must not degrade into 'everything is assignable'",
    );
}

/// r9w4 vm4: `is_assignable_to_name` now dedupes with [`VisitedClasses`]
/// (stack array + `Vec` spill) instead of an `FxHashSet`. Pin the two
/// behaviours the set exists for on a DAG wider than the inline capacity:
/// a shared super-interface reached through 40 branches is still found,
/// and an unrelated name is still refused (the walk terminates instead of
/// going exponential, and the spill keeps deduping past 32 entries).
#[test]
fn r9w4_assignable_to_name_dedupes_a_wide_interface_diamond() {
    let mut store = ClassStore::new();
    let root = store.next_id();
    store.add(make_class(
        root,
        "p/Root",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    let mut branches = Vec::new();
    for i in 0..40 {
        let id = store.next_id();
        store.add(make_class(
            id,
            &format!("p/Branch{i}"),
            None,
            vec![root],
            vec![],
            vec![],
            0,
            0,
        ));
        branches.push(id);
    }
    let leaf = store.next_id();
    store.add(make_class(
        leaf,
        "p/Leaf",
        None,
        branches,
        vec![],
        vec![],
        0,
        0,
    ));
    let leaf_class = store.get(leaf).expect("leaf present");
    assert!(leaf_class.is_assignable_to_name("p/Root", &store));
    assert!(leaf_class.is_assignable_to_name("p/Branch39", &store));
    assert!(!leaf_class.is_assignable_to_name("p/Unrelated", &store));
    // The supers-only walk shares the set type; a class with no superclass
    // chain to the target is still a plain negative.
    assert!(!leaf_class.is_subclass_of_by_name("p/Root", &store));
}

/// The two name-based walks are NOT interchangeable, and the pair of
/// asserts below is the whole reason to prefer one at a call site.
///
/// `is_subclass_of_by_name` exists for exception `catch_type` matching,
/// where the target is never an interface, so it walks only the superclass
/// chain. Its name does not say so, and a caller asking about an INTERFACE
/// gets a silent, permanent `false` rather than an error — which is
/// indistinguishable from a correct negative and so cannot be noticed
/// without a test like this one.
///
/// It has been read the wrong way twice. The `Path.toString()` branch in
/// `vm/src/runtime/invokedynamic.rs` carries the first write-up (that
/// version "never actually fires"); the second was
/// `interpreter::typecheck::unmod_backing_reaches`, which decides between
/// `Collections$UnmodifiableList` and `Collections$UnmodifiableRandomAccessList`
/// for the class the `instanceof`/`checkcast` opcodes consult. It asked
/// about `java/util/RandomAccess` — an interface — so it answered `false`
/// for every list, and `x instanceof RandomAccess` disagreed with both
/// `RandomAccess.class.isInstance(x)` and `x.getClass()`.
///
/// The hierarchy below is `ArrayList`'s, shortened to the part that
/// matters: the marker is reached through `interfaces`, and there is no
/// route to it through `superclass` at all.
#[test]
fn the_supers_only_name_walk_cannot_see_an_interface_the_dag_walk_finds() {
    let mut store = ClassStore::new();

    let random_access = store.next_id();
    store.add(make_class(
        random_access,
        "java/util/RandomAccess",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    let abstract_list = store.next_id();
    store.add(make_class(
        abstract_list,
        "java/util/AbstractList",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    let array_list = store.next_id();
    store.add(make_class(
        array_list,
        "java/util/ArrayList",
        Some(abstract_list),
        vec![random_access],
        vec![],
        vec![],
        0,
        0,
    ));

    let al = store.get(array_list).expect("ArrayList present");

    // The superclass leg: both walks agree, because a superclass IS on the
    // supers-only path. Asserted so the negative below reads as "interface"
    // and not as "this walk is broken".
    assert!(al.is_subclass_of_by_name("java/util/AbstractList", &store));
    assert!(al.is_assignable_to_name("java/util/AbstractList", &store));

    // The interface leg: they diverge. This is the trap.
    assert!(
        !al.is_subclass_of_by_name("java/util/RandomAccess", &store),
        "if this starts passing, `is_subclass_of_by_name` grew an interface \
         walk — which is a behaviour change for every exception-`catch_type` \
         caller and must be justified there, not discovered here",
    );
    assert!(
        al.is_assignable_to_name("java/util/RandomAccess", &store),
        "the DAG walk is the one a caller asking about an interface must use",
    );

    // Still a real check in both directions.
    assert!(!al.is_assignable_to_name("java/util/Deque", &store));
}

/// Reordering must not disturb which *index* names which field: the storage
/// kind recorded at index `i` is still field `i`'s, whatever offset it got.
#[test]
fn width_packing_keeps_field_kinds_on_their_own_indices() {
    let mut store = ClassStore::new();
    let id = store.next_id();
    store.add(make_class(
        id,
        "java/lang/String",
        None,
        vec![],
        string_shaped_fields(),
        vec![],
        0,
        4,
    ));

    let layout = store.build_compact_layout_ordered(id, true).unwrap();
    use cratonvm_types::FieldStorageKind::*;
    assert_eq!(layout.field_kinds, vec![Reference, Byte, Int, Boolean]);
    assert_eq!(layout.is_ref, vec![true, false, false, false]);
}

/// The parent-prefix property is what lets a `getfield` through a
/// supertype-typed reference use one offset table, and it is exactly what a
/// reorder could silently break. A parent's own fields must land on the
/// same offsets whether the layout being built is the parent's or a
/// child's — which holds only because the sort is scoped to one ancestor's
/// contribution rather than applied across the flattened field list.
#[test]
fn width_packing_preserves_the_parent_prefix_property() {
    let mut store = ClassStore::new();

    let parent = store.next_id();
    store.add(make_class(
        parent,
        "Parent",
        None,
        vec![],
        vec![
            typed_field("flag", "Z"),
            typed_field("ref", "Ljava/lang/Object;"),
            typed_field("n", "I"),
        ],
        vec![],
        0,
        3,
    ));

    let child = store.next_id();
    store.add(make_class(
        child,
        "Child",
        Some(parent),
        vec![],
        vec![typed_field("extra", "J"), typed_field("small", "S")],
        vec![],
        3,
        5,
    ));

    let parent_layout = store.build_compact_layout_ordered(parent, true).unwrap();
    let child_layout = store.build_compact_layout_ordered(child, true).unwrap();

    assert_eq!(
        child_layout.field_disps[..parent_layout.field_disps.len()],
        parent_layout.field_disps[..],
        "the parent's fields must keep their offsets inside the child"
    );
    assert_eq!(
        child_layout.field_kinds[..parent_layout.field_kinds.len()],
        parent_layout.field_kinds[..],
    );
    // ref(8) @0, n(4) @8, flag(1) @12 -> parent body rounds to 16; the
    // child's own fields continue from the parent's *unrounded* 13.
    assert_eq!(parent_layout.field_disps, vec![20, 8, 16]);
    assert_eq!(parent_layout.total_size, 8 + 16);
}

/// Two fields of equal width put the reference first, so the oop-map the GC
/// walks stays contiguous. `ref_offsets` must also come back ascending —
/// assignment order is width-descending, so it is no longer sorted by
/// construction, and the scan loops read it in order.
#[test]
fn width_packing_groups_references_and_sorts_the_oop_map() {
    let mut store = ClassStore::new();
    let id = store.next_id();
    store.add(make_class(
        id,
        "Mixed",
        None,
        vec![],
        vec![
            typed_field("a", "J"),
            typed_field("r1", "Ljava/lang/Object;"),
            typed_field("b", "D"),
            typed_field("r2", "[I"),
        ],
        vec![],
        0,
        4,
    ));

    let layout = store.build_compact_layout_ordered(id, true).unwrap();
    assert_eq!(layout.ref_disps, vec![8, 16], "refs first, ascending");
    assert!(
        layout.ref_disps.windows(2).all(|w| w[0] < w[1]),
        "ref_disps must be ascending: {:?}",
        layout.ref_disps
    );
    assert_eq!(layout.total_size, 8 + 32);
}

/// `java.util.LinkedList`, the shape that caught plain width-descending
/// making an object **bigger**.
///
/// `AbstractList` contributes `int modCount` and leaves the running offset
/// at 4. Declaration order then fills that 4-byte hole with `int size`
/// before the two `Node` references and lands on 24. Naive widest-first put
/// a reference there instead, wasted the hole, ended at 28 and rounded to
/// **32** — a measured regression, found by censusing both arms of a real
/// run rather than by reading the sort. Hole-first placement must get it
/// back to 24.
#[test]
fn width_packing_fills_an_inherited_hole_instead_of_wasting_it() {
    let mut store = ClassStore::new();

    let parent = store.next_id();
    store.add(make_class(
        parent,
        "java/util/AbstractList",
        None,
        vec![],
        vec![typed_field("modCount", "I")],
        vec![],
        0,
        1,
    ));

    let child = store.next_id();
    store.add(make_class(
        child,
        "java/util/LinkedList",
        Some(parent),
        vec![],
        vec![
            typed_field("size", "I"),
            typed_field("first", "Ljava/util/LinkedList$Node;"),
            typed_field("last", "Ljava/util/LinkedList$Node;"),
        ],
        vec![],
        1,
        4,
    ));

    let packed = store.build_compact_layout_ordered(child, true).unwrap();
    let declared = store.build_compact_layout_ordered(child, false).unwrap();
    assert_eq!(
        declared.total_size,
        8 + 24,
        "declaration order is already 24 here"
    );
    assert_eq!(
        packed.total_size,
        8 + 24,
        "hole-first must match declaration order, not regress to 32"
    );
    // modCount @0, size @4 (into the hole), first @8, last @16.
    assert_eq!(packed.field_disps, vec![8, 12, 16, 24]);
}

/// The "never larger than declaration order" guarantee, asserted over every
/// arrangement of a field set chosen to expose the bad cases: an odd
/// leading primitive, mixed widths, and a reference that has to align.
///
/// This is the property the fallback exists for. A heuristic that is
/// usually better is not the same as one that is never worse, and only the
/// second is safe to turn on by default across a whole heap.
#[test]
fn width_packing_is_never_larger_than_declaration_order() {
    let descriptors = ["B", "Ljava/lang/Object;", "I", "J", "S", "Z", "[I"];
    // Every rotation of the declaration order, so the incoming alignment
    // and the tail both vary.
    for rot in 0..descriptors.len() {
        let mut store = ClassStore::new();
        let id = store.next_id();
        let fields: Vec<ClassFileField> = (0..descriptors.len())
            .map(|i| {
                let d = descriptors[(i + rot) % descriptors.len()];
                typed_field(&format!("f{i}"), d)
            })
            .collect();
        store.add(make_class(
            id,
            "Rotated",
            None,
            vec![],
            fields,
            vec![],
            0,
            descriptors.len(),
        ));

        let packed = store.build_compact_layout_ordered(id, true).unwrap();
        let declared = store.build_compact_layout_ordered(id, false).unwrap();
        assert!(
            packed.total_size <= declared.total_size,
            "rotation {rot}: packed {} > declaration order {}",
            packed.total_size,
            declared.total_size
        );
        // And every field must still be within the body and aligned.
        for (i, (&offset, &kind)) in packed
            .field_disps
            .iter()
            .zip(packed.field_kinds.iter())
            .enumerate()
        {
            assert_eq!(
                offset % kind.alignment_runtime(),
                0,
                "rotation {rot} field {i} at {offset} is misaligned for {kind:?}"
            );
            assert!(offset + kind.size_runtime() <= packed.total_size);
        }
        // No two fields may overlap.
        let mut spans: Vec<(u32, u32)> = packed
            .field_disps
            .iter()
            .zip(packed.field_kinds.iter())
            .map(|(&o, &k)| (o, o + k.size_runtime()))
            .collect();
        spans.sort_unstable();
        for w in spans.windows(2) {
            assert!(
                w[0].1 <= w[1].0,
                "rotation {rot}: fields overlap {:?} {:?}",
                w[0],
                w[1]
            );
        }
    }
}

/// A padded class is still refused outright, reorder or not — the reorder
/// must not have turned an untyped slot into something the oop-map claims
/// to know the type of.
#[test]
fn width_packing_still_refuses_a_padded_class() {
    let mut store = ClassStore::new();
    let id = store.next_id();
    store.add(make_class(
        id,
        "Padded",
        None,
        vec![],
        vec![typed_field("n", "I")],
        vec![],
        0,
        4, // three synthetic slots with no descriptor
    ));

    assert!(store.build_compact_layout_ordered(id, true).is_none());
    assert!(store.build_compact_layout_ordered(id, false).is_none());
}

#[test]
fn class_store_add_and_get() {
    let mut store = ClassStore::new();
    assert!(store.is_empty());

    let id0 = store.next_id();
    assert_eq!(id0, ClassId::new(0));

    let class0 = make_class(id0, "java/lang/Object", None, vec![], vec![], vec![], 0, 0);
    store.add(class0);

    assert_eq!(store.len(), 1);
    assert!(!store.is_empty());

    let c = store.get(id0).unwrap();
    assert_eq!(&*c.name, "java/lang/Object");
    assert!(c.superclass.is_none());
}

#[test]
fn class_store_find_by_name() {
    let mut store = ClassStore::new();

    let id0 = store.next_id();
    store.add(make_class(
        id0,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    let id1 = store.next_id();
    store.add(make_class(
        id1,
        "java/lang/String",
        Some(id0),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    assert!(store.find_by_name("java/lang/String").is_some());
    assert!(store.find_by_name("java/lang/Integer").is_none());
}

/// SBR5 `clload`: the name index answers what the whole-store scan did (first live id, the
/// loader compared at lookup time) and forgets removed classes.
#[test]
fn class_store_name_index_matches_the_scan_and_follows_remove() {
    let mut store = ClassStore::new();
    let mut ids = Vec::new();
    for name in ["a/X", "b/Y", "a/X"] {
        let id = store.next_id();
        store.add(make_class(id, name, None, vec![], vec![], vec![], 0, 0));
        ids.push(id);
    }
    // `make_class` gives every class the same loader.
    let loader = store.get(ids[0]).unwrap().loader_id;
    assert_eq!(
        store.find_id_by_loader_and_name("a/X", loader),
        Some(ids[0])
    );
    assert_eq!(
        store.find_id_by_loader_and_name("b/Y", loader),
        Some(ids[1])
    );
    assert_eq!(store.find_id_by_loader_and_name("c/Z", loader), None);
    let other = if loader == ClassLoaderId::Bootstrap {
        ClassLoaderId::Application
    } else {
        ClassLoaderId::Bootstrap
    };
    assert_eq!(store.find_id_by_loader_and_name("a/X", other), None);
    store.remove(ids[0]);
    assert_eq!(
        store.find_id_by_loader_and_name("a/X", loader),
        Some(ids[2])
    );
    store.remove(ids[2]);
    assert_eq!(store.find_id_by_loader_and_name("a/X", loader), None);
}

#[test]
fn find_method_in_class() {
    let id = ClassId::new(0);
    let methods = vec![
        make_method("<init>", "()V"),
        make_method("toString", "()Ljava/lang/String;"),
        make_method("hashCode", "()I"),
    ];
    let class = make_class(id, "Foo", None, vec![], vec![], methods, 0, 0);

    assert!(class
        .find_method("toString", "()Ljava/lang/String;")
        .is_some());
    assert!(class.find_method("toString", "()I").is_none());
    assert!(class.find_method("missing", "()V").is_none());
}

#[test]
fn find_own_field() {
    let id = ClassId::new(0);
    let fields = vec![make_field("x"), make_field("y"), make_field("z")];
    let class = make_class(id, "Point", None, vec![], fields, vec![], 2, 5);

    // Field "x" is at local index 0 → absolute index 2 (first_field_index=2)
    let (idx, field) = class.find_own_field("x").unwrap();
    assert_eq!(idx, 2);
    assert_eq!(&*field.name, "x");

    // Field "z" is at local index 2 → absolute index 4
    let (idx, _) = class.find_own_field("z").unwrap();
    assert_eq!(idx, 4);

    assert!(class.find_own_field("missing").is_none());
}

/// JVMS §4.5 forbids two fields in one class file sharing a name **and**
/// descriptor. Sharing a NAME alone is legal, and this is the case the
/// name-only lookup answered wrongly: it returns whichever comes first in
/// declaration order, so a fieldref naming the OTHER one got that field's
/// slot index.
///
/// Asserted as a DIFFERENCE between the two keys on the same class, not as
/// "the descriptor form works": a test that only checked the new function
/// would still pass if it silently ignored the descriptor.
#[test]
fn two_fields_may_share_a_name_and_only_the_descriptor_tells_them_apart() {
    let id = ClassId::new(0);
    let fields = vec![
        make_field_desc("x", "Ljava/lang/Object;"),
        make_field_desc("x", "I"),
    ];
    let class = make_class(id, "Shadow", None, vec![], fields, vec![], 0, 2);

    // The name-only key cannot distinguish them and takes the first.
    let (idx, field) = class.find_own_field("x").unwrap();
    assert_eq!(idx, 0);
    assert_eq!(&*field.descriptor, "Ljava/lang/Object;");

    // The JVMS key reaches either one.
    let (idx0, f0) = class
        .find_own_field_by_descriptor("x", "Ljava/lang/Object;")
        .unwrap();
    assert_eq!((idx0, &*f0.descriptor), (0, "Ljava/lang/Object;"));

    let (idx1, f1) = class.find_own_field_by_descriptor("x", "I").unwrap();
    assert_eq!(
        (idx1, &*f1.descriptor),
        (1, "I"),
        "the second `x` is reachable only by descriptor, and it is the one a \
         fieldref with descriptor `I` names"
    );

    // A descriptor no field has matches nothing, rather than falling back
    // to a same-named field of another type.
    assert!(class.find_own_field_by_descriptor("x", "J").is_none());
}

/// The same distinction across a hierarchy: a subclass shadowing an
/// inherited field name with a different TYPE. `find_field_recursive` stops
/// at the subclass's field whatever the fieldref asked for; the descriptor
/// form walks past it to the one that matches.
#[test]
fn a_shadowing_subclass_field_does_not_capture_a_supertype_fieldref() {
    let mut store = ClassStore::new();
    let base_id = store.next_id();
    store.add(make_class(
        base_id,
        "Base",
        None,
        vec![],
        vec![make_field_desc("x", "Ljava/lang/String;")],
        vec![],
        0,
        1,
    ));
    let sub_id = store.next_id();
    store.add(make_class(
        sub_id,
        "Sub",
        Some(base_id),
        vec![],
        vec![make_field_desc("x", "I")],
        vec![],
        1,
        2,
    ));

    // Name only: the subclass's `int x` shadows, whatever was asked for.
    let (idx, field, declaring) = find_field_recursive(sub_id, "x", &store).unwrap();
    assert_eq!((idx, &*field.descriptor, declaring), (1, "I", sub_id));

    // With the descriptor, a fieldref for `Base.x:Ljava/lang/String;`
    // resolved from `Sub` reaches Base's field and Base's slot.
    let (idx, field, declaring) =
        find_field_recursive_by_descriptor(sub_id, "x", "Ljava/lang/String;", &store).unwrap();
    assert_eq!(
        (idx, &*field.descriptor, declaring),
        (0, "Ljava/lang/String;", base_id),
        "the descriptor key must walk PAST the shadowing field, not stop at it"
    );

    // And the subclass's own field is still reachable by its descriptor.
    let (idx, _, declaring) =
        find_field_recursive_by_descriptor(sub_id, "x", "I", &store).unwrap();
    assert_eq!((idx, declaring), (1, sub_id));

    // No field of that name and descriptor anywhere: `None`, not a
    // same-named field of another type.
    assert!(find_field_recursive_by_descriptor(sub_id, "x", "J", &store).is_none());
}

#[test]
fn field_at_index() {
    let id = ClassId::new(0);
    let fields = vec![make_field("a"), make_field("b")];
    let class = make_class(id, "Foo", None, vec![], fields, vec![], 3, 5);

    // Index 3 → local 0 → "a"
    assert_eq!(&*class.field_at_index(3).unwrap().name, "a");
    // Index 4 → local 1 → "b"
    assert_eq!(&*class.field_at_index(4).unwrap().name, "b");
    // Index 2 → belongs to superclass → None
    assert!(class.field_at_index(2).is_none());
    // Index 5 → out of bounds → None
    assert!(class.field_at_index(5).is_none());
}

/// Regression: `java.util.regex.Matcher` declares two static constants
/// (`ENDANCHOR`, `NOANCHOR`) interleaved between its instance fields.
/// `field_at_index` must map an absolute instance index back to the
/// N-th *instance* field, skipping the interleaved statics — otherwise
/// every instance field declared after a static is misresolved, which
/// caused `Matcher.find()` to loop forever on zero-width matches.
#[test]
fn field_at_index_skips_interleaved_statics() {
    let id = ClassId::new(0);
    // Declaration order: i0, i1, S(static), i2, S(static), i3
    let fields = vec![
        make_field("i0"),
        make_field("i1"),
        make_static_field("S_A"),
        make_field("i2"),
        make_static_field("S_B"),
        make_field("i3"),
    ];
    // first_field_index = 0, 4 instance fields total.
    let class = make_class(id, "Matcher", None, vec![], fields, vec![], 0, 4);

    // find_own_field gives the absolute index; field_at_index must invert it.
    for name in ["i0", "i1", "i2", "i3"] {
        let (abs, _) = class.find_own_field(name).unwrap();
        assert_eq!(
            &*class.field_at_index(abs).unwrap().name,
            name,
            "field_at_index({abs}) should round-trip back to {name}"
        );
    }
    // Direct index checks: instance fields are 0..4 regardless of statics.
    assert_eq!(&*class.field_at_index(0).unwrap().name, "i0");
    assert_eq!(&*class.field_at_index(1).unwrap().name, "i1");
    assert_eq!(&*class.field_at_index(2).unwrap().name, "i2");
    assert_eq!(&*class.field_at_index(3).unwrap().name, "i3");
    assert!(class.field_at_index(4).is_none());
}

#[test]
fn is_subclass_of_same_class() {
    let mut store = ClassStore::new();
    let id0 = store.next_id();
    store.add(make_class(id0, "Foo", None, vec![], vec![], vec![], 0, 0));

    let foo = store.get(id0).unwrap();
    assert!(foo.is_subclass_of(id0, &store));
}

#[test]
fn is_subclass_of_parent() {
    let mut store = ClassStore::new();

    // java/lang/Object (id=0)
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Foo extends Object (id=1)
    let foo_id = store.next_id();
    store.add(make_class(
        foo_id,
        "Foo",
        Some(object_id),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Bar extends Foo (id=2)
    let bar_id = store.next_id();
    store.add(make_class(
        bar_id,
        "Bar",
        Some(foo_id),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    let bar = store.get(bar_id).unwrap();

    // Bar is subclass of Bar (self)
    assert!(bar.is_subclass_of(bar_id, &store));
    // Bar is subclass of Foo (parent)
    assert!(bar.is_subclass_of(foo_id, &store));
    // Bar is subclass of Object (grandparent)
    assert!(bar.is_subclass_of(object_id, &store));

    // Foo is NOT subclass of Bar
    let foo = store.get(foo_id).unwrap();
    assert!(!foo.is_subclass_of(bar_id, &store));
}

#[test]
fn is_subclass_of_interface() {
    let mut store = ClassStore::new();

    // java/lang/Object (id=0)
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Serializable interface (id=1) — extends Object
    let serial_id = store.next_id();
    store.add(make_class(
        serial_id,
        "java/io/Serializable",
        Some(object_id),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Comparable interface (id=2) — extends Object
    let comparable_id = store.next_id();
    store.add(make_class(
        comparable_id,
        "java/lang/Comparable",
        Some(object_id),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // MyClass implements Serializable, Comparable (id=3)
    let my_id = store.next_id();
    store.add(make_class(
        my_id,
        "MyClass",
        Some(object_id),
        vec![serial_id, comparable_id],
        vec![],
        vec![],
        0,
        0,
    ));

    let my = store.get(my_id).unwrap();
    assert!(my.is_subclass_of(serial_id, &store));
    assert!(my.is_subclass_of(comparable_id, &store));
    assert!(my.is_subclass_of(object_id, &store));

    // Serializable is NOT subclass of MyClass
    let serial = store.get(serial_id).unwrap();
    assert!(!serial.is_subclass_of(my_id, &store));
}

#[test]
fn find_field_recursive_walks_superclass() {
    let mut store = ClassStore::new();

    // Object with field "hash"
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![make_field("hash")],
        vec![],
        0,
        1,
    ));

    // Foo extends Object, adds field "x"
    let foo_id = store.next_id();
    store.add(make_class(
        foo_id,
        "Foo",
        Some(object_id),
        vec![],
        vec![make_field("x")],
        vec![],
        1, // inherited 1 field from Object
        2,
    ));

    // Find "x" in Foo — should be in Foo at absolute index 1
    let (idx, field, declaring_class) = find_field_recursive(foo_id, "x", &store).unwrap();
    assert_eq!(&*field.name, "x");
    assert_eq!(idx, 1);
    assert_eq!(declaring_class, foo_id);

    // Find "hash" in Foo — should walk up to Object, absolute index 0
    let (idx, field, declaring_class) = find_field_recursive(foo_id, "hash", &store).unwrap();
    assert_eq!(&*field.name, "hash");
    assert_eq!(idx, 0);
    assert_eq!(declaring_class, object_id);

    // "missing" not found
    assert!(find_field_recursive(foo_id, "missing", &store).is_none());
}

/// JVMS §5.4.3.2 searches superinterfaces depth-first:
/// `C implements I1, I2`, `I1 extends I3`, `I3` and `I2` both declare `F`
/// — HotSpot answers `I3.F` (interpreter round i1 wave 29,
/// `i29-L4-field-resolution-searches-superinterfaces-breadth-first`).
#[test]
fn interface_fields_are_found_depth_first() {
    let mut store = ClassStore::new();
    let i3 = store.next_id();
    store.add(make_class(
        i3,
        "I3",
        None,
        vec![],
        vec![make_field("F")],
        vec![],
        0,
        1,
    ));
    let i2 = store.next_id();
    store.add(make_class(
        i2,
        "I2",
        None,
        vec![],
        vec![make_field("F")],
        vec![],
        0,
        1,
    ));
    let i1 = store.next_id();
    store.add(make_class(i1, "I1", None, vec![i3], vec![], vec![], 0, 0));
    let c = store.next_id();
    store.add(make_class(c, "C", None, vec![i1, i2], vec![], vec![], 0, 0));
    let (_, field, declaring) = find_field_recursive(c, "F", &store).unwrap();
    assert_eq!(&*field.name, "F");
    assert_eq!(
        declaring, i3,
        "I1's superinterface I3 before the second direct one, I2"
    );
}

#[test]
fn find_method_recursive_walks_superclass() {
    let mut store = ClassStore::new();

    // Object with toString
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![make_method("toString", "()Ljava/lang/String;")],
        0,
        0,
    ));

    // Foo extends Object, adds foo()
    let foo_id = store.next_id();
    store.add(make_class(
        foo_id,
        "Foo",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("foo", "()V")],
        0,
        0,
    ));

    // Find "foo" in Foo
    let (method, declaring) = find_method_recursive(foo_id, "foo", "()V", &store).unwrap();
    assert_eq!(&*method.name, "foo");
    assert_eq!(declaring, foo_id);

    // Find "toString" in Foo → walks to Object
    let (method, declaring) =
        find_method_recursive(foo_id, "toString", "()Ljava/lang/String;", &store).unwrap();
    assert_eq!(&*method.name, "toString");
    assert_eq!(declaring, object_id);

    // "bar" not found
    assert!(find_method_recursive(foo_id, "bar", "()V", &store).is_none());
}

/// `invokespecial_selection_start` — the JVMS §6.5 super-call redirect.
/// `Grandparent` <- `Parent` (overrides `m`) <- `Child`. `Child`'s
/// bytecode does `invokespecial Grandparent.m()V` (the CP entry names
/// the ancestor, not the direct superclass — the shape a synthetic
/// bridge or a multi-level `super` chain produces). Naive resolution
/// starting at `Grandparent` would find `Grandparent.m`, skipping
/// `Parent`'s override entirely; the JVMS-correct answer starts the
/// search at `Child`'s own direct superclass (`Parent`) and finds
/// `Parent.m`.
#[test]
fn invokespecial_selection_start_redirects_to_callers_direct_superclass() {
    let mut store = ClassStore::new();

    let grandparent_id = store.next_id();
    store.add(make_class(
        grandparent_id,
        "Grandparent",
        None,
        vec![],
        vec![],
        vec![make_method("m", "()V")],
        0,
        0,
    ));

    let parent_id = store.next_id();
    store.add(make_class(
        parent_id,
        "Parent",
        Some(grandparent_id),
        vec![],
        vec![],
        vec![make_method("m", "()V")],
        0,
        0,
    ));

    let child_id = store.next_id();
    store.add(make_class(
        child_id,
        "Child",
        Some(parent_id),
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // The CP entry names Grandparent directly.
    let start = invokespecial_selection_start(child_id, grandparent_id, false, "m", &store);
    assert_eq!(
        start, parent_id,
        "must redirect to the caller's direct superclass"
    );

    let (method, declaring) = find_method_recursive(start, "m", "()V", &store).unwrap();
    assert_eq!(&*method.name, "m");
    assert_eq!(
        declaring, parent_id,
        "must land on Parent's override, not Grandparent's"
    );
}

/// No redirect for `<init>` — constructors are never subject to the
/// super-call selection rule.
#[test]
fn invokespecial_selection_start_never_redirects_init() {
    let mut store = ClassStore::new();
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![make_method("<init>", "()V")],
        0,
        0,
    ));
    let child_id = store.next_id();
    store.add(make_class(
        child_id,
        "Child",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("<init>", "()V")],
        0,
        0,
    ));
    let start = invokespecial_selection_start(child_id, object_id, false, "<init>", &store);
    assert_eq!(start, object_id);
}

/// No redirect for a genuinely ordinary (non-super, e.g. private-method
/// or same-class) `invokespecial`: when the CP-referenced class is NOT
/// an ancestor of the caller, the plain CP-referenced class is returned
/// unchanged.
#[test]
fn invokespecial_selection_start_no_redirect_when_not_a_superclass() {
    let mut store = ClassStore::new();
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    let unrelated_id = store.next_id();
    store.add(make_class(
        unrelated_id,
        "Unrelated",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("m", "()V")],
        0,
        0,
    ));
    let caller_id = store.next_id();
    store.add(make_class(
        caller_id,
        "Caller",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("m", "()V")],
        0,
        0,
    ));
    // `Unrelated` is not a superclass of `Caller` — no redirect.
    let start = invokespecial_selection_start(caller_id, unrelated_id, false, "m", &store);
    assert_eq!(start, unrelated_id);
}

/// No redirect for an interface-method reference — JVMS §6.5's redirect
/// only applies when the symbolic reference names a class.
#[test]
fn invokespecial_selection_start_no_redirect_for_interface_reference() {
    let mut store = ClassStore::new();
    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    let iface_id = store.next_id();
    store.add(make_class(
        iface_id,
        "Iface",
        None,
        vec![],
        vec![],
        vec![make_method("m", "()V")],
        0,
        0,
    ));
    let child_id = store.next_id();
    store.add(make_class(
        child_id,
        "Child",
        Some(object_id),
        vec![iface_id],
        vec![],
        vec![],
        0,
        0,
    ));
    let start = invokespecial_selection_start(child_id, iface_id, true, "m", &store);
    assert_eq!(start, iface_id);
}

/// Regression test for the Infinispan `GlobalConfiguration` /
/// `GlobalConfigurationBuilder` `isClustered()` `NoSuchMethodError` bug
/// (keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod-FIXED.md).
///
/// Two closely-named, UNRELATED classes (no inheritance between them,
/// both extend plain `Object` — mirroring the real
/// `org.infinispan.configuration.global.GlobalConfiguration` /
/// `GlobalConfigurationBuilder` pair, which are siblings in the same
/// package with overlapping names but no subclass relationship). Only
/// `WidgetConfig` (standing in for `GlobalConfiguration`) declares
/// `isClustered()Z`; `WidgetConfigBuilder` (standing in for
/// `GlobalConfigurationBuilder`) does not.
///
/// The actual root cause of the bug was NOT in `find_method_recursive` —
/// it was a native `build()` override that returned the receiver
/// (`WidgetConfigBuilder`) itself instead of constructing a genuinely
/// distinct `WidgetConfig`, so runtime dispatch correctly-but-confusingly
/// resolved `isClustered()` against the Builder's (real) class identity
/// and threw NoSuchMethodError naming the Builder. This test pins the
/// class-resolution invariant that guards against a REGRESSION in the
/// other direction: `find_method_recursive` must resolve `isClustered()`
/// on the class that actually declares it and must NEVER silently
/// satisfy the lookup from the similarly-named sibling class, however the
/// receiver's class_id was obtained.
#[test]
fn find_method_recursive_does_not_confuse_similarly_named_sibling_classes() {
    let mut store = ClassStore::new();

    let object_id = store.next_id();
    store.add(make_class(
        object_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![make_method("toString", "()Ljava/lang/String;")],
        0,
        0,
    ));

    // WidgetConfigBuilder — mirrors GlobalConfigurationBuilder: siblings
    // with WidgetConfig (same package, overlapping name prefix), NOT its
    // superclass or subclass. Deliberately does NOT declare isClustered().
    let builder_id = store.next_id();
    store.add(make_class(
        builder_id,
        "com/example/config/WidgetConfigBuilder",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("build", "()Lcom/example/config/WidgetConfig;")],
        0,
        0,
    ));

    // WidgetConfig — mirrors GlobalConfiguration. Only this class
    // declares isClustered().
    let config_id = store.next_id();
    store.add(make_class(
        config_id,
        "com/example/config/WidgetConfig",
        Some(object_id),
        vec![],
        vec![],
        vec![make_method("isClustered", "()Z")],
        0,
        0,
    ));

    // Resolving isClustered() on the class that actually declares it
    // (WidgetConfig) must succeed and must be attributed to WidgetConfig.
    let (method, declaring) = find_method_recursive(config_id, "isClustered", "()Z", &store)
        .expect("isClustered() must resolve on WidgetConfig, the declaring class");
    assert_eq!(&*method.name, "isClustered");
    assert_eq!(declaring, config_id);

    // The similarly-named sibling (WidgetConfigBuilder) genuinely lacks
    // isClustered() — resolving it there must fail (a real
    // NoSuchMethodError), never silently succeed by picking up
    // WidgetConfig's method.
    assert!(find_method_recursive(builder_id, "isClustered", "()Z", &store).is_none());

    // Sanity: build() is where WidgetConfigBuilder and WidgetConfig
    // actually connect in real Infinispan bytecode. It resolves on the
    // Builder and is absent from WidgetConfig.
    let (method, declaring) = find_method_recursive(
        builder_id,
        "build",
        "()Lcom/example/config/WidgetConfig;",
        &store,
    )
    .expect("build() must resolve on WidgetConfigBuilder");
    assert_eq!(&*method.name, "build");
    assert_eq!(declaring, builder_id);
    assert!(find_method_recursive(
        config_id,
        "build",
        "()Lcom/example/config/WidgetConfig;",
        &store
    )
    .is_none());
}

#[test]
fn class_display() {
    let class = make_class(
        ClassId::new(7),
        "java/util/ArrayList",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    );
    assert_eq!(format!("{class}"), "Class[7] java/util/ArrayList");
}

#[test]
fn class_access_flags() {
    let mut class = make_class(ClassId::new(0), "Foo", None, vec![], vec![], vec![], 0, 0);
    assert!(class.is_public());
    assert!(!class.is_interface());
    assert!(!class.is_abstract());
    assert!(!class.is_final());
    assert!(!class.is_enum());

    class.access_flags =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    assert!(class.is_interface());
    assert!(class.is_abstract());
}

// -----------------------------------------------------------------------
// Tests for iterative find_field_recursive / find_method_recursive
// -----------------------------------------------------------------------

#[test]
fn find_field_iterative_deep_chain() {
    // Build a chain: C0 (has "base_field") <- C1 <- C2 <- ... <- C99
    // The field should be found at the bottom of the chain.
    let mut store = ClassStore::new();
    let depth = 100;

    let root_id = store.next_id();
    store.add(make_class(
        root_id,
        "C0",
        None,
        vec![],
        vec![make_field("base_field")],
        vec![],
        0,
        1,
    ));

    let mut prev_id = root_id;
    for i in 1..depth {
        let id = store.next_id();
        store.add(make_class(
            id,
            &format!("C{i}"),
            Some(prev_id),
            vec![],
            vec![],
            vec![],
            1,
            1,
        ));
        prev_id = id;
    }

    // Look up "base_field" from the leaf class — must walk 99 levels.
    let (idx, field, declaring) = find_field_recursive(prev_id, "base_field", &store).unwrap();
    assert_eq!(&*field.name, "base_field");
    assert_eq!(idx, 0);
    assert_eq!(declaring, root_id);

    // Missing field returns None.
    assert!(find_field_recursive(prev_id, "no_such", &store).is_none());
}

#[test]
fn find_method_iterative_deep_chain() {
    let mut store = ClassStore::new();
    let depth = 100;

    let root_id = store.next_id();
    store.add(make_class(
        root_id,
        "C0",
        None,
        vec![],
        vec![],
        vec![make_method("base_method", "()V")],
        0,
        0,
    ));

    let mut prev_id = root_id;
    for i in 1..depth {
        let id = store.next_id();
        store.add(make_class(
            id,
            &format!("C{i}"),
            Some(prev_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        prev_id = id;
    }

    let (method, declaring) =
        find_method_recursive(prev_id, "base_method", "()V", &store).unwrap();
    assert_eq!(&*method.name, "base_method");
    assert_eq!(declaring, root_id);

    assert!(find_method_recursive(prev_id, "no_such", "()V", &store).is_none());
}

#[test]
fn find_field_iterative_own_field_found_first() {
    // Child declares same field name as parent — child's version wins.
    let mut store = ClassStore::new();

    let parent_id = store.next_id();
    store.add(make_class(
        parent_id,
        "Parent",
        None,
        vec![],
        vec![make_field("x")],
        vec![],
        0,
        1,
    ));

    let child_id = store.next_id();
    store.add(make_class(
        child_id,
        "Child",
        Some(parent_id),
        vec![],
        vec![make_field("x")],
        vec![],
        1,
        2,
    ));

    let (idx, _, declaring) = find_field_recursive(child_id, "x", &store).unwrap();
    assert_eq!(declaring, child_id);
    assert_eq!(idx, 1); // child's first_field_index
}

#[test]
fn find_method_iterative_own_method_found_first() {
    let mut store = ClassStore::new();

    let parent_id = store.next_id();
    store.add(make_class(
        parent_id,
        "Parent",
        None,
        vec![],
        vec![],
        vec![make_method("run", "()V")],
        0,
        0,
    ));

    let child_id = store.next_id();
    store.add(make_class(
        child_id,
        "Child",
        Some(parent_id),
        vec![],
        vec![],
        vec![make_method("run", "()V")],
        0,
        0,
    ));

    let (_, declaring) = find_method_recursive(child_id, "run", "()V", &store).unwrap();
    assert_eq!(declaring, child_id);
}

#[test]
fn is_subclass_of_bounded_depth_limit() {
    // Build a chain deeper than MAX_HIERARCHY_DEPTH to verify it doesn't
    // stack-overflow and instead returns false when the limit is exceeded.
    let mut store = ClassStore::new();
    let chain_len = MAX_HIERARCHY_DEPTH + 10;

    let root_id = store.next_id();
    store.add(make_class(
        root_id,
        "Root",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    let mut prev_id = root_id;
    for i in 1..chain_len {
        let id = store.next_id();
        store.add(make_class(
            id,
            &format!("D{i}"),
            Some(prev_id),
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
        prev_id = id;
    }

    let leaf = store.get(prev_id).unwrap();
    // Should return false (depth exceeded) rather than stack-overflow.
    assert!(!leaf.is_subclass_of(root_id, &store));
    // Self-check still works.
    assert!(leaf.is_subclass_of(prev_id, &store));
}

// -- M2: Default interface method resolution tests -------------------------

fn make_abstract_method(name: &str, descriptor: &str) -> ClassFileMethod {
    ClassFileMethod {
        access_flags: MethodAccessFlags::ABSTRACT | MethodAccessFlags::PUBLIC,
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    }
}

fn make_default_method(name: &str, descriptor: &str) -> ClassFileMethod {
    // Default method = non-abstract on an interface (has code).
    ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC,
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    }
}

/// Build a class flagged as an `interface` (so `Class::is_interface()`
/// reports true). `make_class` hard-codes `PUBLIC | SUPER`, which is fine
/// for concrete-class tests but wrong when the test needs the node to be
/// recognised as an interface (e.g. interface-method resolution where
/// `class_id` is itself an interface).
fn make_interface(
    id: ClassId,
    name: &str,
    object_id: ClassId,
    superinterfaces: Vec<ClassId>,
    methods: Vec<ClassFileMethod>,
) -> Class {
    let mut c = make_class(
        id,
        name,
        Some(object_id),
        superinterfaces,
        vec![],
        methods,
        0,
        0,
    );
    c.access_flags =
        ClassAccessFlags::PUBLIC | ClassAccessFlags::INTERFACE | ClassAccessFlags::ABSTRACT;
    c
}

#[test]
fn m2_find_method_recursive_finds_default_on_interface() {
    let mut store = ClassStore::new();

    // Object (no superclass)
    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Interface Greeting with abstract greet() and default shout()
    let iface_id = store.next_id();
    store.add(make_class(
        iface_id,
        "Greeting",
        Some(obj_id),
        vec![],
        vec![],
        vec![
            make_abstract_method("greet", "(Ljava/lang/String;)Ljava/lang/String;"),
            make_default_method("shout", "(Ljava/lang/String;)Ljava/lang/String;"),
        ],
        0,
        0,
    ));

    // Concrete class CasualGreeting implements Greeting, has greet() but NOT shout()
    let class_id = store.next_id();
    store.add(make_class(
        class_id,
        "CasualGreeting",
        Some(obj_id),
        vec![iface_id],
        vec![],
        vec![make_method(
            "greet",
            "(Ljava/lang/String;)Ljava/lang/String;",
        )],
        0,
        0,
    ));

    // greet() is on the concrete class → found directly
    let found = find_method_recursive(
        class_id,
        "greet",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &store,
    );
    assert!(found.is_some(), "greet should be found on concrete class");
    assert_eq!(found.unwrap().1, class_id);

    // shout() is a default method on the interface → should be found via interface walk
    let found = find_method_recursive(
        class_id,
        "shout",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &store,
    );
    assert!(
        found.is_some(),
        "default method shout should be found via interface"
    );
    assert_eq!(
        found.unwrap().1,
        iface_id,
        "shout should resolve to the Greeting interface"
    );
}

#[test]
fn m2_find_method_recursive_maximally_specific_default_wins_over_superinterface() {
    // Mirrors Hibernate's SessionImpl shape: a superclass binds a
    // superinterface's default method; the subclass adds the subinterface
    // that overrides it AND re-lists the superinterface directly. The
    // maximally-specific (subinterface) default must win. The previous
    // first-BFS-match logic returned the superinterface's default — which
    // for `SharedSessionContractImplementor.asSessionImplementor` throws
    // ClassCastException in Hibernate's flush path.
    let mut store = ClassStore::new();

    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Shared interface: default f()
    let shared_id = store.next_id();
    store.add(make_interface(
        shared_id,
        "Shared",
        obj_id,
        vec![],
        vec![make_default_method("f", "()V")],
    ));

    // Impl extends Shared, overriding the default f()
    let impl_id = store.next_id();
    store.add(make_interface(
        impl_id,
        "Impl",
        obj_id,
        vec![shared_id],
        vec![make_default_method("f", "()V")],
    ));

    // AbstractShared (class) implements only Shared, declares no f().
    let abs_id = store.next_id();
    store.add(make_class(
        abs_id,
        "AbstractShared",
        Some(obj_id),
        vec![shared_id],
        vec![],
        vec![],
        0,
        0,
    ));

    // Sess extends AbstractShared, implements [Shared, Impl] — the exact
    // ordering that previously made the BFS return Shared.f() first.
    let sess_id = store.next_id();
    store.add(make_class(
        sess_id,
        "Sess",
        Some(abs_id),
        vec![shared_id, impl_id],
        vec![],
        vec![],
        0,
        0,
    ));

    let found = find_method_recursive(sess_id, "f", "()V", &store);
    assert!(found.is_some(), "default f() must resolve");
    assert_eq!(
        found.unwrap().1,
        impl_id,
        "maximally-specific Impl.f() must win over the Shared.f() superinterface default"
    );
}

#[test]
fn m2_find_method_recursive_prefers_concrete_over_abstract_interface_methods() {
    let mut store = ClassStore::new();

    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Interface with only abstract method (no default)
    let iface_id = store.next_id();
    store.add(make_interface(
        iface_id,
        "Runnable",
        obj_id,
        vec![],
        vec![make_abstract_method("run", "()V")],
    ));

    // Concrete class implementing Runnable with run()
    let class_id = store.next_id();
    store.add(make_class(
        class_id,
        "MyTask",
        Some(obj_id),
        vec![iface_id],
        vec![],
        vec![make_method("run", "()V")],
        0,
        0,
    ));

    // run() found on concrete class — the concrete declaration must win
    // over the abstract interface declaration.
    let found = find_method_recursive(class_id, "run", "()V", &store);
    assert!(found.is_some());
    assert_eq!(found.unwrap().1, class_id);

    // missing() not found anywhere
    let found = find_method_recursive(class_id, "missing", "()V", &store);
    assert!(found.is_none());
}

/// Regression: interface-method resolution must walk transitive
/// superinterfaces and return an *abstract* inherited declaration.
///
/// Mirrors the Kafka 3.7.0 boot failure: `invokeinterface
/// java/util/concurrent/ScheduledFuture.cancel(Z)Z`. `cancel(boolean)` is
/// declared abstractly on `java/util/concurrent/Future`, and
/// `ScheduledFuture` only inherits it (via `extends Delayed, Future`).
/// Resolving against the `ScheduledFuture` interface itself must still
/// find `Future.cancel` (JVMS §5.4.3.4) — previously Phase 2 skipped all
/// abstract methods and returned `None`, surfacing a spurious
/// `NoSuchMethodError`.
#[test]
fn m2_find_method_recursive_resolves_inherited_abstract_interface_method() {
    let mut store = ClassStore::new();

    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // `Future` — declares `cancel(Z)Z` abstractly.
    let future_id = store.next_id();
    store.add(make_interface(
        future_id,
        "java/util/concurrent/Future",
        obj_id,
        vec![],
        vec![make_abstract_method("cancel", "(Z)Z")],
    ));

    // `Delayed` — sibling superinterface, declares nothing relevant.
    let delayed_id = store.next_id();
    store.add(make_interface(
        delayed_id,
        "java/util/concurrent/Delayed",
        obj_id,
        vec![],
        vec![make_abstract_method(
            "getDelay",
            "(Ljava/util/concurrent/TimeUnit;)J",
        )],
    ));

    // `ScheduledFuture extends Delayed, Future` — declares no methods of
    // its own; `cancel` is inherited from `Future`.
    let scheduled_id = store.next_id();
    store.add(make_interface(
        scheduled_id,
        "java/util/concurrent/ScheduledFuture",
        obj_id,
        vec![delayed_id, future_id],
        vec![],
    ));

    // Resolving `cancel(Z)Z` against the `ScheduledFuture` interface must
    // succeed, returning the abstract declaration from `Future`.
    let found = find_method_recursive(scheduled_id, "cancel", "(Z)Z", &store);
    assert!(
        found.is_some(),
        "cancel(Z)Z inherited from Future must resolve via ScheduledFuture",
    );
    let (method, declaring) = found.unwrap();
    assert_eq!(declaring, future_id, "cancel is declared on Future");
    assert!(method.is_abstract(), "the resolved declaration is abstract");

    // A truly absent method still resolves to None.
    assert!(find_method_recursive(scheduled_id, "nope", "()V", &store).is_none());
}

#[test]
fn m2_find_method_recursive_walks_super_interfaces() {
    let mut store = ClassStore::new();

    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Super-interface with default method
    let super_iface_id = store.next_id();
    store.add(make_class(
        super_iface_id,
        "Base",
        Some(obj_id),
        vec![],
        vec![],
        vec![make_default_method("baseMethod", "()V")],
        0,
        0,
    ));

    // Sub-interface extending Base (no new methods)
    let sub_iface_id = store.next_id();
    store.add(make_class(
        sub_iface_id,
        "Extended",
        Some(obj_id),
        vec![super_iface_id],
        vec![],
        vec![],
        0,
        0,
    ));

    // Concrete class implements Extended (which extends Base)
    let class_id = store.next_id();
    store.add(make_class(
        class_id,
        "Impl",
        Some(obj_id),
        vec![sub_iface_id],
        vec![],
        vec![],
        0,
        0,
    ));

    // baseMethod() should be found on the super-interface
    let found = find_method_recursive(class_id, "baseMethod", "()V", &store);
    assert!(
        found.is_some(),
        "default method on super-interface should be found"
    );
    assert_eq!(found.unwrap().1, super_iface_id);
}

#[test]
fn m2_find_method_recursive_concrete_overrides_default() {
    let mut store = ClassStore::new();

    let obj_id = store.next_id();
    store.add(make_class(
        obj_id,
        "java/lang/Object",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));

    // Interface with default method
    let iface_id = store.next_id();
    store.add(make_class(
        iface_id,
        "Iface",
        Some(obj_id),
        vec![],
        vec![],
        vec![make_default_method("doIt", "()V")],
        0,
        0,
    ));

    // Concrete class overrides the default method
    let class_id = store.next_id();
    store.add(make_class(
        class_id,
        "Impl",
        Some(obj_id),
        vec![iface_id],
        vec![],
        vec![make_method("doIt", "()V")],
        0,
        0,
    ));

    // Should find the override on the concrete class, NOT the default
    let found = find_method_recursive(class_id, "doIt", "()V", &store);
    assert!(found.is_some());
    assert_eq!(
        found.unwrap().1,
        class_id,
        "concrete override takes priority over default"
    );
}

// -----------------------------------------------------------------------
// T10: interned accessors — verify class-level consumers of StringPool
// yield shared `Arc<str>` storage.
// -----------------------------------------------------------------------

#[test]
fn t10_intern_class_name_shared_across_loads() {
    // Build two distinct Class instances with the same name; their
    // `interned_name()` must return the same Arc<str> backing.
    let class_a = make_class(
        ClassId::new(0),
        "pkg/Shared",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    );
    let class_b = make_class(
        ClassId::new(1),
        "pkg/Shared",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    );

    let name_a = class_a.interned_name();
    let name_b = class_b.interned_name();

    assert_eq!(&*name_a, "pkg/Shared");
    assert_eq!(&*name_b, "pkg/Shared");
    assert!(
        Arc::ptr_eq(&name_a, &name_b),
        "same class name across two Class instances must share Arc<str> allocation"
    );
}

#[test]
fn t10_intern_method_name_and_descriptor_shared() {
    // Two classes each with a method `run()V` — the interned method
    // name and descriptor must share the same Arc<str> between the two.
    let methods_a = vec![make_method("run", "()V")];
    let methods_b = vec![make_method("run", "()V")];
    let class_a = make_class(ClassId::new(0), "A", None, vec![], vec![], methods_a, 0, 0);
    let class_b = make_class(ClassId::new(1), "B", None, vec![], vec![], methods_b, 0, 0);

    let name_a = class_a.interned_method_name(0).unwrap();
    let name_b = class_b.interned_method_name(0).unwrap();
    assert_eq!(&*name_a, "run");
    assert!(Arc::ptr_eq(&name_a, &name_b));

    let desc_a = class_a.interned_method_descriptor(0).unwrap();
    let desc_b = class_b.interned_method_descriptor(0).unwrap();
    assert_eq!(&*desc_a, "()V");
    assert!(Arc::ptr_eq(&desc_a, &desc_b));
}

#[test]
fn t10_intern_field_name_and_descriptor_shared() {
    let fields_a = vec![make_field("count")];
    let fields_b = vec![make_field("count")];
    let class_a = make_class(ClassId::new(0), "A", None, vec![], fields_a, vec![], 0, 1);
    let class_b = make_class(ClassId::new(1), "B", None, vec![], fields_b, vec![], 0, 1);

    let name_a = class_a.interned_field_name(0).unwrap();
    let name_b = class_b.interned_field_name(0).unwrap();
    assert_eq!(&*name_a, "count");
    assert!(Arc::ptr_eq(&name_a, &name_b));

    let desc_a = class_a.interned_field_descriptor(0).unwrap();
    let desc_b = class_b.interned_field_descriptor(0).unwrap();
    assert_eq!(&*desc_a, "I");
    assert!(Arc::ptr_eq(&desc_a, &desc_b));
}

#[test]
fn t10_intern_out_of_range_returns_none() {
    let class = make_class(ClassId::new(0), "Empty", None, vec![], vec![], vec![], 0, 0);
    assert!(class.interned_method_name(0).is_none());
    assert!(class.interned_method_descriptor(0).is_none());
    assert!(class.interned_field_name(0).is_none());
    assert!(class.interned_field_descriptor(0).is_none());
}

// -----------------------------------------------------------------------
// T10.9.C — `Class.name: Arc<str>` field migration
//
// With the `name` field now stored as a shared `Arc<str>` that originates
// from the interned constant-pool storage, two `Class` instances carrying
// the same name string must share a single backing allocation. Cloning
// the field is a refcount bump (not an allocation).
// -----------------------------------------------------------------------

#[test]
fn t10_9_c_two_loads_share_name_arc() {
    // Two Class instances with the same logical name — names were
    // interned through the global pool at construction time, so their
    // `Arc<str>` backing must be physically identical.
    let class_a = make_class(
        ClassId::new(0),
        "java/lang/String",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    );
    let class_b = make_class(
        ClassId::new(1),
        "java/lang/String",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    );

    // Simulate the `class_manager` construction path that interns the
    // name via `cratonvm_types::intern_arc`. `make_class` constructs via
    // `Arc::from(name)` directly in tests; reset both sides through the
    // global pool to verify the interning semantics hold end-to-end.
    let name1: Arc<str> = cratonvm_types::intern_arc(&class_a.name);
    let name2: Arc<str> = cratonvm_types::intern_arc(&class_b.name);

    assert_eq!(&*name1, "java/lang/String");
    assert_eq!(&*name2, "java/lang/String");
    assert!(
        Arc::ptr_eq(&name1, &name2),
        "interned class names must share a single Arc<str> allocation"
    );

    // And the direct field — if both were interned at construction time,
    // `Arc::ptr_eq` on the struct field clones returns true as well.
    let field1 = Arc::clone(&class_a.name);
    let field2 = Arc::clone(&class_b.name);
    // We verify this holds when names are funneled through the intern
    // pool, which is the path `class_manager::define_class_with_options`
    // takes at load time.
    let pooled1 = cratonvm_types::intern_arc(&field1);
    let pooled2 = cratonvm_types::intern_arc(&field2);
    assert!(Arc::ptr_eq(&pooled1, &pooled2));
}

#[test]
fn find_id_by_loader_and_name_matches_a_scan_across_loaders_and_removal() {
    let (app, u7, u8) = (
        ClassLoaderId::Application,
        ClassLoaderId::UserDefined(7),
        ClassLoaderId::UserDefined(8),
    );
    let mut store = ClassStore::new();
    // ids: 0 app Foo, 1 u7 Foo, 2 u7 Foo (second copy), 3 u8 Foo, 4 u7 Bar
    for (i, (name, loader)) in [
        ("x/Foo", app),
        ("x/Foo", u7),
        ("x/Foo", u7),
        ("x/Foo", u8),
        ("x/Bar", u7),
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = make_class(
            ClassId::new(i as u32),
            name,
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        );
        c.loader_id = loader;
        store.add(c);
    }
    let scan = |s: &ClassStore, n: &str, l: ClassLoaderId| {
        s.iter()
            .find(|c| c.loader_id == l && &*c.name == n)
            .map(|c| c.id)
    };
    let check = |s: &ClassStore| {
        for n in ["x/Foo", "x/Bar", "x/Missing"] {
            for l in [app, u7, u8, ClassLoaderId::UserDefined(9)] {
                assert_eq!(
                    s.find_id_by_loader_and_name(n, l),
                    scan(s, n, l),
                    "{n} {l:?}"
                );
            }
        }
    };
    check(&store);
    // Same name, different loaders resolve to different ids; first match wins.
    assert_eq!(
        store.find_id_by_loader_and_name("x/Foo", u7),
        Some(ClassId::new(1))
    );
    assert_eq!(
        store.find_id_by_loader_and_name("x/Foo", u8),
        Some(ClassId::new(3))
    );
    // Removing the first u7 copy exposes the second, never a tombstone.
    store.remove(ClassId::new(1));
    assert_eq!(
        store.find_id_by_loader_and_name("x/Foo", u7),
        Some(ClassId::new(2))
    );
    check(&store);
    // loader_id changes after add (stub upgrade) are seen at lookup time.
    store.get_mut(ClassId::new(0)).unwrap().loader_id = ClassLoaderId::UserDefined(9);
    assert_eq!(
        store.find_id_by_loader_and_name("x/Foo", ClassLoaderId::UserDefined(9)),
        Some(ClassId::new(0))
    );
    assert_eq!(store.find_id_by_loader_and_name("x/Foo", app), None);
    check(&store);
    // Removing the last carrier of a name drops it entirely.
    store.remove(ClassId::new(4));
    assert_eq!(store.find_id_by_loader_and_name("x/Bar", u7), None);
    store.remove(ClassId::new(2));
    store.remove(ClassId::new(3));
    store.remove(ClassId::new(0));
    check(&store);
    assert!(store.by_name.is_empty());
}


#[test]
fn unloaded_slots_are_tombstoned_and_never_reused() {
    let mut store = ClassStore::new();
    store.add(make_class(
        ClassId::new(0),
        "dead/C",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    store.add(make_class(
        ClassId::new(1),
        "live/C",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    assert!(store.remove(ClassId::new(0)).is_some());
    assert!(store.get(ClassId::new(0)).is_none());
    assert_eq!(store.len(), 1);
    assert_eq!(store.next_id(), ClassId::new(2));

    let new_id = store.add(make_class(
        ClassId::new(2),
        "new/C",
        None,
        vec![],
        vec![],
        vec![],
        0,
        0,
    ));
    assert_eq!(new_id, ClassId::new(2));
    assert!(store.get(ClassId::new(0)).is_none());
}

// -------------------------------------------------------------------
// The direct-implementor adjacency index and `subtypes_of`
// (`NOTES-cha.md` C4). These live here rather than beside the
// `unique_implementor` tests because the index is the store's, and
// `vm/src/vm/tests.rs` is `#[cfg(all(test, feature = "synthetic-jdk"))]`
// — a test of the index itself should not need a feature build.
// -------------------------------------------------------------------

/// Shorthand for the CHA fixtures below: a class with no fields, no
/// methods and no layout, named only by its position in a hierarchy.
fn add_subtype_fixture(
    store: &mut ClassStore,
    name: &str,
    superclass: Option<ClassId>,
    interfaces: Vec<ClassId>,
) -> ClassId {
    let id = store.next_id();
    store.add(make_class(
        id,
        name,
        superclass,
        interfaces,
        vec![],
        vec![],
        0,
        0,
    ));
    id
}

/// `is_subclass_of` (display + walk) against the bare walk for every
/// ordered pair of stored classes.
fn assert_display_agrees_with_walk(store: &ClassStore, ids: &[ClassId]) {
    for &s in ids {
        let class = store.get(s).expect("stored");
        for &t in ids {
            assert_eq!(
                class.is_subclass_of(t, store),
                class.is_subclass_of_walk(t, store),
                "{s} <: {t}"
            );
        }
    }
}

/// The primary display answers class targets in one compare — positive
/// and negative — and defers interface targets and roots to the walk.
#[test]
fn primary_display_answers_class_targets_and_agrees_with_the_walk() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
    let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
    let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
    let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
    let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
    let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![i]);
    let ids = [object, i, j, a, b, c, d];

    assert_eq!(store.primary_display_verdict(c, a), Some(true));
    assert_eq!(store.primary_display_verdict(c, b), Some(true));
    assert_eq!(store.primary_display_verdict(d, a), Some(false));
    assert_eq!(store.primary_display_verdict(a, c), Some(false));
    // Interface targets (implementors keys) and the root walk.
    assert_eq!(store.primary_display_verdict(c, i), None);
    assert_eq!(store.primary_display_verdict(c, j), None);
    assert_eq!(store.primary_display_verdict(c, object), None);
    assert_eq!(store.subtype_display_coverage(), (ids.len(), true));
    assert_display_agrees_with_walk(&store, &ids);
}

/// A class named as an interface whose own chain is deep (malformed, or a
/// fabricated stub) makes the walk reach a class target through an
/// interface edge; the display must stop answering negatives.
#[test]
fn a_deep_interface_key_turns_off_negative_display_answers() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
    let k = add_subtype_fixture(&mut store, "p/K", Some(a), vec![]);
    let e = add_subtype_fixture(&mut store, "p/E", Some(object), vec![k]);
    assert!(store.get(e).unwrap().is_subclass_of(a, &store));
    assert_eq!(store.primary_display_verdict(e, a), None);
    assert!(!store.subtype_display_coverage().1);
    assert_display_agrees_with_walk(&store, &[object, a, k, e]);
    // Dropping the edge restores negative answers.
    store.set_interfaces(e, vec![]);
    assert_eq!(store.primary_display_verdict(e, a), Some(false));
}

/// Re-parenting recomputes the class and its descendants; a parent added
/// after the child named it validates the child; a cycle and a removal
/// invalidate.
#[test]
fn primary_display_follows_reparenting_forward_parents_cycles_and_removal() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
    let x = add_subtype_fixture(&mut store, "p/X", Some(object), vec![]);
    let y = add_subtype_fixture(&mut store, "p/Y", Some(x), vec![]);
    assert_eq!(store.primary_display_verdict(y, a), Some(false));
    store.set_superclass(x, Some(a));
    assert_eq!(store.primary_display_verdict(y, a), Some(true));
    assert_display_agrees_with_walk(&store, &[object, a, x, y]);

    // Forward parent: `p` names an id that does not exist yet.
    let p = add_subtype_fixture(&mut store, "p/P", Some(object), vec![]);
    let future = ClassId::new(store.next_id().as_u32() + 1);
    store.set_superclass(p, Some(future));
    assert_eq!(store.primary_display_verdict(p, object), None);
    let filler = add_subtype_fixture(&mut store, "p/Filler", Some(a), vec![]);
    let q = add_subtype_fixture(&mut store, "p/Q", Some(a), vec![]);
    assert_eq!(q, future);
    assert_eq!(store.primary_display_verdict(p, a), Some(true));
    assert_display_agrees_with_walk(&store, &[object, a, x, y, p, filler, q]);

    // Cycle: a -> y -> x -> a.
    store.set_superclass(a, Some(y));
    assert_eq!(store.primary_display_verdict(y, x), None);
    assert_eq!(store.primary_display_verdict(q, a), None);
    store.set_superclass(a, Some(object));
    assert_eq!(store.primary_display_verdict(y, x), Some(true));

    // Removal invalidates the slot.
    store.remove(y);
    assert_eq!(store.primary_display_verdict(y, x), None);
}

/// Interface targets are answered from the secondary-supers closure, and
/// each of the four edge mutators empties every closure that could see
/// its change — including a closure that stopped at a forward reference.
#[test]
fn secondary_supers_answer_interface_targets_and_follow_every_edge_change() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
    let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
    let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
    let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
    let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
    let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![]);
    let all = [object, i, j, a, b, c, d];

    assert_eq!(store.primary_display_verdict(c, i), None);
    assert_eq!(store.secondary_supers_verdict(c, i), Some(true));
    assert_eq!(store.secondary_supers_verdict(c, j), Some(true));
    assert_eq!(store.secondary_supers_verdict(d, i), Some(false));
    assert_display_agrees_with_walk(&store, &all);

    // `set_interfaces` high in the DAG reaches C's closure two hops down.
    store.set_interfaces(j, vec![]);
    assert_eq!(store.secondary_supers_verdict(c, i), Some(false));
    assert_display_agrees_with_walk(&store, &all);

    // `set_superclass` moves D under B, which implements J.
    assert_eq!(store.secondary_supers_verdict(d, j), Some(false));
    store.set_superclass(d, Some(b));
    assert_eq!(store.secondary_supers_verdict(d, j), Some(true));
    assert_display_agrees_with_walk(&store, &all);

    // `add` of a forward-referenced id grows the closure that stopped there.
    let e = add_subtype_fixture(&mut store, "p/E", Some(object), vec![]);
    let future = store.next_id();
    store.set_interfaces(e, vec![future]);
    assert_eq!(store.secondary_supers_verdict(e, future), Some(true));
    assert_eq!(store.secondary_supers_verdict(e, i), Some(false));
    let f = add_subtype_fixture(&mut store, "p/F", Some(object), vec![i]);
    assert_eq!(f, future);
    assert_eq!(store.secondary_supers_verdict(e, i), Some(true));
    assert_display_agrees_with_walk(&store, &[object, i, j, a, b, c, d, e, f]);

    // `remove` shrinks it again; the removed id stays a direct edge target,
    // which the walk still matches.
    store.remove(f);
    assert_eq!(store.secondary_supers_verdict(e, i), Some(false));
    assert_eq!(store.secondary_supers_verdict(e, f), Some(true));
    assert_display_agrees_with_walk(&store, &[object, i, j, a, b, c, d, e]);
}

/// A closure larger than the walk's depth bound could disagree with the
/// bounded walk, so it is stored empty and the question walks.
#[test]
fn a_closure_past_the_walk_bound_is_not_answered() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let ifaces: Vec<ClassId> = (0..MAX_HIERARCHY_DEPTH)
        .map(|k| add_subtype_fixture(&mut store, &format!("p/I{k}"), Some(object), vec![]))
        .collect();
    let wide = add_subtype_fixture(&mut store, "p/Wide", Some(object), ifaces.clone());
    assert_eq!(store.secondary_supers_verdict(wide, ifaces[0]), None);
    assert!(store.get(wide).unwrap().is_subclass_of(ifaces[7], &store));
    let narrow =
        add_subtype_fixture(&mut store, "p/Narrow", Some(object), ifaces[..8].to_vec());
    assert_eq!(
        store.secondary_supers_verdict(narrow, ifaces[3]),
        Some(true)
    );
    assert_eq!(
        store.secondary_supers_verdict(narrow, ifaces[9]),
        Some(false)
    );
    // The lock-free view refuses a too-large closure too.
    assert_eq!(store.published_supers().verdict(wide, ifaces[0]), None);
    assert_eq!(
        store.published_supers().verdict(narrow, ifaces[3]),
        Some(true)
    );
}

/// Stage 3: the published view answers with no `&ClassStore` at all, only
/// for closures a locked question already filled, and every edge mutator
/// empties what its change can alter — and then moves
/// `class_definition_epoch`, which the per-thread and JIT subtype memos
/// are tagged with.
#[test]
fn published_supers_answer_without_the_store_and_follow_every_edge_change() {
    let mut store = ClassStore::new();
    if !store.display_enabled {
        return; // switched off by the environment
    }
    let published = store.published_supers();
    let object = add_subtype_fixture(&mut store, "java/lang/Object", None, vec![]);
    let i = add_subtype_fixture(&mut store, "p/I", Some(object), vec![]);
    let j = add_subtype_fixture(&mut store, "p/J", Some(object), vec![i]);
    let a = add_subtype_fixture(&mut store, "p/A", Some(object), vec![]);
    let b = add_subtype_fixture(&mut store, "p/B", Some(a), vec![j]);
    let c = add_subtype_fixture(&mut store, "p/C", Some(b), vec![]);
    let d = add_subtype_fixture(&mut store, "p/D", Some(object), vec![]);

    // Nothing is published until a locked question fills it.
    assert_eq!(published.verdict(c, i), None);
    assert_eq!(store.secondary_supers_verdict(c, i), Some(true));
    assert_eq!(published.verdict(c, i), Some(true));
    assert_eq!(published.verdict(c, a), Some(true));
    assert_eq!(published.verdict(c, c), Some(true));
    assert_eq!(published.verdict(c, d), Some(false));
    assert_eq!(published.published_count(), 1);
    // A class id that is not stored is never published.
    assert_eq!(
        store.secondary_supers_verdict(ClassId::new(9_999), object),
        None
    );
    assert_eq!(published.verdict(ClassId::new(9_999), object), None);

    // `set_interfaces` two hops up empties C's closure, then moves the epoch.
    let e0 = crate::class_definition_epoch();
    store.set_interfaces(j, vec![]);
    assert!(crate::class_definition_epoch() > e0);
    assert_eq!(published.verdict(c, i), None);
    assert_eq!(store.secondary_supers_verdict(c, i), Some(false));
    assert_eq!(published.verdict(c, i), Some(false));

    // `set_superclass` below it.
    assert_eq!(store.secondary_supers_verdict(d, a), Some(false));
    let e1 = crate::class_definition_epoch();
    store.set_superclass(d, Some(b));
    assert!(crate::class_definition_epoch() > e1);
    assert_eq!(published.verdict(d, a), None);
    assert_eq!(store.secondary_supers_verdict(d, a), Some(true));
    assert_eq!(published.verdict(d, a), Some(true));

    // `remove` empties the removed class's own slot (nothing reaches C);
    // a closure that never passed through it stays published.
    let e2 = crate::class_definition_epoch();
    store.remove(c);
    assert!(crate::class_definition_epoch() > e2);
    assert_eq!(published.verdict(c, a), None);
    assert_eq!(published.verdict(d, a), Some(true));
    // Removing an ancestor empties its dependants' closures.
    store.remove(b);
    assert_eq!(published.verdict(d, a), None);
    assert_eq!(store.secondary_supers_verdict(d, a), Some(false));

    // Every published answer equals the walk.
    for &s in &[object, i, j, a, d] {
        let class = store.get(s).expect("stored");
        for &t in &[object, i, j, a, b, c, d] {
            let _ = store.secondary_supers_verdict(s, t);
            if let Some(v) = published.verdict(s, t) {
                assert_eq!(v, class.is_subclass_of_walk(t, &store), "{s} <: {t}");
            }
        }
    }
}

/// The arena: a closure never straddles two chunks, the directory bounds
/// are refusals rather than panics, a full arena stops publishing, and a
/// racing second fill keeps the first.
#[test]
fn published_supers_arena_packs_bounds_and_first_fill_wins() {
    let p = PublishedSupers::new();
    // Straddle: two words left in chunk 0, a four-word closure moves on.
    *p.cursor.lock() = SUPERS_ARENA_CHUNK - 2;
    let enc = p.publish(5, &[1, 2, 3, 5]).expect("room in chunk 1");
    assert_eq!((enc >> 16) as usize, SUPERS_ARENA_CHUNK);
    assert_eq!(p.answer(enc, 3), Some(true));
    assert_eq!(p.answer(enc, 4), Some(false));
    assert_eq!(p.verdict(ClassId::new(5), ClassId::new(5)), Some(true));
    // First fill wins; the loser is handed the winner's value.
    assert_eq!(p.publish(5, &[5, 9]), Some(enc));
    assert_eq!(p.verdict(ClassId::new(5), ClassId::new(9)), Some(false));
    assert_eq!(p.published_count(), 1);
    // Too large: remembered, never answered.
    assert_eq!(p.publish(6, &[]), Some(SUPERS_TOO_LARGE));
    assert_eq!(p.verdict(ClassId::new(6), ClassId::new(6)), None);
    // Past the slot directory: refused.
    assert_eq!(p.publish(u32::MAX - 1, &[1]), None);
    assert_eq!(p.verdict(ClassId::new(u32::MAX - 1), ClassId::new(1)), None);
    // A full arena publishes nothing new.
    assert!(p.can_publish(7));
    *p.cursor.lock() = SUPERS_ARENA_CHUNK * SUPERS_ARENA_CHUNKS - 1;
    assert_eq!(p.publish(7, &[1, 7]), None);
    assert_eq!(p.verdict(ClassId::new(7), ClassId::new(7)), None);
    assert!(
        !p.can_publish(8),
        "a full arena stops the callers computing closures"
    );
    // Forgetting keeps the count exact.
    p.forget(5);
    p.forget(6);
    p.forget(5);
    assert_eq!(p.published_count(), 0);
    assert_eq!(p.verdict(ClassId::new(5), ClassId::new(5)), None);
}

/// The base case: a class naming an interface in its `interfaces` list is
/// a subtype of that interface.
///
/// The control matters as much as the assertion. `descendants_of` must
/// still answer "nothing" for the same interface, because it walks the
/// SUPERCLASS index alone and its caller
/// (`ClassManager::recompute_subclass_layouts`) consumes its result as a
/// topological order of that one relation. The two walks are separate
/// methods for exactly this reason.
#[test]
fn subtypes_of_sees_a_direct_implementor_and_descendants_of_does_not() {
    let mut store = ClassStore::new();
    let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
    let implementor = add_subtype_fixture(&mut store, "cha/Impl", None, vec![iface]);

    assert_eq!(store.subtypes_of(iface, 64), Some(vec![implementor]));
    assert!(
        store.descendants_of(iface).is_empty(),
        "descendants_of stays superclass-only: widening it would hand \
         recompute_subclass_layouts entries in the wrong order"
    );
    // Nothing is its own subtype, in either direction.
    assert_eq!(store.subtypes_of(implementor, 64), Some(vec![]));
}

/// zt t22/df10: `direct_subtype_ids` answers one level of both indices --
/// a subclass, an implementor and a sub-interface of the root -- and
/// nothing below them (the built-in `findLoadedClass` rule asks for the
/// classes whose OWN define resolved the root, JVMS §5.3.5 step 3).
#[test]
fn zt22df10_direct_subtype_ids_reads_one_level_of_both_indices() {
    let mut store = ClassStore::new();
    let root = add_subtype_fixture(&mut store, "df10/Root", None, vec![]);
    let sub = add_subtype_fixture(&mut store, "df10/Sub", Some(root), vec![]);
    let implementor = add_subtype_fixture(&mut store, "df10/Impl", None, vec![root]);
    let deeper = add_subtype_fixture(&mut store, "df10/Deeper", Some(sub), vec![]);
    let direct: Vec<ClassId> = store.direct_subtype_ids(root).collect();
    assert_eq!(direct, vec![sub, implementor]);
    assert!(!direct.contains(&deeper), "one level only");
    assert_eq!(
        store.direct_subtype_ids(sub).collect::<Vec<_>>(),
        vec![deeper]
    );
    assert_eq!(store.direct_subtype_ids(deeper).count(), 0);
}




/// A subclass of an implementor is an implementor too, which is why the
/// two relations have to be walked TOGETHER rather than one after the
/// other: reaching `Sub` from `Iface` needs an interface edge followed by
/// a superclass edge.
#[test]
fn subtypes_of_follows_a_class_edge_below_an_implementor() {
    let mut store = ClassStore::new();
    let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
    let implementor = add_subtype_fixture(&mut store, "cha/Impl", None, vec![iface]);
    let sub = add_subtype_fixture(&mut store, "cha/Sub", Some(implementor), vec![]);
    let deeper = add_subtype_fixture(&mut store, "cha/Deeper", Some(sub), vec![]);

    assert_eq!(
        store.subtypes_of(iface, 64),
        Some(vec![implementor, sub, deeper]),
        "an implementor's whole subclass tree is below the interface"
    );
    // And the class half still answers the class question on its own.
    assert_eq!(store.descendants_of(implementor), vec![sub, deeper]);
}

/// `interface Derived extends Base` is an edge like any other — it is
/// recorded in `Class::interfaces` exactly as `implements` is — so a class
/// implementing `Derived` is a subtype of `Base` two hops away.
#[test]
fn subtypes_of_follows_an_interface_extending_an_interface() {
    let mut store = ClassStore::new();
    let base = add_subtype_fixture(&mut store, "cha/Base", None, vec![]);
    let derived = add_subtype_fixture(&mut store, "cha/Derived", None, vec![base]);
    let concrete = add_subtype_fixture(&mut store, "cha/Concrete", None, vec![derived]);

    assert_eq!(store.subtypes_of(base, 64), Some(vec![derived, concrete]));
    assert_eq!(store.subtypes_of(derived, 64), Some(vec![concrete]));
}

/// A diamond reaches the same class twice — once directly, once through
/// the sub-interface — and it must be reported once. A duplicate would
/// both waste a slot against the caller's cap and make
/// `unique_implementor` read "two providers" for a hierarchy that has one.
#[test]
fn subtypes_of_reports_a_diamond_implementor_once() {
    let mut store = ClassStore::new();
    let base = add_subtype_fixture(&mut store, "cha/Base", None, vec![]);
    let derived = add_subtype_fixture(&mut store, "cha/Derived", None, vec![base]);
    // Names BOTH, which a real class file routinely does.
    let concrete = add_subtype_fixture(&mut store, "cha/Concrete", None, vec![base, derived]);

    assert_eq!(store.subtypes_of(base, 64), Some(vec![derived, concrete]));
}

/// `Class::interfaces` may only be mutated by [`ClassStore::set_interfaces`]
/// (and read everywhere else).
///
/// Nothing in the type system stops a caller from mutating
/// `Class::interfaces` through a `get_mut` and leaving the store's mirrors
/// stale — the field is `pub`, and six production sites did exactly that
/// until the CHA lane routed them through [`ClassStore::set_interfaces`].
/// Three whole-VM answers depend on the mirror being exact:
///
///  * class-hierarchy analysis (`subtypes_of`, `unique_implementor`) — a
///    missed edge silently loses a subtype, so CHA quietly binds nothing;
///  * the primary display's NEGATIVE answers
///    ([`ClassStore::primary_display_verdict`]), whose proof needs every
///    interface edge to be an `implementors` key;
///  * the secondary-supers closures
///    ([`ClassStore::secondary_supers_verdict`]), which are invalidated by
///    walking the same index backwards.
///
/// A stale mirror does not crash and does not fail at the point of the
/// mistake; the display's `debug_assert_eq!` against the walk catches it
/// only if some debug test happens to ask about that edge. That is the
/// shape of defect a source scan is for.
///
/// The scan is [`raw_edge_field_mutations`], shared with the `superclass`
/// gate below: every workspace crate's `src` and `tests` directories,
/// INCLUDING test modules (a fixture that pushes an edge raw is exactly
/// what the display cannot tolerate), every mutation spelling. The one
/// legitimate writer is the `mem::replace` in
/// [`ClassStore::set_interfaces`], exempted by its exact text.
#[test]
fn no_raw_write_to_a_stored_class_interfaces_bypasses_set_interfaces() {
    // Assembled so this scanner does not match its own source.
    let field = format!(".{}", "interfaces");
    let the_writer = format!("let old = std::mem::replace(&mut class{field}, new_interfaces);");
    let (writer_seen, offenders) = raw_edge_field_mutations("interfaces", &the_writer, &[]);
    // The scanner must still recognise the one writer it exempts, or a
    // rewrite of `set_interfaces` has turned it blind to that spelling.
    assert!(
        writer_seen,
        "the scan no longer sees `ClassStore::set_interfaces`' own write — \
         update `the_writer` together with that function"
    );
    assert!(
        offenders.is_empty(),
        "`Class::interfaces` mutated directly, bypassing \
         `ClassStore::set_interfaces` and desynchronising the implementor \
         index (class-hierarchy analysis) and the subtype display. Call \
         `ClassManager::set_interfaces` / `ClassStore::set_interfaces` \
         instead. If the receiver is not a stored `Class`, say so on the \
         line with a `// gate-ok(not-a-Class): <reason>` comment:\n  {}",
        offenders.join("\n  ")
    );
}

/// `Class::superclass` may only be written by [`ClassStore::set_superclass`].
///
/// The store keeps a direct-subclass adjacency index that
/// `recompute_subclass_layouts` walks, the primary display, and the
/// secondary-supers closures (published lock-free, [`PublishedSupers`]),
/// all maintained by that one mutator. A raw write through `get_mut`
/// compiles, runs, and looks right — and silently removes the class from
/// the layout walk (its fields then overlap the parent's on the next
/// superclass growth) and leaves a published closure answering the OLD
/// hierarchy without any lock. There is no failure at the point of the
/// mistake, which is exactly why this is a gate and not a comment.
///
/// Moved here from `class_manager.rs` (round i1 wave 5) onto the matcher
/// the `interfaces` gate uses: the old scan read `src` only and matched the
/// one spelling `.superclass = `, so `Option::take` / `replace` /
/// `insert` / `get_or_insert*`, `&mut <x>.superclass` (`mem::replace` /
/// `mem::swap`), a chain split across lines by `rustfmt`, and any write in
/// a `tests` directory all went unseen. `native-builtins/src/cds.rs` stays
/// exempt as a whole file, as before: its `superclass` is an archive
/// record's superclass NAME (`String`), with no index behind it. Anything
/// else that is not a `Class` opts a line out with a
/// `gate-ok(not-a-Class): <reason>` comment on it or the line above.
#[test]
fn superclass_is_only_ever_written_through_set_superclass() {
    // Assembled so this scanner does not match its own source.
    let field = format!(".{}", "superclass");
    let the_writer = format!("class{field} = new_super;");
    let (writer_seen, offenders) =
        raw_edge_field_mutations("superclass", &the_writer, &["native-builtins/src/cds.rs"]);
    assert!(
        writer_seen,
        "the scan no longer sees `ClassStore::set_superclass`' own write — \
         update `the_writer` together with that function"
    );
    assert!(
        offenders.is_empty(),
        "`Class::superclass` written directly, bypassing \
         `ClassStore::set_superclass` and desynchronising the \
         direct-subclass index that `recompute_subclass_layouts` walks and \
         the subtype display. Call `ClassManager::set_superclass` / \
         `ClassStore::set_superclass` instead. If the receiver is not a \
         `Class` at all — a different struct that happens to have a \
         `superclass` field — say so on the line with a \
         `// gate-ok(not-a-Class): <reason>` comment:\n  {}",
        offenders.join("\n  ")
    );
}

/// The workspace source scan behind the two edge gates above: every
/// mutation of `.<field_name>` in every workspace crate's `src` and
/// `tests` directories, as `(writer_seen, offenders)`.
///
/// It matches every mutation spelling — assignment (also `[i] = `), the
/// mutating `Vec` and `Option` methods, and `&mut <x>.<field>` (which
/// covers `mem::take` / `mem::replace` / `mem::swap`) — across line
/// breaks, since `rustfmt` splits a long `get_mut(..).expect(..).<field>
/// .push(..)` chain one call per line. String literals and comments are
/// ignored.
///
/// `the_writer` is the one legitimate writer, exempted by its exact
/// (trimmed) text in `classloading/src/class.rs`; `writer_seen` says the
/// scan still recognised it. `exempt_files` are path suffixes skipped
/// whole. A line whose receiver is not a stored `Class` opts out with a
/// `gate-ok(not-a-Class): <reason>` comment on it or on the line directly
/// above (the reason is mandatory).
fn raw_edge_field_mutations(
    field_name: &str,
    the_writer: &str,
    exempt_files: &[&str],
) -> (bool, Vec<String>) {
    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                collect_rs(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// The line with string-literal contents blanked and any `//`
    /// comment cut. Per line: a string spanning lines is scanned as code
    /// on its inner lines, which can only over-report.
    fn code_of(line: &str) -> String {
        // Char literals that would otherwise open a string.
        let line = line.replace("'\"'", "' '").replace("'\\\"'", "' '");
        let mut out = String::with_capacity(line.len());
        let mut in_str = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if in_str {
                if c == '\\' {
                    chars.next();
                } else if c == '"' {
                    in_str = false;
                    out.push('"');
                }
                continue;
            }
            if c == '"' {
                in_str = true;
                out.push('"');
            } else if c == '/' && chars.peek() == Some(&'/') {
                break;
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Is the `.<field>` occurrence between `before` and `after` a
    /// mutation? The `Vec` mutators serve `interfaces`, the `Option` ones
    /// (`take`, `replace`, `insert`, `get_or_insert*`, `as_mut`,
    /// `as_deref_mut`, `take_if`) serve `superclass`; every name listed
    /// mutates whichever of the two types has it, so one list serves both.
    fn is_mutation(before: &str, after: &str) -> bool {
        const MUTATORS: &[&str] = &[
            "take",
            "take_if",
            "replace",
            "get_or_insert",
            "get_or_insert_with",
            "get_or_insert_default",
            "as_deref_mut",
            "push",
            "pop",
            "insert",
            "remove",
            "swap_remove",
            "retain",
            "retain_mut",
            "clear",
            "truncate",
            "drain",
            "extend",
            "extend_from_slice",
            "extend_from_within",
            "append",
            "splice",
            "split_off",
            "resize",
            "resize_with",
            "dedup",
            "dedup_by",
            "dedup_by_key",
            "sort",
            "sort_by",
            "sort_by_key",
            "sort_unstable",
            "sort_unstable_by",
            "sort_unstable_by_key",
            "reverse",
            "swap",
            "fill",
            "rotate_left",
            "rotate_right",
            "iter_mut",
            "get_mut",
            "first_mut",
            "last_mut",
            "as_mut",
            "as_mut_slice",
            "as_mut_ptr",
        ];
        fn is_assignment(rest: &str) -> bool {
            rest.strip_prefix('=').is_some_and(|r| !r.starts_with('='))
        }
        let rest = after.trim_start();
        if is_assignment(rest) {
            return true;
        }
        if rest.starts_with('[') {
            if let Some(close) = rest.find(']') {
                if is_assignment(rest[close + 1..].trim_start()) {
                    return true;
                }
            }
        }
        if let Some(call) = rest.strip_prefix('.') {
            let name: String = call
                .trim_start()
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if MUTATORS.contains(&name.as_str()) {
                return true;
            }
        }
        // `&mut <receiver>.interfaces`: strip the receiver expression.
        let head = before
            .trim_end_matches(|c: char| c.is_alphanumeric() || "_.()[]:?".contains(c))
            .trim_end();
        head.ends_with("&mut")
    }

    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the classloading crate directory must have a parent")
        .to_path_buf();
    assert!(
        workspace.join("Cargo.toml").is_file(),
        "workspace root not found at {} — failing rather than scanning nothing",
        workspace.display()
    );
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&workspace).expect("workspace root is readable") {
        let crate_dir = entry.expect("readable directory entry").path();
        for sub in ["src", "tests"] {
            let dir = crate_dir.join(sub);
            if dir.is_dir() {
                collect_rs(&dir, &mut files);
            }
        }
    }
    assert!(
        files.len() > 100,
        "only {} source files found under {} — the directory walk is broken",
        files.len(),
        workspace.display()
    );

    let field = format!(".{field_name}");
    let marker = "gate-ok(not-a-Class):";

    let mut writer_seen = false;
    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&workspace)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        if exempt_files.iter().any(|e| rel.ends_with(e)) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let code: Vec<String> = lines.iter().map(|l| code_of(l)).collect();
        let joined = code.join("\n");
        for (pos, _) in joined.match_indices(field.as_str()) {
            let after = &joined[pos + field.len()..];
            // `.interfaces_foo` is another identifier.
            if after
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                continue;
            }
            if !is_mutation(&joined[..pos], after) {
                continue;
            }
            let n = joined[..pos].matches('\n').count();
            if rel.ends_with("classloading/src/class.rs") && code[n].trim() == the_writer {
                writer_seen = true;
                continue;
            }
            let excused = [Some(lines[n]), n.checked_sub(1).map(|p| lines[p])]
                .into_iter()
                .flatten()
                .any(|l| {
                    l.split(marker)
                        .nth(1)
                        .is_some_and(|reason| !reason.trim().is_empty())
                });
            if excused {
                continue;
            }
            offenders.push(format!("{rel}:{}: {}", n + 1, lines[n].trim()));
        }
    }
    (writer_seen, offenders)
}

/// The deferred-resolution shape: several production paths register a
/// class with an EMPTY interface list (loading an interface can recursively
/// demand the very class being minted) and fill it in afterwards, and the
/// synthetic-stub upgrade replaces the list wholesale. `set_interfaces` is
/// the only call that keeps the index in step with either.
#[test]
fn set_interfaces_creates_and_destroys_implementor_edges() {
    let mut store = ClassStore::new();
    let first = add_subtype_fixture(&mut store, "cha/First", None, vec![]);
    let second = add_subtype_fixture(&mut store, "cha/Second", None, vec![]);
    let impl_id = add_subtype_fixture(&mut store, "cha/Impl", None, vec![]);

    assert_eq!(store.subtypes_of(first, 64), Some(vec![]));

    store.set_interfaces(impl_id, vec![first]);
    assert_eq!(store.subtypes_of(first, 64), Some(vec![impl_id]));
    assert_eq!(
        store.get(impl_id).map(|c| c.interfaces.clone()),
        Some(vec![first]),
        "the field and the index are written together or not at all"
    );

    // The upgrade case: the real bytecode names a different interface.
    store.set_interfaces(impl_id, vec![second]);
    assert_eq!(
        store.subtypes_of(first, 64),
        Some(vec![]),
        "the old edge must be withdrawn, not merely shadowed"
    );
    assert_eq!(store.subtypes_of(second, 64), Some(vec![impl_id]));

    // A list naming the same interface twice must still produce one edge.
    store.set_interfaces(impl_id, vec![second, second]);
    assert_eq!(store.subtypes_of(second, 64), Some(vec![impl_id]));
}

/// Unloading has to clear BOTH directions, or `subtypes_of` reports
/// tombstones as subtypes: they survive `get` as `None` — so
/// `unique_implementor` skips them — but they still count against its cap,
/// which turns an answerable hierarchy into a refusal.
#[test]
fn remove_leaves_no_dangling_interface_edges() {
    let mut store = ClassStore::new();
    let iface = add_subtype_fixture(&mut store, "cha/Iface", None, vec![]);
    let one = add_subtype_fixture(&mut store, "cha/One", None, vec![iface]);
    let two = add_subtype_fixture(&mut store, "cha/Two", None, vec![iface]);
    assert_eq!(store.subtypes_of(iface, 64), Some(vec![one, two]));

    // Up-edge: the unloaded implementor is no longer under its interface.
    assert!(store.remove(one).is_some());
    assert_eq!(store.subtypes_of(iface, 64), Some(vec![two]));

    // Down-edge: the unloaded interface's own implementor list goes too.
    assert!(store.remove(iface).is_some());
    assert_eq!(
        store.subtypes_of(iface, 64),
        Some(vec![]),
        "a tombstoned interface must not keep answering for its implementors"
    );
}

/// Over the cap the walk answers `None` — "cannot answer" — and never a
/// truncated list. The caller concludes "exactly one provider" from a
/// `Some`, so a silently short list is the one way this index can produce
/// a wrong CHA answer rather than merely a missed one.
#[test]
fn subtypes_of_reports_the_cap_as_cannot_answer() {
    let mut store = ClassStore::new();
    let iface = add_subtype_fixture(&mut store, "cha/Wide", None, vec![]);
    for i in 0..5 {
        add_subtype_fixture(&mut store, &format!("cha/Impl{i}"), None, vec![iface]);
    }

    assert_eq!(store.subtypes_of(iface, 4), None);
    assert_eq!(
        store.subtypes_of(iface, 5).map(|v| v.len()),
        Some(5),
        "exactly at the cap is still an answer"
    );
    // The cap is on the COMBINED walk: three implementors plus two
    // subclasses of one of them is five subtypes, not two indices of three.
    let mut store = ClassStore::new();
    let iface = add_subtype_fixture(&mut store, "cha/Mixed", None, vec![]);
    let impl_id = add_subtype_fixture(&mut store, "cha/MixedImpl", None, vec![iface]);
    add_subtype_fixture(&mut store, "cha/MixedSubA", Some(impl_id), vec![]);
    add_subtype_fixture(&mut store, "cha/MixedSubB", Some(impl_id), vec![]);
    assert_eq!(store.subtypes_of(iface, 2), None);
    assert_eq!(store.subtypes_of(iface, 3).map(|v| v.len()), Some(3));
}

/// gcd d1/c (`gengc-r5w4-conc8-class-store-remove-unregisters-another-domains-layout`):
/// two stores share class id N; the first owns N's layout slot. The second
/// store removing its own class N must leave the first store's layout
/// registered. (Staged at a high id so no other test's store is likely to
/// have claimed it; if one did, there is nothing to stage and the test
/// says so by returning.)
#[test]
fn gcd_d1c_removing_a_class_leaves_another_stores_layout_at_its_id() {
    use cratonvm_types::{
        class_layout, register_class_layout, CompactLayout, FieldStorageKind,
    };
    const N: u32 = 700;
    let owner = ClassStore::new();
    let mut other = ClassStore::new();
    let layout = std::sync::Arc::new(CompactLayout {
        field_disps: vec![8],
        is_ref: vec![true],
        field_kinds: vec![FieldStorageKind::Reference],
        ref_disps: vec![8],
        total_size: 16,
    });
    register_class_layout(owner.layout_domain(), N, std::sync::Arc::clone(&layout));
    let ours = || class_layout(N).is_some_and(|l| std::sync::Arc::ptr_eq(&l, &layout));
    if !ours() {
        eprintln!("slot {N} already belongs to another store in this binary; nothing to stage");
        return;
    }
    for i in 0..=N {
        other.add(make_class(
            ClassId::new(i),
            "gcd/Shared",
            None,
            vec![],
            vec![],
            vec![],
            0,
            0,
        ));
    }
    assert!(ours(), "the second store's registration at N was refused");
    assert!(owner.owns_layout_slot(N));
    assert!(!other.owns_layout_slot(N));
    assert!(other.remove(ClassId::new(N)).is_some());
    assert!(
        ours(),
        "removing the second store's class N cleared the first store's layout"
    );
    cratonvm_types::unregister_class_layout(N);
    assert!(class_layout(N).is_none());
}
