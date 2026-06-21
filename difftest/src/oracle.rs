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
//! ## Status: Step 1
//!
//! [`parse_exception`] and the four-channel [`compare`] are wired (strict
//! equality). The opt-in normalizers (path/hash/thread-id stripping, line
//! sorting) are declared but inert — strict line-ending hygiene is the only
//! transform applied, so nothing can silently mask a real diff. Joining the
//! per-mode verdicts into a `Classification` (`JitOnly`, …) is Step 2.

use crate::ledger::{Channel, Classification, JvmException, Observation};
use crate::runner::Mode;

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
    /// Step 1 applies **only** the always-safe line-ending hygiene
    /// ([`normalize_line_endings`]); the opt-in transforms above are declared
    /// but inert until a later step wires them (each gated behind its flag, so
    /// strict mode stays byte-exact).
    pub fn apply(&self, s: &str) -> String {
        let normalized = normalize_line_endings(s);
        if self.sort_lines {
            let mut lines: Vec<&str> = normalized.lines().collect();
            lines.sort_unstable();
            return lines.join("\n");
        }
        normalized
    }
}

/// CRLF→LF and trailing-whitespace trim, so a stray platform `\r` cannot
/// masquerade as a behavioral divergence. Lifted from
/// `vm/tests/intrinsic_diff.rs`'s `normalize`.
pub fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Exception parsing
// ---------------------------------------------------------------------------

/// Parse the uncaught-exception banner out of a captured `stderr` blob.
///
/// HotSpot (and CratonVM) print:
///
/// ```text
/// Exception in thread "main" <fqcn>: <message>
///     at <frame>
///     at <frame>
///     ...
/// ```
///
/// or, for a message-less throwable, `Exception in thread "main" <fqcn>`.
/// Returns `None` when no such banner is present (a clean run). Only the first
/// banner is parsed (the primary uncaught exception); `Caused by:` chains are
/// captured as additional frames.
pub fn parse_exception(stderr: &str) -> Option<JvmException> {
    let mut lines = stderr.lines();
    let header = lines
        .by_ref()
        .find(|l| l.trim_start().starts_with("Exception in thread "))?;

    // After `Exception in thread "<name>" ` comes `<fqcn>[: <message>]`.
    // Split at the first space following the quoted thread name.
    let after_thread = header
        .trim_start()
        .strip_prefix("Exception in thread ")?
        .split_once(' ')
        .map(|(_thread, rest)| rest)?
        .trim();

    let (fqcn, message) = match after_thread.split_once(": ") {
        Some((c, m)) => (c.trim().to_string(), m.trim().to_string()),
        None => (after_thread.to_string(), String::new()),
    };

    // Subsequent indented `at <frame>` lines, in source order.
    let top_frames: Vec<String> = lines
        .map(str::trim)
        .filter(|l| l.starts_with("at "))
        .map(|l| l.trim_start_matches("at ").trim().to_string())
        .collect();

    Some(JvmException {
        fqcn,
        message,
        top_frames,
    })
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// One concrete channel disagreement between the two VMs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelDiff {
    pub channel: Channel,
    pub cratonvm: String,
    pub hotspot: String,
}

/// The result of comparing CratonVM vs HotSpot for one program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Observations agree on every gated channel.
    Agree,
    /// A confirmed divergence on one or more channels.
    Diverge(Vec<ChannelDiff>),
}

impl Verdict {
    /// True when the two observations agreed.
    pub fn agrees(&self) -> bool {
        matches!(self, Verdict::Agree)
    }
}

/// Compare two observations under `normalizer`.
///
/// Hard channels (any disagreement is a divergence): **exit code**, **uncaught
/// exception identity** (fqcn exact, message exact-after-normalize, frames in
/// order), and **stdout** (exact after normalize). A CratonVM **timeout** while
/// HotSpot finished is reported on the exit-code channel (`<timeout>`). stderr
/// is contextual and not gated here (warnings differ legitimately) — exception
/// identity is the gated slice of stderr.
pub fn compare(cratonvm: &Observation, hotspot: &Observation, normalizer: &Normalizer) -> Verdict {
    let mut diffs = Vec::new();

    // Exit code (a timeout is reported as a distinct, never-matching token).
    let c_exit = exit_token(cratonvm);
    let h_exit = exit_token(hotspot);
    if c_exit != h_exit {
        diffs.push(ChannelDiff {
            channel: Channel::ExitCode,
            cratonvm: c_exit,
            hotspot: h_exit,
        });
    }

    // Uncaught exception identity.
    if let Some(diff) = compare_exception(&cratonvm.exception, &hotspot.exception, normalizer) {
        diffs.push(diff);
    }

    // stdout (strict after normalization).
    let c_out = normalizer.apply(&cratonvm.stdout);
    let h_out = normalizer.apply(&hotspot.stdout);
    if c_out != h_out {
        diffs.push(ChannelDiff {
            channel: Channel::Stdout,
            cratonvm: c_out,
            hotspot: h_out,
        });
    }

    if diffs.is_empty() {
        Verdict::Agree
    } else {
        Verdict::Diverge(diffs)
    }
}

