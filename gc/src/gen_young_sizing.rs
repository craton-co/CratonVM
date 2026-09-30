// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot young-generation sizing (`-Xmn`, `-XX:NewSize`, `-XX:MaxNewSize`,
//! `-XX:NewRatio`) for the generational backend.
//!
//! gen r4w2/alloc2 (2026-09-23). Until this module the four flags were
//! swallowed by the launcher's `-X…` / `-XX:…` catch-alls and the young
//! generation was hard-wired by [`crate::gen_heap::GenerationalHeap::with_capacity`]
//! to half of `-Xmx` (one quarter per semi-space) —
//! `docs/internal/gc/gengc-r4-alloc-xmn-and-newratio-are-silently-ignored-FIXED-20260923.md`.
//!
//! # Semantics
//!
//! HotSpot's young generation is eden plus two survivor spaces; this
//! collector's is the from/to semi-space PAIR. A young size therefore maps to
//! the pair, and each semi-space gets half of it.
//!
//! * `-Xmn<size>` sets the young size exactly (HotSpot: `NewSize = MaxNewSize
//!   = Xmn`). It wins over the other three, as in HotSpot.
//! * Otherwise the base is `-Xmx / (NewRatio + 1)` when `-XX:NewRatio` is
//!   given, else this collector's own default (`-Xmx / 2`).
//! * `-XX:NewSize` is a FLOOR and `-XX:MaxNewSize` a CEILING on that base.
//!   HotSpot starts the young generation at `NewSize` and lets it grow toward
//!   `MaxNewSize`; this collector's young pair does not resize once built (the
//!   `-Xmx` budget leaves the growth path nothing to give — see
//!   `with_capacity`), so the honest single-number reading of the pair is
//!   "the base, held between the two". A `NewSize` above `MaxNewSize` raises
//!   the ceiling to `NewSize`, which is HotSpot's resolution too.
//! * The old generation must stay non-empty: the pair is clamped so at least
//!   [`MIN_OLD_GEN_BYTES`] (or half the heap, on a heap smaller than twice
//!   that) is left for it, and each semi-space is at least [`YOUNG_SEMI_MIN`].
//!   Every clamp or override is reported in [`YoungGenPlan::adjustments`], which
//!   the launcher prints once at startup.
//!
//! With none of the four given, [`plan_young_gen`] answers `None` and the heap
//! is built by `with_capacity`, byte for byte as before.
//!
//! G1 and ZGC do not consult this: G1 sizes its young generation adaptively
//! from region counts, ZGC has no young generation. The launcher tells the
//! operator so, once.

/// The smallest young semi-space this module will plan: the floor
/// `GenerationalHeap::with_sizes_and_max_young` applies to every arena
/// (`young_semi_size.max(1024)`), so a planned pair is never silently widened
/// past the `-Xmx` budget by the constructor.
pub const YOUNG_SEMI_MIN: usize = 1024;

/// The old generation keeps at least this much of the heap when an explicit
/// young size would otherwise take it all (or half the heap, when the heap is
/// smaller than twice this). HotSpot's equivalent floor is one generation
/// alignment; one MiB is of that order and keeps a tenure from failing on the
/// very first promoted object.
pub const MIN_OLD_GEN_BYTES: usize = 1 << 20;

/// The smallest total heap the generational constructors build; mirrors
/// `with_capacity`'s `total_bytes.max(4096)`.
const MIN_TOTAL_BYTES: usize = 4096;

/// What the operator asked for, flag by flag. `None` = not given.
///
/// Per VM (it travels on `VmConfig`), never a process global.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct YoungGenSizing {
    /// `-Xmn<size>`, bytes.
    pub xmn: Option<usize>,
    /// `-XX:NewSize=<size>`, bytes.
    pub new_size: Option<usize>,
    /// `-XX:MaxNewSize=<size>`, bytes.
    pub max_new_size: Option<usize>,
    /// `-XX:NewRatio=<n>`: old / young.
    pub new_ratio: Option<usize>,
}

