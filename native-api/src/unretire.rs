// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Turn named rows of the retirement tables back OFF at runtime, so bisecting a
//! wave costs a RUN instead of a BUILD.
//!
//! # The problem this solves
//!
//! [`crate::retired_shadow`] is a compile-time constant. When a retirement wave
//! lands and one corpus vector goes red, the only way to find which of the
//! wave's triples did it has been to edit the table, rebuild the VM (~20-26
//! minutes on the shared host) and re-run — once per hypothesis. Lane 4 wave 1
//! spent three of its seven builds on exactly that, and still could not
//! attribute `RJdkSecurity` per-triple: eighteen retired triples were reachable
//! from that vector and the file-handle group among them had to be reported as
//! a GROUP, un-attributed, in the lane page.
//!
//! With this switch the same bisection is a sequence of runs:
//!
//! ```text
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=all              does un-retiring fix it?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/         which package?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File     which class?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File.isAbsolute()Z   which row?
//! ```
//!
//! # It is a DIAGNOSTIC, and the default is provably inert
//!
//! Unset — which is every shipping configuration and every CI arm — parses to
//! `None` and [`is_excluded`] returns `false` for every input without
//! consulting anything. `the_default_is_inert` asserts that against the whole
//! of `RETIRED_SHADOW_TABLES` rather than against a sample, because "a guard's
//! default must be chosen by measuring the inert one" and a switch that
//! *nearly* does nothing by default is a correctness bug in every lane at once.
//!
//! # Why it reports at ARM time, and why a rule matching nothing is loud
//!
//! The failure this instrument would otherwise invite is the one that makes
//! bisection worse than useless: a mistyped rule matches no row, the vector
//! still fails, and the reader concludes the triple is exonerated. That is a
//! false NEGATIVE produced by the tool itself.
//!
//! So the rules are resolved against the tables **once, at parse time, before
//! any dispatch**, and the row count for each is printed. A rule matching zero
//! rows is called out by name. You learn your list is wrong in the first
//! milliseconds of the run rather than from a clean result an hour later.
//!
//! ```text
//!   [cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW armed, 2 rule(s):
//!       java/io/File                              51 row(s)
//!       java/io/Reader.close                       0 row(s)   <-- MATCHES NOTHING
//!   [cratonvm] 1 rule matches no retired row. A rule that matches nothing
//!              cannot exonerate anything: the run below is NOT a test of it.
//! ```
//!
//! The count is TABLE ROWS -- distinct triples in
//! `RETIRED_SHADOW_TABLES` -- not registrations. A census taken either side of
//! the switch moves by more: `java/io/File` reports 51 rows and returns 71
//! registrations to `Bridge`, because 54 distinct `java/io/File` triples are
//! registered at 74 ordinals and the re-tag flips each one. The stub ratchet
//! documents the same distinction for the same reason.
//!
//! # What it does NOT do
//!
//! It un-retires; it does not re-retire, and it cannot make a triple retired
//! that no table carries. It is therefore incapable of turning a green run red
//! by itself — the worst it can do is restore the pre-wave behaviour, which is
//! the behaviour the wave was measured against.
//!
//! Nor is it the dial. `CRATONVM_ENFORCE_NATIVE_SHADOW` declines a native at
//! DISPATCH and its decline is conditional — when the receiver has no concrete
//! body it runs the native anyway. This switch edits the TABLE, so what it
//! turns off is off unconditionally. That asymmetry is the whole reason the
//! dial cannot certify a wave and this can bisect one.

use std::sync::OnceLock;

/// One parsed entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// `all` — every row of every table.
    All,
    /// `java/io/` — every class under a package prefix (trailing `/`).
    Prefix(String),
    /// `java/io/File` — every row of one class.
    Class(String),
    /// `java/io/File.isAbsolute` — every overload of one method.
    ClassMethod(String, String),
    /// `java/io/File.isAbsolute()Z` — exactly one row.
    Triple(String, String, String),
}

