// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The diff oracle: compare two [`Observation`](crate::ledger::Observation)s
//! per **dimension**, normalize, and classify (design §3.3).
//!
//! Dimensions and their comparison discipline (each independently reported, so
//! a divergence names which one moved — see [`Channel`]):
//! - **exit code** — exact match; a CratonVM timeout is a distinct, never-
//!   matching token, so a hang can never read as a clean exit;
//! - **exception presence** — one side threw and the other did not;
//! - **exception type** — `fqcn`, exact;
//! - **exception message** — exact after normalization;
//! - **exception frames** — compared *in order*, so reversed-stacktrace bugs
//!   surface as an ordering diff;
//! - **stdout** — exact after normalization (strict by default; a seed must
//!   *declare* it needs a normalizer);
//! - **stderr (non-exception)** — gated only under the profile modes'
//!   normalizer, after the VM's own diagnostics are removed;
//! - **checksum** — the program's own declared quantities
//!   ([`crate::checksum`]), compared on **un-normalized** stdout so no
//!   normalization rule can launder them.
//!
//! ## Normalization is a named rule set, never a buried regex
//!
//! Every transform this module can apply is a
//! [`NormalizationRule`](crate::normalize::NormalizationRule) with a documented
//! target, justification and risk. The
//! [`Normalizer`] here is only a selection of which rules are on; the rules
//! themselves, their order, and the argument for each live next to their
//! implementation. `strict()` selects none of them beyond line-ending hygiene,
//! so the committed gate baseline still judges byte-exact output.

use crate::checksum::{self, Checksums};
use crate::ledger::{Channel, Classification, JvmException, Observation};
use crate::normalize::{self, NormalizationRule};
use crate::runner::Mode;

pub use crate::normalize::{normalize_line_endings, strip_ansi};

/// Per-dimension normalization knobs — a **selection** over
/// [`crate::normalize::ORDER`], not an implementation.
///
/// The **default is strict** (every knob off): a seed must explicitly opt into
/// a rule via its header pragma so we never silently mask a real divergence
/// (design §3.3). Each field names the rule it enables, and that rule's
/// `justification` / `risk` is the argument for turning it on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Normalizer {
    /// [`normalize::ABSOLUTE_PATH`] — strip absolute filesystem paths, keeping
    /// the basename.
    pub strip_paths: bool,
    /// [`normalize::IDENTITY_HASH`] + [`normalize::HEX_ADDRESS`] — mask
    /// `Object.toString` identity hashes (`@<hash>`) and `0x<hex>` blobs.
    pub mask_hashes: bool,
    /// [`normalize::THREAD_ID`] — mask thread / process ids.
    pub mask_thread_ids: bool,
    /// [`normalize::TIMESTAMP`] — mask ISO-8601 wall-clock timestamps.
    ///
    /// Off even under `jdk_only()`: `java.time` formatting is itself a live
    /// bug area for this VM, and a rule that erases the digits would erase the
    /// finding with them.
    pub mask_timestamps: bool,
    /// [`normalize::PATH_SEPARATOR`] — rewrite `\` to `/` inside path-shaped
    /// tokens, so a ledger row captured on Windows is readable on Linux.
    pub normalize_path_separators: bool,
    /// [`normalize::FRAME_LINE_NUMBERS`] — mask `Foo.java:<n>` inside frames.
    ///
    /// The most dangerous rule in the set (this VM has had real wrong-bci
    /// bugs), so it is never enabled by a built-in profile: only a seed that
    /// crosses into JDK internals asks for it, explicitly.
    pub mask_frame_line_numbers: bool,
    /// [`normalize::SORT_LINES`] — sort output lines (only for explicitly-
    /// tagged nondeterministic seeds; erases every ordering bug).
    pub sort_lines: bool,
    /// **stderr only**: drop CratonVM's own diagnostic chatter — ANSI colour
    /// codes, `tracing` lines from `cratonvm_*` targets, and the `[cratonvm]` /
    /// `[NativeBridge]` prefixed notices. Never applied to stdout, which stays
    /// byte-exact.
    pub strip_vm_diagnostics: bool,
    /// Compare stderr as a gated channel. Off for every historical mode:
    /// CratonVM's warnings legitimately differ from HotSpot's silence, and
    /// gating them would turn the whole corpus red. On for the profile modes,
    /// where a strict refusal *is* the observable and it is printed to stderr.
    pub gate_stderr: bool,
}

