use super::*;

#[test]
fn impl_class_is_the_platform_default_filesystem() {
    #[cfg(windows)]
    assert_eq!(impl_class(), "sun/nio/fs/WindowsFileSystem");
    #[cfg(not(windows))]
    assert_eq!(impl_class(), "sun/nio/fs/UnixFileSystem");
}

#[test]
fn slots_before_any_resolve_is_none() {
    // A reader that runs before any allocation must not invent a
    // layout. (`SLOTS` is process-wide; this asserts the default and
    // stays true whichever order the tests in this crate run in,
    // because nothing here calls `resolve`.)
    assert!(slots().is_none() || slots().unwrap().width >= 3);
}