impl YoungGenSizing {
    /// `true` when none of the four flags was given — the default sizing.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The HotSpot spellings of the flags that were given, in a fixed order —
    /// for the launcher's "ignored on this collector" note.
    pub fn given_flag_names(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.xmn.is_some() {
            out.push("-Xmn");
        }
        if self.new_size.is_some() {
            out.push("-XX:NewSize");
        }
        if self.max_new_size.is_some() {
            out.push("-XX:MaxNewSize");
        }
        if self.new_ratio.is_some() {
            out.push("-XX:NewRatio");
        }
        out
    }
}

/// The resolved geometry for an explicit young size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoungGenPlan {
    /// The heap budget the plan divides (`-Xmx`, floored like `with_capacity`).
    pub total_bytes: usize,
    /// Each young semi-space, bytes (8-aligned).
    pub young_semi: usize,
    /// The old generation, bytes: `total_bytes - 2 * young_semi`.
    pub old_size: usize,
    /// The young pair the flags asked for, before any clamp.
    pub requested_pair: usize,
    /// Which flag decided the size (`"-Xmn"`, `"-XX:NewRatio"`, ...).
    pub source: &'static str,
    /// One human-readable line per clamp or override. Empty when the request
    /// was honoured exactly.
    pub adjustments: Vec<String>,
}

impl YoungGenPlan {
    /// The young pair actually planned (`2 * young_semi`).
    pub fn young_pair(&self) -> usize {
        self.young_semi.saturating_mul(2)
    }

    /// One line for `--verbose:gc`.
    pub fn summary(&self) -> String {
        format!(
            "[cratonvm] generational young generation: {} ({}; 2 x {} semi-spaces), \
             old generation {}, heap {}",
            fmt_bytes(self.young_pair()),
            self.source,
            fmt_bytes(self.young_semi),
            fmt_bytes(self.old_size),
            fmt_bytes(self.total_bytes),
        )
    }
}

/// Render a byte count the way the flags are usually written.
fn fmt_bytes(n: usize) -> String {
    const K: usize = 1024;
    if n >= K * K && n.is_multiple_of(K * K) {
        format!("{} MiB", n / (K * K))
    } else if n >= K && n.is_multiple_of(K) {
        format!("{} KiB", n / K)
    } else {
        format!("{n} bytes")
    }
}

/// This collector's default young semi-space for a `total_bytes` heap — the
/// arithmetic `GenerationalHeap::with_capacity` uses.
pub fn default_young_semi(total_bytes: usize) -> usize {
    (total_bytes.max(MIN_TOTAL_BYTES) / 4).max(YOUNG_SEMI_MIN)
}

