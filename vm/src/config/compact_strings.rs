// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot's `-XX:±CompactStrings` as a per-VM policy (j27 wave 9, lane W9VER;
//! page `docs/feature-designs/real-jdk-mode.md`).
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
mod tests;
