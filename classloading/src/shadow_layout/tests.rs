use super::*;
use crate::class::{ClassLoaderId, ClassState};
use crate::class_origin::ClassOrigin;
use cratonvm_reader::class_access_flags::{ClassAccessFlags, FieldAccessFlags};
use cratonvm_reader::class_file_version::ClassFileVersion;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

fn field(name: &str, desc: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::empty(),
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(desc),
        attributes: vec![],
    }
}

fn static_field(name: &str, desc: &str) -> ClassFileField {
    ClassFileField {
        access_flags: FieldAccessFlags::STATIC,
        name: cratonvm_types::intern_arc(name),
        descriptor: cratonvm_types::intern_arc(desc),
        attributes: vec![],
    }
}

fn anon(n: usize) -> Vec<ClassFileField> {
    (0..n)
        .map(|i| field(&format!("_f{i}"), "Ljava/lang/Object;"))
        .collect()
}

fn make_class(
    id: ClassId,
    name: &str,
    superclass: Option<ClassId>,
    fields: Vec<ClassFileField>,
    first_field_index: usize,
    num_total_fields: usize,
) -> Class {
    Class {
        id,
        loader_id: ClassLoaderId::Bootstrap,
        name: cratonvm_types::intern_arc(name),
        source_file: None,
        version: ClassFileVersion::JAVA_8,
        state: ClassState::Loaded,
        initializing_thread: None,
        constant_pool: ConstantPool::new(vec![ConstantPoolEntry::Tombstone]),
        access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        superclass,
        interfaces: vec![],
        fields,
        methods: vec![],
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

/// Register a real (non-stub) class with `fields`, optionally under a
/// superclass, and return its id.
fn add_real(
    store: &mut ClassStore,
    name: &str,
    superclass: Option<ClassId>,
    fields: Vec<ClassFileField>,
) -> ClassId {
    let first_field_index = superclass
        .and_then(|s| store.get(s))
        .map_or(0, |c| c.num_total_fields);
    let own_instance = fields.iter().filter(|f| !f.is_static()).count();
    let id = store.next_id();
    let class = make_class(
        id,
        name,
        superclass,
        fields,
        first_field_index,
        first_field_index + own_instance,
    );
    store.add(class);
    id
}

#[test]
fn anonymous_model_over_a_real_primitive_is_a_type_mismatch() {
    let mut store = ClassStore::new();
    // Real class: slot 0 reference, slot 1 int.
    let cid = add_real(
        &mut store,
        "java/util/Fake",
        None,
        vec![field("defaults", "Ljava/util/Fake;"), field("count", "I")],
    );
    let diff = diff_against_model(&store, cid, &anon(2)).expect("diff");
    assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::Agrees);
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::TypeMismatch);
    assert!(diff.is_disagreeing(1));
    assert!(!diff.is_disagreeing(0));
    assert_eq!(diff.disagreement_count(), 1);
}

/// Gap 2, the one the type-based hunter cannot see: our model and the image
/// agree that the slot holds an `int`, and it is still the wrong `int`.
/// Only a NAMED model can say so.
#[test]
fn named_model_catches_a_same_kind_wrong_slot() {
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/util/Fake",
        None,
        vec![field("threshold", "I"), field("modCount", "I")],
    );
    let model = vec![field("size", "I"), field("modCount", "I")];
    let diff = diff_against_model(&store, cid, &model).expect("diff");
    assert_eq!(
        diff.slot(0).unwrap().verdict,
        SlotVerdict::NameMismatch,
        "model `size` over real `threshold` — same type class, different field"
    );
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::Agrees);
    assert_eq!(diff.disagreement_count(), 1);
}

