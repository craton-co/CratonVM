// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Reads the checked-in JDK 25 surface baselines in `scripts/baselines/`.
//!
//! `docs/feature-designs/native-dispatch-architecture.md` §7
//! found thirty guards in this crate whose expected set was transcribed from
//! the registrar they audit, and named the cause: *"not one test reads a
//! checked-in baseline file, and not one invokes `javap`"*. This module is the
//! read side of the fix. The write side is `scripts/jdk-baseline/generate.py`,
//! which emits the files from `jrt:/modules/<module>/<binary/name>.class` on
//! openjdk 25.0.3+9; see
//! `docs/feature-designs/native-dispatch-architecture.md`.
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

pub const ABSTRACT_COLLECTION: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.AbstractCollection.tsv");
pub const ABSTRACT_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.AbstractQueue.tsv");
pub const BASE64: &str = include_str!("../../scripts/baselines/jdk25-java.util.Base64.tsv");
pub const BASE64_DECODER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.Base64$Decoder.tsv");
pub const BASE64_ENCODER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.Base64$Encoder.tsv");
pub const BIG_INTEGER: &str =
    include_str!("../../scripts/baselines/jdk25-java.math.BigInteger.tsv");
pub const BLOCKING_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.BlockingQueue.tsv");
pub const CDS: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.misc.CDS.tsv");
pub const CHARACTER: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.Character.tsv");
pub const FLOW_PUBLISHER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.Flow$Publisher.tsv");
pub const KEY_STORE: &str =
    include_str!("../../scripts/baselines/jdk25-java.security.KeyStore.tsv");
pub const MAC: &str = include_str!("../../scripts/baselines/jdk25-javax.crypto.Mac.tsv");
pub const MANAGEMENT_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.management.ManagementFactory.tsv");
pub const MODULE: &str = include_str!("../../scripts/baselines/jdk25-java.lang.Module.tsv");
pub const MODULE_DESCRIPTOR: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.module.ModuleDescriptor.tsv");
pub const MODULE_JAVA_BASE: &str =
    include_str!("../../scripts/baselines/jdk25-module-java.base.tsv");
pub const MODULE_LAYER: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.ModuleLayer.tsv");
pub const REFLECTION_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.reflect.ReflectionFactory.tsv");
pub const RUNTIME_MXBEAN: &str =
    include_str!("../../scripts/baselines/jdk25-java.lang.management.RuntimeMXBean.tsv");
pub const SHARED_SECRETS: &str =
    include_str!("../../scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv");
pub const SOCKET: &str = include_str!("../../scripts/baselines/jdk25-java.net.Socket.tsv");
pub const SSL_PARAMETERS: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLParameters.tsv");
pub const SSL_SESSION: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLSession.tsv");
pub const SSL_SOCKET: &str =
    include_str!("../../scripts/baselines/jdk25-javax.net.ssl.SSLSocket.tsv");
pub const STRUCTURED_TASK_SCOPE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope.tsv");
pub const STRUCTURED_TASK_SCOPE_CONFIGURATION: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Configuration.tsv"
);
pub const STRUCTURED_TASK_SCOPE_JOINER: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Joiner.tsv"
);
pub const STRUCTURED_TASK_SCOPE_SUBTASK: &str = include_str!(
    "../../scripts/baselines/jdk25-java.util.concurrent.StructuredTaskScope$Subtask.tsv"
);
pub const SUBMISSION_PUBLISHER: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SubmissionPublisher.tsv");
/// `sun.management.ManagementFactoryHelper` — the declared owner of
/// `cds.rs`'s `getCDSMetrics()Lsun/management/CDSMetrics;`. It declares 22
/// public methods and **`getCDSMetrics` is not one of them**; see
/// `management_factory_helper_does_not_declare_get_cds_metrics`.
pub const SUN_MANAGEMENT_FACTORY_HELPER: &str =
    include_str!("../../scripts/baselines/jdk25-sun.management.ManagementFactoryHelper.tsv");