impl Rule {
    fn matches(&self, class: &str, method: &str, descriptor: &str) -> bool {
        match self {
            Rule::All => true,
            Rule::Prefix(p) => class.starts_with(p.as_str()),
            Rule::Class(c) => class == c,
            Rule::ClassMethod(c, m) => class == c && method == m,
            Rule::Triple(c, m, d) => class == c && method == m && descriptor == d,
        }
    }
}

/// Parse one entry.
///
/// The grammar is unambiguous without lookahead because of what JVM names can
/// contain: a descriptor always starts at the first `(`, a class name never
/// contains `.` (it is internal form, `java/io/File`), and a method name
/// contains neither `.` nor `(`. So the first `(` splits off the descriptor and
/// the last `.` before it splits class from method.
fn parse_rule(entry: &str) -> Option<Rule> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if entry.eq_ignore_ascii_case("all") {
        return Some(Rule::All);
    }
    if entry.ends_with('/') {
        return Some(Rule::Prefix(entry.to_string()));
    }
    let (head, descriptor) = match entry.find('(') {
        Some(i) => (&entry[..i], Some(entry[i..].to_string())),
        None => (entry, None),
    };
    match head.rfind('.') {
        Some(i) => {
            let class = head[..i].to_string();
            let method = head[i + 1..].to_string();
            if class.is_empty() || method.is_empty() {
                return None;
            }
            Some(match descriptor {
                Some(d) => Rule::Triple(class, method, d),
                None => Rule::ClassMethod(class, method),
            })
        }
        // No `.` at all: a bare class name. A descriptor without a method is
        // meaningless, so reject it rather than silently matching the class.
        None => {
            if descriptor.is_some() {
                None
            } else {
                Some(Rule::Class(head.to_string()))
            }
        }
    }
}

/// The parsed list and, beside each rule, how many retired rows it reaches.
struct Armed {
    rules: Vec<Rule>,
    /// `(source text, rows matched)`, in the order given.
    report: Vec<(String, usize)>,
}

fn armed() -> Option<&'static Armed> {
    static ARMED: OnceLock<Option<Armed>> = OnceLock::new();
    ARMED.get_or_init(parse_env).as_ref()
}

fn parse_env() -> Option<Armed> {
    let raw = cratonvm_types::flags::runtime_var("CRATONVM_UNRETIRE_NATIVE_SHADOW").ok()?;
    build(&raw, true)
}

/// Split, parse and resolve a list. `announce` is false in tests so the suite
/// does not write to stderr.
fn build(raw: &str, announce: bool) -> Option<Armed> {
    let mut rules = Vec::new();
    let mut report = Vec::new();
    let mut rejected = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match parse_rule(entry) {
            Some(rule) => {
                let rows = rows_matched(&rule);
                report.push((entry.to_string(), rows));
                rules.push(rule);
            }
            None => rejected.push(entry.to_string()),
        }
    }
    if rules.is_empty() && rejected.is_empty() {
        return None;
    }
    if announce {
        announce_arm(&report, &rejected);
    }
    Some(Armed { rules, report })
}

/// How many rows of the retirement tables a rule reaches.
///
/// Resolved once, at arm time. This is the number that makes a typo visible
/// BEFORE the run rather than after it.
fn rows_matched(rule: &Rule) -> usize {
    crate::retired_shadow::RETIRED_SHADOW_TABLES
        .iter()
        .flat_map(|table| table.iter())
        .filter(|(c, m, d)| rule.matches(c, m, d))
        .count()
}

fn announce_arm(report: &[(String, usize)], rejected: &[String]) {
    eprintln!(
        "[cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW armed, {} rule(s):",
        report.len()
    );
    for (text, rows) in report {
        let flag = if *rows == 0 {
            "   <-- MATCHES NOTHING"
        } else {
            ""
        };
        eprintln!("    {text:<48} {rows:>5} table row(s){flag}");
    }
    let empty = report.iter().filter(|(_, n)| *n == 0).count();
    if empty > 0 {
        eprintln!(
            "[cratonvm] {empty} rule(s) match no retired row. A rule that matches \
             nothing cannot\n           exonerate anything: this run is NOT a test \
             of it. Check the spelling\n           against --dump-native-registry \
             (class names are internal form, `java/io/File`)."
        );
    }
    for text in rejected {
        eprintln!("[cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW: cannot parse {text:?} — ignored.");
    }
}