/// Kind 3: the model parks a VM value on a slot the JDK owns. Neither
/// alternative spelling reports it — an anonymous `_fN` over a real
/// reference is unfalsifiable, and naming the slot after the real field
/// makes the diff *agree* with the overlay. `_vmN` is the only spelling
/// that says "a value with no home is living here".
#[test]
fn a_vm_internal_slot_over_a_real_field_is_reported() {
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/io/Writer",
        None,
        vec![
            field("writeBuffer", "[C"),
            field("lock", "Ljava/lang/Object;"),
        ],
    );
    // What `java/io/BufferedWriter`'s model says today.
    let model = vec![
        field("_vm0", "Ljava/lang/Object;"),
        field("lock", "Ljava/lang/Object;"),
    ];
    let diff = diff_against_model(&store, cid, &model).expect("diff");
    assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::VmInternal);
    assert_eq!(diff.slot(0).unwrap().verdict.tag(), "VM");
    assert!(diff.is_disagreeing(0));
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::Agrees);
    assert_eq!(diff.disagreement_count(), 1);

    // The control: spelling the same slot anonymously reports NOTHING, which
    // is the state this verdict exists to end. If this half ever starts
    // failing, `_vmN` has stopped being load-bearing.
    let anonymous = vec![
        field("_f0", "Ljava/lang/Object;"),
        field("lock", "Ljava/lang/Object;"),
    ];
    let quiet = diff_against_model(&store, cid, &anonymous).expect("diff");
    assert_eq!(quiet.disagreement_count(), 0);
}

/// A `_vmN` past the real field count is the SHAPE TO AIM FOR — the value
/// sits on padding nobody owns — so it must not be reported. Otherwise
/// fixing an overlay by anchoring it past the layout would look like
/// causing one.
#[test]
fn a_vm_internal_slot_anchored_past_the_layout_is_a_pad() {
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/io/Writer",
        None,
        vec![field("writeBuffer", "[C")],
    );
    let model = vec![
        field("writeBuffer", "[C"),
        field("_vm1", "Ljava/lang/Object;"),
    ];
    let diff = diff_against_model(&store, cid, &model).expect("diff");
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::ModelOverruns);
    assert_eq!(diff.disagreement_count(), 0);
}

#[test]
fn model_slots_past_the_real_layout_are_pad_not_disagreements() {
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/util/Fake",
        None,
        vec![field("map", "Ljava/util/Map;")],
    );
    let diff = diff_against_model(&store, cid, &anon(3)).expect("diff");
    assert_eq!(diff.slot(0).unwrap().verdict, SlotVerdict::Agrees);
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::ModelOverruns);
    assert_eq!(diff.slot(2).unwrap().verdict, SlotVerdict::ModelOverruns);
    assert_eq!(
        diff.disagreement_count(),
        0,
        "a pad slot belongs to no real field, so writing it corrupts nothing"
    );
}

/// The real `java.util.Properties` shape: the inherited `Hashtable` fields
/// push the interesting slots down, which is the whole reason a model
/// numbered from 0 lands on the wrong ones.
#[test]
fn inherited_fields_are_walked_so_absolute_indices_line_up() {
    let mut store = ClassStore::new();
    let sup = add_real(
        &mut store,
        "java/util/Hashtable",
        None,
        vec![
            field("table", "[Ljava/util/Hashtable$Entry;"),
            field("count", "I"),
            field("threshold", "I"),
            field("loadFactor", "F"),
        ],
    );
    let cid = add_real(
        &mut store,
        "java/util/Properties",
        Some(sup),
        vec![field("defaults", "Ljava/util/Properties;")],
    );
    let diff = diff_against_model(&store, cid, &anon(6)).expect("diff");
    assert_eq!(diff.slot(0).unwrap().real_name.as_deref(), Some("table"));
    assert_eq!(diff.slot(1).unwrap().real_name.as_deref(), Some("count"));
    assert_eq!(
        diff.slot(3).unwrap().real_name.as_deref(),
        Some("loadFactor")
    );
    assert_eq!(diff.slot(4).unwrap().real_name.as_deref(), Some("defaults"));
    // 1, 2 and 3 are primitives under an anonymous reference model.
    assert_eq!(diff.slot(1).unwrap().verdict, SlotVerdict::TypeMismatch);
    assert_eq!(diff.slot(2).unwrap().verdict, SlotVerdict::TypeMismatch);
    assert_eq!(diff.slot(3).unwrap().verdict, SlotVerdict::TypeMismatch);
    assert_eq!(diff.slot(5).unwrap().verdict, SlotVerdict::ModelOverruns);
}

