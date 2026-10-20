// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reproducer minimizer (design §3.4).
//!
//! On a confirmed divergence, shrink to a minimal reproducer that **still
//! diverges** (re-confirmed each step) under an interestingness predicate
//! supplied by the caller — for `cratonvm-difftest min` that predicate is "the candidate
//! still compiles AND still diverges from HotSpot" (see `cmd_min`).
//!
//! The algorithm is classic **ddmin** (Zeller & Hildebrandt's delta debugging)
//! over source *lines*: partition into `n` chunks and try removing each chunk;
//! on success keep the smaller input and coarsen, otherwise refine (`n → 2n`).
//! Line granularity composes naturally with Java structure — removing a `{`
//! without its `}` fails to compile, so the predicate keeps structurally-needed
//! lines automatically.
//!
//! ## Status: Step 6
//!
//! Wired. The bytecode tier reuses `cargo fuzz tmin` over the
//! `difftest_bytecode` target with the predicate swapped to "diverges" (design
//! §3.4); this module is the source tier.

/// Outcome of a minimization pass.
#[derive(Debug, Clone)]
pub struct Minimized {
    /// The reduced source (trailing newline normalized).
    pub source: String,
    /// Number of successful reduction steps.
    pub steps: usize,
    /// Line count of the reduced source.
    pub lines: usize,
}

fn join(lines: &[&str]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// Shrink `source` while `still_diverges` holds.
///
/// `still_diverges(candidate) == true` means the candidate is still
/// "interesting" (compiles and diverges). Runs [`ddmin_pass`] to a **fixpoint**
/// — a single pass refines granularity (`n → 2n`) and stops, but re-running it
/// on the result retries coarse chunks the refinement skipped (which is how a
/// trailing empty `try { } catch { }` block gets removed). The returned source
/// is a local minimum no chunk removal can shrink further.
pub fn minimize_source<F>(source: &str, still_diverges: F) -> Minimized
where
    F: Fn(&str) -> bool,
{
    let mut current = source.to_string();
    let mut total_steps = 0usize;
    loop {
        let pass = ddmin_pass(&current, &still_diverges);
        total_steps += pass.steps;
        current = pass.source;
        if pass.steps == 0 {
            return Minimized {
                lines: pass.lines,
                source: current,
                steps: total_steps,
            };
        }
    }
}

/// One ddmin pass (Zeller & Hildebrandt): partition into `n` chunks, try
/// removing each; on success keep the smaller input and coarsen, else refine.
fn ddmin_pass<F>(source: &str, still_diverges: &F) -> Minimized
where
    F: Fn(&str) -> bool,
{
    let mut lines: Vec<&str> = source.lines().collect();
    let mut steps = 0usize;
    let mut n = 2usize;

    while lines.len() >= 2 {
        let chunk = lines.len().div_ceil(n);
        let mut reduced = false;
        let mut start = 0usize;
        while start < lines.len() {
            let end = (start + chunk).min(lines.len());
            // Candidate = everything except the chunk [start, end).
            let candidate: Vec<&str> = lines[..start]
                .iter()
                .chain(lines[end..].iter())
                .copied()
                .collect();
            if !candidate.is_empty()
                && candidate.len() < lines.len()
                && still_diverges(&join(&candidate))
            {
                lines = candidate;
                steps += 1;
                n = n.saturating_sub(1).max(2);
                reduced = true;
                break; // restart over the smaller input
            }
            start = end;
        }
        if !reduced {
            if n >= lines.len() {
                break; // already at single-line granularity, nothing removable
            }
            n = (n * 2).min(lines.len());
        }
    }

    Minimized {
        lines: lines.len(),
        source: join(&lines),
        steps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pure predicate: the candidate is interesting iff it still contains the
    /// marker line `KEEP`. ddmin should strip everything else.
    #[test]
    fn ddmin_strips_to_the_interesting_line() {
        let src = "a\nb\nKEEP\nc\nd\ne\nf\n";
        let m = minimize_source(src, |s| s.lines().any(|l| l == "KEEP"));
        assert_eq!(m.source.trim(), "KEEP");
        assert_eq!(m.lines, 1);
        assert!(m.steps > 0);
    }

    #[test]
    fn ddmin_keeps_two_required_lines() {
        // Interesting iff BOTH markers survive (and in order).
        let src = "x\nONE\ny\nz\nTWO\nw\n";
        let m = minimize_source(src, |s| {
            let ls: Vec<&str> = s.lines().collect();
            ls.contains(&"ONE") && ls.contains(&"TWO")
        });
        let kept: Vec<&str> = m.source.lines().collect();
        assert_eq!(kept, vec!["ONE", "TWO"]);
    }

    #[test]
    fn ddmin_noop_when_nothing_removable() {
        // Every line is required ⇒ no reduction.
        let src = "ONE\nTWO\n";
        let m = minimize_source(src, |s| {
            let ls: Vec<&str> = s.lines().collect();
            ls.contains(&"ONE") && ls.contains(&"TWO")
        });
        assert_eq!(m.lines, 2);
        assert_eq!(m.steps, 0);
    }

    #[test]
    fn ddmin_single_line_input_is_stable() {
        let m = minimize_source("only\n", |_| true);
        assert_eq!(m.source.trim(), "only");
    }
}
