use super::*;

#[test]
fn javac_binary_name_from_path_matches_javac_path_file_objects() {
    assert_eq!(
        javac_binary_name_from_relative_path("org/springframework/aot/Hint.class"),
        "org.springframework.aot.Hint"
    );
    assert_eq!(
        javac_binary_name_from_relative_path("\\org\\springframework\\aot\\Hint.class"),
        "org.springframework.aot.Hint"
    );
    assert_eq!(
        javac_binary_name_from_jrt_path("/modules/java.base/java/lang/String.class"),
        Some("java.lang.String".to_string())
    );
    assert_eq!(
        javac_binary_name_from_jrt_path("/not-modules/String.class"),
        None
    );
}