/// Resolve `req` against a `total_bytes` heap. `None` when no flag was given:
/// the caller must then build the default heap, unchanged.
pub fn plan_young_gen(total_bytes: usize, req: &YoungGenSizing) -> Option<YoungGenPlan> {
    if req.is_default() {
        return None;
    }
    let total = total_bytes.max(MIN_TOTAL_BYTES);
    let mut adjustments = Vec::new();

    let (pair, source) = if let Some(xmn) = req.xmn {
        if req.new_size.is_some() || req.max_new_size.is_some() {
            adjustments.push(
                "-Xmn sets the young size exactly; -XX:NewSize / -XX:MaxNewSize are ignored"
                    .to_string(),
            );
        }
        if req.new_ratio.is_some() {
            adjustments
                .push("-Xmn sets the young size exactly; -XX:NewRatio is ignored".to_string());
        }
        (xmn, "-Xmn")
    } else {
        let (base, base_source) = match req.new_ratio {
            Some(r) => (total / r.saturating_add(1), "-XX:NewRatio"),
            None => (
                default_young_semi(total).saturating_mul(2),
                "default -Xmx/2",
            ),
        };
        let floor = req.new_size;
        let mut ceiling = req.max_new_size;
        if let (Some(n), Some(m)) = (floor, ceiling) {
            if n > m {
                adjustments.push(format!(
                    "-XX:NewSize ({}) is larger than -XX:MaxNewSize ({}); a maximum young \
                     size of {} will be used",
                    fmt_bytes(n),
                    fmt_bytes(m),
                    fmt_bytes(n),
                ));
                ceiling = Some(n);
            }
        }
        let mut pair = base;
        let mut source = base_source;
        if let Some(m) = ceiling {
            if pair > m {
                pair = m;
                source = "-XX:MaxNewSize";
            }
        }
        if let Some(n) = floor {
            if pair < n {
                pair = n;
                source = "-XX:NewSize";
            }
        }
        (pair, source)
    };

    // Clamp: the old generation stays non-empty, and each semi is usable.
    let min_old = MIN_OLD_GEN_BYTES.min(total / 2);
    let max_semi = ((total - min_old) / 2) & !7;
    let mut young_semi = (pair / 2) & !7;
    if young_semi > max_semi {
        adjustments.push(format!(
            "{source} asks for a {} young generation in a {} heap; clamped to {} so the \
             old generation keeps {}",
            fmt_bytes(pair),
            fmt_bytes(total),
            fmt_bytes(max_semi * 2),
            fmt_bytes(total - max_semi * 2),
        ));
        young_semi = max_semi;
    }
    if young_semi < YOUNG_SEMI_MIN {
        adjustments.push(format!(
            "{source} asks for a {} young generation; raised to the minimum of {}",
            fmt_bytes(pair),
            fmt_bytes(YOUNG_SEMI_MIN * 2),
        ));
        young_semi = YOUNG_SEMI_MIN;
    }
    let old_size = total - young_semi * 2;
    Some(YoungGenPlan {
        total_bytes: total,
        young_semi,
        old_size,
        requested_pair: pair,
        source,
        adjustments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: usize = 1 << 20;
    const G: usize = 1 << 30;

    #[test]
    fn no_flag_means_no_plan() {
        assert!(YoungGenSizing::default().is_default());
        assert_eq!(plan_young_gen(G, &YoungGenSizing::default()), None);
    }

    #[test]
    fn xmn_is_the_young_pair() {
        let req = YoungGenSizing {
            xmn: Some(256 * M),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("a flag was given");
        assert_eq!(p.young_semi, 128 * M);
        assert_eq!(p.young_pair(), 256 * M);
        assert_eq!(p.old_size, G - 256 * M);
        assert_eq!(p.source, "-Xmn");
        assert!(p.adjustments.is_empty(), "{:?}", p.adjustments);
    }

    #[test]
    fn new_ratio_divides_the_heap() {
        // NewRatio=2: young = 1/3 of the heap.
        let req = YoungGenSizing {
            new_ratio: Some(2),
            ..Default::default()
        };
        let p = plan_young_gen(3 * G, &req).expect("plan");
        assert_eq!(p.young_pair(), G);
        assert_eq!(p.old_size, 2 * G);
        // NewRatio=1 is this collector's default split.
        let one = YoungGenSizing {
            new_ratio: Some(1),
            ..Default::default()
        };
        let p1 = plan_young_gen(G, &one).expect("plan");
        assert_eq!(p1.young_semi, default_young_semi(G));
    }

    #[test]
    fn xmn_wins_over_the_other_three_and_says_so() {
        let req = YoungGenSizing {
            xmn: Some(64 * M),
            new_size: Some(512 * M),
            max_new_size: Some(512 * M),
            new_ratio: Some(1),
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_pair(), 64 * M);
        assert_eq!(p.adjustments.len(), 2, "{:?}", p.adjustments);
    }

    #[test]
    fn new_size_is_a_floor_and_max_new_size_a_ceiling() {
        // Default base is 512 MiB of a 1 GiB heap.
        let cap = YoungGenSizing {
            max_new_size: Some(128 * M),
            ..Default::default()
        };
        assert_eq!(plan_young_gen(G, &cap).expect("plan").young_pair(), 128 * M);
        let above = YoungGenSizing {
            max_new_size: Some(900 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &above).expect("plan").young_pair(),
            512 * M
        );
        let floor = YoungGenSizing {
            new_size: Some(768 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &floor).expect("plan").young_pair(),
            768 * M
        );
        let below = YoungGenSizing {
            new_size: Some(64 * M),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &below).expect("plan").young_pair(),
            512 * M
        );
        // Both, and with a ratio base.
        let both = YoungGenSizing {
            new_size: Some(100 * M),
            max_new_size: Some(200 * M),
            new_ratio: Some(3),
            ..Default::default()
        };
        assert_eq!(
            plan_young_gen(G, &both).expect("plan").young_pair(),
            200 * M
        );
    }

    #[test]
    fn a_new_size_above_max_new_size_raises_the_ceiling() {
        let req = YoungGenSizing {
            new_size: Some(300 * M),
            max_new_size: Some(100 * M),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_pair(), 300 * M);
        assert_eq!(p.adjustments.len(), 1, "{:?}", p.adjustments);
    }

    #[test]
    fn the_old_generation_is_never_emptied() {
        for req in [
            YoungGenSizing {
                xmn: Some(G),
                ..Default::default()
            },
            YoungGenSizing {
                xmn: Some(4 * G),
                ..Default::default()
            },
            YoungGenSizing {
                new_ratio: Some(0),
                ..Default::default()
            },
            YoungGenSizing {
                new_size: Some(usize::MAX),
                ..Default::default()
            },
        ] {
            let p = plan_young_gen(G, &req).expect("plan");
            assert!(p.old_size >= MIN_OLD_GEN_BYTES, "{req:?} -> {p:?}");
            assert_eq!(p.old_size + p.young_pair(), G, "{req:?}");
            assert_eq!(p.young_semi % 8, 0);
            assert_eq!(
                p.adjustments.len(),
                1,
                "exactly one clamp note: {:?}",
                p.adjustments
            );
        }
    }

    #[test]
    fn a_tiny_young_request_is_raised_to_the_minimum() {
        let req = YoungGenSizing {
            xmn: Some(8),
            ..Default::default()
        };
        let p = plan_young_gen(G, &req).expect("plan");
        assert_eq!(p.young_semi, YOUNG_SEMI_MIN);
        assert_eq!(p.adjustments.len(), 1);
    }

    #[test]
    fn a_tiny_heap_still_plans_a_coherent_geometry() {
        for total in [0usize, 4096, 64 * 1024, 3 * M] {
            let req = YoungGenSizing {
                xmn: Some(usize::MAX),
                ..Default::default()
            };
            let p = plan_young_gen(total, &req).expect("plan");
            assert!(p.young_semi >= YOUNG_SEMI_MIN);
            assert!(p.old_size > 0);
            assert_eq!(p.old_size + p.young_pair(), total.max(MIN_TOTAL_BYTES));
        }
    }

    #[test]
    fn the_flag_names_follow_what_was_given() {
        let req = YoungGenSizing {
            xmn: Some(1),
            new_ratio: Some(2),
            ..Default::default()
        };
        assert_eq!(req.given_flag_names(), vec!["-Xmn", "-XX:NewRatio"]);
        assert!(YoungGenSizing::default().given_flag_names().is_empty());
    }

    #[test]
    fn the_summary_names_the_source_and_the_split() {
        let req = YoungGenSizing {
            xmn: Some(256 * M),
            ..Default::default()
        };
        let s = plan_young_gen(G, &req).expect("plan").summary();
        assert!(s.contains("256 MiB (-Xmn; 2 x 128 MiB semi-spaces)"), "{s}");
        assert!(s.contains("old generation 768 MiB"), "{s}");
    }
}
