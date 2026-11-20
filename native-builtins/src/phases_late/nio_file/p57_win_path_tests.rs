//! keycloak-15: Windows `WindowsPath.isAbsolute()` / `getParent()` semantics.
//! Pure-function tests (no VM); each expectation matches HotSpot
//! `sun.nio.fs.WindowsPath` exactly (cross-checked against JDK 25 via the
//! `PVerify` repro). The parser accepts both `\` and the `/`-canonical
//! internal form, so both spellings are exercised.
#[cfg(windows)]
use super::p57_trim_path_trailing_separator;
use super::{p57_win_is_absolute, p57_win_parent_of};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn is_absolute_matches_hotspot() {
    // Absolute: drive + root, or UNC.
    assert!(p57_win_is_absolute("C:\\foo\\bar"));
    assert!(p57_win_is_absolute("C:/foo/bar")); // `/`-canonical internal form
    assert!(p57_win_is_absolute("\\\\server\\share\\d"));
    assert!(p57_win_is_absolute("//server/share/d"));
    // NOT absolute: drive-relative, driveless-rooted, relative.
    assert!(!p57_win_is_absolute("C:foo"));
    assert!(!p57_win_is_absolute("\\foo\\bar"));
    assert!(!p57_win_is_absolute("/foo/bar"));
    assert!(!p57_win_is_absolute("a\\b\\c"));
    assert!(!p57_win_is_absolute("a/b/c"));
    assert!(!p57_win_is_absolute(""));
}

#[test]
fn parent_keeps_curdir_and_root_boundary() {
    // Trailing `.` must be kept (Rust Path::parent would over-trim to "C:\a").
    assert_eq!(p57_win_parent_of("C:\\a\\b\\."), "C:\\a\\b");
    assert_eq!(p57_win_parent_of("C:/a/b/."), "C:\\a\\b");
    // Drive-absolute.
    assert_eq!(p57_win_parent_of("C:\\foo\\bar"), "C:\\foo");
    assert_eq!(p57_win_parent_of("C:\\foo"), "C:\\");
    // Drive-relative: the first name attaches to `C:` with no separator.
    assert_eq!(p57_win_parent_of("C:foo\\bar"), "C:foo");
    assert_eq!(p57_win_parent_of("C:foo"), "C:");
    // Driveless-rooted.
    assert_eq!(p57_win_parent_of("\\foo\\bar"), "\\foo");
    assert_eq!(p57_win_parent_of("\\foo"), "\\");
    // UNC.
    assert_eq!(
        p57_win_parent_of("\\\\server\\share\\d\\e"),
        "\\\\server\\share\\d"
    );
    // Relative.
    assert_eq!(p57_win_parent_of("a\\b\\c"), "a\\b");
    // No parent → "" (caller maps to null).
    assert_eq!(p57_win_parent_of("a"), "");
    assert_eq!(p57_win_parent_of("C:\\"), "");
    assert_eq!(p57_win_parent_of("\\\\server\\share\\"), "");
}

#[test]
#[cfg(windows)]
fn path_construction_trims_only_non_root_trailing_separators() {
    assert_eq!(p57_trim_path_trailing_separator("a\\"), "a");
    assert_eq!(p57_trim_path_trailing_separator("C:/a/"), "C:/a");
    assert_eq!(p57_trim_path_trailing_separator("C:/"), "C:/");
    assert_eq!(
        p57_trim_path_trailing_separator("\\\\server\\share\\"),
        "\\\\server\\share\\"
    );
    assert_eq!(p57_trim_path_trailing_separator("\\"), "\\");
}
