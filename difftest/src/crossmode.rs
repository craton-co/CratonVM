// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Cross-path comparison: CratonVM against **itself**.
//!
//! Everything else in this crate compares CratonVM to a reference JDK. That is
//! the right primary oracle, but it has a structural weakness the C2 review
//! calls out: a divergence against HotSpot is only evidence once you have ruled
//! out the harness, the environment and the reference. A divergence between two
//! of *CratonVM's own* execution paths has no such escape route.
//!
//! The VM has four semantic implementations — the interpreter's raw
//! superinstruction fast path, the interpreter's decoded fallback, the
//! single-pass (direct) x64 emitter and the optimizing IR pipeline — plus OSR
//! entry and deoptimization as cross-cutting transitions between them. Every
//! one of them is supposed to implement the *same* JVMS semantics. So for a
//! deterministic program:
//!
//! > If `nojit` and `ir-jit` print different things, one of them is wrong. No
//! > reference JDK is needed to know that, no normalization rule can be blamed
//! > for it, and no environment difference explains it — the two runs differ
//! > only in which of the VM's own executors ran the bytecode.
//!
//! That is the strongest signal this harness can produce, and it is the one the
//! report's P1 ("one executable bytecode-semantics contract") asks for.
//!
//! ## Two rules that keep it honest
//!
//! 1. **Same compatibility profile only.** Comparing a `--jdk-only` run against
//!    a compatible one would report the *policy* difference — a refused
//!    compatibility class, a missing native — as a semantic path split. Those
//!    pairs are skipped; the policy axis has its own ledger rows.
//! 2. **Report, never gate.** A `JitOnly` divergence already on the committed
//!    ledger *is* a path split by construction (`jit-on` disagrees with HotSpot,
//!    `nojit` agrees, so the two disagree with each other). Gating on path
//!    splits would fail the frozen baseline wholesale on findings that are
//!    already tracked. Path splits are therefore reported alongside the gate
//!    verdict and deliberately excluded from its exit code, exactly as the
//!    `jdk-only` violation census is.

use crate::ledger::{Channel, Observation};
use crate::oracle::{self, ChannelDiff, Normalizer, Verdict};
use crate::runner::Mode;

/// Two of CratonVM's own execution paths disagreeing on the same program.
#[derive(Debug, Clone)]
pub struct PathDisagreement {
    /// The first mode, in `--modes` order.
    pub a: Mode,
    /// The second mode.
    pub b: Mode,
    /// The dimensions on which they disagreed.
    pub diffs: Vec<ChannelDiff>,
}

impl PathDisagreement {
    /// The channels this split covers, in report order.
    pub fn channels(&self) -> Vec<Channel> {
        self.diffs.iter().map(|d| d.channel).collect()
    }

    /// `nojit≠ir-jit[stdout]` — the compact form used in run/gate output.
    pub fn label(&self) -> String {
        let chans: Vec<&str> = self.channels().iter().map(|c| c.label()).collect();
        format!("{}≠{}[{}]", self.a.label(), self.b.label(), chans.join(","))
    }
}

/// Whether two modes may be compared against each other.
///
/// Same compatibility profile (rule 1 above) and the same normalizer — the
/// second is implied by the first today, and asserted rather than assumed so a
/// future mode that changes normalization cannot silently start producing
/// normalization diffs dressed as path splits.
pub fn comparable(a: Mode, b: Mode) -> bool {
    a != b
        && a.jdk_profile() == b.jdk_profile()
        && Normalizer::for_mode(a) == Normalizer::for_mode(b)
}

/// Compare every comparable pair of CratonVM runs against each other and return
/// the pairs that disagreed.
///
/// `runs` is `(mode, observation)` in `--modes` order; the output preserves that
/// order (`a` before `b`), so a report reads left-to-right down the matrix.
pub fn disagreements(runs: &[(Mode, &Observation)]) -> Vec<PathDisagreement> {
    let mut out = Vec::new();
    for (i, (mode_a, obs_a)) in runs.iter().enumerate() {
        for (mode_b, obs_b) in runs.iter().skip(i + 1) {
            if !comparable(*mode_a, *mode_b) {
                continue;
            }
            // Neither side is a reference: `compare`'s two arguments are
            // positional, and the `cratonvm`/`hotspot` field names on a
            // `ChannelDiff` read as "first path" / "second path" here.
            if let Verdict::Diverge(diffs) =
                oracle::compare(obs_a, obs_b, &Normalizer::for_mode(*mode_a))
            {
                out.push(PathDisagreement {
                    a: *mode_a,
                    b: *mode_b,
                    diffs,
                });
            }
        }
    }
    out
}

