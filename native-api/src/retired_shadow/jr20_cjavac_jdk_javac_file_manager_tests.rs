use super::*;
use cratonvm_types::HotSpotAlignment as Row;

#[test]
fn the_row_takes_the_eight_file_manager_bridges_and_nothing_else() {
    assert_eq!(COMPAT_JAVAC_FILE_MANAGER_TRIPLES.len(), 8);
    for (c, m, d) in COMPAT_JAVAC_FILE_MANAGER_TRIPLES {
        assert_eq!(
            compat_row_of_shadow(c, m, d),
            Some(Row::JdkJavacFileManager),
            "{c}.{m}{d}"
        );
        assert!(!triple_is_retired_shadow(c, m, d), "{c}.{m}{d}");
    }
    for (c, m, d) in [
        (
            "javax/tools/StandardLocation",
            "computeIsModuleOrientedLocation",
            "(Ljava/lang/String;)Z",
        ),
        (
            "com/sun/tools/javac/util/Name",
            "equals",
            "(Ljava/lang/Object;)Z",
        ),
        (
            "com/sun/tools/javac/file/RelativePath",
            "toString",
            "()Ljava/lang/String;",
        ),
        (
            "com/sun/tools/javac/file/JavacFileManager",
            "list",
            "(Ljavax/tools/JavaFileManager$Location;Ljava/lang/String;)Ljava/lang/Iterable;",
        ),
    ] {
        assert_ne!(
            compat_row_of_shadow(c, m, d),
            Some(Row::JdkJavacFileManager),
            "{c}.{m}{d}"
        );
    }
    assert!(Row::JdkJavacFileManager.default_on() && Row::JdkJavacFileManager.needs_real_jdk());
}