impl Normalizer {
    /// The strict (no-op beyond line-ending hygiene) normalizer.
    ///
    /// Byte-identical to Step 1's: every knob off, stdout exact, stderr
    /// ungated. The historical modes must keep judging exactly what they judged
    /// before this feature existed.
    pub fn strict() -> Self {
        Self::default()
    }

    /// The normalizer for a JDK-only / real-compatible run: stderr is gated,
    /// after the VM's own diagnostics are removed from it.
    ///
    /// stdout is deliberately left strict — a policy refusal that changed a
    /// program's *output* is a plain divergence and must be caught as one.
    pub fn jdk_only() -> Self {
        Self {
            strip_vm_diagnostics: true,
            gate_stderr: true,
            ..Self::default()
        }
    }

    /// The normalizer `mode` is compared under: [`jdk_only`](Self::jdk_only)
    /// for the four profile modes, [`strict`](Self::strict) for every
    /// historical one.
    pub fn for_mode(mode: Mode) -> Self {
        if mode.collects_census() {
            Self::jdk_only()
        } else {
            Self::strict()
        }
    }

    /// The rules this normalizer applies to a **shared** stream (stdout, and
    /// an exception message), in [`normalize::ORDER`].
    ///
    /// Deriving the list from the single ordered table — rather than open-
    /// coding the sequence here — is what keeps "which rules ran" answerable
    /// from a report: [`describe`](Self::describe) prints exactly what
    /// [`apply`](Self::apply) did.
    pub fn active_rules(&self) -> Vec<&'static NormalizationRule> {
        normalize::ORDER
            .iter()
            .filter(|r| !r.stderr_only && self.selects(r.id))
            .collect()
    }

    /// The rules this normalizer applies to a captured **stderr**: the
    /// stderr-only pair first (ANSI, then VM diagnostics — the tracing lines
    /// are colourised, so a match on `cratonvm_` has to happen after the escape
    /// codes are gone), then the shared rules.
    pub fn active_stderr_rules(&self) -> Vec<&'static NormalizationRule> {
        normalize::ORDER
            .iter()
            .filter(|r| self.selects(r.id))
            .collect()
    }

    /// Whether this normalizer selects the rule named `id`.
    fn selects(&self, id: &str) -> bool {
        match id {
            // Always on: line-ending hygiene is applied at capture time too.
            "line-endings" => true,
            "identity-hash" | "hex-address" => self.mask_hashes,
            "thread-id" => self.mask_thread_ids,
            "timestamp" => self.mask_timestamps,
            "path-separator" => self.normalize_path_separators,
            "absolute-path" => self.strip_paths,
            "frame-line-numbers" => self.mask_frame_line_numbers,
            "sort-lines" => self.sort_lines,
            "ansi" | "vm-diagnostics" => self.strip_vm_diagnostics,
            _ => false,
        }
    }

    /// A one-line, human-readable description of the active rule set, for the
    /// header of a run/gate report. A reader must be able to see, without
    /// reading the source, which transforms stood between the two VMs.
    pub fn describe(&self) -> String {
        let ids: Vec<&str> = self.active_stderr_rules().iter().map(|r| r.id).collect();
        format!(
            "normalization: {} | stderr {}",
            ids.join(","),
            if self.gate_stderr { "gated" } else { "ungated" }
        )
    }

    /// Apply normalization to one captured stream (stdout, or an exception
    /// message).
    ///
    /// `strip_vm_diagnostics` is **not** applied here — it is stderr-only, see
    /// [`apply_stderr`](Self::apply_stderr).
    pub fn apply(&self, s: &str) -> String {
        self.active_rules()
            .into_iter()
            .fold(s.to_string(), |acc, r| r.apply(&acc))
    }

    /// Apply normalization to a captured **stderr**: as [`apply`](Self::apply),
    /// plus the two stderr-only rules when `strip_vm_diagnostics` is set.
    pub fn apply_stderr(&self, s: &str) -> String {
        self.active_stderr_rules()
            .into_iter()
            .fold(s.to_string(), |acc, r| r.apply(&acc))
    }
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