/// Render the exit channel: a timed-out run never matches a clean exit.
fn exit_token(o: &Observation) -> String {
    if o.timed_out {
        "<timeout>".to_string()
    } else {
        match o.exit_code {
            Some(c) => c.to_string(),
            None => "<signal>".to_string(),
        }
    }
}

/// Compare two optional exceptions; returns a `ChannelDiff` on disagreement.
/// fqcn and ordered frames are exact; the message is compared after the
/// normalizer (so a path/hash in a message can be masked when opted in).
fn compare_exception(
    cratonvm: &Option<JvmException>,
    hotspot: &Option<JvmException>,
    normalizer: &Normalizer,
) -> Option<ChannelDiff> {
    let render = |e: &Option<JvmException>| match e {
        None => "<none>".to_string(),
        Some(ex) => format!(
            "{}: {} [{}]",
            ex.fqcn,
            normalizer.apply(&ex.message),
            ex.top_frames.join(" / ")
        ),
    };
    let c = render(cratonvm);
    let h = render(hotspot);
    if c != h {
        Some(ChannelDiff {
            channel: Channel::Exception,
            cratonvm: c,
            hotspot: h,
        })
    } else {
        None
    }
}

/// One CratonVM mode's outcome, distilled for classification: did it diverge
/// from HotSpot, and did it hang / crash?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeVerdict {
    pub mode: Mode,
    pub diverged: bool,
    pub timed_out: bool,
    /// Exited by signal/abort (no exit code, and not a timeout).
    pub crashed: bool,
}

