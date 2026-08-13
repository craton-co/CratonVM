// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reads the checked-in JDK 25 surface baselines in `scripts/baselines/`.
//!
//! `docs/known-issues/jdk-only/E25-R11-GUARD-POPULATION-SWEEP-20260813.md` §7
//! found thirty guards in this crate whose expected set was transcribed from
//! the registrar they audit, and named the cause: *"not one test reads a
//! checked-in baseline file, and not one invokes `javap`"*. This module is the
//! read side of the fix. The write side is `scripts/jdk-baseline/generate.py`,
//! which emits the files from `jrt:/modules/<module>/<binary/name>.class` on
//! openjdk 25.0.3+9; see
//! `docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md`.
//!
//! # The one rule for anything built on this
//!
//! A guard must use [`audit`], not a bare membership loop. A one-way read —
//! "for each thing I registered, assert the JDK has it" — is the same
//! restatement with a new data source: it cannot notice a gap that closed
//! (kind 3) or a triage row that rotted (kind 4), and those are the two
//! directions E20 found already broken the first time anyone looked (three of
//! six `already_triaged` rows were standing approval for defects that had
//! since been fixed). `[both halves]`.
//!
//! | # | condition | name in the output |
//! |---|---|---|
//! | 1 | a JDK member with no triage row | `UNCOVERED` |
//! | 2 | `expect_registered: true`, not registered | `DROPPED` |
//! | 3 | `expect_registered: false`, **is** registered | `CLOSED` |
//! | 4 | a triage row naming a member this JDK does not declare | `STALE` |
//!
//! # Never hand-edit a baseline
//!
//! `scripts/baselines/README.md`'s rule applies verbatim: every row is written
//! by the generator from a census it took. A row typed by a person is the
//! defect these files exist to remove, reintroduced one line at a time. The
//! parser enforces what it can — see [`parse`], which validates each file
//! against three counts the *generator* wrote and this module recomputes.

use std::collections::BTreeSet;

// ---------------------------------------------------------------------------
// The baselines. The file name is a mechanical function of the binary name, so
// adding a class is one `include_str!` plus one `ALL` row and nothing else.
// Regenerate with `python scripts/jdk-baseline/generate.py --update`.
//
// All thirty checked-in baselines are wired in, not just the ones a guard
// reads today: `every_checked_in_baseline_is_wired_in_and_parses` is a
// two-way source witness over this list, and it can only be two-way if the
// list is meant to be complete. A baseline nobody includes is a file that can
// never go red, which is the property this whole capability exists to remove.
// ---------------------------------------------------------------------------

pub(crate) const ABSTRACT_COLLECTION: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.AbstractCollection.tsv");
pub(crate) const ABSTRACT_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.AbstractQueue.tsv");
pub(crate) const BASE64: &str = include_str!("../../scripts/baselines/jdk25-java.util.Base64.tsv");
pub(crate) const BASE64_DECODER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.Base64$Decoder.tsv");
pub(crate) const BASE64_ENCODER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.Base64$Encoder.tsv");
pub(crate) const BIG_INTEGER: &str =
    include_str!("../../scripts/baselines/jdk25-java.math.BigInteger.tsv");
pub(crate) const BLOCKING_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.BlockingQueue.tsv");
pub(crate) const CDS: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv");
pub(crate) const CHARACTER: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.Character.tsv");
pub(crate) const FLOW_PUBLISHER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.Flow$Publisher.tsv");
pub(crate) const KEY_STORE: &str =
    include_str!("../../scripts/baselines/jdk25-java.security.KeyStore.tsv");
pub(crate) const MAC: &str = include_str!("../../scripts/baselines/jdk25-javax.crypto.Mac.tsv");
pub(crate) const MANAGEMENT_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.management.ManagementFactory.tsv");
pub(crate) const MODULE: &str = include_str!("../../scripts/baselines/jdk25-java.lang.Module.tsv");
pub(crate) const MODULE_DESCRIPTOR: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.module.ModuleDescriptor.tsv");
pub(crate) const MODULE_JAVA_BASE: &str =
    include_str!("../../scripts/baselines/jdk25-module-java.base.tsv");
pub(crate) const MODULE_LAYER: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.ModuleLayer.tsv");
pub(crate) const REFLECTION_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.reflect.ReflectionFactory.tsv");
pub(crate) const RUNTIME_MXBEAN: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.management.RuntimeMXBean.tsv");
pub(crate) const SHARED_SECRETS: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv");
pub(crate) const SOCKET: &str = include_str!("../../scripts/baselines/jdk25-java.net.Socket.tsv");
pub(crate) const SSL_PARAMETERS: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLParameters.tsv");
pub(crate) const SSL_SESSION: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLSession.tsv");
pub(crate) const SSL_SOCKET: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLSocket.tsv");
pub(crate) const STRUCTURED_TASK_SCOPE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope.tsv");
pub(crate) const STRUCTURED_TASK_SCOPE_CONFIGURATION: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Configuration.tsv"
);
pub(crate) const STRUCTURED_TASK_SCOPE_JOINER: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Joiner.tsv"
);
pub(crate) const STRUCTURED_TASK_SCOPE_SUBTASK: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Subtask.tsv"
);
pub(crate) const SUBMISSION_PUBLISHER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv");
pub(crate) const SYNCHRONOUS_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SynchronousQueue.tsv");

