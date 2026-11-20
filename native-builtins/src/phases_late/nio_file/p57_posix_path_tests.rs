//! POSIX (`sun.nio.fs.UnixPath`) construction / root / parent semantics.
//! Pure-function tests (no VM); every expectation was cross-checked against
//! the host JDK on Linux (`PathMatrix` repro) — see
//! `resourcestests-trailing-slash-path-normalization-FIXED-20260804.md`.
//! The Windows twin lives in `p57_win_path_tests`.
use super::{
    jarfs_encode, p57_alloc_path, p57_parse_root, p57_posix_parent_of, p57_read_path,
    p57_trim_path_trailing_separator,
};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn path_construction_normalizes_like_unixpath() {
    // The defect: an ordinary trailing separator survived into the stored
    // string, so `Files.writeString` got a directory-shaped path (EISDIR).
    assert_eq!(p57_trim_path_trailing_separator("/tmp/a/b/"), "/tmp/a/b");
    assert_eq!(p57_trim_path_trailing_separator("a/b/"), "a/b");
    assert_eq!(p57_trim_path_trailing_separator("a/b///"), "a/b");
    // Runs of `/` collapse, exactly as `normalizeAndCheck` does.
    assert_eq!(p57_trim_path_trailing_separator("//tmp/x"), "/tmp/x");
    assert_eq!(p57_trim_path_trailing_separator("a//b"), "a/b");
    // The root keeps its separator; the empty path stays empty.
    assert_eq!(p57_trim_path_trailing_separator("/"), "/");
    assert_eq!(p57_trim_path_trailing_separator("//"), "/");
    assert_eq!(p57_trim_path_trailing_separator(""), "");
    // `\\` is an ordinary filename character on Unix, never a separator.
    assert_eq!(p57_trim_path_trailing_separator("a\\b\\"), "a\\b\\");
    // Encoded virtual-FS paths are left alone: the trailing `/` there is
    // part of the jar/jrt entry representation.
    let jar = jarfs_encode("/tmp/x.jar", "dir/");
    assert_eq!(p57_trim_path_trailing_separator(&jar), jar);
}

#[test]
fn alloc_path_stores_the_normalized_string() {
    // Pins the WIRING, not just the helper. The defect was that
    // `p57_alloc_path` had no normalization step at all on this platform
    // (`let stored = path.to_string();`), so a helper-only test would have
    // stayed green straight through it. Everything that builds a Path —
    // `Paths.get`, `resolve`, `getParent`, `toAbsolutePath` — funnels here,
    // and the stored string is what the file-IO bridge syscalls with.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let mut p = p57_alloc_path(&mut ctx, "/tmp/one/two/three/").expect("alloc Path");
    assert_eq!(p57_read_path(&mut ctx, &mut p), "/tmp/one/two/three");
    let mut root = p57_alloc_path(&mut ctx, "/").expect("alloc Path");
    assert_eq!(p57_read_path(&mut ctx, &mut root), "/");
    let mut collapsed = p57_alloc_path(&mut ctx, "/tmp//x/").expect("alloc Path");
    assert_eq!(p57_read_path(&mut ctx, &mut collapsed), "/tmp/x");
}

#[test]
fn parse_root_uses_posix_syntax() {
    // Absolute: the one root is `/` (the Windows parser answered `\\`).
    assert_eq!(p57_parse_root("/tmp/x").0.as_deref(), Some("/"));
    assert_eq!(p57_parse_root("/").0.as_deref(), Some("/"));
    assert_eq!(p57_parse_root("/tmp/x").1, vec!["tmp", "x"]);
    assert_eq!(p57_parse_root("/").1.len(), 0);
    // Relative: no root.
    assert_eq!(p57_parse_root("a/b").0, None);
    assert_eq!(p57_parse_root("a/b").1, vec!["a", "b"]);
    // A drive letter is NOT syntax here — `C:` is an ordinary name.
    assert_eq!(p57_parse_root("C:/x").0, None);
    assert_eq!(p57_parse_root("C:/x").1, vec!["C:", "x"]);
    // No UNC either: `//tmp/x` is just `/tmp/x` (2 names, not a share).
    assert_eq!(p57_parse_root("//tmp/x").0.as_deref(), Some("/"));
    assert_eq!(p57_parse_root("//tmp/x").1, vec!["tmp", "x"]);
    // `\` is a filename character, so it never splits a name.
    assert_eq!(p57_parse_root("a\\b").1, vec!["a\\b"]);
}

#[test]
fn parent_keeps_curdir_and_root_boundary() {
    // Trailing `.` must be kept (Rust's Path::parent would over-trim to "a").
    assert_eq!(p57_posix_parent_of("a/b/."), "a/b");
    assert_eq!(p57_posix_parent_of("/a/b/."), "/a/b");
    // Ordinary splits.
    assert_eq!(p57_posix_parent_of("/tmp/a/b"), "/tmp/a");
    assert_eq!(p57_posix_parent_of("a/b/c"), "a/b");
    // A single element under the root leaves the root itself.
    assert_eq!(p57_posix_parent_of("/foo"), "/");
    // No parent -> "" (caller maps to null).
    assert_eq!(p57_posix_parent_of("a"), "");
    assert_eq!(p57_posix_parent_of("/"), "");
    assert_eq!(p57_posix_parent_of(""), "");
}
