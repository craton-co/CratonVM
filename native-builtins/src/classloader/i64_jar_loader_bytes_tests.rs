use super::is_jar_loader_anonymous_class;

/// The receiver gate names `checkResource`'s anonymous `Resource` by its
/// outer class and a numeric suffix, never by the number.
#[test]
fn jar_loader_anonymous_classes_are_recognised_by_shape() {
    for name in [
        "jdk/internal/loader/URLClassPath$JarLoader$1",
        "jdk/internal/loader/URLClassPath$JarLoader$2",
        "jdk/internal/loader/URLClassPath$JarLoader$12",
    ] {
        assert!(is_jar_loader_anonymous_class(name), "{name}");
    }
    for name in [
        "jdk/internal/loader/URLClassPath$JarLoader",
        "jdk/internal/loader/URLClassPath$JarLoader$",
        "jdk/internal/loader/URLClassPath$JarLoader$Inner",
        "jdk/internal/loader/URLClassPath$JarLoader$2$1",
        "jdk/internal/loader/URLClassPath$Loader$1",
        "jdk/internal/loader/URLClassPath$FileLoader$1",
        "p/URLClassPath$JarLoader$2",
    ] {
        assert!(!is_jar_loader_anonymous_class(name), "{name}");
    }
}