/// Every baseline this crate embeds, as `(file name, contents)`.
///
/// Exists so `every_checked_in_baseline_is_wired_in_and_parses` can compare it
/// against `read_dir("scripts/baselines")` in **both** directions — a file on
/// disk that nothing includes, and an include that names a file no longer on
/// disk. The dead-row half is the one that decays;
/// `types/tests/flag_declaration_guard.rs::the_allowlist_has_no_dead_rows` is
/// the in-tree pattern this copies.
pub(crate) const ALL: &[(&str, &str)] = &[
    ("jdk25-java.lang.Character.tsv", CHARACTER),
    ("jdk25-java.lang.Module.tsv", MODULE),
    ("jdk25-java.lang.ModuleLayer.tsv", MODULE_LAYER),
    (
        "jdk25-java.lang.management.ManagementFactory.tsv",
        MANAGEMENT_FACTORY,
    ),
    (
        "jdk25-java.lang.management.RuntimeMXBean.tsv",
        RUNTIME_MXBEAN,
    ),
    (
        "jdk25-java.lang.module.ModuleDescriptor.tsv",
        MODULE_DESCRIPTOR,
    ),
    ("jdk25-java.math.BigInteger.tsv", BIG_INTEGER),
    ("jdk25-java.net.Socket.tsv", SOCKET),
    ("jdk25-java.security.KeyStore.tsv", KEY_STORE),
    (
        "jdk25-java.util.AbstractCollection.tsv",
        ABSTRACT_COLLECTION,
    ),
    ("jdk25-java.util.AbstractQueue.tsv", ABSTRACT_QUEUE),
    ("jdk25-java.util.Base64$Decoder.tsv", BASE64_DECODER),
    ("jdk25-java.util.Base64$Encoder.tsv", BASE64_ENCODER),
    ("jdk25-java.util.Base64.tsv", BASE64),
    (
        "jdk25-java.util.concurrent.BlockingQueue.tsv",
        BLOCKING_QUEUE,
    ),
    (
        "jdk25-java.util.concurrent.Flow$Publisher.tsv",
        FLOW_PUBLISHER,
    ),
    (
        "jdk25-java.util.concurrent.StructuredTaskScope$Configuration.tsv",
        STRUCTURED_TASK_SCOPE_CONFIGURATION,
    ),
    (
        "jdk25-java.util.concurrent.StructuredTaskScope$Joiner.tsv",
        STRUCTURED_TASK_SCOPE_JOINER,
    ),
    (
        "jdk25-java.util.concurrent.StructuredTaskScope$Subtask.tsv",
        STRUCTURED_TASK_SCOPE_SUBTASK,
    ),
    (
        "jdk25-java.util.concurrent.StructuredTaskScope.tsv",
        STRUCTURED_TASK_SCOPE,
    ),
    (
        "jdk25-java.util.concurrent.SubmissionPublisher.tsv",
        SUBMISSION_PUBLISHER,
    ),
    (
        "jdk25-java.util.concurrent.SynchronousQueue.tsv",
        SYNCHRONOUS_QUEUE,
    ),
    ("jdk25-javax.crypto.Mac.tsv", MAC),
    ("jdk25-javax.net.ssl.SSLParameters.tsv", SSL_PARAMETERS),
    ("jdk25-javax.net.ssl.SSLSession.tsv", SSL_SESSION),
    ("jdk25-javax.net.ssl.SSLSocket.tsv", SSL_SOCKET),
    (
        "jdk25-jdk.internal.access.SharedSecrets.tsv",
        SHARED_SECRETS,
    ),
    ("jdk25-jdk.internal.misc.CDS.tsv", CDS),
    (
        "jdk25-jdk.internal.reflect.ReflectionFactory.tsv",
        REFLECTION_FACTORY,
    ),
    ("jdk25-module-java.base.tsv", MODULE_JAVA_BASE),
];

/// The `# jdk-baseline` format version this parser understands.
const FORMAT_VERSION: &str = "1";

/// The `java.version` prefix these baselines were taken on.
///
/// A baseline regenerated on JDK 26 silently re-baselines every guard that
/// reads it. The pin makes the bump a decision rather than a side effect —
/// E25 §5 step 4's residual, closed.
const JAVA_VERSION_PIN: &str = "25.";

/// The `# columns` header a class baseline must carry, verbatim.
///
/// Checked because the grammar and the format version can drift apart: a
/// generator that swaps two columns without bumping `# jdk-baseline` would
/// otherwise be read silently, with descriptors landing in `name`.
const CLASS_COLUMNS: &str = "kind\tname\tdescriptor\tflags";
/// As [`CLASS_COLUMNS`]; a module baseline's third column is the export target.
const MODULE_COLUMNS: &str = "kind\tname\ttarget\tflags";

