use super::SupertypeLinks;

/// `CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`: a row answers only for its own
/// loader and name, a repeat adds nothing, and the table starts over at
/// its cap rather than growing without bound.
#[test]
fn rows_are_per_loader_and_the_table_clears_at_its_cap() {
    let mut links = SupertypeLinks::default();
    assert!(!links.contains(3, "java/io/Serializable"));
    links.insert(3, "java/io/Serializable");
    assert!(links.contains(3, "java/io/Serializable"));
    assert!(
        !links.contains(4, "java/io/Serializable"),
        "another loader asks for itself"
    );
    assert!(!links.contains(3, "java/lang/Cloneable"));
    links.insert(3, "java/io/Serializable");
    assert_eq!(links.len(), 1, "a repeat is one row");

    for i in 0..SupertypeLinks::CAP - 1 {
        links.insert(5, &format!("p/C{i}"));
    }
    assert_eq!(links.len(), SupertypeLinks::CAP);
    assert!(links.contains(3, "java/io/Serializable"));
    links.insert(6, "p/Over");
    assert_eq!(
        links.len(),
        1,
        "the full table was cleared before the new row"
    );
    assert!(links.contains(6, "p/Over"));
    assert!(!links.contains(3, "java/io/Serializable"));
}

/// A row keeps the class the loader answered (interpreter round i4 wave
/// 96, lane L5): a later define that skips the ask links that class. A
/// repeat without an identity keeps the one recorded; one with an
/// identity replaces it.
#[test]
fn a_row_keeps_the_answered_class() {
    use cratonvm_types::ClassId;
    let mut links = SupertypeLinks::default();
    links.insert_answer(5, "api/Svc", Some(ClassId::new(41)));
    assert!(links.contains(5, "api/Svc"));
    assert_eq!(links.answer(5, "api/Svc"), Some(ClassId::new(41)));
    assert_eq!(links.answer(6, "api/Svc"), None, "another loader");
    links.insert(5, "api/Svc");
    assert_eq!(links.answer(5, "api/Svc"), Some(ClassId::new(41)));
    links.insert_answer(5, "api/Svc", Some(ClassId::new(42)));
    assert_eq!(links.answer(5, "api/Svc"), Some(ClassId::new(42)));
    assert_eq!(links.len(), 1);
    links.insert(5, "api/Other");
    assert!(links.contains(5, "api/Other"));
    assert_eq!(links.answer(5, "api/Other"), None);
}

/// Without a per-VM slot (mock contexts) nothing is known and nothing is
/// recorded: the pre-pass asks the loader every time, as without the
/// switch.
#[test]
fn a_context_without_a_vm_slot_knows_no_link() {
    let ctx = crate::test_utils::MockNativeContext::new();
    super::note_supertype_link(&ctx, 7, "java/io/Serializable", None);
    assert!(!super::supertype_link_known(
        &ctx,
        7,
        "java/io/Serializable"
    ));
    assert!(!super::supertype_link_known(
        &ctx,
        1,
        "java/io/Serializable"
    ));
}
