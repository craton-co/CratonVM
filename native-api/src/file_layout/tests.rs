use super::*;

#[cfg(not(windows))]
#[test]
fn prefix_length_is_the_unixfilesystem_rule() {
    assert_eq!(prefix_length("/tmp/x"), 1);
    assert_eq!(prefix_length("/"), 1);
    assert_eq!(prefix_length("rel.txt"), 0);
    assert_eq!(prefix_length(""), 0);
}

/// Every case in `WinNTFileSystem.prefixLength`'s own table, read
/// directly from `java.base/java/io/WinNTFileSystem.java` (JDK 25) --
/// see this module's doc comment on the Windows `prefix_length`.
#[cfg(windows)]
#[test]
fn prefix_length_is_the_winntfilesystem_rule() {
    assert_eq!(prefix_length(""), 0);
    assert_eq!(prefix_length("rel.txt"), 0);
    assert_eq!(prefix_length("a\\rel.txt"), 0);
    assert_eq!(prefix_length("\\foo"), 1);
    assert_eq!(prefix_length("\\"), 1);
    assert_eq!(prefix_length("\\\\server\\share"), 2);
    assert_eq!(prefix_length("\\\\"), 2);
    assert_eq!(prefix_length("z:foo"), 2);
    assert_eq!(prefix_length("C:"), 2);
    assert_eq!(prefix_length("z:\\foo"), 3);
    assert_eq!(prefix_length("C:\\"), 3);
    assert_eq!(prefix_length("C:\\Users\\x"), 3);
    // '/' is accepted as an alias for '\' at the positions the rule
    // inspects, matching what `WinNTFileSystem.normalize` would have
    // folded before a real File ever saw this path.
    assert_eq!(prefix_length("//server/share"), 2);
    assert_eq!(prefix_length("C:/Users/x"), 3);
    assert_eq!(prefix_length("/foo"), 1);
    // A single letter followed by end-of-string is not "letter + ':'".
    assert_eq!(prefix_length("C"), 0);
}