/// `Class::fields` interleaves statics with instance fields, so a walker
/// that indexes the raw vec reports the wrong field. `Matcher` is the
/// in-tree example (two static constants in the middle of its instance
/// fields) and it has already caused one silent mis-typing.
#[test]
fn static_fields_do_not_shift_the_instance_indices() {
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/util/Fake",
        None,
        vec![
            static_field("serialVersionUID", "J"),
            field("key", "[B"),
            static_field("NOANCHOR", "I"),
            field("algorithm", "Ljava/lang/String;"),
        ],
    );
    let diff = diff_against_model(&store, cid, &anon(2)).expect("diff");
    assert_eq!(diff.slot(0).unwrap().real_name.as_deref(), Some("key"));
    assert_eq!(
        diff.slot(1).unwrap().real_name.as_deref(),
        Some("algorithm")
    );
    assert_eq!(diff.disagreement_count(), 0);
}

#[test]
fn a_synthetic_stub_has_no_real_layout_to_disagree_with() {
    let mut store = ClassStore::new();
    let id = store.next_id();
    let mut class = make_class(id, "java/util/Fake", None, anon(2), 0, 2);
    class.set_origin(crate::class_origin::ClassOrigin::compatibility_stub(
        "test fixture",
    ));
    store.add(class);
    assert!(diff_against_model(&store, id, &anon(2)).is_none());
}

#[test]
fn an_empty_model_produces_no_diff() {
    let mut store = ClassStore::new();
    let cid = add_real(&mut store, "java/util/Fake", None, vec![field("a", "I")]);
    assert!(diff_against_model(&store, cid, &[]).is_none());
    assert!(diff_against_model(&store, cid, &[static_field("S", "I")]).is_none());
}

/// The injected-violation test for gaps 1 and 2, run against the
/// **production** model table rather than a fixture of one.
///
/// `java/util/Properties` is `instance_fields(16)` — sixteen anonymous
/// reference slots — and a real `Properties` inherits `Hashtable`'s
/// primitives at 1..=4. Every one of those is a slot on which a positional
/// native access is addressing a different field than it thinks, and none
/// of them has a per-access tell unless the value happens to be a reference.
///
/// A detector that reports nothing on injected input is decoration; this
/// asserts a floor on what it finds, so a table edit that quietly empties
/// the model fails here instead of turning the census silently green.
#[test]
fn the_production_model_for_properties_disagrees_with_a_real_hashtable_layout() {
    let model = crate::class_manager::synthetic_stub_field_model("java/util/Properties");
    assert!(
        model.iter().filter(|f| !f.is_static()).count() >= 8,
        "the Properties model must still have slots for this test to mean anything"
    );
    let mut store = ClassStore::new();
    // JDK 25 `java.util.Hashtable`, in declaration order.
    let sup = add_real(
        &mut store,
        "java/util/Hashtable",
        None,
        vec![
            field("table", "[Ljava/util/Hashtable$Entry;"),
            field("count", "I"),
            field("threshold", "I"),
            field("loadFactor", "F"),
            field("modCount", "I"),
        ],
    );
    let cid = add_real(
        &mut store,
        "java/util/Properties",
        Some(sup),
        vec![
            field("defaults", "Ljava/util/Properties;"),
            field("map", "Ljava/util/concurrent/ConcurrentHashMap;"),
        ],
    );
    let diff = diff_against_model(&store, cid, &model).expect("Properties has a model");
    for slot in [1usize, 2, 3, 4] {
        assert_eq!(
            diff.slot(slot).unwrap().verdict,
            SlotVerdict::TypeMismatch,
            "slot {slot} is an inherited Hashtable primitive under a reference model",
        );
        assert!(diff.is_disagreeing(slot));
    }
    // Slot 3 is the measured one: `PROPS_FIELD_DEFAULTS` in
    // `native-collections`, `float loadFactor` in the image.
    assert_eq!(
        diff.slot(3).unwrap().real_name.as_deref(),
        Some("loadFactor")
    );
    assert!(diff.disagreement_count() >= 4);
    // And the rendering has to actually name it, or the census is useless.
    let rendered = diff.render();
    assert!(rendered.contains("loadFactor"), "{rendered}");
    assert!(rendered.contains("TYPE"), "{rendered}");
}