/// Is this triple excluded from retirement by the switch?
///
/// `false` for every input when the variable is unset, which is every shipping
/// configuration. See the module header on why the default is asserted inert
/// against the whole table rather than a sample.
#[must_use]
pub fn is_excluded(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    match armed() {
        None => false,
        Some(a) => a
            .rules
            .iter()
            .any(|r| r.matches(class_name, method_name, descriptor)),
    }
}

/// AUDIT lever (SBR round 2, C2-path audit of the reviewed `Intrinsic`s): the
/// rows named by `CRATONVM_AUDIT_YIELD_INTRINSIC` (comma-separated
/// `java/class/Prefix` or `java/class/Name.method` prefixes; `all` = every
/// reviewed `Intrinsic`) are registered as a dropped stub instead, so under
/// `--jdk-only` the JDK's own bytecode runs, through every tier including C2.
/// It exists only to MEASURE whether a reviewed `Intrinsic` earns its keep
/// against the real bytecode; unset (every shipping configuration) it is
/// `false` without consulting anything else. Never a product switch.
#[must_use]
pub fn audit_yields_intrinsic(class_name: &str, method_name: &str) -> bool {
    use std::sync::OnceLock;
    static RULES: OnceLock<Vec<String>> = OnceLock::new();
    let rules = RULES.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_AUDIT_YIELD_INTRINSIC")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    });
    if rules.is_empty() {
        return false;
    }
    let dotted = format!("{class_name}.{method_name}");
    rules
        .iter()
        .any(|r| r == "all" || dotted.starts_with(r.as_str()))
}

/// PROBE lever (opt-in pure-bytecode mode): `CRATONVM_PURE_BYTECODE=1` makes
/// every `--jdk-only` dispatch door prefer the JDK's own bytecode over ANY
/// registered native (`Bridge` and reviewed `Intrinsic` alike) whenever the
/// method has real `Code`. `ACC_NATIVE` methods and `Code`-less interface
/// methods still reach their registration: no bytecode exists to prefer. It
/// exists to probe what breaks with no Rust shadow at all, and is neither a
/// product switch nor a `VmConfig` policy yet; unset it is `false`.
#[must_use]
pub fn pure_bytecode() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_PURE_BYTECODE")
            .is_ok_and(|v| !v.is_empty() && v != "0")
    })
}

/// The lever's exemptions. First, the `Math` methods HotSpot itself never runs as
/// bytecode. Its interpreter and both JITs replace them with stubs
/// (`dpow`, `dsin`, ...), whose results differ from the JDK's `FdLibm` bytecode
/// in the sign bit of a NaN (`Math.pow(1.0, +Inf)` is `0x7ff8...` on HotSpot,
/// `0xfff8...` from `y - y`). The lever exists to run what HotSpot runs, so for
/// these a reviewed `Intrinsic` stays in front of the bytecode.
#[must_use]
pub fn lever_keeps_hotspot_intrinsic(class_name: &str, method_name: &str) -> bool {
    (class_name == "java/lang/Math"
        && matches!(
            method_name,
            "pow" | "sin" | "cos" | "tan" | "exp" | "log" | "log10"
        ))
        // `Class.getModule()` is `return module;` and no Java code writes
        // `Class.module`: the VM fills it only for classes whose module it knows by
        // name (`fixup_class_module_fields`), so a class of a module created at run
        // time (`jdk.proxyN`, from `Proxy` / annotation proxies) read `null` under the
        // lever and `TypeLibrary.invokeAnnotation` (JFR) died with an NPE. The reviewed
        // `Intrinsic` (`ClassModuleSweep`, lib.rs) answers by identity; HotSpot fills the
        // field at define time. SBR round 5, lane fix1.
        || (class_name == "java/lang/Class" && method_name == "getModule")
        || lever_keeps_platform_native(class_name)
        || (cfg!(windows)
            && class_name == "sun/nio/fs/WindowsFileSystemProvider"
            && method_name == "checkAccess")
}

