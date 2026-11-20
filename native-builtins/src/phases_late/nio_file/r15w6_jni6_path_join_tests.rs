use super::p57_get_path_join;

fn join(first: &str, more: &[&str]) -> String {
    let mut acc = first.to_string();
    for part in more {
        p57_get_path_join(&mut acc, part);
    }
    acc
}

/// Failed before K5: `Path.of` resolved `"/b"` onto `"a"` (answering
/// `/b`) and `Paths.get("", "b")` answered the absolute `/b`.
#[test]
fn more_parts_are_joined_not_resolved() {
    assert_eq!(join("a", &["/b"]), "a/b");
    assert_eq!(join("a/", &["/b"]), "a/b");
    assert_eq!(join("", &["b"]), "b");
    assert_eq!(join("", &["", "b", "", "c"]), "b/c");
    assert_eq!(join("/", &["b"]), "/b");
    assert_eq!(join("", &["/b"]), "/b");
    assert_eq!(join("a", &["b", "c"]), "a/b/c");
    assert_eq!(join("a", &[""]), "a");
}
