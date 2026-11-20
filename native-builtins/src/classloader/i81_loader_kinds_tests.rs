use super::{LoaderKind, LoaderKinds};

/// `CRATONVM_LOADER_KIND_MEMO`: a row answers only for its own class id,
/// a repeat replaces in place, and the table starts over at its cap
/// rather than growing without bound.
#[test]
fn rows_are_per_class_and_the_table_clears_at_its_cap() {
    let user = LoaderKind {
        user_defined: true,
        bare_url: false,
    };
    let bare = LoaderKind {
        user_defined: false,
        bare_url: true,
    };
    let mut kinds = LoaderKinds::default();
    assert_eq!(kinds.get(7), None);
    kinds.insert(7, user);
    kinds.insert(9, bare);
    assert_eq!(kinds.get(7), Some(user));
    assert_eq!(kinds.get(9), Some(bare));
    assert_eq!(kinds.get(8), None, "another class asks for itself");
    kinds.insert(7, user);
    assert_eq!(kinds.len(), 2, "a repeat is one row");

    for id in 100..(100 + LoaderKinds::CAP as u32 - 2) {
        kinds.insert(id, user);
    }
    assert_eq!(kinds.len(), LoaderKinds::CAP);
    kinds.insert(7, bare);
    assert_eq!(
        kinds.len(),
        LoaderKinds::CAP,
        "replacing a held row does not clear"
    );
    assert_eq!(kinds.get(7), Some(bare));
    kinds.insert(5, user);
    assert_eq!(kinds.len(), 1, "a new row at the cap starts the table over");
    assert_eq!(kinds.get(5), Some(user));
    assert_eq!(kinds.get(7), None);
}