/// Compare two observations under `normalizer`, one **dimension at a time**.
///
/// Every gated dimension is judged independently and contributes its own
/// [`ChannelDiff`], so a divergence report says *which* observable moved rather
/// than "these two runs differ". Hard dimensions (any disagreement is a
/// divergence): exit code, exception presence / type / message / frames,
/// stdout, and the program's declared checksums. A CratonVM **timeout** while
/// HotSpot finished is reported on the exit-code channel (`<timeout>`). stderr
/// is contextual and gated only under the profile modes' normalizer, because
/// CratonVM's warnings legitimately differ from HotSpot's silence.
///
/// The `cratonvm` / `hotspot` field names are positional: the first argument is
/// whatever is under test and the second is the reference. [`crate::crossmode`]
/// reuses this to compare two CratonVM paths against *each other*, where
/// neither side is HotSpot.
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

    // Uncaught exception: presence, then type / message / frames.
    diffs.extend(compare_exception(
        &cratonvm.exception,
        &hotspot.exception,
        normalizer,
    ));

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

    // Program-declared checksums, read from **un-normalized** stdout.
    //
    // This is the guard against a normalization rule laundering a real result:
    // `sort-lines` or `hex-address` can make the stdout dimension agree, but
    // they cannot reach the checksum, so the divergence still surfaces — and
    // names the quantity rather than the byte offset.
    if let Some(diff) = compare_checksums(cratonvm, hotspot) {
        diffs.push(diff);
    }

    // stderr — gated only under the profile modes' normalizer, and only after
    // the VM's own diagnostics have been removed from it.
    if normalizer.gate_stderr {
        let c_err = normalizer.apply_stderr(&cratonvm.stderr);
        let h_err = normalizer.apply_stderr(&hotspot.stderr);
        if c_err != h_err {
            diffs.push(ChannelDiff {
                channel: Channel::Stderr,
                cratonvm: c_err,
                hotspot: h_err,
            });
        }
    }

    if diffs.is_empty() {
        Verdict::Agree
    } else {
        Verdict::Diverge(diffs)
    }
}

/// The declared checksums of an observation's stdout, read through line-ending
/// hygiene only — no maskable rule may touch them.
pub fn checksums_of(o: &Observation) -> Checksums {
    checksum::extract(&normalize_line_endings(&o.stdout))
}

/// Compare the two sides' declared checksums. `None` when they agree (including
/// when neither program declared any, in which case the dimension is simply
/// absent rather than vacuously passing).
fn compare_checksums(cratonvm: &Observation, hotspot: &Observation) -> Option<ChannelDiff> {
    let c = checksums_of(cratonvm);
    let h = checksums_of(hotspot);
    let names = checksum::differing_names(&c, &h);
    if names.is_empty() {
        return None;
    }
    // Report only the differing names: a seed declaring twenty checksums must
    // not bury the one that moved.
    let pick = |sums: &Checksums| {
        names
            .iter()
            .map(|n| match sums.get(n) {
                Some(v) => format!("{n}={v}"),
                None => format!("{n}=<not declared>"),
            })
            .collect::<Vec<String>>()
            .join("\n")
    };
    Some(ChannelDiff {
        channel: Channel::Checksum,
        cratonvm: pick(&c),
        hotspot: pick(&h),
    })
}