/// Row kinds a `# kind class` baseline may use.
const CLASS_ROW_KINDS: &[&str] = &[
    "CLASS",
    "EXTENDS",
    "IMPLEMENTS",
    "SUPERTYPE",
    "METHOD",
    "FIELD",
];
/// Row kinds a `# kind module` baseline may use.
const MODULE_ROW_KINDS: &[&str] = &[
    "REQUIRES", "EXPORTS", "OPENS", "USES", "PROVIDES", "PACKAGE",
];

/// One row of a baseline file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Row {
    /// `CLASS`, `EXTENDS`, `IMPLEMENTS`, `SUPERTYPE`, `METHOD`, `FIELD` — or,
    /// for a module baseline, `REQUIRES`, `EXPORTS`, `OPENS`, `USES`,
    /// `PROVIDES`, `PACKAGE`.
    pub kind: &'static str,
    pub name: &'static str,
    /// The JVM descriptor for a class baseline; the export target for a module
    /// one. Empty where the kind has neither.
    pub descriptor: &'static str,
    /// Comma-separated, in the generator's fixed order:
    /// `public,protected,static,final,abstract,interface,native,synchronized,bridge,synthetic`.
    pub flags: &'static str,
}

impl Row {
    pub(crate) fn has_flag(&self, f: &str) -> bool {
        self.flags.split(',').any(|x| x == f)
    }
}

/// A parsed baseline. Construct with [`parse`]; there is no other constructor,
/// so every instance has passed the self-tests in [`parse`].
pub(crate) struct Baseline {
    /// The binary name, from the `# name` header — `java.util.Base64$Encoder`.
    pub class: &'static str,
    /// `class` or `module`, from the `# kind` header.
    pub kind: &'static str,
    /// From the `# java.version` header, e.g. `25.0.3`.
    pub java_version: &'static str,
    pub rows: Vec<Row>,
}

/// Parse a baseline, validating it against its own header.
///
/// **Panics rather than returning an error**, deliberately: every caller is a
/// `#[test]`, and a baseline that will not parse must not degrade into an empty
/// population that audits nothing. E25 row 32 is a "regression gate" that
/// returns green in 0.00 s with no corpus; a guard auditing zero rows is the
/// same thing wearing a different name.
///
/// # The self-tests, and why each is a cross-check and not a restatement
///
/// The headers are written by `generate.py` from the class file; the counts
/// below are recomputed here from the *rows*, by a different program in a
/// different language. Two independent counts of the same file disagreeing
/// means the file was hand-edited, truncated, or the parser is wrong — and a
/// parser that silently mis-parses is the same defect class as the guards this
/// capability exists to fix.
///
/// * `# jdk-baseline` must be a version this parser knows.
/// * `# columns` must be the exact grammar this parser splits on.
/// * `# java.version` must start with `25.`.
/// * `# rows` must equal the number of rows found.
/// * `# public-methods` (class) must equal a recount of the public,
///   non-`<init>`/`<clinit>` `METHOD` rows.
/// * `# unqualified-exports` (module) must equal a recount of the unqualified
///   `EXPORTS` rows.
/// * every row must have exactly four fields and a kind legal for its
///   baseline kind.
pub(crate) fn parse(text: &'static str) -> Baseline {
    let mut class = "";
    let mut kind = "";
    let mut java_version = "";
    let mut format = "";
    let mut columns = "";
    let mut declared_public_methods: Option<usize> = None;
    let mut declared_unqualified_exports: Option<usize> = None;
    let mut declared_rows: Option<usize> = None;
    let mut rows: Vec<Row> = Vec::new();

    // `str::lines` already splits on "\r\n"; the extra trim covers a lone '\r'
    // and costs nothing. NOM E32-1 pins these files to LF, but a checkout made
    // before that lands is CRLF on Windows and this parser must not care.
    for (lineno, line) in text.lines().map(|l| l.trim_end_matches('\r')).enumerate() {
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let mut kv = rest.splitn(2, '\t');
            let k = kv.next().unwrap_or("");
            let v = kv.next().unwrap_or("");
            match k {
                "jdk-baseline" => format = v,
                "kind" => kind = v,
                "name" => class = v,
                "java.version" => java_version = v,
                "columns" => columns = v,
                "public-methods" => declared_public_methods = v.parse().ok(),
                "unqualified-exports" => declared_unqualified_exports = v.parse().ok(),
                "rows" => declared_rows = v.parse().ok(),
                _ => {}
            }
            continue;
        }
        let fields: Vec<&'static str> = line.split('\t').collect();
        assert_eq!(
            fields.len(),
            4,
            "line {} of the {class:?} baseline has {} tab-separated fields, not 4: {line:?}. \
             No field in this format can contain a tab — a descriptor, a binary name, a \
             package name and a module name are each drawn from alphabets that exclude it — \
             so this is a malformed file, not an escaping problem.",
            lineno + 1,
            fields.len()
        );
        rows.push(Row {
            kind: fields[0],
            name: fields[1],
            descriptor: fields[2],
            flags: fields[3],
        });
    }

    assert_eq!(
        format, FORMAT_VERSION,
        "unknown jdk-baseline format version {format:?}. The row grammar changed under this \
         parser; read \
         docs/known-issues/jdk-only/E32-R11-JDK-BASELINE-CAPABILITY-20260813.md §2 before \
         widening this check."
    );
    assert!(
        !class.is_empty() && !rows.is_empty(),
        "empty or headerless baseline. An include_str! that resolved to nothing would \
         otherwise audit a population of zero and pass — which is the failure shape this \
         whole module exists to remove, not to reproduce."
    );
    assert!(
        java_version.starts_with(JAVA_VERSION_PIN),
        "{class}: this baseline was taken on java.version {java_version:?}, not a \
         {JAVA_VERSION_PIN}x. Regenerating on a newer JDK silently re-baselines every guard \
         that reads it; make the bump a decision, not a side effect."
    );
    assert_eq!(
        Some(rows.len()),
        declared_rows,
        "{class}: the file's own `# rows` header says {declared_rows:?} and this parser \
         found {}. The file was hand-edited or truncated. Never hand-edit a row: run \
         `python scripts/jdk-baseline/generate.py --update`.",
        rows.len()
    );

    let (expected_columns, legal_kinds) = match kind {
        "class" => (CLASS_COLUMNS, CLASS_ROW_KINDS),
        "module" => (MODULE_COLUMNS, MODULE_ROW_KINDS),
        other => {
            panic!("{class}: unknown `# kind` {other:?}. This parser knows `class` and `module`.")
        }
    };
    assert_eq!(
        columns, expected_columns,
        "{class}: `# columns` is {columns:?}, not {expected_columns:?}. The column order \
         changed without the `# jdk-baseline` version changing, so this parser would read \
         descriptors as names and never say so."
    );
    for r in &rows {
        assert!(
            legal_kinds.contains(&r.kind),
            "{class}: row kind {:?} is not one of {legal_kinds:?} for a `{kind}` baseline. A \
             kind this parser does not know is a row it silently ignores.",
            r.kind
        );
    }

    let b = Baseline {
        class,
        kind,
        java_version,
        rows,
    };
    match kind {
        "class" => assert_eq!(
            Some(b.public_methods().len()),
            declared_public_methods,
            "{class}: the file's own `# public-methods` header says \
             {declared_public_methods:?} and a recount of its rows says {}. Two programs \
             counting the same file must agree; one of them is wrong.",
            b.public_methods().len()
        ),
        "module" => assert_eq!(
            Some(b.unqualified_exports().len()),
            declared_unqualified_exports,
            "{class}: the file's own `# unqualified-exports` header says \
             {declared_unqualified_exports:?} and a recount of its rows says {}.",
            b.unqualified_exports().len()
        ),
        _ => unreachable!("kind already validated above"),
    }
    b
}

