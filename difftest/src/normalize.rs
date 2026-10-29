// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Named, documented normalization rules.
//!
//! A differential harness that hides a regex inside its comparison function is
//! a harness nobody can audit: the reader cannot tell whether a divergence was
//! *neutralized* (a nondeterminism the JLS does not fix) or *laundered* (a real
//! VM bug the harness quietly erased). The retired divergence log records the
//! failure mode from the other side too — a harness that reports its own
//! environment noise as a VM defect burns a day of bisection on nothing.
//!
//! So every transform lives here as a [`NormalizationRule`] with four fields a
//! reviewer can argue with:
//!
//! * `id` — the stable name printed in reports and written in seed pragmas;
//! * `target` — precisely what nondeterminism it neutralizes;
//! * `justification` — why that thing is *not* a semantic observable;
//! * `risk` — the real divergence this rule would hide if it fired too widely.
//!
//! Nothing here is on by default except [`LINE_ENDINGS`]. [`crate::oracle`]'s
//! `Normalizer::strict()` leaves every other rule off, so the committed gate
//! baseline keeps judging byte-exact output; a seed opts in per rule.
//!
//! ## Ordering
//!
//! Rules are not commutative, so [`crate::oracle::Normalizer::apply`] applies
//! them in the fixed order given by [`ORDER`]: line-ending hygiene first (so
//! later scanners never see a stray `\r`), then the masking rules (which are
//! independent of each other by construction — see the "only its target" test),
//! then `sort-lines` last, because sorting must observe the final line text.
//! `ansi` and `vm-diagnostics` are stderr-only and run *before* the shared
//! rules: the tracing lines are colourised, so a match on `cratonvm_` has to
//! happen after the escape codes are gone.

// ---------------------------------------------------------------------------
// The rule type
// ---------------------------------------------------------------------------

/// One named normalization rule.
///
/// Constructed only as a `const` in this module: the set of transforms the
/// harness may apply is closed and reviewable, and a caller cannot smuggle in an
/// anonymous closure.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationRule {
    /// Stable identifier, e.g. `"identity-hash"`. Printed in reports and used
    /// in a seed's `// difftest:` pragma.
    pub id: &'static str,
    /// Exactly what nondeterminism this rule neutralizes.
    pub target: &'static str,
    /// Why the target is not a semantic observable — the argument for applying
    /// the rule at all.
    pub justification: &'static str,
    /// The real divergence this rule would hide if it fired too widely. Every
    /// rule has one; a rule whose risk is "none" is a rule nobody checked.
    pub risk: &'static str,
    /// `true` for rules that may only be applied to a captured **stderr**.
    /// stdout stays byte-exact modulo the shared rules, because a program's
    /// stdout *is* the semantic transcript.
    pub stderr_only: bool,
    /// A representative input this rule rewrites, used by the rule tests to
    /// prove the rule hits its target and by `docs/testing/differential.md`.
    pub probe: &'static str,
    /// What [`probe`](Self::probe) must become.
    pub probe_expected: &'static str,
    apply_fn: fn(&str) -> String,
}