/// Wave-2 step 4's non-vacuity check: the claim checker must go RED on a
/// layout that contradicts a claim, and green on the one it was written
/// against.
///
/// Both halves matter. A checker that never fires is decoration, and one
/// that fires on the correct layout would have to be switched off the first
/// time it cried wolf.
#[test]
fn a_safe_positional_claim_is_checked_against_the_image() {
    // JDK 9+ `java.lang.String`, in declaration order — what every claim
    // about it asserts.
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/lang/String",
        None,
        vec![
            field("value", "[B"),
            field("coder", "B"),
            field("hash", "I"),
            field("hashIsZero", "Z"),
        ],
    );
    assert!(
        check_positional_claims(&store, cid).is_empty(),
        "the claims must hold against the layout they were written for"
    );

    // Pre-9 `String`: `char[] value; int hash;`. Slot 1 is `hash`, not
    // `coder` — the exact regression the marker's own text warns about.
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/lang/String",
        None,
        vec![field("value", "[C"), field("hash", "I")],
    );
    let broken = check_positional_claims(&store, cid);
    assert_eq!(
        broken.len(),
        4,
        "all four String claims should break on a pre-9 layout, got {broken:?}"
    );
    assert!(broken[0].contains("value"), "{broken:?}");
    assert!(broken[1].contains("coder"), "{broken:?}");
    assert!(
        broken[3].contains("does not exist"),
        "slot 3 is past a two-field layout: {broken:?}"
    );

    // A same-name, different-TYPE field must still break the claim: two
    // ints reorder without a name check noticing.
    let mut store = ClassStore::new();
    let cid = add_real(
        &mut store,
        "java/util/concurrent/atomic/AtomicInteger",
        None,
        vec![field("value", "J")],
    );
    assert_eq!(check_positional_claims(&store, cid).len(), 1);
}

/// A class with no claim, and a fabricated stub, are both silent — so the
/// tripwire cannot become background noise on a synthetic boot.
#[test]
fn claims_are_silent_for_unclaimed_and_fabricated_classes() {
    let mut store = ClassStore::new();
    let cid = add_real(&mut store, "java/util/Fake", None, vec![field("a", "I")]);
    assert!(check_positional_claims(&store, cid).is_empty());

    let id = store.next_id();
    let mut class = make_class(id, "java/lang/String", None, anon(1), 0, 1);
    class.set_origin(crate::class_origin::ClassOrigin::compatibility_stub(
        "test fixture",
    ));
    store.add(class);
    assert!(
        check_positional_claims(&store, id).is_empty(),
        "a fabricated stub has no image to contradict a claim"
    );
}

/// Every claim names a site and a plausible descriptor, and the table is
/// not empty — the cheapest way for step 4 to become vacuous is for
/// somebody to empty this table while the checker stays wired in.
#[test]
fn every_safe_claim_names_a_real_site() {
    assert!(SAFE_POSITIONAL_CLAIMS.len() >= 6);
    for c in SAFE_POSITIONAL_CLAIMS {
        assert!(!c.class.is_empty() && c.class.contains('/'), "{c:?}");
        assert!(!c.name.is_empty(), "{c:?}");
        assert!(!c.descriptor.is_empty(), "{c:?}");
        assert!(c.site.contains(".rs"), "a claim must name its code: {c:?}");
    }
}

#[test]
fn anonymous_model_field_recognition() {
    assert!(is_anonymous_model_field("_f0"));
    assert!(is_anonymous_model_field("_f17"));
    assert!(!is_anonymous_model_field("_f"));
    assert!(!is_anonymous_model_field("_fx"));
    assert!(!is_anonymous_model_field("loadFactor"));
    assert!(!is_anonymous_model_field("f0"));
}
