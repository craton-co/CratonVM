use super::*;

#[test]
fn fallback_is_the_pre_2026_09_10_layout() {
    // The two constants this module replaced. A change here silently
    // relocates every Path allocated before `sun.nio.fs.UnixPath` loads.
    assert_eq!(FALLBACK.string, 0);
    assert_eq!(FALLBACK.fs, 1);
    assert_eq!(FALLBACK.bytes, None);
    assert_eq!(FALLBACK.width, 2);
}

#[test]
fn fallback_has_no_type_or_root_slot() {
    // The stub layout predates the carrier ever being genuinely
    // `WindowsPath`-shaped; a fallback that invented a `type`/`root`
    // slot would be claiming a layout it never resolved.
    assert_eq!(FALLBACK.kind, None);
    assert_eq!(FALLBACK.root, None);
    assert_eq!(FALLBACK.offsets, None);
}

#[test]
fn impl_class_matches_the_get_class_display_alias() {
    // `native-builtins/src/lib.rs` maps `java/nio/file/Path` to exactly
    // this class for `getClass()`. The layout written has to be the layout
    // of the class claimed.
    #[cfg(windows)]
    assert_eq!(impl_class(), "sun/nio/fs/WindowsPath");
    #[cfg(not(windows))]
    assert_eq!(impl_class(), "sun/nio/fs/UnixPath");
}

#[test]
fn slots_before_any_resolve_is_the_fallback() {
    // A reader that runs before any allocation must not invent a layout.
    // (`SLOTS` is process-wide; this asserts the default, and stays true
    // whichever order the tests in this crate run in, because nothing here
    // calls `resolve`.)
    let s = slots();
    assert!(s.width >= 2);
    assert!(s.string < s.width && s.fs < s.width);
}