/// The classes whose real JDK implementation needs machinery this VM models
/// natively and does not provide to bytecode, so under the lever their registered
/// natives stay in front of the bytecode:
///
/// * every platform: `WeakReference` and `Reference` of `java.lang.ref` (the GC
///   discovers a reference in its native constructor; the bytecode constructor
///   leaves it strong, so nothing unloads), `jdk.jfr`
///   (minus `events`) and `java.lang.foreign` / `jdk.internal.foreign` (minus
///   `abi`) (the JVM-side event and downcall machinery), the AWT headless
///   carriers (`java.awt.GraphicsEnvironment`, `BufferedImage`,
///   `javax.imageio`, `sun.awt`, `sun.java2d`: no libawt);
/// * Windows only: `ModuleReferenceImpl` (its `open` native opens the
///   module-path reference lazily; on Linux the bytecode passes without it),
///   `sun.nio.ch` (`WEPoll` / `Iocp` have no native layer here), the Windows
///   watch-service and file-system classes, `MappedMemoryUtils`, the datagram
///   sockets. Empty off Windows, where the bytecode runs on real natives.
///
/// Each family is measured: dropping it (`CRATONVM_LEVER_DROP`) turns a lever
/// vector red again (see `SBR2-HANDOFF-remaining-work-20261002.md`).
pub fn lever_keeps_platform_native(class_name: &str) -> bool {
    !lever_drop_env(class_name)
        && (lever_keep_env(class_name)
        // SBR3 lang (Linux lever bisection, `CRATONVM_LEVER_DROP`): of
        // `java.lang.ref` only `WeakReference` (whose native constructor is where
        // the GC discovers the referent: RClassUnloadSweep / RClassUnloadSweepGen
        // go red without it) and `Reference` stay; `Cleaner`, `ReferenceQueue`,
        // `SoftReference`, `PhantomReference`, `Finalizer`, `FinalReference` and
        // `Reference$*` run the JDK's own bytecode (15 GC / executor / JMX vectors
        // green).
        || class_name == "java/lang/ref/WeakReference"
        || class_name == "java/lang/ref/Reference"
        // `java.lang.reflect.Constructor` is no longer exempt (SBR3 lang): the compiled-code
        // `ClassCastException` of the serialization constructor was the real allocator
        // handle allocating its member's class, not its `instanceClass`
        // (`mh_real_direct_dispatch`, fixed). Measured on Linux only.
        // The JDK's `jdk.jfr` layer: `jdk/jfr/internal/` (its own classes) and
        // `jdk/jfr/consumer/` are needed (RJfrRecordingStream); `jdk/jfr/events/`
        // and the `internal/{consumer,util,JVM,event,jfc,settings,dcmd,management,
        // tool,periodic,instrument}` subpackages were each dropped alone green.
        || (class_name.starts_with("jdk/jfr/") && !class_name.starts_with("jdk/jfr/events/"))
        // `ModuleReferenceImpl.open` reads the module-path reference lazily. On
        // Linux the JDK's own bytecode passes RJdkModule and the four module /
        // service vectors without it (SBR3 lang), so the row is kept on Windows
        // only, where it has not been re-measured.
        || (cfg!(windows) && class_name == "jdk/internal/module/ModuleReferenceImpl")
        // FFM: `jdk/internal/foreign/abi/` (the linker's call-shape machinery)
        // runs as bytecode (SBR3 lang: RJdkForeign / RSegmentBulkCopy / layout
        // vectors unchanged); the rest of the layer is still modelled natively.
        || (class_name.starts_with("jdk/internal/foreign/")
            && !class_name.starts_with("jdk/internal/foreign/abi/"))
        || class_name.starts_with("java/lang/foreign/")
        || class_name.starts_with("cratonvm/internal/foreign/")
        // AWT: the headless carriers need libawt (`Font.initIDs`, ...) that no
        // platform provides here; RJdkAwtHeadless is red on Linux without them.
        || class_name.starts_with("java/awt/GraphicsEnvironment")
        || class_name.starts_with("java/awt/image/BufferedImage")
        || class_name.starts_with("javax/imageio/")
        || class_name.starts_with("sun/awt/")
        || class_name.starts_with("sun/java2d/")
        || (cfg!(windows)
        && (class_name.starts_with("sun/nio/ch/")
            || class_name.starts_with("sun/nio/fs/WindowsWatch")
            || class_name == "sun/nio/fs/WindowsFileSystem"
            || class_name == "sun/nio/fs/WindowsPath"
            || class_name.starts_with("sun/nio/fs/WindowsSecurity")
            || class_name.starts_with("java/nio/channels/")
            || class_name.starts_with("java/nio/Mapped")
            || class_name.starts_with("java/net/DatagramSocket")
            || class_name.starts_with("java/net/MulticastSocket"))))
}

