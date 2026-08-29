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
//! # Two questions, two populations (format version 2, F23-1)
//!
//! Version 1 of the baselines emitted a member only if it was `public` or
//! `protected`. That is structurally blind to package-private and private
//! members — **the access level most JDK natives live at** — so auditing a
//! *native* registrar against it audited the one population the oracle omits.
//! It cost a false positive and a false negative on the first class anybody
//! pointed `javap -p` at (`jdk.internal.misc.CDS`): a real `private static
//! native logLambdaFormInvoker(String)` read as off-surface, and two real,
//! unregistered natives (`getCDSConfigStatus()I`,
//! `needsClassInitBarrier0(Class)Z`) could not be reported at all. Across the
//! 32 baselines the filter hid **528 of 1908 rows — 27.7% of the surface**.
//!
//! Version 2 emits everything. That makes "does this member exist" and "is this
//! member public" two different questions, and **this module answers them from
//! two different populations on purpose**:
//!
//! | question | accessor | used by |
//! |---|---|---|
//! | is this a real, dispatchable member? | [`Baseline::declares`], [`Baseline::declared_surface`] | [`audit`] kind 4, [`audit_off_surface`] kind 5 |
//! | must every member of this have a row? | [`Baseline::public_surface`] | [`audit`] kind 1 (`UNCOVERED`) |
//!
//! **The `UNCOVERED` denominator was deliberately NOT widened.** Widening it
//! would demand a triage row for each of those 528 members before any converted
//! guard could go green — a different and much larger job than the one this
//! change is. The cost of not widening it is stated plainly so nobody reads
//! silence as coverage: **[`audit`] still cannot report a missing *private*
//! native.** [`Baseline::native_surface`] is the opt-in for guards that want
//! exactly that population — 8 methods on `CDS`, 5 on `java.lang.Module`, not
//! 528 members.
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
// All 32 checked-in baselines are wired in, not just the ones a guard
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
/// `sun.management.ManagementFactoryHelper` — the declared owner of
/// `cds.rs`'s `getCDSMetrics()Lsun/management/CDSMetrics;`. It declares 22
/// public methods and **`getCDSMetrics` is not one of them**; see
/// `management_factory_helper_does_not_declare_get_cds_metrics`.
pub(crate) const SUN_MANAGEMENT_FACTORY_HELPER: &str =
    include_str!("../../scripts/baselines/jdk25-sun.management.ManagementFactoryHelper.tsv");
/// `sun.reflect.ReflectionFactory` (module `jdk.unsupported`) — the twin of
/// [`REFLECTION_FACTORY`], and the class two of the four triples in
/// `lib.rs::essential_path_does_not_override_reflection_factory_serialization`
/// name. The two types are **not** the same surface: this one has 14 public
/// methods to the `jdk.internal` type's 25, and its
/// `newOptionalDataExceptionForSerialization` takes a `Z` and returns an
/// `OptionalDataException` where the `jdk.internal` one takes nothing and
/// returns a `Constructor`.
pub(crate) const SUN_REFLECTION_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-sun.reflect.ReflectionFactory.tsv");
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
    (
        "jdk25-sun.management.ManagementFactoryHelper.tsv",
        SUN_MANAGEMENT_FACTORY_HELPER,
    ),
    (
        "jdk25-sun.reflect.ReflectionFactory.tsv",
        SUN_REFLECTION_FACTORY,
    ),
];

/// The `# jdk-baseline` format version this parser understands.
///
/// `2` (F23-1) is the all-access population. A version-1 file carries only
/// public and protected members, and this parser must refuse it rather than
/// answer [`Baseline::declares`] from a set narrower than the caller expects —
/// which is exactly the silent narrowing that produced the CDS false positive.
const FORMAT_VERSION: &str = "2";

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
    /// `public,protected,private,static,final,synchronized,bridge,varargs,native,abstract,strictfp,synthetic`.
    ///
    /// Package-private members carry **no** access token at all, which is why
    /// [`Row::is_public`] is a positive test and there is no `is_package_private`
    /// written as `!is_private()`.
    pub flags: &'static str,
}

impl Row {
    pub(crate) fn has_flag(&self, f: &str) -> bool {
        self.flags.split(',').any(|x| x == f)
    }

