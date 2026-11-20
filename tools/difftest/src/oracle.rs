// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

    /// The normalizer for a Real-JDK / real-compatible run: stderr is gated,
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
/// 2. **JdkOnlyViolation** — a `--real-jdk` run recorded a policy violation.
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
mod tests;
