// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Which AMD architecture a kernel is rendered for, and the one
//! property of it that changes generated code.
//!
//! # The analogue of `jit_cuda::target`, and how it differs
//!
//! `jit_cuda::target` exists because a PTX module's `.version` and
//! `.target` are not independent: each ISA version admits a closed set
//! of targets, and naming one outside it makes the module fail to load
//! — which made GPU offload a silent no-op on every device newer than
//! Ampere for as long as the crate existed.
//!
//! HIP has no such pairing. `gcnArchName` goes to `hiprtc` verbatim as
//! `--offload-arch=`, and an architecture hiprtc does not know is a
//! clean compile error carrying a diagnostic — not a silent load
//! failure. There is also nothing to clamp DOWN to: a `gfx90a` code
//! object does not run on a `gfx1100`, unlike an `sm_80` module on an
//! `sm_90` device.
//!
//! What does change generated code is the **wavefront width**, and it
//! is not reported in the arch string. That is what this module is
//! really for.

/// The device a kernel is rendered for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HipTarget {
    /// `hipDeviceProp_t::gcnArchName` verbatim — feature suffixes and
    /// all, e.g. `gfx90a:sramecc+:xnack-`.
    ///
    /// Passed to `hiprtc` unmodified. The suffixes are load-bearing:
    /// stripping them yields a code object the runtime refuses to load,
    /// so a helper that "cleaned up" the name would be a bug.
    pub gcn_arch_name: String,
    /// Lanes per wavefront: 64 on CDNA/GCN, 32 on RDNA.
    pub wavefront_size: u32,
    /// `double` arithmetic. Correct on every AMD target, so effectively
    /// always true; carried because the lowering asks, and an answer of
    /// "assume yes" should be a recorded assumption rather than an
    /// absent one.
    pub fp64: bool,
    /// 64-bit integer atomics in global memory. Gates `long`
    /// reductions, which lower to an atomic add.
    pub global_int64_atomics: bool,
}

impl HipTarget {
    /// A target for `arch`, with the wavefront width derived from it.
    ///
    /// `None` when the architecture is not in [`GFX_TABLE`] — see
    /// [`wavefront_for_arch`] for why that refuses rather than
    /// guessing.
    pub fn from_arch(arch: &str, fp64: bool, global_int64_atomics: bool) -> Option<Self> {
        Some(Self {
            gcn_arch_name: arch.to_string(),
            wavefront_size: wavefront_for_arch(arch)?,
            fp64,
            global_int64_atomics,
        })
    }
}

/// Wavefront width per architecture family, longest prefix wins.
///
/// The table is written out rather than derived from the number,
/// because the rule ("gfx10xx and above are 32") is a summary of
/// history and not a specification — and the cost of being wrong is
/// not an error.
const GFX_TABLE: &[(&str, u32)] = &[
    // GCN / Vega / CDNA: 64 lanes.
    ("gfx803", 64),
    ("gfx900", 64),
    ("gfx902", 64),
    ("gfx906", 64),
    ("gfx908", 64),
    ("gfx90a", 64),
    ("gfx90c", 64),
    ("gfx940", 64),
    ("gfx941", 64),
    ("gfx942", 64),
    ("gfx950", 64),
    // RDNA 1-4: 32 lanes.
    ("gfx1010", 32),
    ("gfx1011", 32),
    ("gfx1012", 32),
    ("gfx1013", 32),
    ("gfx1030", 32),
    ("gfx1031", 32),
    ("gfx1032", 32),
    ("gfx1033", 32),
    ("gfx1034", 32),
    ("gfx1035", 32),
    ("gfx1036", 32),
    ("gfx1100", 32),
    ("gfx1101", 32),
    ("gfx1102", 32),
    ("gfx1103", 32),
    ("gfx1150", 32),
    ("gfx1151", 32),
    ("gfx1200", 32),
    ("gfx1201", 32),
];

/// Lanes per wavefront for `arch`, or `None` when it is unrecognised.
///
/// # Why this refuses instead of defaulting
///
/// A warp-level reduction folded for the wrong width does not crash,
/// does not trip the failure flag and does not deopt: it returns a
/// plausible number that is wrong by half. Guessing 32 on a CDNA part
/// silently halves every reduction; guessing 64 on RDNA reads lanes
/// that are not there.
///
/// `jit_cuda::target` makes the same argument in the other direction —
/// its tables "saturate downward" because guessing high produces a
/// module that cannot load. Here there is no safe direction, so the
/// answer is to refuse, and an architecture new enough to be missing
/// runs on the CPU until someone adds a row and says which width it is.
///
/// The feature suffixes (`:sramecc+`, `:xnack-`) are stripped for the
/// lookup only. They never reach `--offload-arch`, which takes the
/// string whole.
pub fn wavefront_for_arch(arch: &str) -> Option<u32> {
    let base = arch.split(':').next().unwrap_or(arch).trim();
    // Longest match wins so `gfx1100` cannot be matched by a shorter
    // `gfx110` row if one is ever added.
    GFX_TABLE
        .iter()
        .filter(|(prefix, _)| base == *prefix)
        .map(|(_, w)| *w)
        .next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdna_is_sixty_four_lanes_and_rdna_is_thirty_two() {
        assert_eq!(wavefront_for_arch("gfx90a"), Some(64));
        assert_eq!(wavefront_for_arch("gfx942"), Some(64));
        assert_eq!(wavefront_for_arch("gfx906"), Some(64));
        assert_eq!(wavefront_for_arch("gfx1100"), Some(32));
        assert_eq!(wavefront_for_arch("gfx1030"), Some(32));
        assert_eq!(wavefront_for_arch("gfx1201"), Some(32));
    }

    /// The suffixes are part of the name the runtime wants, and are
    /// only ignored for the table lookup.
    #[test]
    fn feature_suffixes_are_stripped_for_lookup_and_kept_for_the_target() {
        assert_eq!(wavefront_for_arch("gfx90a:sramecc+:xnack-"), Some(64));
        let t = HipTarget::from_arch("gfx90a:sramecc+:xnack-", true, true).unwrap();
        assert_eq!(
            t.gcn_arch_name, "gfx90a:sramecc+:xnack-",
            "the arch string goes to --offload-arch whole; stripping it yields \
             a code object the runtime refuses to load"
        );
        assert_eq!(t.wavefront_size, 64);
    }

    /// The whole point of the table. An unknown architecture must not
    /// resolve to a default, because both defaults are wrong on half
    /// the hardware and neither failure is visible.
    #[test]
    fn an_unknown_architecture_refuses_rather_than_guessing() {
        assert_eq!(wavefront_for_arch("gfx9999"), None);
        assert_eq!(wavefront_for_arch(""), None);
        assert_eq!(wavefront_for_arch("sm_75"), None);
        assert!(HipTarget::from_arch("gfx9999", true, true).is_none());
    }

    /// A prefix that is a prefix of another row must not match it.
    #[test]
    fn a_partial_name_is_not_a_match() {
        assert_eq!(wavefront_for_arch("gfx110"), None);
        assert_eq!(wavefront_for_arch("gfx9"), None);
    }
}