/// Classify a divergence from the per-mode verdict map (design §3.3) — the
/// automation of the manual `--nojit` bisection. Returns `None` when **every**
/// mode agreed with HotSpot.
///
/// Decision order:
/// 1. **Hang** — any mode timed out (dominates; the most urgent bucket).
/// 2. **Crash** — any mode exited by signal/abort.
/// 3. **JitOnly** — the interpreter (`nojit`) ran and *agreed*, but a JIT-
///    enabled mode (`jit-on` / `low-jit-threshold`) diverged ⇒ a JIT bug.
/// 4. **GcMode** — `jit-on` and `nojit` both agree, but a GC mode (`moving-gc`)
///    diverged ⇒ the divergence is GC-specific.
/// 5. **Universal** — otherwise (the bug is in the shared interpreter/native
///    path, present with and without the JIT).
pub fn classify(modes: &[ModeVerdict]) -> Option<Classification> {
    if modes.iter().any(|m| m.timed_out) {
        return Some(Classification::Hang);
    }
    if modes.iter().any(|m| m.crashed) {
        return Some(Classification::Crash);
    }
    if !modes.iter().any(|m| m.diverged) {
        return None; // everything agreed
    }

    // Look up a specific mode's diverged status (None = that mode wasn't run).
    let diverged = |want: Mode| modes.iter().find(|m| m.mode == want).map(|m| m.diverged);
    let jit_modes_diverge =
        diverged(Mode::JitOn) == Some(true) || diverged(Mode::LowJitThreshold) == Some(true);

    // JitOnly needs the interpreter baseline (nojit) to have run AND agreed.
    if diverged(Mode::NoJit) == Some(false) && jit_modes_diverge {
        return Some(Classification::JitOnly);
    }
    // GcMode: both the interpreter and default-GC JIT agree, a GC mode diverges.
    if diverged(Mode::JitOn) == Some(false)
        && diverged(Mode::NoJit) != Some(true)
        && diverged(Mode::MovingGc) == Some(true)
    {
        return Some(Classification::GcMode);
    }
    Some(Classification::Universal)
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

    fn obs(stdout: &str, stderr: &str, exit: Option<i32>) -> Observation {
        Observation {
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_code: exit,
            exception: parse_exception(stderr),
            timed_out: false,
            wall_ms: 0,
        }
    }

    #[test]
    fn line_endings_normalized() {
        assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize_line_endings("a\nb\n\n  "), "a\nb");
    }

    #[test]
    fn parses_exception_with_message_and_frames() {
        let stderr = "Exception in thread \"main\" java.lang.ArithmeticException: / by zero\n\
                      \tat ArithEdge.main(ArithEdge.java:5)\n";
        let ex = parse_exception(stderr).expect("parsed");
        assert_eq!(ex.fqcn, "java.lang.ArithmeticException");
        assert_eq!(ex.message, "/ by zero");
        assert_eq!(ex.top_frames, vec!["ArithEdge.main(ArithEdge.java:5)"]);
    }

    #[test]
    fn parses_exception_without_message() {
        let stderr = "Exception in thread \"main\" java.lang.NullPointerException\n";
        let ex = parse_exception(stderr).expect("parsed");
        assert_eq!(ex.fqcn, "java.lang.NullPointerException");
        assert_eq!(ex.message, "");
    }

    #[test]
    fn no_banner_is_none() {
        assert!(parse_exception("hello\nworld\n").is_none());
    }

    #[test]
    fn identical_runs_agree() {
        let a = obs("42\n", "", Some(0));
        let b = obs("42\n", "", Some(0));
        assert!(compare(&a, &b, &Normalizer::strict()).agrees());
    }

    #[test]
    fn stdout_divergence_is_caught() {
        let a = obs("42", "", Some(0));
        let b = obs("43", "", Some(0));
        let v = compare(&a, &b, &Normalizer::strict());
        match v {
            Verdict::Diverge(d) => {
                assert_eq!(d.len(), 1);
                assert_eq!(d[0].channel, Channel::Stdout);
            }
            Verdict::Agree => panic!("expected divergence"),
        }
    }

    #[test]
    fn exception_type_mismatch_is_caught() {
        let a = obs(
            "",
            "Exception in thread \"main\" java.lang.ClassCastException: x\n",
            Some(1),
        );
        let b = obs(
            "",
            "Exception in thread \"main\" java.lang.IllegalStateException: x\n",
            Some(1),
        );
        let v = compare(&a, &b, &Normalizer::strict());
        match v {
            Verdict::Diverge(d) => assert!(d.iter().any(|c| c.channel == Channel::Exception)),
            Verdict::Agree => panic!("expected exception divergence"),
        }
    }

    #[test]
    fn timeout_never_matches_clean_exit() {
        let mut a = obs("", "", None);
        a.timed_out = true;
        let b = obs("", "", Some(0));
        let v = compare(&a, &b, &Normalizer::strict());
        match v {
            Verdict::Diverge(d) => assert!(d.iter().any(|c| c.channel == Channel::ExitCode)),
            Verdict::Agree => panic!("timeout must diverge from clean exit"),
        }
    }

    fn mv(mode: Mode, diverged: bool) -> ModeVerdict {
        ModeVerdict {
            mode,
            diverged,
            timed_out: false,
            crashed: false,
        }
    }

    #[test]
    fn classify_all_agree_is_none() {
        let modes = [mv(Mode::JitOn, false), mv(Mode::NoJit, false)];
        assert_eq!(classify(&modes), None);
    }

    #[test]
    fn classify_jit_only() {
        // jit-on diverges, nojit agrees ⇒ JitOnly (the --nojit bisection).
        let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, false)];
        assert_eq!(classify(&modes), Some(Classification::JitOnly));
    }

    #[test]
    fn classify_universal_when_both_diverge() {
        // The ExceptionId case: an interpreter/native gap shows in BOTH modes.
        let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, true)];
        assert_eq!(classify(&modes), Some(Classification::Universal));
    }

    #[test]
    fn classify_gc_mode() {
        // jit-on + nojit agree, only the moving-GC mode diverges.
        let modes = [
            mv(Mode::JitOn, false),
            mv(Mode::NoJit, false),
            mv(Mode::MovingGc, true),
        ];
        assert_eq!(classify(&modes), Some(Classification::GcMode));
    }

    #[test]
    fn classify_low_threshold_is_jit_only() {
        // A JIT bug that only trips when compilation is forced early.
        let modes = [
            mv(Mode::JitOn, false),
            mv(Mode::NoJit, false),
            mv(Mode::LowJitThreshold, true),
        ];
        assert_eq!(classify(&modes), Some(Classification::JitOnly));
    }

    #[test]
    fn classify_hang_and_crash_dominate() {
        let mut hang = mv(Mode::JitOn, true);
        hang.timed_out = true;
        assert_eq!(classify(&[hang]), Some(Classification::Hang));
        let mut crash = mv(Mode::JitOn, true);
        crash.crashed = true;
        assert_eq!(classify(&[crash]), Some(Classification::Crash));
    }

    #[test]
    fn first_diff_points_at_line() {
        let d = first_diff("a\nb\nc", "a\nX\nc");
        assert_eq!(d, Some((2, "b".to_string(), "X".to_string())));
        assert_eq!(first_diff("same", "same"), None);
    }
}