impl Baseline {
    /// Public, non-constructor methods, as the `(name, descriptor)` pair
    /// `NativeMethodRegistry::find` is keyed by.
    ///
    /// Bridges are included: a bridge is a real method-table entry and a real
    /// dispatch target this VM must answer for, and excluding them would make
    /// this disagree with `javap` — the instrument the two known-answer counts
    /// in `generate.py` were taken with.
    pub(crate) fn public_methods(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| {
                r.kind == "METHOD"
                    && r.has_flag("public")
                    && r.name != "<init>"
                    && r.name != "<clinit>"
            })
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    /// Public methods **and** public constructors — the population a native
    /// registrar can plausibly cover, since `<init>` is a registrable name.
    ///
    /// This is the denominator [`audit`] uses.
    pub(crate) fn public_surface(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.has_flag("public") && r.name != "<clinit>")
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    /// Whether this class declares a method with exactly this name and
    /// descriptor, at any access level.
    pub(crate) fn declares(&self, name: &str, descriptor: &str) -> bool {
        self.rows
            .iter()
            .any(|r| r.kind == "METHOD" && r.name == name && r.descriptor == descriptor)
    }

    /// Every descriptor this class declares under `name`, at any access level.
    ///
    /// Used by [`audit`] to tell "you spelled the descriptor wrong" apart from
    /// "this member does not exist" — two very different repairs.
    pub(crate) fn descriptors_named(&self, name: &str) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.name == name)
            .map(|r| r.descriptor)
            .collect()
    }

    /// Every transitive supertype, so "not declared here" can be told apart
    /// from "not in the JDK at all". `javap -public` never lists an inherited
    /// member, so a guard reading only the leaf class reads one as *absent*.
    pub(crate) fn supertypes(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "SUPERTYPE")
            .map(|r| r.name)
            .collect()
    }

    /// The unqualified `exports` of a module baseline, in binary form
    /// (`java.util.stream`).
    ///
    /// **`JAVA_BASE_EXPORTS` (`jdk25_language.rs:32`) spells packages in
    /// internal form (`java/util/stream`).** A guard reading one against the
    /// other must map; see [`Self::unqualified_exports_internal`].
    pub(crate) fn unqualified_exports(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "EXPORTS" && r.flags.is_empty())
            .map(|r| r.name)
            .collect()
    }

    /// [`Self::unqualified_exports`] with `.` rewritten to `/`, which is the
    /// form every package constant in this crate uses.
    pub(crate) fn unqualified_exports_internal(&self) -> Vec<String> {
        self.unqualified_exports()
            .into_iter()
            .map(|p| p.replace('.', "/"))
            .collect()
    }

    /// The class name in internal form — `java/util/concurrent/Flow$Publisher`
    /// — which is how `NativeMethodRegistry` is keyed.
    ///
    /// **Assert this against the registrar's own class constant.** That single
    /// line is what would have caught `StructuredTaskScope$Config`: a class
    /// name a hand list invented cannot have a baseline, because the generator
    /// refuses to write one for a type not in the runtime image. See
    /// `kind_four_would_have_caught_structured_task_scope_config` below.
    pub(crate) fn internal_name(&self) -> String {
        self.class.replace('.', "/")
    }
}

