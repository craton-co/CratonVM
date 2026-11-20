/// Wave 136 (lane L1b): the date is read from the image's `release`
/// file, quoted as the JDK build writes it.
#[test]
fn the_version_date_is_the_release_files() {
    let release = "IMPLEMENTOR=\"Eclipse Adoptium\"\nJAVA_VERSION=\"25.0.3\"\nJAVA_VERSION_DATE=\"2026-04-21\"\n";
    assert_eq!(super::release_version_date(release).as_deref(), Some("2026-04-21"));
    assert_eq!(super::release_version_date("JAVA_VERSION=\"25\"\n"), None);
    assert_eq!(super::release_version_date("JAVA_VERSION_DATE=\"\"\n"), None);
}
