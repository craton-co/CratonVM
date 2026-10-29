//! HotSpot's `-XX:±CompactStrings` as a per-VM policy (j27 wave 9, lane W9VER;
//! page `docs/internal/jit/j27-compact-strings-off-not-honoured-FIXED-20261029.md`).
//!
//! HotSpot writes `String.COMPACT_STRINGS` from the flag
//! (`java_lang_String::set_compact_strings(CompactStrings)`), and with it off
//! every String is UTF16. This is the JDK's flag, NOT the VM's String-layout
//! bit `ClassRealm::compact_strings` ("`value` is a `byte[]` + `coder`"),
//! which stays true over a real JDK in both arms.

use super::VmConfig;

/// The last `-XX:+CompactStrings` / `-XX:-CompactStrings` of `jvm_args`
/// wins, as HotSpot's argument parser does; absent, `true` (HotSpot 25's
/// default).
pub fn compact_strings_from_jvm_args(jvm_args: &[String]) -> bool {
    jvm_args
        .iter()
        .rev()
        .find_map(|arg| match arg.as_str() {
            "-XX:+CompactStrings" => Some(true),
            "-XX:-CompactStrings" => Some(false),
            _ => None,
        })
        .unwrap_or(true)
}

impl VmConfig {
    /// HotSpot's `CompactStrings` for this VM ([`compact_strings_from_jvm_args`]).
    pub fn compact_strings_enabled(&self) -> bool {
        compact_strings_from_jvm_args(&self.jvm_args)
    }
}

#[cfg(test)]
mod tests {
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
}
