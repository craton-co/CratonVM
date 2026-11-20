use super::*;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_last_compact_strings_option_wins_and_the_default_is_on() {
    assert!(compact_strings_from_jvm_args(&[]));
    assert!(compact_strings_from_jvm_args(&args(&["-Xmx1g"])));
    assert!(!compact_strings_from_jvm_args(&args(&["-XX:-CompactStrings"])));
    assert!(compact_strings_from_jvm_args(&args(&["-XX:-CompactStrings", "-XX:+CompactStrings"])));
    assert!(!compact_strings_from_jvm_args(&args(&["-XX:+CompactStrings", "-XX:-CompactStrings"])));
    let mut config = VmConfig::default();
    assert!(config.compact_strings_enabled());
    config.jvm_args = args(&["-XX:-CompactStrings"]);
    assert!(!config.compact_strings_enabled());
}