/// Render the path splits for one program, one per line, indented for the run
/// summary. Empty when the paths agreed.
pub fn render(program: &str, splits: &[PathDisagreement]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for split in splits {
        let _ = writeln!(s, "  PATH  {program} — {}", split.label());
        for d in &split.diffs {
            let _ = writeln!(
                s,
                "          {}: {} != {}",
                d.channel.label(),
                one_line(&d.cratonvm),
                one_line(&d.hotspot)
            );
        }
    }
    s
}

/// Collapse a captured blob to a single short line for the summary.
fn one_line(s: &str) -> String {
    let flat = s.replace('\n', " / ");
    if flat.chars().count() <= 80 {
        return flat;
    }
    let head: String = flat.chars().take(77).collect();
    format!("{head}...")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(stdout: &str) -> Observation {
        Observation {
            stdout: stdout.to_string(),
            exit_code: Some(0),
            ..Observation::empty()
        }
    }

    #[test]
    fn agreeing_paths_produce_no_split() {
        let a = obs("42");
        let b = obs("42");
        let c = obs("42");
        let runs = [(Mode::NoJit, &a), (Mode::DirectEmit, &b), (Mode::IrJit, &c)];
        assert!(disagreements(&runs).is_empty());
    }

    #[test]
    fn a_path_split_is_reported_without_any_reference() {
        // The interpreter and the IR pipeline printed different things. That is
        // a wrong-code finding on its own — no HotSpot observation involved.
        let interp = obs("42");
        let ir = obs("43");
        let runs = [(Mode::NoJit, &interp), (Mode::IrJit, &ir)];
        let splits = disagreements(&runs);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].a, Mode::NoJit);
        assert_eq!(splits[0].b, Mode::IrJit);
        assert_eq!(splits[0].channels(), vec![Channel::Stdout]);
        assert_eq!(splits[0].label(), "nojit≠ir-jit[stdout]");
        assert!(render("P", &splits).contains("PATH  P — nojit≠ir-jit[stdout]"));
    }

    #[test]
    fn every_disagreeing_pair_is_reported_not_just_the_first() {
        // Three paths, one odd one out ⇒ two splits, so a report cannot leave
        // the reader guessing which path is the outlier.
        let a = obs("42");
        let b = obs("42");
        let odd = obs("99");
        let runs = [
            (Mode::NoJit, &a),
            (Mode::DirectEmit, &b),
            (Mode::IrJit, &odd),
        ];
        let splits = disagreements(&runs);
        assert_eq!(splits.len(), 2);
        let labels: Vec<String> = splits.iter().map(|s| s.label()).collect();
        assert!(labels.contains(&"nojit≠ir-jit[stdout]".to_string()));
        assert!(labels.contains(&"direct-emit≠ir-jit[stdout]".to_string()));
    }

    #[test]
    fn a_policy_pair_is_never_compared_as_a_path_split() {
        // `--jdk-only` refusing a compatibility class is a policy finding with
        // its own ledger row, not a semantic disagreement between executors.
        assert!(!comparable(Mode::JitOn, Mode::JdkOnlyJit));
        assert!(!comparable(Mode::NoJit, Mode::JdkOnlyNoJit));
        // Within one profile the pairs are comparable.
        assert!(comparable(Mode::NoJit, Mode::IrJit));
        assert!(comparable(Mode::JdkOnlyJit, Mode::JdkOnlyNoJit));
        // A mode is never compared with itself.
        assert!(!comparable(Mode::NoJit, Mode::NoJit));

        let strict = obs("refused");
        let compat = obs("ran");
        let runs = [(Mode::JdkOnlyJit, &strict), (Mode::JitOn, &compat)];
        assert!(
            disagreements(&runs).is_empty(),
            "a cross-policy pair must not be reported as a path split"
        );
    }

    #[test]
    fn a_timeout_on_one_path_only_is_a_split() {
        // The `Hang` shape, seen without a reference: one executor finished and
        // another did not.
        let ok = obs("done");
        let mut hung = obs("");
        hung.timed_out = true;
        hung.exit_code = None;
        let runs = [(Mode::NoJit, &ok), (Mode::OsrEager, &hung)];
        let splits = disagreements(&runs);
        assert_eq!(splits.len(), 1);
        assert!(splits[0].channels().contains(&Channel::ExitCode));
    }

    #[test]
    fn one_line_truncates_long_blobs() {
        assert_eq!(one_line("a\nb"), "a / b");
        let long = "x".repeat(200);
        let flat = one_line(&long);
        assert_eq!(flat.chars().count(), 80);
        assert!(flat.ends_with("..."));
    }
}