/// Whether two observations are equal on the **gated** observables — stdout
/// (normalized), exit/timeout status, and uncaught-exception identity — i.e.
/// the channels [`compare`] judges. Ignores `stderr` (non-gated warnings) and
/// `wall_ms` (timing). Used by the gate to detect drift in a `known`
/// divergence's CratonVM side.
pub fn gated_eq(a: &Observation, b: &Observation, normalizer: &Normalizer) -> bool {
    exit_token(a) == exit_token(b)
        && normalizer.apply(&a.stdout) == normalizer.apply(&b.stdout)
        && a.exception == b.exception
        && (!normalizer.gate_stderr
            || normalizer.apply_stderr(&a.stderr) == normalizer.apply_stderr(&b.stderr))
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

/// Compare two optional exceptions across the four exception dimensions.
///
/// * **presence** — one side threw and the other did not. This short-circuits:
///   when only one side has an exception there is nothing to compare its type
///   or message against, and reporting three more diffs against `<none>` would
///   triple-count one finding.
/// * **type** — `fqcn`, exact. Never normalized: a class name is a semantic
///   observable, and no rule in the set has any business rewriting one.
/// * **message** — exact after the normalizer, so a masked path or identity
///   hash inside a message can be neutralized when a seed opts in.
/// * **frames** — compared *in order*, one per line, each normalized (this is
///   the dimension `frame-line-numbers` exists for).
fn compare_exception(
    cratonvm: &Option<JvmException>,
    hotspot: &Option<JvmException>,
    normalizer: &Normalizer,
) -> Vec<ChannelDiff> {
    let summarize = |e: &JvmException| format!("{}: {}", e.fqcn, e.message);
    let (c, h) = match (cratonvm, hotspot) {
        (None, None) => return Vec::new(),
        (Some(c), None) => {
            return vec![ChannelDiff {
                channel: Channel::Exception,
                cratonvm: summarize(c),
                hotspot: "<none>".to_string(),
            }]
        }
        (None, Some(h)) => {
            return vec![ChannelDiff {
                channel: Channel::Exception,
                cratonvm: "<none>".to_string(),
                hotspot: summarize(h),
            }]
        }
        (Some(c), Some(h)) => (c, h),
    };

    let mut diffs = Vec::new();
    if c.fqcn != h.fqcn {
        diffs.push(ChannelDiff {
            channel: Channel::ExceptionType,
            cratonvm: c.fqcn.clone(),
            hotspot: h.fqcn.clone(),
        });
    }
    let c_msg = normalizer.apply(&c.message);
    let h_msg = normalizer.apply(&h.message);
    if c_msg != h_msg {
        diffs.push(ChannelDiff {
            channel: Channel::ExceptionMessage,
            cratonvm: c_msg,
            hotspot: h_msg,
        });
    }
    let render_frames = |e: &JvmException| {
        e.top_frames
            .iter()
            .map(|f| normalizer.apply(f))
            .collect::<Vec<String>>()
            .join("\n")
    };
    let c_frames = render_frames(c);
    let h_frames = render_frames(h);
    if c_frames != h_frames {
        diffs.push(ChannelDiff {
            channel: Channel::ExceptionFrames,
            cratonvm: c_frames,
            hotspot: h_frames,
        });
    }
    diffs
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
    /// The run enforced the strict policy **and** recorded a violation under
    /// it. False for a compatible run, whose census records what a strict run
    /// *would* have rejected — a measurement, not a violation.
    pub jdk_only_violation: bool,
}

/// Classify a divergence from the per-mode verdict map (design §3.3) — the
/// automation of the manual `--nojit` bisection. Returns `None` when **every**
/// mode agreed with HotSpot and none recorded a policy violation.
///
/// Decision order:
/// 1. **Hang** — any mode timed out (dominates; the most urgent bucket).
/// 2. **JdkOnlyViolation** — a `--jdk-only` run recorded a policy violation.
///    Ranked above `Crash` because it *names the cause*: a strict run that
///    aborted on a `MissingNative` is a policy finding with a known fix, not an
///    unexplained crash to be bisected.
/// 3. **Crash** — any mode exited by signal/abort.
/// 4. **JitOnly** — the interpreter (`nojit`) ran and *agreed*, but a JIT-
///    enabled mode (`jit-on` / `low-jit-threshold`) diverged ⇒ a JIT bug.
/// 5. **GcMode** — `jit-on` and `nojit` both agree, but a GC mode (`moving-gc`)
///    diverged ⇒ the divergence is GC-specific.
/// 6. **Universal** — otherwise (the bug is in the shared interpreter/native
///    path, present with and without the JIT).
pub fn classify(modes: &[ModeVerdict]) -> Option<Classification> {
    if modes.iter().any(|m| m.timed_out) {
        return Some(Classification::Hang);
    }
    if modes.iter().any(|m| m.jdk_only_violation) {
        return Some(Classification::JdkOnlyViolation);
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

    /// The channels a verdict reported, in report order — the shape every
    /// "planted divergence" test below asserts on.
    fn channels(v: &Verdict) -> Vec<Channel> {
        match v {
            Verdict::Agree => Vec::new(),
            Verdict::Diverge(d) => d.iter().map(|c| c.channel).collect(),
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

    // -- one planted divergence per dimension --------------------------------
    //
    // The harness's own soundness property: each dimension must *independently*
    // detect a divergence planted in it alone, and must name itself when it
    // does. A dimension that only ever fires together with `stdout` is not a
    // dimension, it is a duplicate.

    #[test]
    fn exit_code_divergence_is_caught_alone() {
        let a = obs("same", "", Some(0));
        let b = obs("same", "", Some(1));
        assert_eq!(
            channels(&compare(&a, &b, &Normalizer::strict())),
            vec![Channel::ExitCode]
        );
    }

    #[test]
    fn exception_presence_divergence_is_caught_alone() {
        // One VM threw, the other exited cleanly. Reported once, on the
        // presence channel — not three times against `<none>`.
        let a = obs(
            "",
            "Exception in thread \"main\" java.lang.IllegalStateException: boom\n",
            Some(1),
        );
        let b = obs("", "", Some(1));
        assert_eq!(
            channels(&compare(&a, &b, &Normalizer::strict())),
            vec![Channel::Exception]
        );
    }

    #[test]
    fn exception_type_divergence_is_caught_alone() {
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
        assert_eq!(
            channels(&compare(&a, &b, &Normalizer::strict())),
            vec![Channel::ExceptionType],
            "a wrong type must not be reported as a message or frame diff"
        );
    }

    #[test]
    fn exception_message_divergence_is_caught_alone() {
        // The committed `ExceptionId` shape: right type, right frames, a
        // message missing HotSpot's module/loader detail.
        let a = obs(
            "",
            "Exception in thread \"main\" java.lang.ClassCastException: \
             java.lang.String cannot be cast to java.lang.Integer\n\tat X.main(X.java:3)\n",
            Some(1),
        );
        let b = obs(
            "",
            "Exception in thread \"main\" java.lang.ClassCastException: \
             class java.lang.String cannot be cast to class java.lang.Integer \
             (in module java.base)\n\tat X.main(X.java:3)\n",
            Some(1),
        );
        assert_eq!(
            channels(&compare(&a, &b, &Normalizer::strict())),
            vec![Channel::ExceptionMessage]
        );
    }

    #[test]
    fn exception_frame_order_divergence_is_caught_alone() {
        // Same type, same message, frames in the wrong order — the reversed-
        // stack-trace bug this dimension exists for.
        let a = obs(
            "",
            "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
             \tat A.a(A.java:1)\n\tat B.b(B.java:2)\n",
            Some(1),
        );
        let b = obs(
            "",
            "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
             \tat B.b(B.java:2)\n\tat A.a(A.java:1)\n",
            Some(1),
        );
        assert_eq!(
            channels(&compare(&a, &b, &Normalizer::strict())),
            vec![Channel::ExceptionFrames]
        );
    }

    #[test]
    fn checksum_divergence_is_caught_and_names_the_quantity() {
        let a = obs("##DIFFTEST-CHECKSUM## arith 111\n", "", Some(0));
        let b = obs("##DIFFTEST-CHECKSUM## arith 222\n", "", Some(0));
        match compare(&a, &b, &Normalizer::strict()) {
            Verdict::Diverge(d) => {
                // stdout moved too (the declaration is on stdout), but the
                // checksum dimension is what names the quantity.
                let sum = d
                    .iter()
                    .find(|c| c.channel == Channel::Checksum)
                    .expect("checksum channel");
                assert_eq!(sum.cratonvm, "arith=111");
                assert_eq!(sum.hotspot, "arith=222");
            }
            Verdict::Agree => panic!("expected a checksum divergence"),
        }
    }

    #[test]
    fn a_checksum_survives_an_over_normalizing_stdout_rule() {
        // The false-negative guard, and the reason the checksum is read from
        // un-normalized stdout. Two runs computed different digests; the seed
        // prints them in `0x…` form, so `mask_hashes` rewrites both to
        // `0x<addr>` and the stdout dimension goes quiet. The checksum
        // dimension does not — and it is the only one that fires, so the report
        // says exactly what happened.
        let a = obs("##DIFFTEST-CHECKSUM## total 0xdeadbeef\n", "", Some(0));
        let b = obs("##DIFFTEST-CHECKSUM## total 0xcafebabe\n", "", Some(0));
        let n = Normalizer {
            mask_hashes: true,
            ..Normalizer::strict()
        };
        assert!(
            n.apply(&a.stdout) == n.apply(&b.stdout),
            "the rule must genuinely have hidden the stdout diff"
        );
        assert_eq!(
            channels(&compare(&a, &b, &n)),
            vec![Channel::Checksum],
            "an over-normalizing rule hid the stdout diff; the checksum must not be hidden too"
        );
    }

    #[test]
    fn matching_checksums_are_not_a_dimension() {
        // Neither declaring, and both declaring the same value, must be silent
        // — a vacuous pass is not evidence.
        let a = obs("plain\n", "", Some(0));
        let b = obs("plain\n", "", Some(0));
        assert!(compare(&a, &b, &Normalizer::strict()).agrees());
        let c = obs("##DIFFTEST-CHECKSUM## k v\n", "", Some(0));
        let d = obs("##DIFFTEST-CHECKSUM## k v\n", "", Some(0));
        assert!(compare(&c, &d, &Normalizer::strict()).agrees());
    }

    #[test]
    fn identical_observations_report_no_divergence_on_any_dimension() {
        // The other half of every dimension test: a same-input/same-output pair
        // must be silent on all eight channels, under every built-in profile.
        let stderr = "Exception in thread \"main\" java.lang.IllegalStateException: x\n\
                      \tat A.a(A.java:1)\n";
        let a = obs("out\n##DIFFTEST-CHECKSUM## k 1\n", stderr, Some(1));
        let b = a.clone();
        for n in [Normalizer::strict(), Normalizer::jdk_only()] {
            assert!(
                compare(&a, &b, &n).agrees(),
                "identical observations diverged under {}",
                n.describe()
            );
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
            jdk_only_violation: false,
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

    // -- profile-mode normalization ------------------------------------------

    /// A real captured CratonVM stderr: colourised `tracing` lines plus the
    /// VM's own bracketed notices (lifted from the committed ledger's
    /// `ExceptionId` row).
    const VM_CHATTER: &str = concat!(
        "\u{1b}[2m2026-07-30T18:35:56.284528Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m ",
        "\u{1b}[2mcratonvm_vm::vm::vm_object\u{1b}[0m\u{1b}[2m:\u{1b}[0m ",
        "[NativeBridge] 1 unregistered native methods:\n",
        "[cratonvm] main-vm run() returned Ok — VM main exiting normally\n"
    );

    #[test]
    fn strict_normalizer_is_byte_identical_to_step_1() {
        let n = Normalizer::strict();
        assert!(!n.gate_stderr);
        assert!(!n.strip_vm_diagnostics);
        assert_eq!(n, Normalizer::default());
        // stdout and stderr both get line-ending hygiene and nothing else — the
        // VM chatter is preserved verbatim, because a historical mode's
        // recorded stderr must not change shape.
        assert_eq!(n.apply("a\r\nb\r\n"), "a\nb");
        assert_eq!(
            n.apply_stderr(VM_CHATTER),
            normalize_line_endings(VM_CHATTER)
        );
        assert!(n.apply_stderr(VM_CHATTER).contains("\u{1b}["));
    }

    #[test]
    fn strict_normalizer_does_not_gate_stderr() {
        // The historical behaviour: stderr differs wildly and is not judged.
        let a = obs("42", "[cratonvm] chatter", Some(0));
        let b = obs("42", "", Some(0));
        assert!(compare(&a, &b, &Normalizer::strict()).agrees());
    }

    #[test]
    fn jdk_only_normalizer_strips_ansi_and_vm_lines() {
        let n = Normalizer::jdk_only();
        assert!(n.gate_stderr && n.strip_vm_diagnostics);
        assert_eq!(n.apply_stderr(VM_CHATTER), "");
        // A program's own stderr survives; only VM chatter goes.
        let mixed = format!("{VM_CHATTER}real program stderr\n");
        assert_eq!(n.apply_stderr(&mixed), "real program stderr");
    }

    #[test]
    fn jdk_only_normalizer_leaves_stdout_byte_exact() {
        // stdout must never be laundered: a policy refusal that changed the
        // program's output is a plain divergence and has to be caught as one.
        let n = Normalizer::jdk_only();
        let noisy = "[cratonvm] this came from the program\ncratonvm_vm printed this";
        assert_eq!(n.apply(noisy), noisy);
    }

    #[test]
    fn jdk_only_normalizer_gates_stderr_after_stripping() {
        let n = Normalizer::jdk_only();
        // Chatter-only stderr vs HotSpot's silence: agrees.
        let a = obs("42", VM_CHATTER, Some(0));
        let b = obs("42", "", Some(0));
        assert!(
            compare(&a, &b, &n).agrees(),
            "VM chatter alone is not a diff"
        );

        // A real refusal message on stderr is a Stderr-channel divergence.
        let c = obs("42", "jdk-only: refused java/foo/Bar", Some(0));
        match compare(&c, &b, &n) {
            Verdict::Diverge(d) => {
                assert_eq!(d.len(), 1);
                assert_eq!(d[0].channel, Channel::Stderr);
                assert_eq!(d[0].cratonvm, "jdk-only: refused java/foo/Bar");
            }
            Verdict::Agree => panic!("a strict refusal on stderr must be gated"),
        }
    }

    #[test]
    fn for_mode_gates_only_the_four_profile_modes() {
        for &m in Mode::all() {
            let n = Normalizer::for_mode(m);
            assert_eq!(
                n.gate_stderr,
                m.collects_census(),
                "{} stderr gating",
                m.label()
            );
            if m.collects_census() {
                assert_eq!(n, Normalizer::jdk_only(), "{}", m.label());
            } else {
                assert_eq!(n, Normalizer::strict(), "{}", m.label());
            }
        }
    }

    #[test]
    fn strict_selects_only_line_ending_hygiene() {
        let n = Normalizer::strict();
        let ids: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["line-endings"]);
        assert_eq!(n.active_stderr_rules().len(), 1);
        // Every opt-in knob is off, including the three added for the C2 review.
        assert!(!n.mask_timestamps);
        assert!(!n.normalize_path_separators);
        assert!(!n.mask_frame_line_numbers);
        assert!(n.describe().contains("line-endings"));
        assert!(n.describe().contains("ungated"));
    }

    #[test]
    fn jdk_only_selects_the_two_stderr_rules_in_order() {
        let n = Normalizer::jdk_only();
        let ids: Vec<&str> = n.active_stderr_rules().iter().map(|r| r.id).collect();
        // ANSI must precede vm-diagnostics: the tracing lines are colourised,
        // so a match on `cratonvm_` only works once the escapes are gone.
        assert_eq!(ids, vec!["ansi", "vm-diagnostics", "line-endings"]);
        // The stderr-only pair never touches stdout.
        let shared: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
        assert_eq!(shared, vec!["line-endings"]);
        assert!(n.describe().contains("stderr gated"));
    }

    #[test]
    fn each_knob_selects_exactly_its_own_rules() {
        let cases: [(Normalizer, &[&str]); 6] = [
            (
                Normalizer {
                    mask_hashes: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "identity-hash", "hex-address"],
            ),
            (
                Normalizer {
                    mask_thread_ids: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "thread-id"],
            ),
            (
                Normalizer {
                    mask_timestamps: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "timestamp"],
            ),
            (
                Normalizer {
                    normalize_path_separators: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "path-separator"],
            ),
            (
                Normalizer {
                    strip_paths: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "absolute-path"],
            ),
            (
                Normalizer {
                    mask_frame_line_numbers: true,
                    ..Normalizer::strict()
                },
                &["line-endings", "frame-line-numbers"],
            ),
        ];
        for (n, expected) in cases {
            let mut ids: Vec<&str> = n.active_rules().iter().map(|r| r.id).collect();
            let mut want: Vec<&str> = expected.to_vec();
            ids.sort_unstable();
            want.sort_unstable();
            assert_eq!(ids, want, "{}", n.describe());
        }
    }

    #[test]
    fn an_opted_in_rule_neutralizes_only_its_target_in_a_real_comparison() {
        // End to end: two runs that differ *only* in an identity hash agree
        // under `mask_hashes`, and a run that differs in a real value still
        // diverges under the same normalizer.
        let n = Normalizer {
            mask_hashes: true,
            ..Normalizer::strict()
        };
        let a = obs("java.lang.Object@1b6d3586\nvalue=7", "", Some(0));
        let b = obs("java.lang.Object@7ffe0102\nvalue=7", "", Some(0));
        assert!(compare(&a, &b, &n).agrees());

        let c = obs("java.lang.Object@7ffe0102\nvalue=8", "", Some(0));
        assert_eq!(
            channels(&compare(&a, &c, &n)),
            vec![Channel::Stdout],
            "masking the hash must not mask the value"
        );
        // …and under the strict normalizer the hash difference is a divergence
        // again, which is what makes the rule an explicit opt-in.
        assert!(!compare(&a, &b, &Normalizer::strict()).agrees());
    }

    #[test]
    fn gated_eq_ignores_stderr_unless_the_normalizer_gates_it() {
        let a = obs("42", "[cratonvm] chatter", Some(0));
        // Both must be *VM* chatter for the profile normalizer to strip both;
        // `strip_vm_diagnostics` keys on the `[cratonvm]`/`[NativeBridge]`
        // markers, so an unmarked line survives and is compared.
        let b = obs("42", "[cratonvm] different chatter entirely", Some(0));
        assert!(gated_eq(&a, &b, &Normalizer::strict()));
        // Under the profile normalizer, chatter is stripped from both, so they
        // still match — drift is judged on what's left.
        assert!(gated_eq(&a, &b, &Normalizer::jdk_only()));
        let c = obs("42", "a real message", Some(0));
        assert!(!gated_eq(&a, &c, &Normalizer::jdk_only()));
        assert!(gated_eq(&a, &c, &Normalizer::strict()));
    }

    #[test]
    fn strip_ansi_removes_csi_sequences_only() {
        assert_eq!(strip_ansi("\u{1b}[2mdim\u{1b}[0m"), "dim");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[38;5;196mred\u{1b}[0m!"), "red!");
        // Brackets that aren't escapes stay put.
        assert_eq!(strip_ansi("[cratonvm] ok"), "[cratonvm] ok");
    }

    // -- classification ranking ----------------------------------------------

    fn violating(mode: Mode, diverged: bool) -> ModeVerdict {
        ModeVerdict {
            jdk_only_violation: true,
            ..mv(mode, diverged)
        }
    }

    #[test]
    fn classify_jdk_only_violation_outranks_crash_but_not_hang() {
        // Rank: Hang > JdkOnlyViolation > Crash > JitOnly > GcMode > Universal.
        let mut crash = violating(Mode::JdkOnlyJit, true);
        crash.crashed = true;
        assert_eq!(classify(&[crash]), Some(Classification::JdkOnlyViolation));

        let mut hang = violating(Mode::JdkOnlyJit, true);
        hang.timed_out = true;
        assert_eq!(classify(&[hang]), Some(Classification::Hang));
    }

    #[test]
    fn classify_jdk_only_violation_outranks_the_bisection_labels() {
        // A JIT-shaped bisection that also tripped the policy is reported as
        // the policy finding: the violation names the cause.
        let modes = [
            violating(Mode::JdkOnlyJit, true),
            mv(Mode::JdkOnlyNoJit, false),
        ];
        assert_eq!(classify(&modes), Some(Classification::JdkOnlyViolation));
    }

    #[test]
    fn classify_violation_on_an_agreeing_run_is_still_reported() {
        // Wave 1 is measurement (contract §10): a violation that did not change
        // behaviour is still recorded. The gate's exit code does not move —
        // that is `GateReport::strict_violations`' job, not this label's.
        let modes = [violating(Mode::JdkOnlyJit, false)];
        assert_eq!(classify(&modes), Some(Classification::JdkOnlyViolation));
    }

    #[test]
    fn classify_compatible_modes_never_report_a_violation() {
        // A compatible run records *would-be* refusals, which are never a
        // violation — so the historical labels are untouched.
        let modes = [mv(Mode::JitOn, true), mv(Mode::NoJit, false)];
        assert_eq!(classify(&modes), Some(Classification::JitOnly));
    }
}