impl NormalizationRule {
    /// Apply this rule to one captured stream.
    pub fn apply(&self, s: &str) -> String {
        (self.apply_fn)(s)
    }
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// CRLF→LF and trailing-whitespace trim.
///
/// The only always-on rule: it is applied at capture time
/// ([`crate::runner::run_subprocess`]) and again in the oracle, so a platform
/// `\r` cannot masquerade as a behavioral divergence between a Windows CratonVM
/// run and a Linux reference.
pub const LINE_ENDINGS: NormalizationRule = NormalizationRule {
    id: "line-endings",
    target: "CRLF line terminators and trailing whitespace at end of stream",
    justification: "The line terminator is chosen by the C runtime's text mode, \
                    not by the program. Both VMs print the same characters; only \
                    the host decides how a '\\n' reaches the pipe.",
    risk: "A program that *deliberately* prints a trailing space or a bare \\r \
           as its last byte cannot be distinguished from one that does not. No \
           seed in the corpus does this; a seed that needs it must print a \
           sentinel after the whitespace.",
    stderr_only: false,
    probe: "a\r\nb\r\n",
    probe_expected: "a\nb",
    apply_fn: normalize_line_endings,
};

/// `Object.toString` / default-`hashCode` identity hashes (`Foo@1b6d3586`).
pub const IDENTITY_HASH: NormalizationRule = NormalizationRule {
    id: "identity-hash",
    target: "`@<hex>` suffixes with at least four hex digits, as printed by \
             java.lang.Object.toString and Integer.toHexString(hashCode())",
    justification: "System.identityHashCode is explicitly unspecified: the JLS \
                    and the Object.hashCode contract require only that it be \
                    stable within one execution. HotSpot derives it from a \
                    thread-local PRNG; CratonVM derives it from the object \
                    address. Neither number is a semantic observable.",
    risk: "A program that prints a *meaningful* value which happens to be \
           spelled `name@hexdigits` (an email address with a hex-only domain \
           label, a fully-qualified JMX ObjectName) is masked. This is why the \
           rule requires >= 4 hex digits and a non-identifier terminator, and \
           why it is opt-in.",
    stderr_only: false,
    probe: "java.lang.Object@1b6d3586 end",
    probe_expected: "java.lang.Object@<idhash> end",
    apply_fn: |s: &str| scan_replace(s, scan_identity_hash),
};

/// Bare `0x…` pointer / address blobs.
pub const HEX_ADDRESS: NormalizationRule = NormalizationRule {
    id: "hex-address",
    target: "`0x<hex>` runs not preceded by an identifier character",
    justification: "Raw addresses appear in Unsafe / DirectByteBuffer / \
                    MemorySegment diagnostics and in JNI handles. Two VMs with \
                    different allocators can never agree on one, and the JLS \
                    fixes none of them.",
    risk: "A program whose *result* is a hex string — a checksum printed via \
           `0x` + Long.toHexString, a parsed literal echoed back — is masked. \
           Programs that compute a digest should print it through the checksum \
           channel (see `crate::checksum`), which this rule never touches.",
    stderr_only: false,
    probe: "addr=0xdeadbeef.",
    probe_expected: "addr=0x<addr>.",
    apply_fn: |s: &str| scan_replace(s, scan_hex_address),
};

/// Thread and process identifiers.
pub const THREAD_ID: NormalizationRule = NormalizationRule {
    id: "thread-id",
    target: "`Thread-<n>`, `tid=<n>`, `pid=<n>` (case-insensitive marker)",
    justification: "Thread numbering is a counter over threads the runtime \
                    itself started; CratonVM and HotSpot start different \
                    numbers of internal threads before `main`. The OS pid is \
                    per-run by definition.",
    risk: "A program that names its own threads `Thread-1`, `Thread-2` and \
           asserts on the names loses that assertion. Name application threads \
           explicitly (`new Thread(r, \"worker-a\")`) rather than relying on \
           the default.",
    stderr_only: false,
    probe: "Thread-17 tid=42 done",
    probe_expected: "<thread-id> <thread-id> done",
    apply_fn: |s: &str| scan_replace(s, scan_thread_id),
};

/// Wall-clock timestamps.
pub const TIMESTAMP: NormalizationRule = NormalizationRule {
    id: "timestamp",
    target: "ISO-8601 date, optionally followed by `T`/space and a time with \
             optional fraction and `Z`",
    justification: "Wall-clock time advances between the reference run and the \
                    VM run by construction — the harness runs them \
                    sequentially. No amount of determinism in the VM can make \
                    two runs at different instants print the same clock.",
    risk: "A program that formats a *fixed* instant (an epoch constant, a \
           parsed literal) is masked, and so is a real DateTimeFormatter bug \
           that changes only the digits. `java.time` formatting bugs are a \
           live area for this VM (see MEMORY.md's java.time entries), so this \
           rule must not be enabled on a formatting seed.",
    stderr_only: false,
    probe: "at 2026-07-31T12:00:00.123Z ok",
    probe_expected: "at <timestamp> ok",
    apply_fn: |s: &str| scan_replace(s, scan_timestamp),
};

/// Windows vs POSIX path separators.
pub const PATH_SEPARATOR: NormalizationRule = NormalizationRule {
    id: "path-separator",
    target: "`\\` inside a whitespace-delimited token that already looks like a \
             path (contains `/`, or starts with a drive letter)",
    justification: "`File.separatorChar` is host-dependent and both VMs read it \
                    from the same host — but the *reference* JDK may run on a \
                    Linux CI worker while CratonVM runs on Windows, and a \
                    committed ledger row captured on one must be readable on \
                    the other.",
    risk: "A program printing a literal backslash inside a token that also \
           contains a slash (a Windows-flavoured regex, an escaped string) is \
           rewritten. Restricted to path-shaped tokens for exactly that reason.",
    stderr_only: false,
    probe: "pkg\\sub/file.txt",
    probe_expected: "pkg/sub/file.txt",
    apply_fn: |s: &str| map_tokens(s, path_separator_token),
};

/// Absolute filesystem paths (the directory part).
pub const ABSOLUTE_PATH: NormalizationRule = NormalizationRule {
    id: "absolute-path",
    target: "an absolute path prefix (`/…` or `X:\\…`) up to and including its \
             last separator; the basename is kept",
    justification: "The harness compiles into a pid-namespaced temp directory \
                    and the two VMs are invoked from different install roots, \
                    so any absolute path in output differs for reasons that \
                    have nothing to do with semantics. The basename — which is \
                    what a stack frame or a `FileNotFoundException` is actually \
                    about — survives.",
    risk: "A path-resolution bug that produces the *wrong directory* but the \
           right file name becomes invisible. `File.getCanonicalPath` and \
           `Path.normalize` seeds must therefore not enable this rule.",
    stderr_only: false,
    probe: "/opt/jdk/bin/java",
    probe_expected: "<path>/java",
    apply_fn: |s: &str| scan_replace(s, scan_absolute_path),
};

/// Source line numbers inside stack-trace frames.
pub const FRAME_LINE_NUMBERS: NormalizationRule = NormalizationRule {
    id: "frame-line-numbers",
    target: "`.java:<digits>` inside an `at …` frame",
    justification: "A frame's line number comes from the `LineNumberTable` the \
                    *reference JDK's javac* emitted. When the pinned reference \
                    JDK differs from the one that compiled a committed \
                    reproducer, or when a JDK-internal frame moves between \
                    JDK builds, the digits move without any behaviour changing.",
    risk: "This is the most dangerous rule in the file. Line numbers are how a \
           reader locates a thrown exception, and this VM has had real bugs \
           that reported the *wrong bci* (see MEMORY.md's `athrow_bci` entry). \
           Enable it only for a seed whose frames cross into JDK internals, and \
           never for a seed testing exception attribution.",
    stderr_only: false,
    probe: "at Foo.main(Foo.java:12)",
    probe_expected: "at Foo.main(Foo.java:<line>)",
    apply_fn: |s: &str| scan_replace(s, scan_frame_line),
};

/// Line-order normalization for output the JLS does not order.
pub const SORT_LINES: NormalizationRule = NormalizationRule {
    id: "sort-lines",
    target: "the order of whole output lines",
    justification: "`HashMap`/`HashSet` iteration order is explicitly \
                    unspecified, and CratonVM's table capacity growth differs \
                    from HotSpot's (MEMORY.md: iteration order is a capacity \
                    readout). A seed that prints a set's contents is comparing \
                    a *bag*, not a sequence.",
    risk: "Every ordering bug in the program under test disappears: a reversed \
           list, a stack popped in the wrong direction, a stack trace printed \
           bottom-up all compare equal. Only ever enable it on a seed whose \
           output is genuinely a bag, and prefer sorting *inside the seed* \
           (`new TreeSet<>(…)`) so the comparison can stay strict.",
    stderr_only: false,
    probe: "b\na",
    probe_expected: "a\nb",
    apply_fn: sort_lines,
};

/// ANSI/VT escape sequences.
pub const ANSI: NormalizationRule = NormalizationRule {
    id: "ansi",
    target: "ANSI/VT escape sequences (`ESC [ … final-byte`, and bare two-byte \
             escapes)",
    justification: "CratonVM's `tracing` subscriber colourises its diagnostics \
                    when it believes the stream is a terminal; HotSpot emits \
                    none. Whether the harness's pipe is detected as a tty is a \
                    property of the capture, not of the program.",
    risk: "A program that deliberately emits terminal control codes (a curses- \
           style renderer) has its output flattened. No seed does; this rule is \
           applied to stderr only.",
    stderr_only: true,
    probe: "\u{1b}[2mdim\u{1b}[0m",
    probe_expected: "dim",
    apply_fn: strip_ansi,
};

/// CratonVM's own diagnostic chatter on stderr.
pub const VM_DIAGNOSTICS: NormalizationRule = NormalizationRule {
    id: "vm-diagnostics",
    target: "stderr lines containing `[cratonvm]`, `[NativeBridge]`, or a \
             `cratonvm_*` tracing target",
    justification: "These three shapes are things HotSpot has no counterpart \
                    for: they are the VM narrating its own startup and \
                    shutdown. Gating stderr at all is impossible while they are \
                    present, so removing them is what makes the stderr channel \
                    usable as evidence.",
    risk: "A *program* that prints one of those three tokens on stderr loses \
           that line, and a VM diagnostic that is itself the finding (a warning \
           that should not have fired) is erased. The gate never reads this \
           rule's output as a pass on its own — the `jdk-only` census counts \
           the same events structurally.",
    stderr_only: true,
    probe: "[cratonvm] noise\nkeep",
    probe_expected: "keep",
    apply_fn: drop_vm_diagnostics,
};

/// Every rule, in the order [`crate::oracle::Normalizer`] applies them.
///
/// The two stderr-only rules lead, because the shared rules must see
/// escape-free, chatter-free text; `sort-lines` trails, because sorting must
/// observe final line text.
pub const ORDER: &[NormalizationRule] = &[
    ANSI,
    VM_DIAGNOSTICS,
    LINE_ENDINGS,
    IDENTITY_HASH,
    HEX_ADDRESS,
    THREAD_ID,
    TIMESTAMP,
    PATH_SEPARATOR,
    ABSOLUTE_PATH,
    FRAME_LINE_NUMBERS,
    SORT_LINES,
];

/// Look a rule up by [`NormalizationRule::id`].
pub fn rule(id: &str) -> Option<&'static NormalizationRule> {
    ORDER.iter().find(|r| r.id == id)
}

