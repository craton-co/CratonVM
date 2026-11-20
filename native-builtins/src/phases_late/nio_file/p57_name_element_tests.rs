//! The empty path's name list, on every host. HotSpot:
//! `Paths.get("").getNameCount()` is 1 and `getName(0)` is the empty path,
//! on both `UnixPath` and `WindowsPath`.
use super::{p57_name_elements, p57_parse_root};

#[test]
fn the_empty_path_has_one_empty_name_element() {
    assert_eq!(p57_name_elements(""), vec![String::new()]);
    // …and `p57_parse_root` must keep reporting zero there, because
    // `normalize`/`relativize` depend on it: HotSpot's
    // `Paths.get("").relativize(Paths.get("a"))` is `a`, not `../a`.
    assert!(p57_parse_root("").1.is_empty());
}

#[test]
fn every_other_path_is_unchanged() {
    // Rooted paths report a root, so the empty-name arm cannot fire even
    // when they have no name elements.
    let root = if cfg!(windows) { "C:/" } else { "/" };
    assert!(p57_name_elements(root).is_empty());
    assert_eq!(p57_name_elements("a/b"), vec!["a", "b"]);
    assert_eq!(p57_name_elements("a"), vec!["a"]);
    assert_eq!(p57_name_elements("a/b/"), vec!["a", "b"]);
}
