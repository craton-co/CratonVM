use super::hkcr_string_value;

/// A key that is not in the registry answers `None` rather than an empty
/// string or an error. `RegistryFileTypeDetector`'s JNI returns NULL here,
/// and `AbstractFileTypeDetector` reads that null as "no answer" and falls
/// back to `URLConnection.getFileNameMap()` — so turning it into `Some("")`
/// would suppress the fallback and make `Files.probeContentType` answer
/// null for every extension the registry happens not to carry.
///
/// Deliberately not asserting that `.txt` IS `text/plain`: that is a fact
/// about the machine's registry, not about this code, and a test that
/// depends on it is a latent CI failure on a differently-configured box.
#[test]
fn an_absent_key_is_none_not_an_empty_string() {
    assert_eq!(
        hkcr_string_value(".cratonvm-no-such-extension-zz", "Content Type"),
        None
    );
}

/// A value name that is absent from a key that DOES exist is also `None` —
/// the two failure modes go through different `RegGetValueW` error codes
/// and must not diverge. `.txt` is present on every Windows install; the
/// value name is not.
#[test]
fn an_absent_value_under_a_present_key_is_none() {
    assert_eq!(hkcr_string_value(".txt", "CratonVM No Such Value"), None);
}

/// An embedded NUL would silently truncate the key inside the Win32 call,
/// so the query would run against a DIFFERENT key than the caller named.
/// Refuse instead. (These strings arrive from guest-controlled native
/// memory via `read_native_utf16_c_string`, so this is reachable.)
#[test]
fn an_embedded_nul_is_refused_rather_than_truncated() {
    assert_eq!(hkcr_string_value(".txt\0.exe", "Content Type"), None);
    assert_eq!(hkcr_string_value(".txt", "Content\0Type"), None);
}
