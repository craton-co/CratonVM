use super::*;

#[test]
fn java_io_tmpdir_is_never_empty() {
    assert!(!java_io_tmpdir().is_empty());
}

/// HotSpot on Linux answers `P_tmpdir` whatever `$TMPDIR` says.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn linux_answers_p_tmpdir_not_the_environment() {
    assert_eq!(java_io_tmpdir(), "/tmp");
}
