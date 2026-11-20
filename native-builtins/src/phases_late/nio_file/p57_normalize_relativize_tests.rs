//! `Path.normalize()` / `Path.relativize()` vs HotSpot (JDK 25, via the
//! `PathDeep` repro). Helpers emit `/`-canonical internal form.
use super::{p57_normalize_path, p57_relativize};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
#[cfg(windows)]
fn normalize_preserves_root_and_leading_dotdot() {
    // `..` must not pop above the root.
    assert_eq!(p57_normalize_path("C:/a/../../b"), "C:/b");
    assert_eq!(p57_normalize_path("C:/.."), "C:/");
    assert_eq!(p57_normalize_path("//s/sh/a/.."), "//s/sh/");
    // Leading `..` on a relative path is kept.
    assert_eq!(p57_normalize_path("../../a"), "../../a");
    assert_eq!(p57_normalize_path("a/../../b"), "../b");
    // Ordinary collapses.
    assert_eq!(p57_normalize_path("C:/a/./b/.."), "C:/a");
    assert_eq!(p57_normalize_path("a/./b/.."), "a");
    assert_eq!(p57_normalize_path("a/../b"), "b");
    assert_eq!(p57_normalize_path("C:/a/b"), "C:/a/b");
    // Drive-relative root retained with no separator before the first name.
    assert_eq!(p57_normalize_path("C:a/../b"), "C:b");
}

#[test]
#[cfg(windows)]
fn relativize_backtracks_with_dotdot() {
    assert_eq!(p57_relativize("C:/a/b", "C:/a/x").as_deref(), Some("../x"));
    assert_eq!(p57_relativize("a/b/c", "a/b").as_deref(), Some(".."));
    assert_eq!(
        p57_relativize("C:/a/b", "C:/a/b/c/d").as_deref(),
        Some("c/d")
    );
    assert_eq!(p57_relativize("C:/a", "C:/a").as_deref(), Some(""));
    assert_eq!(p57_relativize("a/b", "a/b/c").as_deref(), Some("c"));
    // Case-insensitive common-prefix match on Windows.
    #[cfg(windows)]
    assert_eq!(p57_relativize("C:/A/b", "C:/a/x").as_deref(), Some("../x"));
    // Different roots → None (caller falls back to target).
    assert_eq!(p57_relativize("C:/a", "D:/b"), None);
    assert_eq!(p57_relativize("C:/a", "rel/b"), None);
}

/// The POSIX twins. These are the same two functions on a host where a
/// drive prefix and a backslash are NOT path syntax; every expectation was
/// cross-checked against the host JDK 25 on Linux (`NormMatrix` repro).
#[test]
#[cfg(not(windows))]
fn normalize_preserves_root_and_leading_dotdot_posix() {
    // `..` must not pop above the root.
    assert_eq!(p57_normalize_path("/a/../../b"), "/b");
    assert_eq!(p57_normalize_path("/.."), "/");
    // Leading `..` on a relative path is kept.
    assert_eq!(p57_normalize_path("../../a"), "../../a");
    assert_eq!(p57_normalize_path("a/../../b"), "../b");
    // Ordinary collapses.
    assert_eq!(p57_normalize_path("/a/./b/.."), "/a");
    assert_eq!(p57_normalize_path("a/./b/.."), "a");
    assert_eq!(p57_normalize_path("a/../b"), "b");
    assert_eq!(p57_normalize_path("/a/b"), "/a/b");
    // A drive prefix is an ordinary NAME here, so `..` cancels it and the
    // Windows parser's "root" reading (`C:b`) is wrong on this host.
    assert_eq!(p57_normalize_path("C:a/../b"), "b");
    assert_eq!(p57_normalize_path("C:/a/../../b"), "b");
    // `\` is a filename character: `a\b` is ONE name, so the `..` that
    // follows cancels the whole thing (the Windows parser answered `a/c`).
    assert_eq!(p57_normalize_path("a\\b/../c"), "c");
    // No UNC: `//s/sh/a/..` is just `/s/sh`.
    assert_eq!(p57_normalize_path("//s/sh/a/.."), "/s/sh");
}

#[test]
#[cfg(not(windows))]
fn relativize_backtracks_with_dotdot_posix() {
    assert_eq!(p57_relativize("/a/b", "/a/x").as_deref(), Some("../x"));
    assert_eq!(p57_relativize("a/b/c", "a/b").as_deref(), Some(".."));
    assert_eq!(p57_relativize("/a/b", "/a/b/c/d").as_deref(), Some("c/d"));
    assert_eq!(p57_relativize("/a", "/a").as_deref(), Some(""));
    assert_eq!(p57_relativize("a/b", "a/b/c").as_deref(), Some("c"));
    // Case-SENSITIVE name matching off Windows.
    assert_eq!(p57_relativize("/A/b", "/a/x").as_deref(), Some("../../a/x"));
    // Drive prefixes are ordinary names, so these ARE relativizable here.
    assert_eq!(
        p57_relativize("C:/a", "D:/b").as_deref(),
        Some("../../D:/b")
    );
    // `\` never splits a name.
    assert_eq!(p57_relativize("/a\\b", "/a\\x").as_deref(), Some("../a\\x"));
    // Absolute vs relative → None (caller falls back to target).
    assert_eq!(p57_relativize("/a", "rel/b"), None);
}
