// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! `-XX:-CompactStrings` (j27 wave 9, lane W9VER; page
//! `docs/feature-designs/real-jdk-mode.md`).
//!
//! Two different bits, kept apart:
//! * `ClassRealm::compact_strings`: the String LAYOUT (`byte[] value` +
//!   `coder`), true over every real JDK image.
//! * `ClassRealm::compact_strings_policy`: HotSpot's `CompactStrings`, the
//!   JDK's `String.COMPACT_STRINGS`. With it `false` every String is UTF16.
//!
//! Every producer that may write a LATIN1 String asks
//! [`ClassRealm::latin1_coder_allowed`]; compiled code reads the byte
//! [`ClassRealm::jit_compact_strings_flag`] as `COMPACT_STRINGS`.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::vm::realms::ClassRealm;

impl ClassRealm {
    /// May a String this VM builds be LATIN1? The JDK's `COMPACT_STRINGS`:
    /// the compact layout is in place AND `-XX:-CompactStrings` was not given.
    pub fn latin1_coder_allowed(&self) -> bool {
        self.compact_strings_policy && self.compact_strings.load(Ordering::Relaxed)
    }

    /// The byte compiled code reads as `COMPACT_STRINGS` (the inline
    /// `coder()` / `isLatin1()`, the concat and `char[]` constructors): the
    /// layout bit itself, or an always-false byte under `-XX:-CompactStrings`.
    /// The policy is fixed at construction, so the address is stable.
    pub fn jit_compact_strings_flag(&self) -> &AtomicBool {
        if self.compact_strings_policy {
            &self.compact_strings
        } else {
            &self.compact_strings_off_flag
        }
    }
}

#[cfg(test)]
mod tests;