// ---------------------------------------------------------------------------
// Rule implementations
// ---------------------------------------------------------------------------

/// CRLF→LF and trailing-whitespace trim ([`LINE_ENDINGS`]).
pub fn normalize_line_endings(s: &str) -> String {
    s.replace("\r\n", "\n").trim_end().to_string()
}

/// Remove ANSI/VT escape sequences ([`ANSI`]).
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
/// rather than program output ([`VM_DIAGNOSTICS`]).
pub fn is_vm_diagnostic(line: &str) -> bool {
    let t = line.trim();
    t.contains("[cratonvm]") || t.contains("[NativeBridge]") || t.contains("cratonvm_")
}

fn drop_vm_diagnostics(s: &str) -> String {
    s.lines()
        .filter(|l| !is_vm_diagnostic(l))
        .collect::<Vec<&str>>()
        .join("\n")
}

fn sort_lines(s: &str) -> String {
    let mut lines: Vec<&str> = s.lines().collect();
    lines.sort_unstable();
    lines.join("\n")
}

// -- character-scanner plumbing --------------------------------------------

/// Replace every run `scan` accepts with the replacement it returns.
///
/// The scanner sees the whole stream as `char`s and the index it is being asked
/// about, so a rule can look at the preceding character (which is how
/// `identity-hash` refuses to fire on a bare `@`).
fn scan_replace(s: &str, scan: fn(&[char], usize) -> Option<(usize, String)>) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < chars.len() {
        match scan(&chars, i) {
            Some((len, repl)) if len > 0 => {
                out.push_str(&repl);
                i += len;
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// Apply `f` to every whitespace-delimited token, preserving whitespace exactly.
fn map_tokens(s: &str, f: fn(&str) -> String) -> String {
    let mut out = String::with_capacity(s.len());
    let mut token = String::new();
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !token.is_empty() {
                out.push_str(&f(&token));
                token.clear();
            }
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    if !token.is_empty() {
        out.push_str(&f(&token));
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn scan_identity_hash(c: &[char], i: usize) -> Option<(usize, String)> {
    if c[i] != '@' || i == 0 {
        return None;
    }
    // Must follow something that can end a class name / identifier.
    let prev = c[i - 1];
    if !(is_ident_char(prev) || prev == '.' || prev == '$' || prev == ';' || prev == ']') {
        return None;
    }
    let mut n = i + 1;
    while n < c.len() && c[n].is_ascii_hexdigit() {
        n += 1;
    }
    // >= 4 hex digits, and not glued to more identifier text (so `@Override`
    // and `@Deprecated` can never match).
    if n - (i + 1) < 4 {
        return None;
    }
    if n < c.len() && is_ident_char(c[n]) {
        return None;
    }
    Some((n - i, "@<idhash>".to_string()))
}

fn scan_hex_address(c: &[char], i: usize) -> Option<(usize, String)> {
    if c[i] != '0' {
        return None;
    }
    if !matches!(c.get(i + 1), Some('x') | Some('X')) {
        return None;
    }
    if i > 0 && is_ident_char(c[i - 1]) {
        return None;
    }
    let mut n = i + 2;
    while n < c.len() && c[n].is_ascii_hexdigit() {
        n += 1;
    }
    if n == i + 2 {
        return None;
    }
    Some((n - i, "0x<addr>".to_string()))
}

/// Case-insensitive markers that introduce a numeric thread/process id.
const THREAD_MARKERS: &[&str] = &["Thread-", "tid=", "pid="];

fn scan_thread_id(c: &[char], i: usize) -> Option<(usize, String)> {
    for marker in THREAD_MARKERS {
        let m: Vec<char> = marker.chars().collect();
        if i + m.len() > c.len() {
            continue;
        }
        let matches_marker = c[i..i + m.len()]
            .iter()
            .zip(m.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if !matches_marker {
            continue;
        }
        // Not glued to a longer identifier on the left.
        if i > 0 && is_ident_char(c[i - 1]) {
            continue;
        }
        let mut n = i + m.len();
        let digits_start = n;
        while n < c.len() && c[n].is_ascii_digit() {
            n += 1;
        }
        if n == digits_start {
            continue;
        }
        return Some((n - i, "<thread-id>".to_string()));
    }
    None
}

fn scan_timestamp(c: &[char], i: usize) -> Option<(usize, String)> {
    let digits = |k: usize, count: usize| {
        (0..count).all(|j| c.get(k + j).map(|ch| ch.is_ascii_digit()).unwrap_or(false))
    };
    // YYYY-MM-DD
    if !digits(i, 4)
        || c.get(i + 4) != Some(&'-')
        || !digits(i + 5, 2)
        || c.get(i + 7) != Some(&'-')
        || !digits(i + 8, 2)
    {
        return None;
    }
    // Not glued to a longer number on the left (so `12026-07-31` is left alone).
    if i > 0 && c[i - 1].is_ascii_digit() {
        return None;
    }
    let mut n = i + 10;
    // [T| ]HH:MM:SS[.frac][Z]
    let has_time = matches!(c.get(n), Some('T') | Some(' '))
        && digits(n + 1, 2)
        && c.get(n + 3) == Some(&':')
        && digits(n + 4, 2)
        && c.get(n + 6) == Some(&':')
        && digits(n + 7, 2);
    if has_time {
        n += 9;
        if c.get(n) == Some(&'.') {
            let mut m = n + 1;
            while m < c.len() && c[m].is_ascii_digit() {
                m += 1;
            }
            if m > n + 1 {
                n = m;
            }
        }
        if c.get(n) == Some(&'Z') {
            n += 1;
        }
    }
    Some((n - i, "<timestamp>".to_string()))
}

fn scan_frame_line(c: &[char], i: usize) -> Option<(usize, String)> {
    const PAT: &str = ".java:";
    let p: Vec<char> = PAT.chars().collect();
    if i + p.len() > c.len() || c[i..i + p.len()] != p[..] {
        return None;
    }
    let mut n = i + p.len();
    let digits_start = n;
    while n < c.len() && c[n].is_ascii_digit() {
        n += 1;
    }
    if n == digits_start {
        return None;
    }
    Some((n - i, ".java:<line>".to_string()))
}

/// Characters that terminate a path run.
fn is_path_terminator(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '"' | '\'' | ')' | ']' | ',' | ';')
}

fn scan_absolute_path(c: &[char], i: usize) -> Option<(usize, String)> {
    let drive_start = c[i].is_ascii_alphabetic()
        && c.get(i + 1) == Some(&':')
        && matches!(c.get(i + 2), Some('\\') | Some('/'))
        && (i == 0 || !is_ident_char(c[i - 1]));
    let root_start = c[i] == '/'
        && (i == 0 || c[i - 1].is_whitespace() || matches!(c[i - 1], '(' | '[' | '"' | '\'' | '='));
    if !drive_start && !root_start {
        return None;
    }
    let mut n = if drive_start { i + 3 } else { i + 1 };
    let mut last_sep = n;
    while n < c.len() {
        let ch = c[n];
        if ch == '/' || ch == '\\' {
            n += 1;
            last_sep = n;
            continue;
        }
        if is_path_terminator(ch) {
            break;
        }
        n += 1;
    }
    if last_sep <= i {
        return None;
    }
    Some((last_sep - i, "<path>/".to_string()))
}

fn path_separator_token(tok: &str) -> String {
    let drive_prefixed = {
        let mut it = tok.chars();
        matches!((it.next(), it.next(), it.next()),
            (Some(a), Some(':'), Some('\\' | '/')) if a.is_ascii_alphabetic())
    };
    if !tok.contains('/') && !drive_prefixed {
        return tok.to_string();
    }
    tok.replace('\\', "/")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_neutralizes_its_own_probe() {
        for r in ORDER {
            assert_eq!(
                r.apply(r.probe),
                r.probe_expected,
                "rule {} did not rewrite its own probe",
                r.id
            );
        }
    }

    /// The other half of the contract: a rule must touch **only** its target.
    ///
    /// `line-endings` is excluded as a *victim* (never as an actor): its probe
    /// is CR/LF-shaped, and `str::lines` legitimately drops the `\r`, so
    /// `sort-lines` rewrites it for a reason that is not over-normalization.
    #[test]
    fn no_rule_touches_another_rules_probe() {
        for actor in ORDER {
            for victim in ORDER {
                if actor.id == victim.id || victim.id == LINE_ENDINGS.id {
                    continue;
                }
                assert_eq!(
                    actor.apply(victim.probe),
                    victim.probe,
                    "rule {} must not rewrite {}'s probe {:?}",
                    actor.id,
                    victim.id,
                    victim.probe
                );
            }
        }
    }

    #[test]
    fn every_rule_has_a_stated_risk_and_a_unique_id() {
        let mut ids: Vec<&str> = ORDER.iter().map(|r| r.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate rule id");
        for r in ORDER {
            assert!(!r.target.is_empty(), "{} has no target", r.id);
            assert!(!r.justification.is_empty(), "{} has no justification", r.id);
            assert!(
                r.risk.len() > 40,
                "{} states no meaningful risk — every rule has one",
                r.id
            );
            assert_eq!(rule(r.id).map(|f| f.id), Some(r.id));
        }
        assert!(rule("no-such-rule").is_none());
    }

    // -- individual rule edge cases -----------------------------------------

    #[test]
    fn identity_hash_needs_four_hex_digits_and_a_name() {
        let f = IDENTITY_HASH;
        assert_eq!(f.apply("Foo@1b6d3586"), "Foo@<idhash>");
        // Annotation-looking text and short suffixes are left alone.
        assert_eq!(f.apply("@Override"), "@Override");
        assert_eq!(f.apply("Foo@ab"), "Foo@ab");
        // A bare `@` with no preceding identifier is not an identity hash.
        assert_eq!(f.apply(" @1b6d3586"), " @1b6d3586");
        // Glued identifier text after the hex run is not a hash either.
        assert_eq!(f.apply("Foo@deadzz"), "Foo@deadzz");
    }

    #[test]
    fn hex_address_ignores_identifier_glued_zero_x() {
        let f = HEX_ADDRESS;
        assert_eq!(f.apply("0xff"), "0x<addr>");
        assert_eq!(f.apply("at 0x7f9c1a2b lives"), "at 0x<addr> lives");
        // `a0x1` is not an address literal.
        assert_eq!(f.apply("a0x1"), "a0x1");
        // A lone `0x` with no digits is not one either.
        assert_eq!(f.apply("0x"), "0x");
    }

    #[test]
    fn timestamp_accepts_date_only_and_rejects_longer_numbers() {
        let f = TIMESTAMP;
        assert_eq!(f.apply("2026-07-31"), "<timestamp>");
        assert_eq!(f.apply("2026-07-31 12:00:00"), "<timestamp>");
        assert_eq!(f.apply("12026-07-31"), "12026-07-31");
    }

    #[test]
    fn absolute_path_keeps_the_basename_and_trailing_punctuation() {
        let f = ABSOLUTE_PATH;
        assert_eq!(
            f.apply("at Foo.main(C:\\work\\repo\\Foo.java:12)"),
            "at Foo.main(<path>/Foo.java:12)"
        );
        assert_eq!(f.apply("/tmp/x/y.txt"), "<path>/y.txt");
        // A relative path is not touched.
        assert_eq!(f.apply("src/main/Foo.java"), "src/main/Foo.java");
        // A URL authority is not a drive letter.
        assert_eq!(f.apply("http://host/p"), "http://host/p");
    }

    #[test]
    fn path_separator_only_rewrites_path_shaped_tokens() {
        let f = PATH_SEPARATOR;
        assert_eq!(f.apply("C:\\a\\b"), "C:/a/b");
        assert_eq!(f.apply("x/y\\z"), "x/y/z");
        // A backslash in ordinary prose or a regex without a slash is kept.
        assert_eq!(f.apply("escaped\\n"), "escaped\\n");
    }

    #[test]
    fn frame_line_numbers_needs_digits() {
        let f = FRAME_LINE_NUMBERS;
        assert_eq!(f.apply("Foo.java:1"), "Foo.java:<line>");
        assert_eq!(f.apply("Foo.java:"), "Foo.java:");
        assert_eq!(f.apply("Foo.java"), "Foo.java");
    }

    #[test]
    fn strip_ansi_removes_csi_sequences_only() {
        assert_eq!(strip_ansi("\u{1b}[2mdim\u{1b}[0m"), "dim");
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\u{1b}[38;5;196mred\u{1b}[0m!"), "red!");
        // Brackets that aren't escapes stay put.
        assert_eq!(strip_ansi("[cratonvm] ok"), "[cratonvm] ok");
    }

    #[test]
    fn line_endings_normalized() {
        assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb");
        assert_eq!(normalize_line_endings("a\nb\n\n  "), "a\nb");
    }
}
