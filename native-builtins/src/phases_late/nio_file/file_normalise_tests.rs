use super::file_normalise_path;

/// The Windows arm, against HotSpot 25.0.3+9 measured with
/// `probes/FilePathSweep.java`. Every row here was a DIFFERING line
/// before 2026-08-26: the arm collapsed no separator runs, and its
/// trailing-separator guard (`len() > 3`) protected short relative
/// paths as well as the drive root it was aimed at.
/// `File(String parent, String child)` with an EMPTY parent, and
/// `File.isAbsolute()` on a UNC root. Both were differing lines against
/// HotSpot 25.0.3+9 in `probes/FilePathSweep.java` before 2026-08-26.
#[cfg(windows)]
#[test]
fn windows_default_parent_and_unc_absolute() {
    use super::{file_is_absolute, file_join_parent_child_units};
    let u = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
    let j = |a: &str, b: &str| -> String {
        String::from_utf16_lossy(&file_join_parent_child_units(&u(a), &u(b)))
    };
    // empty parent is the DEFAULT parent, not 'no parent'
    assert_eq!(j("", "kid"), "\\kid");
    assert_eq!(j("", "/kid"), "\\kid");
    // an ordinary parent still joins normally
    assert_eq!(j("a", "kid"), "a\\kid");
    assert_eq!(j("a/", "kid"), "a\\kid");
    // an empty CHILD yields the normalised parent, unchanged by this fix
    assert_eq!(j("a/", ""), "a");
    // a ROOT parent keeps its separator instead of being stripped
    assert_eq!(j("//", "kid"), "\\\\kid");
    assert_eq!(j("///", "kid"), "\\\\kid");
    assert_eq!(j("C:/", "kid"), "C:\\kid");
    assert_eq!(j("/", "kid"), "\\kid");
    assert_eq!(j("//server/share", "kid"), "\\\\server\\share\\kid");
    // WinNTFileSystem.isAbsolute: UNC yes, drive-rooted yes,
    // driveless-rooted no, drive-relative no
    assert!(file_is_absolute("\\\\"));
    assert!(file_is_absolute("\\\\server\\share"));
    assert!(file_is_absolute("C:\\"));
    assert!(file_is_absolute("C:\\x"));
    assert!(!file_is_absolute("\\foo"));
    assert!(!file_is_absolute("C:foo"));
    assert!(!file_is_absolute("a\\b"));
    assert!(!file_is_absolute(""));
    // A BARE DRIVE is a prefix but NOT a root, so it takes no separator:
    // `WinNTFileSystem.resolve`'s `isDirectoryRelative` arm. `C:kid` names
    // a file in drive C's OWN working directory; `C:\kid` names one in its
    // root, and those coincide only when the process's directory on C:
    // happens to be the root. MEASURED against HotSpot 25.0.3+9
    // (`probes/FilePathSweep.java`, rows `[C:] child` and `[C:] twoArg`),
    // which answers `C:kid`.
    assert_eq!(j("C:", "kid"), "C:kid");
    // ... and the rule is exactly two characters wide: a drive WITH a root
    // is a root and keeps its separator, and a two-character parent that is
    // not `<letter>:` is an ordinary relative name.
    assert_eq!(j("C:/", "kid"), "C:\\kid");
    assert_eq!(j("ab", "kid"), "ab\\kid");
    assert_eq!(j("1:", "kid"), "1:\\kid");
}

#[cfg(windows)]
#[test]
fn windows_normalise_matches_winntfilesystem() {
    // runs of separators collapse
    assert_eq!(file_normalise_path("a//b"), "a\\b");
    assert_eq!(file_normalise_path("a/./b"), "a\\.\\b");
    assert_eq!(file_normalise_path("x///y//z"), "x\\y\\z");
    // a trailing separator goes, at ANY length -- this is the `a/` bug
    assert_eq!(file_normalise_path("a/"), "a");
    assert_eq!(file_normalise_path("a\\\\"), "a");
    assert_eq!(file_normalise_path("trailing/"), "trailing");
    assert_eq!(file_normalise_path("a/b/"), "a\\b");
    // ... but a ROOT keeps its separator and never becomes empty
    assert_eq!(file_normalise_path("/"), "\\");
    assert_eq!(file_normalise_path("C:/"), "C:\\");
    assert_eq!(file_normalise_path("C:\\"), "C:\\");
    // the UNC prefix survives as exactly two, and collapses beyond that
    assert_eq!(file_normalise_path("//"), "\\\\");
    assert_eq!(file_normalise_path("///"), "\\\\");
    assert_eq!(file_normalise_path("//server/share"), "\\\\server\\share");
    assert_eq!(
        file_normalise_path("////server//share"),
        "\\\\server\\share"
    );
    // every leading separator before a drive goes (`normalizePrefix`)
    assert_eq!(file_normalise_path("/C:/foo"), "C:\\foo");
    assert_eq!(file_normalise_path("\\C:\\foo\\"), "C:\\foo");
    assert_eq!(file_normalise_path("\\\\C:\\foo"), "C:\\foo");
    assert_eq!(file_normalise_path("//C:/foo"), "C:\\foo");
    assert_eq!(file_normalise_path("\\/C:/foo"), "C:\\foo");
    assert_eq!(file_normalise_path("\\C:"), "C:");
    assert_eq!(file_normalise_path("\\C:\\"), "C:\\");
    assert_eq!(file_normalise_path("\\c:\\x"), "c:\\x");
    // ... but not before something that is not a drive
    assert_eq!(file_normalise_path("/1:/x"), "\\1:\\x");
    assert_eq!(file_normalise_path("\\\\Cx\\share"), "\\\\Cx\\share");
    // degenerate inputs
    assert_eq!(file_normalise_path(""), "");
    assert_eq!(file_normalise_path("a"), "a");
}

#[cfg(not(windows))]
#[test]
fn unix_normalise_matches_unixfilesystem() {
    assert_eq!(file_normalise_path("/tmp/x/scanned/"), "/tmp/x/scanned");
    assert_eq!(file_normalise_path("/tmp/x//scanned"), "/tmp/x/scanned");
    assert_eq!(file_normalise_path("/tmp/x///scanned//"), "/tmp/x/scanned");
    assert_eq!(file_normalise_path("/"), "/");
    assert_eq!(file_normalise_path("//"), "/");
    assert_eq!(file_normalise_path("relative/dir/"), "relative/dir");
    assert_eq!(file_normalise_path(""), "");
}