/// [`pure_bytecode`] for one method: the lever, minus its one exemption.
#[must_use]
pub fn pure_bytecode_for(class_name: &str, method_name: &str) -> bool {
    pure_bytecode() && !lever_keeps_hotspot_intrinsic(class_name, method_name)
}

/// Which registered kinds the enforcement dial may decline in favour of real
/// bytecode, with the [`pure_bytecode`] lever passed IN so a test can model it
/// without a process global. Default (lever off): `Bridge` only, a reviewed
/// `Intrinsic` being §1.4's exception. Lever on: a reviewed `Intrinsic`
/// yields too. A `SyntheticStub` is refused by §1.3 before any dial is asked.
#[must_use]
pub fn dial_kind_decision(kind: crate::NativeKind, pure_bytecode: bool) -> bool {
    match kind {
        crate::NativeKind::Bridge => true,
        crate::NativeKind::Intrinsic => pure_bytecode,
        crate::NativeKind::SyntheticStub => false,
    }
}

/// `(rule as written, rows of the retirement tables it reaches)`, empty when
/// the switch is not armed.
///
/// Exposed so a caller can fold the same account into a report rather than
/// re-deriving it, and so the zero-match case is testable without capturing
/// stderr.
#[must_use]
pub fn report() -> Vec<(String, usize)> {
    armed().map(|a| a.report.clone()).unwrap_or_default()
}

/// Bisection aid: `CRATONVM_LEVER_KEEP=<internal-name prefix>[,<prefix>...]`
/// keeps the registered natives of those classes in front of the bytecode under
/// the lever, to find which family a lever-red vector depends on.
#[must_use]
pub fn lever_keep_env(class_name: &str) -> bool {
    use std::sync::OnceLock;
    static PREFIXES: OnceLock<Vec<String>> = OnceLock::new();
    let prefixes = PREFIXES.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_LEVER_KEEP")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    });
    !prefixes.is_empty() && prefixes.iter().any(|p| class_name.starts_with(p.as_str()))
}

