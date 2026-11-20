use super::*;
use cratonvm_native_api::ClassModuleIdentity;

fn hashed() -> Vec<String> {
    vec![
        "java.sql".to_string(),
        "java.logging".to_string(),
        "jdk.zipfs".to_string(),
    ]
}

/// Interpreter round i3 wave 74, lane L2: the `format` byte the
/// `StackTraceElement.of` intrinsic stores is `computeFormat()`'s:
/// `BUILTIN_CLASS_LOADER` from the loader test alone, and
/// `JDK_NON_UPGRADEABLE_MODULE` for a boot-layer module in `java.base`'s
/// recorded hashes (or `java.base` itself), never by a `java.` / `jdk.`
/// name screen.
#[test]
fn computed_format_bits_follow_compute_format() {
    let names = hashed();
    let named = |n: &str| ClassModuleIdentity::Named(n.to_string());
    // An application class: the app loader is a BuiltinClassLoader, the
    // unnamed module is not hashed.
    assert_eq!(
        computed_format_bits(true, true, &ClassModuleIdentity::Unnamed, Some(&names[..])),
        STE_FORMAT_BUILTIN_CLASS_LOADER
    );
    // A java.base class: bootstrap loader (null, no bit), java.base hashed.
    assert_eq!(
        computed_format_bits(false, true, &named("java.base"), Some(&names[..])),
        STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
    );
    // A platform-loader module in the hash set: both bits.
    assert_eq!(
        computed_format_bits(true, true, &named("java.sql"), Some(&names[..])),
        STE_FORMAT_BUILTIN_CLASS_LOADER | STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
    );
    // An upgradeable JDK module (not hashed) keeps its version: the name
    // screen the `--compatible` fill uses would have set the bit.
    assert_eq!(
        computed_format_bits(true, true, &named("java.compiler"), Some(&names[..])),
        STE_FORMAT_BUILTIN_CLASS_LOADER
    );
    // No recorded hashes: the JDK's set is empty, java.base included.
    assert_eq!(
        computed_format_bits(false, true, &named("java.base"), None),
        0
    );
    // A non-boot layer's module is never hashed, whatever its name.
    let layer = ClassModuleIdentity::Layer {
        loader_ns: 7,
        name: "java.sql".to_string(),
        version: None,
    };
    assert_eq!(
        computed_format_bits(false, true, &layer, Some(&names[..])),
        0
    );
    // Before the module system is up every module counts as hashed.
    assert_eq!(
        computed_format_bits(false, false, &ClassModuleIdentity::Unnamed, None),
        STE_FORMAT_JDK_NON_UPGRADEABLE_MODULE
    );
}

/// The intrinsic and `initStackTraceElements` share one fill, so the
/// elements the two build cannot drift apart; the intrinsic adds only the
/// format pass.
#[test]
fn the_of_intrinsic_shares_the_init_fill() {
    let src = include_str!("../lang_misc.rs");
    let body = |head: &str| -> String {
        src.split(head)
            .nth(1)
            .and_then(|rest| rest.split("\npub(crate) fn ").next())
            .expect("function body")
            .to_string()
    };
    let of = body("pub fn native_stack_trace_element_of_backtrace(");
    assert!(of.contains("fill_stack_trace_elements_from_backtrace("));
    assert!(of.contains("finish_stack_trace_element_formats("));
    let init = body("pub fn native_init_stack_trace_elements(");
    assert!(init
        .contains("fill_stack_trace_elements_from_backtrace(ctx, elements, Some(backtrace))"));
}
