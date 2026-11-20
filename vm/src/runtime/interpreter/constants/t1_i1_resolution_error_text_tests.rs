use super::resolution_error_entry_text;
use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry as E};

fn pool() -> ConstantPool {
    ConstantPool::new(vec![
        E::Tombstone,
        E::Utf8("p/Missing".into()), // 1
        E::ClassReference { name_index: 1 }, // 2
        E::Utf8("target".into()), // 3
        E::Utf8("(Lp/Missing;)V".into()), // 4
        E::NameAndType {
            name_index: 3,
            descriptor_index: 4,
        }, // 5
        E::MethodReference {
            class_index: 2,
            name_and_type_index: 5,
        }, // 6
        E::MethodHandle {
            reference_kind: 6,
            reference_index: 6,
        }, // 7
        E::MethodType { descriptor_index: 4 }, // 8
        E::Utf8("CONDY".into()), // 9
        E::NameAndType {
            name_index: 9,
            descriptor_index: 1,
        }, // 10
        E::Dynamic {
            bootstrap_method_attr_index: 0,
            name_and_type_index: 10,
        }, // 11
        E::Integer(1), // 12
    ])
}

#[test]
fn each_resolvable_tag_names_its_own_text() {
    let pool = pool();
    // A class entry: its name as written in the pool (internal form).
    assert_eq!(resolution_error_entry_text(&pool, 2).as_deref(), Some("p/Missing"));
    // A method handle: the referenced member's NAME.
    assert_eq!(resolution_error_entry_text(&pool, 7).as_deref(), Some("target"));
    // A method type: its descriptor.
    assert_eq!(resolution_error_entry_text(&pool, 8).as_deref(), Some("(Lp/Missing;)V"));
    // A dynamic constant: its name.
    assert_eq!(resolution_error_entry_text(&pool, 11).as_deref(), Some("CONDY"));
}

#[test]
fn other_tags_and_bad_indices_record_no_text() {
    let pool = pool();
    assert_eq!(resolution_error_entry_text(&pool, 6), None);
    assert_eq!(resolution_error_entry_text(&pool, 12), None);
    assert_eq!(resolution_error_entry_text(&pool, 99), None);
}

/// Every recording path goes through the HotSpot-shaped record.
#[test]
fn both_recording_doors_use_the_entry_record() {
    let src = include_str!("../constants.rs").replace("\r\n", "\n");
    let door = |name: &str| {
        let start = src.find(&format!("\npub(crate) fn {name}(")).expect("door exists");
        let rest = &src[start + 1..];
        rest[..rest.find("\n}\n").expect("door ends")].to_string()
    };
    for name in ["record_resolution_failure", "record_resolution_failure_as_of"] {
        assert!(
            door(name).contains("linkage_error_record_for_entry(shared, class_id, cp_index,"),
            "{name} records through linkage_error_record_for_entry"
        );
    }
}
