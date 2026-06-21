// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The diff oracle: compare two [`Observation`](crate::ledger::Observation)s
//! per channel, normalize, and classify (design §3.3).
//!
//! Channels and their comparison discipline:
//! - **exit code** — exact match;
//! - **uncaught exception identity** — `fqcn` exact, `message` exact after
//!   normalization, `top_frames` compared *in order* (so reversed-stacktrace
//!   bugs surface);
//! - **stdout** — exact after normalization (strict by default; a seed must
//!   *declare* it needs a normalizer);
//! - **stderr (non-exception)** — loose / contextual.
//!
//! ## Status: Step 0
//!
//! [`Normalizer`] and the safe line-ending normalization are **real plumbing**
//! (CRLF→LF + trailing-newline trim is the one normalization already applied
//! everywhere and can never hide a real diff). The actual per-channel
//! comparison ([`compare`]) is a documented stub returning [`Verdict::Unwired`]
//! — the strict equality and the `Classification` join across the mode matrix
//! land in Steps 1–2.

use crate::ledger::{Classification, Observation};

/// Per-channel normalization knobs. The **default is strict** (every knob
/// off): a seed must explicitly opt into a normalizer via its header pragma so
/// we never silently mask a real divergence (design §3.3).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalizer {
    /// Strip absolute filesystem paths.
    pub strip_paths: bool,
    /// Mask `0x<hex>` blobs and `Object.toString` identity hashes (`@<hash>`).
    pub mask_hashes: bool,
    /// Mask thread ids.
    pub mask_thread_ids: bool,
    /// Sort output lines (only for explicitly-tagged nondeterministic seeds).
    pub sort_lines: bool,
}

impl Normalizer {
    /// The strict (no-op beyond line-ending hygiene) normalizer.
    pub fn strict() -> Self {
        Self::default()
    }

    /// Apply normalization to one captured stream.
    ///
    /// Step 0 applies **only** the always-safe line-ending hygiene
    /// ([`normalize_line_endings`]); the opt-in transforms above are declared
    /// but inert until Step 1 wires them (each gated behind its flag, so strict
    /// mode stays byte-exact).
    pub fn apply(&self, s: &str) -> String {
        normalize_line_endings(s)
    }
}

/// CRLF→LF and trailing-whitespace trim, so a stray platform `\r` cannot
/// masquerade as a behavioral divergence. Lifted from
/// `vm/tests/intrinsic_diff.rs`'s `normalize`.
pub fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// The result of comparing CratonVM vs HotSpot for one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Observations agree on every gated channel.
    Agree,
    /// A confirmed divergence, with its triage [`Classification`].
    Diverge(Classification),
    /// The oracle is not wired yet (Step 0).
    Unwired,
}

/// Compare two observations under `normalizer`.
///
/// **Step 0 stub:** returns [`Verdict::Unwired`]. The Step 1 implementation
/// compares exit code, parsed exception identity, and normalized stdout; Step 2
/// joins the per-mode verdicts into a [`Classification`] (`JitOnly` when
/// `jit-on` diverges and `nojit` agrees, etc.).
pub fn compare(
    _cratonvm: &Observation,
    _hotspot: &Observation,
    _normalizer: &Normalizer,
) -> Verdict {
    Verdict::Unwired
}

/// Locate the first line on which two blobs differ — used to localise a
/// divergence in a failure message. Lifted from `intrinsic_diff.rs`.
pub fn first_diff(a: &str, b: &str) -> Option<(usize, String, String)> {
    let la: Vec<&str> = a.lines().collect();
    let lb: Vec<&str> = b.lines().collect();
    for i in 0..la.len().max(lb.len()) {
        let x = la.get(i).copied().unwrap_or("<missing>");
        let y = lb.get(i).copied().unwrap_or("<missing>");
        if x != y {
            return Some((i + 1, x.to_string(), y.to_string()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_normalized() {
        assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize_line_endings("a\nb\n\n  "), "a\nb");
    }

    #[test]
    fn strict_normalizer_only_fixes_line_endings() {
        let n = Normalizer::strict();
        // A path that *would* be stripped by a path normalizer survives strict.
        assert_eq!(n.apply("C:\\tmp\\x.txt\r\n"), "C:\\tmp\\x.txt");
        assert!(!n.strip_paths);
    }

    #[test]
    fn compare_is_unwired_in_step0() {
        let v = compare(
            &Observation::empty(),
            &Observation::empty(),
            &Normalizer::strict(),
        );
        assert_eq!(v, Verdict::Unwired);
    }

    #[test]
    fn first_diff_points_at_line() {
        let d = first_diff("a\nb\nc", "a\nX\nc");
        assert_eq!(d, Some((2, "b".to_string(), "X".to_string())));
        assert_eq!(first_diff("same", "same"), None);
    }
}
