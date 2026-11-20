use super::*;

fn watched(class_name: &str, name: &str, descriptor: &str) -> WatchedField {
    WatchedField {
        class_name: class_name.to_string(),
        name: name.to_string(),
        descriptor: descriptor.to_string(),
    }
}

#[test]
fn a_reference_through_the_declaring_class_or_an_application_class_may_name_the_field() {
    let w = watched("app/Counter", "count", "I");
    assert!(field_ref_may_name("app/Counter", "count", "I", &w));
    assert!(field_ref_may_name("app/SubCounter", "count", "I", &w), "a subclass may");
    assert!(!field_ref_may_name("app/Counter", "count", "J", &w), "another descriptor");
    assert!(!field_ref_may_name("app/Counter", "total", "I", &w), "another name");
}

#[test]
fn a_jdk_class_reference_never_names_an_application_field() {
    let w = watched("app/Counter", "count", "I");
    assert!(!field_ref_may_name("java/lang/StringBuilder", "count", "I", &w));
    let jdk = watched("java/util/ArrayList", "size", "I");
    assert!(field_ref_may_name("java/util/ArrayList", "size", "I", &jdk));
    assert!(field_ref_may_name("app/MyList", "size", "I", &jdk), "an app subclass may");
    assert!(field_ref_may_name("java/util/Other", "size", "I", &jdk), "JDK to JDK may");
}

/// A pool `[_, Utf8 class, Class #1, Utf8 name, Utf8 desc, NameAndType,
/// Fieldref]`: one `Fieldref` to `class.name:desc`.
fn pool_with_fieldref(
    class: &str,
    name: &str,
    desc: &str,
) -> cratonvm_reader::constant_pool::ConstantPool {
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};
    ConstantPool::new(vec![
        E::Tombstone,
        E::Utf8(class.into()),
        E::ClassReference { name_index: 1 },
        E::Utf8(name.into()),
        E::Utf8(desc.into()),
        E::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        },
        E::FieldReference {
            class_index: 2,
            name_and_type_index: 5,
        },
    ])
}

/// Interpreter round i6 wave 116, lane L1: a redefinition's new class
/// file joins the scope when it names a watched field (a JDK class's
/// degrades it), stays out otherwise, and an unreadable file joins and
/// degrades (nothing is known about it).
#[test]
fn a_redefinition_that_names_a_watched_field_joins_before_its_install() {
    let fields = [watched("app/Counter", "count", "I")];
    let names = pool_with_fieldref("app/Counter", "count", "I");
    assert_eq!(
        redefinition_verdict(Some(("app/Bumper", &names)), &fields),
        RedefinitionVerdict::Joins { degrades: false }
    );
    assert_eq!(
        redefinition_verdict(Some(("java/util/Bumper", &names)), &fields),
        RedefinitionVerdict::Joins { degrades: true },
        "a JDK class that names it degrades"
    );
    let other = pool_with_fieldref("app/Counter", "total", "I");
    assert_eq!(
        redefinition_verdict(Some(("app/Bumper", &other)), &fields),
        RedefinitionVerdict::Stays
    );
    assert_eq!(
        redefinition_verdict(None, &fields),
        RedefinitionVerdict::Joins { degrades: true }
    );
}

#[test]
fn the_scope_starts_empty_and_holds_what_it_stores() {
    let scope = FieldWatchScope::new();
    assert!(!scope.armed());
    assert!(!scope.contains(70));
    assert_eq!(scope.store_classes(&[70], false, usize::MAX), vec![70]);
    assert!(scope.contains(70));
    assert!(!scope.contains(70 + 64), "same bit, not in the set");
    assert!(scope.take_grew());
    assert!(!scope.take_grew(), "nothing changed since");
    assert!(scope.store_classes(&[70], false, usize::MAX).is_empty());
    assert!(!scope.take_grew(), "storing a class already there is no growth");
    scope.clear();
    assert!(!scope.contains(70));
    assert!(!scope.serves(&[1]), "never armed");
}
