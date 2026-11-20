use super::{
    hashed_answer_or_name_screen, stack_trace_element_format_bits,
    STE_FORMAT_BUILTIN_CLASS_LOADER, STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE,
};

#[test]
fn format_bits_follow_compute_format() {
    // A JDK frame: bootstrap loader (`null`, not a BuiltinClassLoader).
    assert_eq!(
        stack_trace_element_format_bits(0, true),
        STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
    );
    // A platform-module frame (hashed): both bits.
    assert_eq!(
        stack_trace_element_format_bits(1, true),
        STE_FORMAT_BUILTIN_CLASS_LOADER | STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
    );
    // An upgradeable platform module (`java.compiler`, not hashed):
    // the loader bit only, so `toString()` keeps `@version`.
    assert_eq!(
        stack_trace_element_format_bits(1, false),
        STE_FORMAT_BUILTIN_CLASS_LOADER
    );
    // A class-path frame: the `app/` prefix is dropped, no module.
    assert_eq!(
        stack_trace_element_format_bits(2, false),
        STE_FORMAT_BUILTIN_CLASS_LOADER
    );
    // A named user loader in a named application module: both printed.
    assert_eq!(stack_trace_element_format_bits(3, false), 0);
    assert_eq!(stack_trace_element_format_bits(7, false), 0);
}

/// Wave 79 (lane L2): the recorded hash set decides; the `java.` /
/// `jdk.` name screen is only the no-hashes fallback.
#[test]
fn hashed_set_decides_and_the_name_screen_is_the_fallback() {
    assert!(!hashed_answer_or_name_screen(Some(false), "java.compiler"));
    assert!(hashed_answer_or_name_screen(Some(true), "java.sql"));
    assert!(hashed_answer_or_name_screen(None, "java.compiler"));
    assert!(hashed_answer_or_name_screen(None, "jdk.httpserver"));
    assert!(!hashed_answer_or_name_screen(None, "com.example"));
}