/// `sun.reflect.ReflectionFactory` (module `jdk.unsupported`) — the twin of
/// [`REFLECTION_FACTORY`], and the class two of the four triples in
/// `lib.rs::essential_path_does_not_override_reflection_factory_serialization`
/// name. The two types are **not** the same surface: this one has 14 public
/// methods to the `jdk.internal` type's 25, and its
/// `newOptionalDataExceptionForSerialization` takes a `Z` and returns an
/// `OptionalDataException` where the `jdk.internal` one takes nothing and
/// returns a `Constructor`.
pub const SUN_REFLECTION_FACTORY: &str =
    include_str!("../../scripts/baselines/jdk25-sun.reflect.ReflectionFactory.tsv");
pub const SYNCHRONOUS_QUEUE: &str =
    include_str!("../../scripts/baselines/jdk25-java.util.concurrent.SynchronousQueue.tsv");

/// Every baseline this crate embeds, as `(file name, contents)`.
///
/// Exists so `every_checked_in_baseline_is_wired_in_and_parses` can compare it
/// against `read_dir("scripts/baselines")` in **both** directions — a file on
/// disk that nothing includes, and an include that names a file no longer on
/// disk. The dead-row half is the one that decays;
/// `types/tests/flag_declaration_guard.rs::the_allowlist_has_no_dead_rows` is
/// the in-tree pattern this copies.
pub const ALL: &[(&str, &str)] = &[
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
pub struct Row {
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
    pub fn has_flag(&self, f: &str) -> bool {
        self.flags.split(',').any(|x| x == f)
    }

    /// `true` only for `ACC_PUBLIC`. Protected, private and package-private all
    /// answer `false`.
    pub fn is_public(&self) -> bool {
        self.has_flag("public")
    }
}

/// A parsed baseline. Construct with [`parse`]; there is no other constructor,
/// so every instance has passed the self-tests in [`parse`].
pub struct Baseline {
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
pub fn parse(text: &'static str) -> Baseline {
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
         docs/feature-designs/native-dispatch-architecture.md §2 before \
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
    pub fn public_methods(&self) -> Vec<(&'static str, &'static str)> {
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
    pub fn public_surface(&self) -> Vec<(&'static str, &'static str)> {
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
    pub fn declared_surface(&self) -> Vec<(&'static str, &'static str)> {
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
    pub fn native_surface(&self) -> Vec<(&'static str, &'static str)> {
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
    pub fn declares(&self, name: &str, descriptor: &str) -> bool {
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
    pub fn flags_of(&self, name: &str, descriptor: &str) -> Option<&'static str> {
        self.rows
            .iter()
            .find(|r| r.kind == "METHOD" && r.name == name && r.descriptor == descriptor)
            .map(|r| r.flags)
    }

    /// Every descriptor this class declares under `name`, at any access level.
    ///
    /// Used by [`audit`] to tell "you spelled the descriptor wrong" apart from
    /// "this member does not exist" — two very different repairs.
    pub fn descriptors_named(&self, name: &str) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "METHOD" && r.name == name)
            .map(|r| r.descriptor)
            .collect()
    }

    /// Every transitive supertype, so "not declared here" can be told apart
    /// from "not in the JDK at all". `javap -public` never lists an inherited
    /// member, so a guard reading only the leaf class reads one as *absent*.
    pub fn supertypes(&self) -> Vec<&'static str> {
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
    pub fn unqualified_exports(&self) -> Vec<&'static str> {
        self.rows
            .iter()
            .filter(|r| r.kind == "EXPORTS" && r.flags.is_empty())
            .map(|r| r.name)
            .collect()
    }

    /// [`Self::unqualified_exports`] with `.` rewritten to `/`, which is the
    /// form every package constant in this crate uses.
    pub fn unqualified_exports_internal(&self) -> Vec<String> {
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
    pub fn internal_name(&self) -> String {
        self.class.replace('.', "/")
    }
}

/// One triage row: `(name, descriptor, expect_registered, reason)`.
///
/// `reason` must be non-empty when `expect_registered` is `false` and empty
/// when it is `true` — an absence with no stated reason is an omission wearing
/// a record's clothes, and a reason on a present registration would make a
/// non-empty `reason` stop meaning "absent".
pub type Triage = (&'static str, &'static str, bool, &'static str);

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
pub fn audit(
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
pub type OffSurface = (&'static str, &'static str, &'static str);

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
pub fn audit_off_surface(
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
mod tests;