/// Bisection aid, the inverse of [`lever_keep_env`]: `CRATONVM_LEVER_DROP`
/// removes a class family from the lever's built-in exemptions at run time.
#[must_use]
pub fn lever_drop_env(class_name: &str) -> bool {
    use std::sync::OnceLock;
    static PREFIXES: OnceLock<Vec<String>> = OnceLock::new();
    let prefixes = PREFIXES.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_LEVER_DROP")
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    });
    !prefixes.is_empty() && prefixes.iter().any(|p| class_name.starts_with(p.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retired_shadow::{triple_is_retired_shadow, RETIRED_SHADOW_TABLES};

    fn rules(raw: &str) -> Vec<Rule> {
        build(raw, false).map(|a| a.rules).unwrap_or_default()
    }

    #[test]
    fn the_default_is_inert_across_every_retired_row() {
        // Not a sample. The whole population, because this switch is compiled
        // into every shipping binary and a default that is *nearly* inert is a
        // defect in every lane at once.
        let mut rows = 0;
        for table in RETIRED_SHADOW_TABLES {
            for (c, m, d) in table.iter() {
                assert!(
                    !is_excluded(c, m, d),
                    "{c}.{m}{d} is excluded with the variable unset — the \
                     default is not inert"
                );
                assert!(
                    triple_is_retired_shadow(c, m, d),
                    "{c}.{m}{d} stopped being retired with the variable unset"
                );
                rows += 1;
            }
        }
        assert!(
            rows > 500,
            "expected the tables to be populated, saw {rows}"
        );
        assert!(report().is_empty());
    }

    #[test]
    fn pure_bytecode_dial_kind_rule_is_inert_off_and_yields_intrinsics_on() {
        use crate::NativeKind;
        // Lever off (every shipping configuration): the historical rule.
        assert!(dial_kind_decision(NativeKind::Bridge, false));
        assert!(!dial_kind_decision(NativeKind::Intrinsic, false));
        assert!(!dial_kind_decision(NativeKind::SyntheticStub, false));
        // Lever on: Bridge and reviewed Intrinsic yield; a stub never does
        // (it is refused, not declined).
        assert!(dial_kind_decision(NativeKind::Bridge, true));
        assert!(dial_kind_decision(NativeKind::Intrinsic, true));
        assert!(!dial_kind_decision(NativeKind::SyntheticStub, true));
        // Unset in the test process: the lever itself reads false.
        assert!(!pure_bytecode());
    }

    #[test]
    fn each_shape_parses_to_the_rule_it_reads_as() {
        assert_eq!(rules("all"), vec![Rule::All]);
        assert_eq!(rules("ALL"), vec![Rule::All]);
        assert_eq!(rules("java/io/"), vec![Rule::Prefix("java/io/".into())]);
        assert_eq!(
            rules("java/io/File"),
            vec![Rule::Class("java/io/File".into())]
        );
        assert_eq!(
            rules("java/io/File.isAbsolute"),
            vec![Rule::ClassMethod(
                "java/io/File".into(),
                "isAbsolute".into()
            )]
        );
        assert_eq!(
            rules("java/io/File.isAbsolute()Z"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "isAbsolute".into(),
                "()Z".into()
            )]
        );
    }

    #[test]
    fn a_descriptor_with_slashes_and_parens_survives_parsing() {
        // The grammar claim in `parse_rule` is that the first `(` ends the
        // head and the last `.` before it splits class from method. A
        // descriptor full of `/` and `;` is where that claim earns its keep.
        assert_eq!(
            rules("java/io/File.listFiles()[Ljava/io/File;"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "listFiles".into(),
                "()[Ljava/io/File;".into()
            )]
        );
        assert_eq!(
            rules("java/io/File.<init>(Ljava/lang/String;)V"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "<init>".into(),
                "(Ljava/lang/String;)V".into()
            )]
        );
    }

    #[test]
    fn a_list_is_split_and_blanks_are_dropped() {
        assert_eq!(
            rules(" java/io/ , , java/nio/ "),
            vec![
                Rule::Prefix("java/io/".into()),
                Rule::Prefix("java/nio/".into())
            ]
        );
    }

    #[test]
    fn an_unparseable_entry_is_rejected_rather_than_guessed() {
        assert_eq!(parse_rule("()V"), None);
        assert_eq!(parse_rule(".foo"), None);
        assert_eq!(parse_rule("java/io/File."), None);
        assert_eq!(parse_rule(""), None);
    }

    #[test]
    fn a_rule_reports_the_rows_it_reaches_and_zero_is_visible() {
        // The whole point of resolving at arm time: a plausible-looking rule
        // that reaches nothing is reported as 0 rather than silently
        // exonerating whatever it was meant to name.
        let armed = build("java/io/File,java/io/Reader.close,all", false).unwrap();
        let by_text: Vec<(String, usize)> = armed.report;
        assert_eq!(by_text[0].0, "java/io/File");
        assert!(
            by_text[0].1 >= 50,
            "java/io/File should reach its retired rows, got {}",
            by_text[0].1
        );
        assert_eq!(
            by_text[1].1, 0,
            "java/io/Reader.close is in no table; it must report 0"
        );
        assert!(by_text[2].1 > by_text[0].1, "`all` must reach every row");
    }

    #[test]
    fn matching_is_scoped_exactly_as_written() {
        let c = Rule::Class("java/io/File".into());
        assert!(c.matches("java/io/File", "isAbsolute", "()Z"));
        assert!(!c.matches("java/io/FileWriter", "close", "()V"));

        let p = Rule::Prefix("java/io/".into());
        assert!(p.matches("java/io/FileWriter", "close", "()V"));
        assert!(!p.matches("java/nio/ByteBuffer", "limit", "()I"));

        let t = Rule::Triple("java/io/File".into(), "isAbsolute".into(), "()Z".into());
        assert!(t.matches("java/io/File", "isAbsolute", "()Z"));
        assert!(!t.matches("java/io/File", "isAbsolute", "()I"));
        assert!(!t.matches("java/io/File", "isDirectory", "()Z"));
    }

    /// The lever's exemptions are the documented ones and nothing wider: the
    /// seven `Math` stubs, the VM-modelled families, and (Windows) the channel
    /// layer. Everything else still yields to bytecode.
    #[test]
    fn the_lever_exemptions_are_exactly_the_documented_ones() {
        for m in ["pow", "sin", "cos", "tan", "exp", "log", "log10"] {
            assert!(lever_keeps_hotspot_intrinsic("java/lang/Math", m), "{m}");
        }
        assert!(!lever_keeps_hotspot_intrinsic("java/lang/Math", "abs"));
        assert!(!lever_keeps_hotspot_intrinsic(
            "java/lang/StrictMath",
            "pow"
        ));
        for c in [
            "java/lang/ref/WeakReference",
            "java/lang/ref/Reference",
            "jdk/jfr/internal/JVM",
            "java/lang/foreign/MemorySegment",
            "jdk/internal/foreign/MemorySessionImpl",
            "sun/awt/X11GraphicsEnvironment",
        ] {
            assert!(lever_keeps_platform_native(c), "{c}");
        }
        // SBR3 lang: measured droppable on Linux.
        for c in [
            "java/lang/ref/ReferenceQueue",
            "java/lang/ref/Cleaner",
            "java/lang/ref/SoftReference",
            "java/lang/ref/PhantomReference",
            "java/lang/ref/Finalizer",
            "jdk/internal/foreign/abi/Binding",
            "jdk/jfr/events/FileReadEvent",
        ] {
            assert!(!lever_keeps_platform_native(c), "{c}");
        }
        assert_eq!(
            lever_keeps_platform_native("jdk/internal/module/ModuleReferenceImpl"),
            cfg!(windows)
        );
        assert!(!lever_keeps_platform_native("java/lang/reflect/Method"));
        assert!(!lever_keeps_platform_native(
            "java/lang/reflect/Constructor"
        ));
        assert!(!lever_keeps_platform_native("java/util/HashMap"));
        assert!(!lever_keeps_platform_native("java/security/SecureRandom"));
        // Windows only: no real native layer under these.
        assert_eq!(
            lever_keeps_platform_native("sun/nio/ch/WEPollSelectorImpl"),
            cfg!(windows)
        );
        assert_eq!(
            lever_keeps_hotspot_intrinsic("sun/nio/fs/WindowsFileSystemProvider", "checkAccess"),
            cfg!(windows)
        );
        assert!(!lever_keeps_hotspot_intrinsic(
            "sun/nio/fs/WindowsFileSystemProvider",
            "readAttributes"
        ));
    }
}
