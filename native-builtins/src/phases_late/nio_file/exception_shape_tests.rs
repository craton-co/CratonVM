use super::{is_directory_not_empty, jarfs_encode, jrtfs_encode, p57_exception_path};

/// HotSpot names `C:\Users\...\missing.txt` in every `java.nio.file`
/// exception; CratonVM named `C:/Users/.../missing.txt`, because the
/// builders were handed the VM's internal path form. `Path.toString()`
/// itself was already correct, so only the exception text diverged.
#[test]
fn an_exception_path_uses_the_platform_separator() {
    let internal = "C:/Users/x/AppData/Local/Temp/nioexc/missing.txt";
    let rendered = p57_exception_path(internal);
    if cfg!(windows) {
        assert_eq!(
            rendered,
            "C:\\Users\\x\\AppData\\Local\\Temp\\nioexc\\missing.txt"
        );
    } else {
        assert_eq!(rendered, internal);
    }
}

/// A path already in platform form must come back untouched, and borrowed
/// rather than copied — this runs on every exception CratonVM raises.
#[test]
fn an_exception_path_without_slashes_is_not_rewritten() {
    let already = if cfg!(windows) {
        "C:\\tmp\\x"
    } else {
        "/tmp/x"
    };
    assert!(matches!(
        p57_exception_path(already),
        std::borrow::Cow::Borrowed(_)
    ));
    assert_eq!(p57_exception_path(already), already);
}

/// An archive entry is `/`-separated on every platform — that is what the
/// JDK's own zipfs and jrtfs report — so the separator rewrite must not
/// touch a virtual-filesystem path. It would also destroy the encoding.
#[test]
fn a_virtual_filesystem_path_keeps_its_slashes() {
    for encoded in [
        jarfs_encode("C:/libs/app.jar", "org/example/Missing.class"),
        jrtfs_encode("C:/jdk-25", "modules/java.base/java/lang/Object.class"),
    ] {
        assert_eq!(
            p57_exception_path(&encoded),
            encoded,
            "a VFS path must pass through unchanged"
        );
    }
}

/// `Files.delete` of a non-empty directory is `DirectoryNotEmptyException`
/// on HotSpot, and was a bare `FileSystemException` here — so a caller's
/// `catch (DirectoryNotEmptyException)` recursion branch never ran. The
/// discriminator is the raw OS code: `ErrorKind` reports ENOTEMPTY as
/// `Uncategorized`, which is what made the distinction unavailable.
#[test]
fn a_non_empty_directory_removal_is_recognised() {
    let dir =
        std::env::temp_dir().join(format!("cratonvm-notempty-{}-{}", std::process::id(), "a"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("child")).expect("create nested dir");
    let error = std::fs::remove_dir(&dir).expect_err("removing a non-empty dir must fail");
    assert!(
        is_directory_not_empty(&error),
        "ENOTEMPTY not recognised: kind={:?} raw={:?}",
        error.kind(),
        error.raw_os_error()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The negative half: a missing path must not be mistaken for ENOTEMPTY,
/// or every `NoSuchFileException` would come back as the wrong type.
#[test]
fn a_missing_path_is_not_directory_not_empty() {
    let missing =
        std::env::temp_dir().join(format!("cratonvm-notempty-missing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&missing);
    let error = std::fs::remove_dir(&missing).expect_err("removing a missing dir must fail");
    assert!(!is_directory_not_empty(&error));
}