    /// `true` only for `ACC_PUBLIC`. Protected, private and package-private all
    /// answer `false`.
    pub(crate) fn is_public(&self) -> bool {
        self.has_flag("public")
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
/// * `# declared-methods` (class, v2) must equal a recount of **all** non-
///   `<clinit>` `METHOD` rows, at every access level. This is the count that
///   would have caught the version-1 defect: a generator that reverted to the
///   public filter still reproduces `# public-methods` exactly.
/// * `# declared-fields` (class, v2) must equal a recount of the `FIELD` rows.
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
    let mut declared_all_methods: Option<usize> = None;
    let mut declared_all_fields: Option<usize> = None;
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
                "declared-methods" => declared_all_methods = v.parse().ok(),
                "declared-fields" => declared_all_fields = v.parse().ok(),
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
        "class" => {
            assert_eq!(
                Some(b.public_methods().len()),
                declared_public_methods,
                "{class}: the file's own `# public-methods` header says \
                 {declared_public_methods:?} and a recount of its rows says {}. Two programs \
                 counting the same file must agree; one of them is wrong.",
                b.public_methods().len()
            );
            // The v2 counts. These are the ones that notice a generator quietly
            // reverting to the public/protected filter: `# public-methods` is
            // identical either way, so it cannot.
            let all_methods = b
                .rows
                .iter()
                .filter(|r| r.kind == "METHOD" && r.name != "<clinit>")
                .count();
            assert_eq!(
                Some(all_methods),
                declared_all_methods,
                "{class}: the file's own `# declared-methods` header says \
                 {declared_all_methods:?} and a recount of its rows says {all_methods}. A \
                 MISSING header here means a format-version-1 file — one that lists only \
                 public and protected members — reached a parser that answers \
                 `declares()` as though it were the whole class."
            );
            let all_fields = b.rows.iter().filter(|r| r.kind == "FIELD").count();
            assert_eq!(
                Some(all_fields),
                declared_all_fields,
                "{class}: the file's own `# declared-fields` header says \
                 {declared_all_fields:?} and a recount of its rows says {all_fields}."
            );
            // NO per-file "this baseline must contain a non-public member"
            // assertion, and the reason is measured rather than assumed: nine of
            // the 32 baselines legitimately have none. `Flow$Publisher`,
            // `BlockingQueue`, `RuntimeMXBean`, `SSLSession` and the four
            // `StructuredTaskScope` types are interfaces whose every member is
            // implicitly public, and `module-java.base` has no members at all.
            // A per-file floor would have been red on all nine. The corpus-level
            // floor that DOES bite lives in
            // `the_widened_population_is_present_and_is_not_a_rounding_error`.
        }
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

    /// Every method this class declares **at any access level**, except
    /// `<clinit>` — the population a native registration can legitimately be
    /// keyed on, because a private native is dispatched from the JDK's own
    /// bytecode exactly like a public one.
    ///
    /// This is [`audit_off_surface`]'s reachability test, and it is the half of
    /// the version-2 widening that changes a verdict: under version 1 a
    /// registration on `CDS.logLambdaFormInvoker(Ljava/lang/String;)V` — a real
    /// `private static native` — was reported `OFF-SURFACE`, and acting on that
    /// report would have deleted the only registration for the one overload
    /// that has no bytecode to fall back to.
    ///
    /// `<clinit>` is excluded because nothing can register a native for it: it
    /// is invoked by the VM, never named by a `NativeRegistry` key a caller
    /// could produce.
    pub(crate) fn declared_surface(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.name != "<clinit>")
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    /// Every `native` method this class declares, at any access level.
    ///
    /// **This is the population a native registrar should be censused against**,
    /// and version 1 could not express it: 5 of `CDS`'s 8 natives and all 5 of
    /// `java.lang.Module`'s are private, so a public-only baseline reported 3 and
    /// 0. It is deliberately much smaller than [`Self::declared_surface`] — 13
    /// methods across all 32 baselines — so a guard can be two-way over it
    /// without anybody first writing 538 triage rows.
    pub(crate) fn native_surface(&self) -> Vec<(&'static str, &'static str)> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.has_flag("native"))
            .map(|r| (r.name, r.descriptor))
            .collect()
    }

    /// Whether this class declares a method with exactly this name and
    /// descriptor, at any access level.
    ///
    /// The doc said "at any access level" under version 1 too, and it was not
    /// true: the file it read had no private or package-private rows in it. The
    /// sentence is now backed by the data.
    pub(crate) fn declares(&self, name: &str, descriptor: &str) -> bool {
        self.rows
            .iter()
            .any(|r| r.kind == "METHOD" && r.name == name && r.descriptor == descriptor)
    }

    /// The flags of the method with exactly this name and descriptor, e.g.
    /// `private,static,native`. `None` if the class does not declare it.
    ///
    /// Exists so a diagnostic can say *why* a member is not on the public
    /// surface instead of implying it does not exist — the distinction F17-1
    /// had to make by hand with `javap -p`.
    pub(crate) fn flags_of(&self, name: &str, descriptor: &str) -> Option<&'static str> {
        self.rows
            .iter()
            .find(|r| r.kind == "METHOD" && r.name == name && r.descriptor == descriptor)
            .map(|r| r.flags)
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
        // Kind 4 — the row rotted.
        //
        // F23-1 changed BOTH the test and the order of its sub-cases, and the
        // order is the part that was a bug:
        //
        //  * The test is now "does this JDK declare this exact member at ANY
        //    access level", not "is it public". A triage row naming a real
        //    private native describes a real dispatch target, and demanding its
        //    deletion would be the CDS false positive with a different message
        //    on it. Non-public rows fall through to the expect/registered check
        //    below, which is where they belong.
        //  * The exact-match test now runs FIRST. It used to be second, behind
        //    `descriptors_named(name).is_empty()`, so a private member with a
        //    public overload of the same name got "the name is right and the
        //    descriptor is not" — a confident, specific and wrong diagnosis
        //    pointing at the overload. That is `CDS.logLambdaFormInvoker`
        //    exactly: one private 1-String native, one public 4-String wrapper.
        //
        // What did NOT change is `jdk`, the kind-1 denominator: it is still the
        // PUBLIC surface. See the module doc — widening it would demand a row
        // for each of 538 non-public members before any guard could go green.
        if !jdk.contains(&(name, descriptor)) && !baseline.declares(name, descriptor) {
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
            } else {
                format!(
                    "STALE row: {class}.{name}{descriptor} is not a member of that class \
                     at any access level on java.version {}. Delete the row — a triage row \
                     for a member the JDK does not have can never be satisfied and can \
                     never be noticed. If you meant a member inherited from {}, baseline \
                     the type that declares it; a leaf class's member list never shows one.",
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

/// One row of the off-surface record: `(name, descriptor, reason)`.
///
/// A registration this VM makes on the class whose `(name, descriptor)` the
/// JDK does **not** declare as a public member. See [`audit_off_surface`].
pub(crate) type OffSurface = (&'static str, &'static str, &'static str);

/// The fifth check: **the registry's own rows, audited against the JDK.**
///
/// # Why [`audit`] cannot do this
///
/// [`audit`] walks two populations — the triage rows and the JDK surface. A
/// registration that is in *neither* is invisible to all four of its kinds:
/// kind 4 fires only when somebody wrote a row for it, and the whole premise
/// of E25 is that nobody writes rows for things they are not already thinking
/// about. So `audit` catches a fabricated name in a *hand list* and misses the
/// same fabricated name in the *registrar* — which is the copy that is loaded
/// into a running VM.
///
/// This closes it from the other end. `registrations` is every `(name,
/// descriptor)` the registry holds for the class (from
/// `NativeMethodRegistry::dump_registrations`, filtered to that class);
/// `expected` is the recorded, justified set of registrations that are
/// deliberately off the public surface. Both directions fail:
///
/// | # | condition | name in the output |
/// |---|---|---|
/// | 5 | a registration the JDK does not declare **at any access level**, with no row | `OFF-SURFACE` |
/// | 6 | an `expected` row nothing registers any more | `DEAD OFF-SURFACE row` |
/// | 7a | an `expected` row the JDK declares **publicly** | `STALE OFF-SURFACE row (public)` |
/// | 7b | an `expected` row the JDK declares **non-publicly** | `STALE OFF-SURFACE row (declared, not public)` |
///
/// Kinds 6 and 7 are the same standing-permission shape as [`audit`]'s kind 3:
/// a record that says "we knowingly register something the JDK does not have"
/// must stop saying it the moment either half stops being true.
///
/// # What F23-1 changed, and why it is a narrowing of kind 5
///
/// Kind 5 used to fire for any registration outside the *public* surface. It
/// therefore fired on every native keyed on a private JDK method — which is
/// where most JDK natives are. Five of `jdk.internal.misc.CDS`'s eight natives
/// are `private static native`, and every one of them was reported as a
/// fabrication by a version-1 baseline.
///
/// Kind 7b is the price of that narrowing, paid deliberately: an exemption
/// written while the oracle was blind must now announce itself, because the row
/// says "the JDK does not have this" about something the JDK has.
///
/// Off-surface is not automatically a defect. Three of its shapes are routine
/// in this tree and each needs a different `reason`:
///
/// * **a synthetic `<init>()V`** on a class whose real constructor is absent
///   (an interface). A *private* or *protected* real constructor is no longer
///   off-surface at all — it is on the declared surface, and this is one of the
///   places the version-1 blindness fired most often.
/// * **an inherited member** — a leaf class's own member table never lists one,
///   so `getObjectName` on `RuntimeMXBean` is declared by
///   `PlatformManagedObject` and is genuinely reachable. Baseline the declaring
///   type and the row moves onto the surface.
/// * **a name this JDK does not have anywhere**, which is the `$Config` shape
///   and is always a defect: no real call can produce that key.
///
/// The reason field is what tells the next reader which of the three this is,
/// and the ratchet is what stops the answer from silently going out of date.
pub(crate) fn audit_off_surface(
    baseline: &Baseline,
    registrations: &[(&str, &str)],
    expected: &[OffSurface],
) -> Vec<String> {
    assert_eq!(
        baseline.kind, "class",
        "audit_off_surface() takes a `# kind class` baseline; {} is a `{}` baseline.",
        baseline.class, baseline.kind
    );
    let class = baseline.class;
    // F23-1: the reachability test is the DECLARED surface, not the public one.
    // A native registered on a private JDK method is dispatched by the JDK's own
    // bytecode exactly like a public one — `CDS.<clinit>` calls
    // `getCDSConfigStatus()I`, which is `private static native`. Testing against
    // `public_surface()` here reported five real, reachable CDS natives as
    // fabrications; three of them survived only because the lane acting on the
    // report re-measured with `javap -p` first.
    let surface: BTreeSet<(&str, &str)> = baseline.declared_surface().into_iter().collect();
    let public: BTreeSet<(&str, &str)> = baseline.public_surface().into_iter().collect();
    let recorded: BTreeSet<(&str, &str)> = expected.iter().map(|&(n, d, _)| (n, d)).collect();
    assert_eq!(
        recorded.len(),
        expected.len(),
        "{class}: the off-surface record has a duplicate row; one of the two is unread."
    );
    let live: BTreeSet<(&str, &str)> = registrations.iter().copied().collect();

    let mut problems: Vec<String> = Vec::new();

    // Kind 5 — this VM registers it, the JDK does not declare it, nobody said so.
    for &(name, descriptor) in &live {
        if surface.contains(&(name, descriptor)) || recorded.contains(&(name, descriptor)) {
            continue;
        }
        let others = baseline.descriptors_named(name);
        problems.push(if !others.is_empty() {
            format!(
                "OFF-SURFACE (descriptor): this VM registers {class}.{name}{descriptor}, but \
                 java.version {} declares `{name}` only as {}. A native keyed on this triple \
                 can never be found by a real call. Fix the descriptor, or record it here \
                 with a reason.",
                baseline.java_version,
                others.join(" / ")
            )
        } else {
            format!(
                "OFF-SURFACE: this VM registers {class}.{name}{descriptor} and java.version \
                 {} does not declare it AT ANY ACCESS LEVEL. If it is inherited \
                 from {}, baseline the declaring type; if it is a synthetic <init>, say so; \
                 if the JDK has no such name at all, the registration is unreachable and \
                 must go. Every case needs a row here, not silence.",
                baseline.java_version,
                if baseline.supertypes().is_empty() {
                    "a supertype".to_string()
                } else {
                    baseline.supertypes().join(", ")
                }
            )
        });
    }

    // Kinds 6 and 7 — the record outlived what it records.
    for &(name, descriptor, reason) in expected {
        if reason.trim().is_empty() {
            problems.push(format!(
                "UNJUSTIFIED OFF-SURFACE: {class}.{name}{descriptor} is recorded as a \
                 deliberate off-surface registration with no reason. Say which of the three \
                 shapes it is."
            ));
        }
        if !live.contains(&(name, descriptor)) {
            problems.push(format!(
                "DEAD OFF-SURFACE row: {class}.{name}{descriptor} is recorded here \
                 (\"{reason}\") and nothing registers it any more. Delete the row — an \
                 exemption that outlives its exception is standing permission for the next \
                 one."
            ));
        }
        // Kind 7 has two arms since F23-1, and they need opposite repairs.
        if public.contains(&(name, descriptor)) {
            problems.push(format!(
                "STALE OFF-SURFACE row (public): {class}.{name}{descriptor} IS a public \
                 member on java.version {} (\"{reason}\"). Move it into the TRIAGE table, \
                 where the four-kind ratchet covers it.",
                baseline.java_version
            ));
        } else if surface.contains(&(name, descriptor)) {
            problems.push(format!(
                "STALE OFF-SURFACE row (declared, not public): {class}.{name}{descriptor} \
                 IS declared by java.version {} as `{}` (\"{reason}\"). It is a real \
                 dispatch target, so the registration needs no exemption — DELETE THE ROW, \
                 do not delete the registration. This is the shape a format-version-1 \
                 baseline could not see: it listed only public and protected members, so \
                 every private native read as a fabrication.",
                baseline.java_version,
                baseline.flags_of(name, descriptor).unwrap_or("")
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
        // agreement. Thirty were checked in on 2026-08-13 (E32/E37); E41 added
        // `sun.reflect.ReflectionFactory` and
        // `sun.management.ManagementFactoryHelper`, the second class rows 22
        // and 3 each register on.
        assert!(
            on_disk.len() >= 32,
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
        // F23-1: this floor was `> 1_000` against a corpus of 1380 rows, and the
        // widening took the corpus to 1908 — so the old floor is now cleared by
        // 900 rows and could not notice a third of the corpus vanishing. A floor
        // a widened baseline trivially clears is not a floor. Re-derived from the
        // measured total with the same ~30% headroom the original had, which is
        // enough to absorb a JDK point release and not enough to absorb the
        // version-1 filter coming back (that would take it to 1380 — RED).
        assert!(
            total_rows >= 1_800,
            "the 32 baselines parsed to only {total_rows} rows in total; 1908 were measured \
             on openjdk 25.0.3+9. A truncated include looks exactly like this, and so does \
             a generator that reverted to the format-version-1 public/protected filter \
             (1380). See `the_widened_population_is_present_and_is_not_a_rounding_error` \
             for the floor that names the population directly."
        );
    }

    /// **The anti-vacuity floor that a widened baseline does NOT trivially
    /// clear**, and the measure of how much of the surface the version-1
    /// instrument was not looking at.
    ///
    /// `every_checked_in_baseline_is_wired_in_and_parses` counts rows, and a
    /// row count is exactly the kind of number that drifts upward until it
    /// stops meaning anything. This one counts the population version 1 could
    /// not emit, so a version-1 corpus scores **10** against a floor of 500 —
    /// red by a factor of fifty, no matter how many rows the files have.
    ///
    /// Measured on openjdk 25.0.3+9. The v1 column is `git show HEAD:` over the
    /// 30 tracked baselines (the other two were written the same day by the same
    /// version-1 generator):
    ///
    /// | | v1 | v2 |
    /// |---|---|---|
    /// | rows | 1380 | 1908 |
    /// | member rows (METHOD+FIELD) | 779 | 1307 |
    /// | non-public member rows | **10** | **538** |
    /// | ...of which `protected` | 10 | 10 |
    /// | **...of which private or package-private** | **0** | **528** |
    /// | **native methods visible** | **3** | **13** |
    ///
    /// Version 1 kept `ACC_PROTECTED` as well as `ACC_PUBLIC`, which is why the
    /// v1 figure is 10 and not 0 — and why "non-public" is the wrong word for
    /// what it hid. What it hid was every private and package-private member:
    /// 528 rows, **27.7% of the surface**, and 10 of the 13 `native` methods.
    /// That last number is the one that matters for a native registrar: the
    /// version-1 baselines could see 3 of the JDK natives on the classes they
    /// baseline.
    #[test]
    fn the_widened_population_is_present_and_is_not_a_rounding_error() {
        let mut member_rows = 0usize;
        let mut non_public = 0usize;
        let mut natives = 0usize;
        for &(_, text) in ALL {
            let b = parse(text);
            for r in &b.rows {
                if r.kind == "METHOD" || r.kind == "FIELD" {
                    member_rows += 1;
                    if !r.is_public() {
                        non_public += 1;
                    }
                }
                if r.kind == "METHOD" && r.has_flag("native") {
                    natives += 1;
                }
            }
        }
        assert!(
            member_rows >= 1_250,
            "only {member_rows} member rows across the 32 baselines; 1307 measured."
        );
        assert!(
            non_public >= 500,
            "only {non_public} of {member_rows} member rows are non-public. 538 were \
             measured, and a format-version-1 corpus scores 10 — every one of them \
             `protected`, because that filter kept ACC_PROTECTED and hid only the private \
             and package-private members. This assertion cannot be satisfied by a baseline \
             set that still carries that filter, however many rows it has. `[gate=FR]`"
        );
        assert!(
            natives >= 13,
            "only {natives} `native` methods visible across the 32 baselines; 13 measured, \
             of which 10 are non-public. A public-only corpus sees 3. Auditing a NATIVE \
             registrar against a surface that omits 10 of 13 natives is the defect F23-1 \
             fixed."
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

        // F23-1: the same three counts taken with the OTHER instrument.
        // `javap -public` is what produced the version-1 defect, so a KAT built
        // only on it agrees with a generator that drops every private member —
        // and did, for 32 files. These are `javap -p <class> | grep -c '('` on
        // the same JDK, same day (`generate.py::KNOWN_ANSWERS_ALL_ACCESS`).
        assert_eq!(parse(MAC).declared_surface().len(), 22);
        assert_eq!(parse(CHARACTER).declared_surface().len(), 104);
        assert_eq!(
            parse(CDS).declared_surface().len(),
            28,
            "13 members is what `javap -public jdk.internal.misc.CDS` prints, and 13 is what \
             every guard on this class saw until F23-1."
        );
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

    /// **The planted bypass for F23-1 itself: a version-1 file must not be read
    /// as though it were a whole class.**
    ///
    /// Two mutants, because they are two different accidents:
    ///
    /// 1. the version marker says `1`. A checkout that predates this change has
    ///    32 of these, and every one of them answers `declares()` from a
    ///    public-and-protected-only set.
    /// 2. the version marker says `2` and the *content* is version 1 — a
    ///    generator whose filter came back, with headers rewritten to match its
    ///    own truncated output. This is the one a version check alone cannot
    ///    catch, and it is why `# declared-methods` exists.
    #[test]
    fn a_version_one_baseline_is_refused_in_both_of_its_shapes() {
        let old_marker = leak(CDS.replace("# jdk-baseline\t2", "# jdk-baseline\t1"));
        let msg = parse_must_reject("a format-version-1 marker", old_marker);
        assert!(msg.contains("format version"), "wrong diagnosis: {msg}");

        // Mutant 2. Drop every non-public METHOD row — which is what the
        // version-1 generator did — and rewrite `# rows` and `# declared-methods`
        // the way that generator would have, so the only header left disagreeing
        // is the one this change added. `# public-methods` is untouched and
        // correct in BOTH files, which is the whole point: it cannot notice.
        let kept: Vec<&str> = CDS
            .lines()
            .filter(|l| {
                !l.starts_with("METHOD\t")
                    || l.split('\t')
                        .nth(3)
                        .is_some_and(|f| f.split(',').any(|x| x == "public" || x == "protected"))
            })
            .collect();
        let dropped = CDS.lines().count() - kept.len();
        assert!(
            dropped >= 15,
            "the mutant must actually remove the private members it is standing in for; it \
             removed {dropped}"
        );
        let rows = kept
            .iter()
            .filter(|l| !l.starts_with("# ") && !l.is_empty())
            .count();
        let refiltered = leak(
            kept.iter()
                .map(|l| {
                    if l.starts_with("# rows\t") {
                        format!("# rows\t{rows}")
                    } else {
                        (*l).to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let msg = parse_must_reject(
            "a generator that reverted to the public-only filter",
            refiltered,
        );
        assert!(
            msg.contains("`# declared-methods` header"),
            "the `# rows` and `# public-methods` headers were rewritten to be self-consistent, \
             exactly as a reverted generator would write them, so `# declared-methods` is the \
             only check that can fire — and it must: {msg}"
        );
    }

    /// The mutation check the widening needs in the other direction: a member
    /// that is **known present and known private** must be found by
    /// `declares`/`declared_surface` and must NOT be found by `public_surface`.
    ///
    /// Without this the widening could be a no-op — `declares` would keep
    /// answering `false` for every private member and every guard would stay
    /// green while staying wrong, which is the state the tree was in.
    #[test]
    fn a_known_private_member_is_visible_and_a_known_absent_one_is_not() {
        let b = parse(CDS);
        let public: BTreeSet<_> = b.public_surface().into_iter().collect();
        let declared: BTreeSet<_> = b.declared_surface().into_iter().collect();

        // Present, private, native. `javap -p jdk.internal.misc.CDS`:
        //   private static native int getCDSConfigStatus();
        //   private static native void logLambdaFormInvoker(java.lang.String);
        //   private static native boolean needsClassInitBarrier0(java.lang.Class<?>);
        //   private static native void dumpClassList(java.lang.String);
        //   private static native void dumpDynamicArchive(java.lang.String);
        for (n, d) in [
            ("getCDSConfigStatus", "()I"),
            ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
            ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
            ("dumpClassList", "(Ljava/lang/String;)V"),
            ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
        ] {
            assert!(b.declares(n, d), "{n}{d} is a real member of JDK 25's CDS");
            assert!(
                declared.contains(&(n, d)),
                "{n}{d} must be on the declared surface"
            );
            assert!(
                !public.contains(&(n, d)),
                "{n}{d} is private; it must NOT be on the public surface. If it is, the two \
                 populations have been collapsed into one and `audit`'s UNCOVERED denominator \
                 just grew by 538 members."
            );
            assert_eq!(b.flags_of(n, d), Some("private,static,native"));
        }

        // Absent at every access level. These are the two `cds.rs` really did
        // register on names JDK 25 does not have, and F17-1 really did remove.
        for (n, d) in [("isDumpingClassList", "()Z"), ("isSharingEnabled", "()Z")] {
            assert!(
                !b.declares(n, d) && b.descriptors_named(n).is_empty(),
                "{n}{d} is not a CDS member at ANY access level on 25.0.3+9 — the widening \
                 must not turn a real absence into a false present"
            );
        }

        // And the population that matters for a native registrar.
        assert_eq!(
            b.native_surface().len(),
            8,
            "CDS declares 8 natives; a public-only baseline showed 3 of them: {:?}",
            b.native_surface()
        );
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

    // --- the fifth check: the registry's own rows, audited against the JDK ---

    /// Kind 5, on synthetic input, with the planted bypass in both directions.
    #[test]
    fn off_surface_catches_a_registration_the_jdk_does_not_declare() {
        let b = parse(MAC);
        // `getInstanceStrong` is a `SecureRandom` method, not a `Mac` one.
        let problems = audit_off_surface(
            &b,
            &[
                ("reset", "()V"),
                ("getInstanceStrong", "()Ljavax/crypto/Mac;"),
            ],
            &[],
        );
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0]
                .contains("OFF-SURFACE: this VM registers javax.crypto.Mac.getInstanceStrong"),
            "{problems:#?}"
        );
        // Recorded with a reason, the same input is silent — and that is the
        // whole point of the reason field.
        assert!(audit_off_surface(
            &b,
            &[
                ("reset", "()V"),
                ("getInstanceStrong", "()Ljavax/crypto/Mac;")
            ],
            &[(
                "getInstanceStrong",
                "()Ljavax/crypto/Mac;",
                "deliberate, for this test"
            )],
        )
        .is_empty());
    }

    /// The sharper sub-case: right name, wrong descriptor. This is the one
    /// that reads as a plain absence if you only ever grep for the name.
    #[test]
    fn off_surface_reports_a_wrong_descriptor_as_a_wrong_descriptor() {
        let b = parse(MAC);
        let problems = audit_off_surface(&b, &[("reset", "(I)V")], &[]);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0]
                .contains("OFF-SURFACE (descriptor): this VM registers javax.crypto.Mac.reset(I)V"),
            "{problems:#?}"
        );
        assert!(problems[0].contains("()V"), "{problems:#?}");
    }

    /// Kinds 6 and 7 — the record outliving what it records. These are the two
    /// that get omitted, and they are the reason this is a ratchet and not a
    /// filter.
    #[test]
    fn an_off_surface_row_that_rotted_is_refused_in_both_directions() {
        let b = parse(MAC);
        // Kind 6: nothing registers it any more.
        let dead = audit_off_surface(&b, &[], &[("gone", "()V", "was registered once")]);
        assert!(
            dead.iter().any(|p| p.starts_with("DEAD OFF-SURFACE row")),
            "{dead:#?}"
        );
        // Kind 7: the JDK declares it publicly, so it belongs in TRIAGE.
        let stale = audit_off_surface(
            &b,
            &[("reset", "()V")],
            &[("reset", "()V", "believed not a JDK member")],
        );
        assert!(
            stale.iter().any(|p| p.starts_with("STALE OFF-SURFACE row")),
            "{stale:#?}"
        );
        // An unexplained exemption is an omission wearing a record's clothes.
        let mute = audit_off_surface(&b, &[("gone", "()V")], &[("gone", "()V", "  ")]);
        assert!(
            mute.iter()
                .any(|p| p.starts_with("UNJUSTIFIED OFF-SURFACE")),
            "{mute:#?}"
        );
    }

    // --- live re-enactments: real registrations, real baselines -------------
    //
    // Each of these takes the (class, name, descriptor) triples verbatim out
    // of a registrar in this crate and audits them against the checked-in
    // baseline for the class they are registered on. They are re-enactments,
    // not guards on those files: they prove the mechanism against real data
    // and they do NOT make the originating test red. Converting the guards is
    // E25 rows 3, 4-6 and 16, delivered as nominations in
    // docs/known-issues/jdk-only/E41-R11-TWELVE-GUARDS-CONVERTED-20260813.md.

    /// **Rows 4-6.** `shared_secrets_bridge.rs`'s `FACTORIES` is 15 rows and
    /// `all_factories_listed` asserts `FACTORIES.len() == 15` — a const against
    /// a literal copied out of that const. Two of those fifteen name a method
    /// `jdk.internal.access.SharedSecrets` does not have on JDK 25:
    ///
    /// * `getJavaSecurityAccess` — `JavaSecurityAccess` and its accessor went
    ///   with the Security Manager (JEP 486). There is no such getter; the
    ///   three `getJavaSecurity*Access` methods that DO exist are
    ///   `Properties`, `Signature` and `Spec`.
    /// * `getJavaUtilJarAccess` — the real spelling has never had the `get`
    ///   prefix. `javaUtilJarAccess()` is the method; `getJavaUtilJarAccess`
    ///   is a name this tree invented.
    ///
    /// Both are registered on `jdk/internal/access/SharedSecrets` **and** on
    /// the legacy `jdk/internal/misc/SharedSecrets` alias, so four
    /// registrations are keyed on names no real call can produce, and
    /// `every_factory_returns_access_interface` cannot say so because its
    /// whole test is `starts_with("getJava")` — which both of them pass.
    #[test]
    fn kind_four_catches_two_of_the_fifteen_shared_secrets_factories() {
        let b = parse(SHARED_SECRETS);
        assert_eq!(b.internal_name(), "jdk/internal/access/SharedSecrets");
        assert_eq!(
            b.public_surface().len(),
            65,
            "64 public methods + the public no-arg constructor. E25 rows 4-6 quote `15 of 30` \
             for the `getJava*Access` getters alone; the class a native registrar can key on \
             is 65 members wide."
        );

        // Verbatim from shared_secrets_bridge.rs:61 `FACTORIES`, as
        // `register_factories` keys them: `(method, "()" + ret_desc)`.
        #[rustfmt::skip]
        const AS_THE_TREE_HAS_THEM: &[Triage] = &[
            ("getJavaLangAccess", "()Ljdk/internal/access/JavaLangAccess;", true, ""),
            ("getJavaLangInvokeAccess", "()Ljdk/internal/access/JavaLangInvokeAccess;", true, ""),
            ("getJavaLangRefAccess", "()Ljdk/internal/access/JavaLangRefAccess;", true, ""),
            ("getJavaLangReflectAccess", "()Ljdk/internal/access/JavaLangReflectAccess;", true, ""),
            ("getJavaIOAccess", "()Ljdk/internal/access/JavaIOAccess;", true, ""),
            ("getJavaIORandomAccessFileAccess", "()Ljdk/internal/access/JavaIORandomAccessFileAccess;", true, ""),
            ("getJavaIOFileDescriptorAccess", "()Ljdk/internal/access/JavaIOFileDescriptorAccess;", true, ""),
            ("getJavaNetInetAddressAccess", "()Ljdk/internal/access/JavaNetInetAddressAccess;", true, ""),
            ("getJavaNetUriAccess", "()Ljdk/internal/access/JavaNetUriAccess;", true, ""),
            ("getJavaNioAccess", "()Ljdk/internal/access/JavaNioAccess;", true, ""),
            ("getJavaSecurityAccess", "()Ljdk/internal/access/JavaSecurityAccess;", true, ""),
            ("getJavaUtilJarAccess", "()Ljdk/internal/access/JavaUtilJarAccess;", true, ""),
            ("getJavaUtilZipFileAccess", "()Ljdk/internal/access/JavaUtilZipFileAccess;", true, ""),
            ("getJavaNetHttpCookieAccess", "()Ljdk/internal/access/JavaNetHttpCookieAccess;", true, ""),
            ("getJavaUtilResourceBundleAccess", "()Ljdk/internal/access/JavaUtilResourceBundleAccess;", true, ""),
        ];

        let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
        let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
        assert_eq!(
            stale.len(),
            2,
            "exactly two FACTORIES rows name a member JDK 25's SharedSecrets does not \
             declare: {problems:#?}"
        );
        assert!(
            stale.iter().any(|p| p.contains("getJavaSecurityAccess")),
            "{stale:#?}"
        );
        assert!(
            stale.iter().any(|p| p.contains("getJavaUtilJarAccess")),
            "{stale:#?}"
        );
        // And the real spelling is right there in the same baseline, uncovered.
        assert!(
            b.declares(
                "javaUtilJarAccess",
                "()Ljdk/internal/access/JavaUtilJarAccess;"
            ),
            "the JDK's own spelling carries no `get` prefix"
        );
        assert!(
            !b.declares(
                "getJavaSecurityAccess",
                "()Ljdk/internal/access/JavaSecurityAccess;"
            ),
            "JEP 486 took JavaSecurityAccess out; nothing declares this getter"
        );
    }

    /// **Row 3, and the retraction.** This test used to be called
    /// `kind_four_catches_five_of_the_ten_cds_registrations` and asserted
    /// `stale.len() == 5`. **Three of those five were artifacts of the
    /// version-1 baseline**, and the report they justified came within one
    /// commit of deleting three real, reachable natives.
    ///
    /// The ten triples below are `cds.rs::register_cds_natives` as E41/F8 found
    /// it. Audited against the format-version-2 baseline:
    ///
    /// | registered | v1 verdict | v2 verdict — measured with `javap -p` |
    /// |---|---|---|
    /// | `isDumpingClassList()Z` | STALE | **STALE.** No such name at any access level |
    /// | `isSharingEnabled()Z` | STALE | **STALE.** No such name; `isUsingArchive()Z` is the JDK-true spelling |
    /// | `logLambdaFormInvoker(Ljava/lang/String;)V` | STALE (descriptor) | **REAL.** `private static native`, and the public 4-String overload's whole body is `logLambdaFormInvoker(prefix+" "+holder+…)` — CDS.java:142-146 |
    /// | `dumpClassList(Ljava/lang/String;)V` | STALE | **REAL.** `private static native` |
    /// | `dumpDynamicArchive(Ljava/lang/String;)V` | STALE | **REAL.** `private static native` |
    ///
    /// The `logLambdaFormInvoker` row is the one that shows why the sub-case
    /// ORDER in `audit` mattered: the JDK declares that name twice, once
    /// privately with one parameter and once publicly with four, so the
    /// version-1 code path found the public overload and produced *"the name is
    /// right and the descriptor is not"* — a specific, confident, wrong
    /// instruction to change the registration to a body that drops three
    /// arguments and to drop the only form with no bytecode fallback.
    ///
    /// Two of five survive. `[triage=stale]`.
    #[test]
    fn kind_four_catches_two_of_the_ten_cds_registrations_not_five() {
        let b = parse(CDS);
        assert_eq!(b.internal_name(), "jdk/internal/misc/CDS");

        // Verbatim from cds.rs::register_cds_natives as of E41/F8, BEFORE
        // F17-1 edited it. Kept in that shape on purpose: this test is the
        // re-enactment of the wrong verdict, not a guard on today's file.
        #[rustfmt::skip]
        const AS_F8_FOUND_THEM: &[Triage] = &[
            ("<init>", "()V", true, ""),
            ("isDumpingClassList", "()Z", true, ""),
            ("isDumpingArchive", "()Z", true, ""),
            ("isSharingEnabled", "()Z", true, ""),
            ("initializeFromArchive", "(Ljava/lang/Class;)V", true, ""),
            ("getRandomSeedForDumping", "()J", true, ""),
            ("logLambdaFormInvoker", "(Ljava/lang/String;)V", true, ""),
            ("defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V", true, ""),
            ("dumpClassList", "(Ljava/lang/String;)V", true, ""),
            ("dumpDynamicArchive", "(Ljava/lang/String;)V", true, ""),
        ];

        let problems = audit(&b, AS_F8_FOUND_THEM, |_, _| true);
        let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
        assert_eq!(
            stale.len(),
            2,
            "two of the ten name something JDK 25 does not have, not five: {problems:#?}"
        );
        assert!(stale.iter().any(|p| p.contains("isDumpingClassList")));
        assert!(stale.iter().any(|p| p.contains("isSharingEnabled")));
        assert_eq!(
            stale
                .iter()
                .filter(|p| p.starts_with("STALE row (descriptor)"))
                .count(),
            0,
            "the single `STALE row (descriptor)` the version-1 run produced was \
             `logLambdaFormInvoker`, and it was wrong: {stale:#?}"
        );

        // The three retractions, stated as data rather than as prose.
        for (n, d) in [
            ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
            ("dumpClassList", "(Ljava/lang/String;)V"),
            ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
        ] {
            assert_eq!(
                b.flags_of(n, d),
                Some("private,static,native"),
                "{n}{d} was reported off-surface and is a real native"
            );
        }
        assert!(
            b.declares("isDumpingStaticArchive", "()Z") && b.declares("isUsingArchive", "()Z"),
            "the two methods the tree's `isDumpingClassList`/`isSharingEnabled` were probably \
             meant to be are both right here in the same baseline"
        );
        // Kind 1 is unchanged by the widening, and that is the design: 13 public
        // members, 5 of them covered by a row above, 8 uncovered. Widening this
        // denominator to the declared surface would have made it 23.
        assert_eq!(
            problems
                .iter()
                .filter(|p| p.starts_with("UNCOVERED"))
                .count(),
            8
        );
    }

    /// The other half of the retraction: `cds.rs` **as F17-1 left it** is
    /// entirely on JDK 25's declared surface — eleven registrations, zero
    /// off-surface — and the version-1 oracle would have called five of them
    /// fabrications.
    ///
    /// This is the test that would have prevented the near-miss, and it is
    /// mutation-checked in both directions below: a real private member must
    /// pass, and a name the JDK does not have must not.
    #[test]
    fn f23_1_every_cds_registration_is_on_the_jdk25_declared_surface() {
        let b = parse(CDS);

        // Verbatim from cds.rs::register_cds_natives in the working tree.
        #[rustfmt::skip]
        const AS_THE_TREE_HAS_THEM: &[(&str, &str)] = &[
            ("<init>", "()V"),
            ("isDumpingArchive", "()Z"),
            ("isUsingArchive", "()Z"),
            ("getCDSConfigStatus", "()I"),
            ("needsClassInitBarrier0", "(Ljava/lang/Class;)Z"),
            ("initializeFromArchive", "(Ljava/lang/Class;)V"),
            ("getRandomSeedForDumping", "()J"),
            ("logLambdaFormInvoker", "(Ljava/lang/String;)V"),
            ("defineArchivedModules", "(Ljava/lang/ClassLoader;Ljava/lang/ClassLoader;)V"),
            ("dumpClassList", "(Ljava/lang/String;)V"),
            ("dumpDynamicArchive", "(Ljava/lang/String;)V"),
        ];

        assert!(
            audit_off_surface(&b, AS_THE_TREE_HAS_THEM, &[]).is_empty(),
            "{:#?}",
            audit_off_surface(&b, AS_THE_TREE_HAS_THEM, &[])
        );

        // Mutation, direction 1 — plant a name JDK 25 does not have. If this
        // does not fire, the widening turned the check into a rubber stamp.
        let planted = {
            let mut v = AS_THE_TREE_HAS_THEM.to_vec();
            v.push(("isDumpingClassList", "()Z"));
            v
        };
        let problems = audit_off_surface(&b, &planted, &[]);
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("isDumpingClassList") && problems[0].contains("ANY ACCESS LEVEL"),
            "{problems:#?}"
        );

        // Mutation, direction 2 — five of the eleven are non-public, so under
        // the version-1 rule (public surface as the reachability test) this same
        // input produced five OFF-SURFACE reports. Recomputed here from the
        // public surface directly, because that is the number the deleted-
        // registration proposal was built on.
        let public: BTreeSet<_> = b.public_surface().into_iter().collect();
        let would_have_been_reported: Vec<_> = AS_THE_TREE_HAS_THEM
            .iter()
            .filter(|k| !public.contains(&(k.0, k.1)))
            .collect();
        assert_eq!(
            would_have_been_reported.len(),
            5,
            "the version-1 rule reports five real natives as fabrications: {would_have_been_reported:#?}"
        );
    }

    /// Kind 7b — an off-surface exemption written while the oracle was blind.
    ///
    /// Every such row in the tree is now a lie in the safe direction ("we
    /// knowingly register something the JDK does not have", about something the
    /// JDK has), and the repair is the opposite of what the message for kind 7a
    /// asks for: delete the ROW, keep the registration.
    #[test]
    fn kind_seven_b_an_exemption_written_against_a_blind_oracle_is_refused() {
        let b = parse(CDS);
        let problems = audit_off_surface(
            &b,
            &[("logLambdaFormInvoker", "(Ljava/lang/String;)V")],
            &[(
                "logLambdaFormInvoker",
                "(Ljava/lang/String;)V",
                "the JDK declares a four-String overload under this name",
            )],
        );
        let hit = problems
            .iter()
            .find(|p| p.starts_with("STALE OFF-SURFACE row (declared, not public)"))
            .unwrap_or_else(|| panic!("{problems:#?}"));
        assert!(
            hit.contains("private,static,native") && hit.contains("DELETE THE ROW"),
            "the message must quote the access level and say which of the two things to \
             delete: {hit}"
        );
    }

    /// **Row 3, the other class.** `register_cds_natives` also registers six
    /// natives on `sun/management/CDSMetrics` and a seventh,
    /// `ManagementFactoryHelper.getCDSMetrics()Lsun/management/CDSMetrics;`,
    /// that returns one.
    ///
    /// `sun.management.CDSMetrics` **is not in the JDK 25 runtime image** — a
    /// `jrt:/` walk finds no such class file, which is the same refusal the
    /// generator gave for `StructuredTaskScope$Config`. So there is no
    /// baseline to convert that half of row 3 against, and there never will
    /// be; the missing file is the finding.
    ///
    /// What CAN be checked is the owner, and it says the same thing from the
    /// other side: `sun.management.ManagementFactoryHelper` is in the image,
    /// declares 22 public methods, and `getCDSMetrics` is not among them. A
    /// native registered under a name its own owner class does not declare is
    /// a native no bytecode can reach.
    #[test]
    fn management_factory_helper_does_not_declare_get_cds_metrics() {
        let b = parse(SUN_MANAGEMENT_FACTORY_HELPER);
        assert_eq!(b.internal_name(), "sun/management/ManagementFactoryHelper");
        assert_eq!(b.public_methods().len(), 22);
        let problems = audit_off_surface(
            &b,
            &[("getCDSMetrics", "()Lsun/management/CDSMetrics;")],
            &[],
        );
        assert_eq!(problems.len(), 1, "{problems:#?}");
        assert!(
            problems[0].contains("registers sun.management.ManagementFactoryHelper.getCDSMetrics"),
            "{problems:#?}"
        );
        assert!(
            !b.declares("getCDSMetrics", "()Lsun/management/CDSMetrics;"),
            "not at any access level either — this is not a visibility question"
        );
    }

    /// **Row 16, the half that is not `$Config`.**
    /// `kind_four_would_have_caught_structured_task_scope_config` covers the
    /// fabricated nested type. This covers the outer class, which is real, and
    /// which the tree also gets wrong in four places at once — three names JDK
    /// 25 does not declare and one that is declared with a different return
    /// type:
    ///
    /// * `isShutdown()Z` — JEP 505 shipped `isCancelled()Z`.
    /// * `shutdown()V` — gone; cancellation is the joiner's business now.
    /// * `joinUntil(Ljava/time/Instant;)…` — gone; a deadline is a
    ///   `Configuration.withTimeout`.
    /// * `join()` — declared, but it returns `Ljava/lang/Object;`, not
    ///   `Ljava/util/concurrent/StructuredTaskScope;`. The preview API returned
    ///   the scope for chaining; the final one returns the joiner's result.
    ///
    /// `s52_total_registration_count` asserts none of these: its fourteen
    /// triples are seven `$Joiner`, six `$Config` and exactly one on the outer
    /// class (`open(Joiner)`), so every row above is outside its population.
    #[test]
    fn kind_four_catches_the_outer_structured_task_scope_surface() {
        let b = parse(STRUCTURED_TASK_SCOPE);
        assert_eq!(
            b.internal_name(),
            "java/util/concurrent/StructuredTaskScope"
        );

        // Verbatim from jdk25_concurrency.rs's registrations on CLS_TASK_SCOPE.
        #[rustfmt::skip]
        const AS_THE_TREE_HAS_THEM: &[Triage] = &[
            ("close", "()V", true, ""),
            ("fork", "(Ljava/util/concurrent/Callable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;", true, ""),
            ("isShutdown", "()Z", true, ""),
            ("join", "()Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("joinUntil", "(Ljava/time/Instant;)Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("open", "()Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("open", "(Ljava/util/concurrent/StructuredTaskScope$Joiner;)Ljava/util/concurrent/StructuredTaskScope;", true, ""),
            ("shutdown", "()V", true, ""),
        ];

        let problems = audit(&b, AS_THE_TREE_HAS_THEM, |_, _| true);
        let stale: Vec<&String> = problems.iter().filter(|p| p.starts_with("STALE")).collect();
        assert_eq!(stale.len(), 4, "{problems:#?}");
        assert!(
            stale
                .iter()
                .any(|p| p.starts_with("STALE row (descriptor)") && p.contains(".join()")),
            "`join` exists under a different return type, which is the diagnosis that says \
             `fix the descriptor` rather than `delete the row`: {stale:#?}"
        );
        // The three the JDK declares and this VM registers nothing for.
        assert!(b.declares("isCancelled", "()Z"));
        assert!(b.declares("join", "()Ljava/lang/Object;"));
        assert!(b.declares(
            "fork",
            "(Ljava/lang/Runnable;)Ljava/util/concurrent/StructuredTaskScope$Subtask;"
        ));
    }

    /// The two `ReflectionFactory` types are not one surface, and row 22's
    /// guard names triples on both.
    #[test]
    fn the_two_reflection_factories_are_different_classes() {
        let internal = parse(REFLECTION_FACTORY);
        let unsupported = parse(SUN_REFLECTION_FACTORY);
        assert_eq!(internal.public_surface().len(), 25);
        assert_eq!(unsupported.public_surface().len(), 14);
        // Same name, different descriptor, on the two types — a guard that
        // reads one baseline for both would report a false STALE.
        assert!(internal.declares(
            "newOptionalDataExceptionForSerialization",
            "()Ljava/lang/reflect/Constructor;"
        ));
        assert!(unsupported.declares(
            "newOptionalDataExceptionForSerialization",
            "(Z)Ljava/io/OptionalDataException;"
        ));
        // `getConstantPool` is registered on the jdk.internal type by
        // `register_essential_natives_with_shims` (lib.rs) and is not a member
        // of either. It is `SharedSecrets.getJavaLangAccess().getConstantPool`.
        for b in [&internal, &unsupported] {
            assert!(
                !b.declares(
                    "getConstantPool",
                    "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;"
                ),
                "{} declares getConstantPool",
                b.class
            );
        }
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
