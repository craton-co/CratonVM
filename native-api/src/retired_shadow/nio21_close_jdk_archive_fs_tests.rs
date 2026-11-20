use super::*;
use cratonvm_types::HotSpotAlignment as Row;

#[test]
fn the_row_takes_the_five_archive_provider_rows_and_their_mirrors() {
    for (c, m, d) in RETIRED_SHADOW_ARCHIVE_PROVIDER_TRIPLES {
        assert_eq!(
            compat_row_of_shadow(c, m, d),
            Some(Row::JdkArchiveFs),
            "{c}.{m}{d}"
        );
    }
    let nfs = "(Ljava/nio/file/Path;Ljava/util/Map;)Ljava/nio/file/FileSystem;";
    for c in ARCHIVE_PROVIDER_IMPL_CLASSES {
        assert_eq!(
            compat_row_of_shadow(c, "newFileSystem", nfs),
            Some(Row::JdkArchiveFs)
        );
    }
    // The URI doors of the abstract provider stay: the minted providers
    // still serve them.
    assert_ne!(
        compat_row_of_shadow(
            "java/nio/file/spi/FileSystemProvider",
            "newFileSystem",
            "(Ljava/net/URI;Ljava/util/Map;)Ljava/nio/file/FileSystem;",
        ),
        Some(Row::JdkArchiveFs)
    );
    assert!(Row::JdkArchiveFs.default_on() && Row::JdkArchiveFs.needs_real_jdk());
}
