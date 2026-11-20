use super::bare_native_library_name;

/// The paths `NativeLibraries.load` actually receives, decoded back to the
/// bare names the allowlist is written in. Both spellings must decode on
/// either host, so no `cfg` here.
#[test]
fn decodes_the_canonical_paths_the_jdk_loader_passes() {
    for (path, bare) in [
        (r"C:\Program Files\jdk-25\bin\zip.dll", "zip"),
        (r"C:\Program Files\jdk-25\bin\sunmscapi.dll", "sunmscapi"),
        (r"C:\jdk\bin\SUNEC.DLL", "SUNEC"),
        ("/opt/jdk-25/lib/libzip.so", "zip"),
        ("/opt/jdk-25/lib/libmanagement_ext.so", "management_ext"),
        ("/opt/jdk-25/lib/libjsig.dylib", "jsig"),
        // Already bare, and a name with no platform suffix: returned as-is
        // so it misses the allowlist and is reported as a failure.
        ("net", "net"),
        ("/opt/app/native/libfoo.so.1", "libfoo.so.1"),
    ] {
        assert_eq!(bare_native_library_name(path), bare, "decoding {path}");
    }
}

/// A leading `lib` is a Unix spelling artefact only. On the Windows
/// spelling it is part of the name and must survive.
#[test]
fn windows_spelling_keeps_a_leading_lib() {
    assert_eq!(
        bare_native_library_name(r"C:\app\libcrypto.dll"),
        "libcrypto"
    );
    assert_eq!(bare_native_library_name("/app/libcrypto.so"), "crypto");
}
