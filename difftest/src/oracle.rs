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

    /// Apply normalization to one captured stream.
    ///
    /// Applies the always-safe line-ending hygiene
    /// ([`normalize_line_endings`]); the opt-in transforms above are declared
    /// but inert until a later step wires them (each gated behind its flag, so
    /// strict mode stays byte-exact). `strip_vm_diagnostics` is **not** applied
    /// here — it is stderr-only, see [`apply_stderr`](Self::apply_stderr).
    pub fn apply(&self, s: &str) -> String {
        let normalized = normalize_line_endings(s);
        if self.sort_lines {
            let mut lines: Vec<&str> = normalized.lines().collect();
            lines.sort_unstable();
            return lines.join("\n");
        }
        normalized
    }

    /// Apply normalization to a captured **stderr**.
    ///
    /// Identical to [`apply`](Self::apply) unless `strip_vm_diagnostics` is
    /// set, in which case ANSI escapes are removed first and every surviving
    /// VM-diagnostic line is dropped. Order matters: the tracing lines are
    /// colourised, so a match on `cratonvm_` has to happen *after* the escape
    /// codes are gone.
    pub fn apply_stderr(&self, s: &str) -> String {
        if !self.strip_vm_diagnostics {
            return self.apply(s);
        }
        let plain = strip_ansi(s);
        let kept: Vec<&str> = plain.lines().filter(|l| !is_vm_diagnostic(l)).collect();
        self.apply(&kept.join("\n"))
    }
}

/// Remove ANSI/VT escape sequences (`ESC [ … final-byte`, and a bare `ESC`
/// followed by one byte) so a colourised tracing line can be matched on its
/// text. Non-escape bytes pass through untouched.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            // CSI: consume parameter/intermediate bytes up to the final byte.
            Some('[') => {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // Any other two-byte escape.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Whether an (ANSI-stripped) stderr line is CratonVM's own diagnostic chatter
/// rather than program output.
///
/// Three shapes, all of them things HotSpot has no counterpart for:
/// `tracing` records naming a `cratonvm_*` target, and the two bracketed
/// prefixes the VM prints on its own (`[cratonvm]`, `[NativeBridge]`).
fn is_vm_diagnostic(line: &str) -> bool {
    let t = line.trim();
    t.contains("[cratonvm]") || t.contains("[NativeBridge]") || t.contains("cratonvm_")
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
        assert_eq!(n.apply_stderr(VM_CHATTER), normalize_line_endings(VM_CHATTER));
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
        assert!(compare(&a, &b, &n).agrees(), "VM chatter alone is not a diff");

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
    fn gated_eq_ignores_stderr_unless_the_normalizer_gates_it() {
        let a = obs("42", "[cratonvm] chatter", Some(0));
        let b = obs("42", "different chatter entirely", Some(0));
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