/// One triage row: `(name, descriptor, expect_registered, reason)`.
///
/// `reason` must be non-empty when `expect_registered` is `false` and empty
/// when it is `true` — an absence with no stated reason is an omission wearing
/// a record's clothes, and a reason on a present registration would make a
/// non-empty `reason` stop meaning "absent".
pub(crate) type Triage = (&'static str, &'static str, bool, &'static str);

/// The two-way ratchet. Returns every disagreement; an empty vec is the pass.
///
/// Four failure kinds, all four required. A guard that reports only kinds 1
/// and 2 cannot notice a gap that closed or a row that rotted, which is how
/// E20 found three of six `already_triaged` rows granting standing approval
/// for defects that were already fixed.
///
/// `registered` is called once per triage row and answers "does this VM have a
/// native for this `(name, descriptor)` on the class the caller has in mind" —
/// usually `|n, d| r.find(CLASS, n, d).is_some()`. It is deliberately *not*
/// given the class name: the caller owns that constant, and the caller must
/// pin it with `assert_eq!(baseline.internal_name(), CLASS)`.
pub(crate) fn audit(
    baseline: &Baseline,
    triage: &[Triage],
    registered: impl Fn(&str, &str) -> bool,
) -> Vec<String> {
    assert_eq!(
        baseline.kind, "class",
        "audit() takes a `# kind class` baseline; {} is a `{}` baseline. For a module, \
         compare Baseline::unqualified_exports() directly.",
        baseline.class, baseline.kind
    );

    let mut problems: Vec<String> = Vec::new();
    let jdk: BTreeSet<(&'static str, &'static str)> =
        baseline.public_surface().into_iter().collect();
    let mut seen: BTreeSet<(&'static str, &'static str)> = BTreeSet::new();
    let class = baseline.class;

    for &(name, descriptor, expect, reason) in triage {
        if !seen.insert((name, descriptor)) {
            problems.push(format!(
                "DUPLICATE row: {class}.{name}{descriptor} appears twice. Two rows for one \
                 member means one of them is unread, and which one is unread depends on \
                 iteration order."
            ));
            continue;
        }
        // Kind 4 — the row rotted. Three sub-cases, because they need three
        // different repairs and a single message would hide which one applies.
        if !jdk.contains(&(name, descriptor)) {
            let others = baseline.descriptors_named(name);
            problems.push(if !others.is_empty() {
                format!(
                    "STALE row (descriptor): {class}.{name}{descriptor} — this JDK \
                     ({}) declares `{name}` only as {}. The name is right and the \
                     descriptor is not, so a registration keyed on this triple can never \
                     be found by a real call.",
                    baseline.java_version,
                    others.join(" / ")
                )
            } else if baseline.declares(name, descriptor) {
                format!(
                    "STALE row (not public): {class}.{name}{descriptor} is declared but is \
                     not public on java.version {}, so it is not part of the surface this \
                     census covers.",
                    baseline.java_version
                )
            } else {
                format!(
                    "STALE row: {class}.{name}{descriptor} is not a public member of that \
                     class on java.version {}. Delete the row — a triage row for a member \
                     the JDK does not have can never be satisfied and can never be \
                     noticed. If you meant a member inherited from {}, baseline the type \
                     that declares it; `javap -public` on a leaf class never lists one.",
                    baseline.java_version,
                    if baseline.supertypes().is_empty() {
                        "a supertype".to_string()
                    } else {
                        baseline.supertypes().join(", ")
                    }
                )
            });
            continue;
        }
        match (expect, registered(name, descriptor)) {
            // Kind 2 — a registration was dropped.
            (true, false) => problems.push(format!(
                "DROPPED: {class}.{name}{descriptor} is recorded as registered and is not. \
                 A registration was lost."
            )),
            // Kind 3 — the gap closed and the record did not.
            (false, true) => problems.push(format!(
                "CLOSED: {class}.{name}{descriptor} is registered now, but this row still \
                 records it as absent (\"{reason}\"). Flip the row to `true` and delete the \
                 reason. A closed gap recorded as open is standing permission."
            )),
            (false, false) if reason.trim().is_empty() => problems.push(format!(
                "UNJUSTIFIED: {class}.{name}{descriptor} is recorded as absent with no \
                 reason. Say whether it is unimplemented or deliberately out of scope; an \
                 unexplained `false` is an omission, not a record."
            )),
            (true, true) if !reason.trim().is_empty() => problems.push(format!(
                "NOISY: {class}.{name}{descriptor} is registered; its `reason` field must \
                 be empty so a non-empty reason always means an absence."
            )),
            _ => {}
        }
    }

    // Kind 1 — the JDK declares it and this guard has never considered it.
    for &(name, descriptor) in &jdk {
        if !seen.contains(&(name, descriptor)) {
            problems.push(format!(
                "UNCOVERED: the JDK declares {class}.{name}{descriptor} and this census has \
                 no row for it. Register it, or add a row with `false` and a reason. This \
                 is the direction that made every guard in E25 §3 a restatement: a \
                 population transcribed from the registrar cannot contain a method the \
                 registrar never had."
            ));
        }
    }

    problems.sort();
    problems
}

// ===========================================================================
// The parser's own tests.
//
// E25 §4.4 names the four parts of a guard that is not itself a restatement:
// a dead-row check, a scanner self-test on synthetic input, an anti-vacuity
// floor, and a PLANTED BYPASS — "what would make this red?" written as
// executable code, which is the only answer to that question that cannot go
// stale. All four are below.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// `parse` takes `&'static str` because a baseline is an `include_str!`.
    /// A planted mutant is built at runtime, so it is leaked; a test process
    /// that leaks a few kilobytes and then exits is not a leak worth a design.
    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    /// Run `parse` on a deliberately corrupted baseline and return the panic
    /// message. Fails the test if `parse` *accepts* the corruption — that is
    /// the planted bypass, and a parser that shrugs at it is the defect.
    fn parse_must_reject(what: &str, text: &'static str) -> String {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(|| {
            let b = parse(text);
            b.rows.len()
        });
        std::panic::set_hook(hook);
        match outcome {
            Ok(n) => panic!(
                "parse() ACCEPTED a baseline corrupted by {what}, returning {n} rows. The \
                 self-test that was supposed to catch it does not."
            ),
            Err(e) => e
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_default(),
        }
    }

    /// Both directions: every `jdk25-*.tsv` on disk is included here, and every
    /// include names a file that is still on disk.
    ///
    /// The second half is the one that decays. A `STALE` include would fail to
    /// compile, so what this really guards is the first: a baseline generated
    /// into `scripts/baselines/` that nothing reads is a file that can never
    /// go red, which is `generate.py --check`'s `STALE` classification seen
    /// from the Rust side.
    #[test]
    fn every_checked_in_baseline_is_wired_in_and_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("baselines");
        let on_disk: BTreeSet<String> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("jdk25-") && n.ends_with(".tsv"))
            .collect();

        // Anti-vacuity: an empty or near-empty listing must not read as
        // agreement. Thirty were checked in on 2026-08-13.
        assert!(
            on_disk.len() >= 30,
            "found only {} jdk25-*.tsv in {} — this census is measuring its own reach, not \
             the baselines. `[reach≠defect]`",
            on_disk.len(),
            dir.display()
        );

        let included: BTreeSet<String> = ALL.iter().map(|(n, _)| (*n).to_string()).collect();
        assert_eq!(
            ALL.len(),
            included.len(),
            "ALL has a duplicate file name; one of the two consts is unread."
        );

        let missing: Vec<&String> = on_disk.difference(&included).collect();
        assert!(
            missing.is_empty(),
            "these baselines are checked in and no const includes them, so nothing can ever \
             read them and `--check` will keep regenerating them for nobody: {missing:?}. Add \
             an include_str! and an ALL row, or delete the file and its classes.txt entry."
        );
        let dead: Vec<&String> = included.difference(&on_disk).collect();
        assert!(
            dead.is_empty(),
            "ALL names files that are not on disk: {dead:?}"
        );

        // Parsing each one runs every header self-test in `parse` against every
        // checked-in file — the cheapest place to catch a hand edit.
        let mut total_rows = 0usize;
        for &(name, text) in ALL {
            let b = parse(text);
            assert!(!b.class.is_empty(), "{name}: parsed to an empty class name");
            total_rows += b.rows.len();
        }
        assert!(
            total_rows > 1_000,
            "the thirty baselines parsed to only {total_rows} rows in total; a truncated \
             include would look exactly like this."
        );
    }

    /// The known-answer counts, carried across the language boundary.
    ///
    /// `generate.py`'s KAT asserts these against `javap`, an instrument
    /// different from the class-file reader under test. This asserts the same
    /// two numbers again from Rust, through a *third* implementation — this
    /// parser's own recount — so a header, a Python counter and a Rust counter
    /// must all three agree. E32 §4 has the `javap` transcripts.
    #[test]
    fn the_two_hand_measured_counts_survive_the_rust_parser() {
        assert_eq!(
            parse(MAC).public_methods().len(),
            17,
            "javax.crypto.Mac: `javap -public javax.crypto.Mac | grep -c '('` = 17 on \
             openjdk 25.0.3+9 (E25 §1.1, E32 §4)."
        );
        assert_eq!(
            parse(CHARACTER).public_methods().len(),
            96,
            "java.lang.Character: `javap -public` prints 97 lines, one of which is the \
             public deprecated constructor, so 96 public methods — including the \
             `compareTo(Ljava/lang/Object;)I` bridge, which is a real method-table entry."
        );
        // The denominator E32-4's rewrite is built on, pinned here so it cannot
        // move silently: 17 public methods + 3 public constructors.
        assert_eq!(parse(SUBMISSION_PUBLISHER).public_surface().len(), 20);
    }

    /// The JDK-version pin, and the E25 §5 step 4 residual it closes.
    #[test]
    fn every_baseline_is_pinned_to_this_jdk() {
        for &(name, text) in ALL {
            let b = parse(text);
            assert!(
                b.java_version.starts_with(JAVA_VERSION_PIN),
                "{name} was taken on {}",
                b.java_version
            );
        }
    }

    // --- planted bypasses: five corruptions the parser must refuse ----------

    #[test]
    fn a_deleted_row_is_caught_by_the_rows_header() {
        let mutant = leak(
            MAC.lines()
                .filter(|l| !l.starts_with("METHOD\treset\t"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let msg = parse_must_reject("deleting one METHOD row", mutant);
        assert!(msg.contains("`# rows` header"), "wrong diagnosis: {msg}");
    }

    #[test]
    fn a_row_edited_in_place_is_caught_by_the_public_methods_header() {
        // Row count unchanged — only the flags move. This is the corruption
        // the `# rows` check cannot see, which is why there are two counts.
        let mutant = leak(MAC.replace(
            "METHOD\treset\t()V\tpublic,final",
            "METHOD\treset\t()V\tprotected,final",
        ));
        let msg = parse_must_reject("demoting a public method to protected", mutant);
        assert!(
            msg.contains("`# public-methods` header"),
            "wrong diagnosis: {msg}"
        );
    }

    #[test]
    fn a_baseline_regenerated_on_a_later_jdk_is_refused() {
        let mutant = leak(MAC.replace("# java.version\t25.0.3", "# java.version\t26.0.1"));
        let msg = parse_must_reject("a JDK 26 regeneration", mutant);
        assert!(msg.contains("26.0.1"), "wrong diagnosis: {msg}");
    }

    #[test]
    fn a_truncated_include_is_refused_rather_than_auditing_nothing() {
        let header_only = leak(
            MAC.lines()
                .take_while(|l| l.starts_with("# "))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        parse_must_reject("truncating the file to its header", header_only);
        parse_must_reject("an include_str! that resolved to nothing", "");
    }

    #[test]
    fn a_reordered_column_grammar_is_refused() {
        let mutant = leak(MAC.replace(
            "# columns\tkind\tname\tdescriptor\tflags",
            "# columns\tkind\tname\tflags\tdescriptor",
        ));
        let msg = parse_must_reject("swapping two columns without bumping the version", mutant);
        assert!(msg.contains("`# columns`"), "wrong diagnosis: {msg}");
    }

    // --- the ratchet: one test per failure kind ----------------------------

    /// A registrar that has exactly `getAlgorithm` and `reset`, for the four
    /// synthetic-input tests below.
    fn two_of_macs_methods(name: &str, _descriptor: &str) -> bool {
        name == "getAlgorithm" || name == "reset"
    }

    #[test]
    fn kind_one_uncovered_a_jdk_member_with_no_row() {
        let b = parse(MAC);
        let problems = audit(&b, &[("reset", "()V", true, "")], two_of_macs_methods);
        // 17 public methods + 1 protected ctor(excluded) → 17 in the surface;
        // one is covered, so sixteen are not.
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.starts_with("UNCOVERED"))
                .count(),
            16,
            "{problems:#?}"
        );
    }

    #[test]
    fn kind_two_dropped_a_registration_that_is_gone() {
        let b = parse(MAC);
        let problems = audit(&b, &[("doFinal", "()[B", true, "")], two_of_macs_methods);
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("DROPPED: javax.crypto.Mac.doFinal()[B")),
            "{problems:#?}"
        );
    }

    #[test]
    fn kind_three_closed_a_gap_the_record_still_calls_open() {
        let b = parse(MAC);
        let problems = audit(
            &b,
            &[("reset", "()V", false, "believed unregistered")],
            two_of_macs_methods,
        );
        let hit = problems
            .iter()
            .find(|p| p.starts_with("CLOSED"))
            .unwrap_or_else(|| panic!("{problems:#?}"));
        assert!(
            hit.contains("standing permission"),
            "the kind-3 message must say what a stale `false` row IS: {hit}"
        );
    }

    #[test]
    fn kind_four_stale_a_row_naming_a_member_this_jdk_does_not_declare() {
        let b = parse(MAC);
        let problems = audit(
            &b,
            &[("getInstanceStrong", "()Ljavax/crypto/Mac;", true, "")],
            two_of_macs_methods,
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("STALE row: javax.crypto.Mac")),
            "{problems:#?}"
        );
    }

    #[test]
    fn an_unexplained_false_row_and_a_noisy_true_row_are_both_refused() {
        let b = parse(MAC);
        let problems = audit(
            &b,
            &[
                ("doFinal", "()[B", false, "   "),
                ("reset", "()V", true, "some reason"),
            ],
            two_of_macs_methods,
        );
        assert!(
            problems.iter().any(|p| p.starts_with("UNJUSTIFIED")),
            "{problems:#?}"
        );
        assert!(
            problems.iter().any(|p| p.starts_with("NOISY")),
            "{problems:#?}"
        );
    }

    #[test]
    fn a_duplicated_triage_row_is_refused() {
        let b = parse(MAC);
        let problems = audit(
            &b,
            &[("reset", "()V", true, ""), ("reset", "()V", false, "x")],
            two_of_macs_methods,
        );
        assert!(
            problems.iter().any(|p| p.starts_with("DUPLICATE")),
            "{problems:#?}"
        );
    }

    /// **The argument for this whole capability, re-enacted against real data.**
    ///
    /// `jdk25_concurrency.rs:1524` defines
    /// `CLS_CONFIG = "java/util/concurrent/StructuredTaskScope$Config"`;
    /// `s52_class_name_config` (`:5103`) asserts that spelling and
    /// `s52_total_registration_count` (`:5130-5135`) counts six methods on it.
    /// **There is no such type in the JDK 25 runtime image** — the generator
    /// refused it on its first run (E32 §8), and the real nested type is
    /// `$Configuration`. A hand-maintained list cannot report that one of its
    /// own names is not a JDK name.
    ///
    /// This test takes those six triples verbatim out of the working tree,
    /// pretends every one of them is registered (which it is), and audits them
    /// against the `$Configuration` baseline. **All six come back STALE**, and
    /// three of them come back with the sharper `STALE row (descriptor)`
    /// diagnosis, because the fabricated name is baked into their return types
    /// as well: the JDK's `withName` returns `…$Configuration`, not `…$Config`.
    /// The three real members come back `UNCOVERED`.
    ///
    /// Kind 4 catches this twice over. The blunt way is here. The blunter way
    /// is that a guard converted to read a baseline for `$Config` **does not
    /// compile**: `include_str!` cannot resolve a file the generator refuses to
    /// write. Either way the name is checked against the image instead of
    /// against the code that invented it.
    #[test]
    fn kind_four_would_have_caught_structured_task_scope_config() {
        let b = parse(STRUCTURED_TASK_SCOPE_CONFIGURATION);
        assert_eq!(
            b.internal_name(),
            "java/util/concurrent/StructuredTaskScope$Configuration",
            "the one-line pin every converted guard should carry: \
             assert_eq!(baseline.internal_name(), CLS_CONFIG). Against \
             jdk25_concurrency.rs's CLS_CONFIG (\"…$Config\") this line alone is red."
        );

        // Verbatim from jdk25_concurrency.rs:5130-5135, the six the tree
        // registers on the fabricated class.
        const AS_THE_TREE_HAS_THEM: &[Triage] = &[
            ("<init>", "()V", true, ""),
            (
                "withName",
                "(Ljava/lang/String;)Ljava/util/concurrent/StructuredTaskScope$Config;",
                true,
                "",
            ),
            (
                "withThreadFactory",
                "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/StructuredTaskScope$Config;",
                true,
                "",
            ),
            (
                "withTimeout",
                "(Ljava/time/Duration;)Ljava/util/concurrent/StructuredTaskScope$Config;",
                true,
                "",
            ),
            ("getName", "()Ljava/lang/String;", true, ""),
            (
                "getThreadFactory",
                "()Ljava/util/concurrent/ThreadFactory;",
                true,
                "",
            ),
        ];

        let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
        let stale = problems.iter().filter(|p| p.starts_with("STALE")).count();
        assert_eq!(
            stale, 6,
            "all six registrations name members java.util.concurrent.StructuredTaskScope$\
             Configuration does not have: {problems:#?}"
        );
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.starts_with("STALE row (descriptor)"))
                .count(),
            3,
            "withName/withThreadFactory/withTimeout exist under the right NAME and the \
             wrong descriptor — the fabricated `$Config` is in their return types too: \
             {problems:#?}"
        );
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.starts_with("UNCOVERED"))
                .count(),
            3,
            "and the three real members are covered by nothing: {problems:#?}"
        );
    }

    /// The module accessors, and the internal/binary spelling trap E32 §5
    /// warns about.
    #[test]
    fn the_module_baseline_carries_the_real_export_denominator() {
        let b = parse(MODULE_JAVA_BASE);
        assert_eq!(b.kind, "module");
        assert_eq!(
            b.unqualified_exports().len(),
            58,
            "`java --describe-module java.base` reports 58 unqualified exports on \
             openjdk 25.0.3+9. `jdk25_language.rs:834` \
             test_java_base_exports_has_14_entries asserts 14 as fact (E25 row 13)."
        );
        assert!(b.unqualified_exports().contains(&"java.util.stream"));
        assert!(b
            .unqualified_exports_internal()
            .contains(&"java/util/stream".to_string()));
    }

    /// `audit` must refuse a module baseline rather than compute an empty
    /// public surface and report every triage row as stale.
    #[test]
    fn audit_refuses_a_module_baseline() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(|| {
            let b = parse(MODULE_JAVA_BASE);
            audit(&b, &[], |_, _| false).len()
        });
        std::panic::set_hook(hook);
        assert!(
            outcome.is_err(),
            "audit() accepted a module baseline; its public surface is empty, so every row \
             would read as STALE and an empty triage would read as a pass."
        );
    }
}
