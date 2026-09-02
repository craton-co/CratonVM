// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JDK-only mode telemetry — the aggregate counter set of
//! `docs/feature-designs/jdk-only-mode.md`.
//!
//! # What this is
//!
//! Seven counters, derived once at report time from artefacts the VM already
//! keeps:
//!
//! | Counter | Label | Derived from |
//! |---|---|---|
//! | `cratonvm_jdk_only_violation_total` | `kind` | `JdkOnlyViolation::kind()` |
//! | `cratonvm_class_origin_total` | `origin` | `ClassManager::dump_class_origins()` |
//! | `cratonvm_native_registration_total` | `kind` | `NativeMethodRegistry::census()` |
//! | `cratonvm_native_invocation_total` | `kind` | `NativeCensusEntry::invocations` |
//! | `cratonvm_real_bytecode_shadow_attempt_total` | — | `NativeShadowsBytecode` violations |
//! | `cratonvm_missing_native_total` | `module` | `MissingNative` violations |
//! | `cratonvm_generated_class_total` | `generator` | the four generated class origins |
//!
//! # What this is *not*
//!
//! It is **not a telemetry subsystem**. There is no collector, no background
//! thread, no registry of metrics, no exporter, and no state of its own that
//! could drift from the thing it measures. [`JdkOnlyTelemetry`] is a plain
//! value you build at a reporting point, feed the existing censuses, and drop.
//! Every number in it is a fold over data the VM was already keeping for
//! `--jdk-only-report`, `--dump-class-origins` and `--dump-native-registry`.
//!
//! Two consequences fall out of that shape, and both are deliberate:
//!
//!  * **Nothing here touches a hot path.** No counter in this module is
//!    incremented at dispatch, at class load, or at registration. The native
//!    dispatch counter already exists — one relaxed per-slot `fetch_add` in
//!    `NativeMethodRegistry::record_invocation` — and this module *reads* it.
//!    Adding a second increment anywhere would buy nothing and cost a cache
//!    line.
//!  * **Snapshots do not accumulate.** Because each report point builds a
//!    fresh aggregate over run-cumulative sources, taking two snapshots (the
//!    hang watchdog flushing mid-run, then the shutdown report) yields two
//!    correct readings rather than one doubled one.
//!
//! # Privacy posture (normative — enforced here, not by convention)
//!
//! The plan's rules, and the mechanism that makes each one true:
//!
//! 1. **Disabled by default.** [`JdkOnlyTelemetry::default`] is disabled, and a
//!    disabled aggregate ignores every `add_*` call without so much as
//!    iterating its input. See [`TELEMETRY_ENABLING_FLAGS`] for the opt-in.
//! 2. **Process-local.** This module opens no file, no socket and no shared
//!    memory, and reads no environment variable. [`JdkOnlyCounters::to_json`]
//!    hands a `String` back to the caller; whether it ever leaves the process
//!    is the operator's decision, taken by passing `--jdk-only-report <FILE>`.
//! 3. **Aggregate-only.** The counters are `u64` totals. No identity of any
//!    class, method, descriptor, loader or call site survives into a snapshot;
//!    the aggregating functions read a violation's `kind()` and nothing else.
//! 4. **No classpath contents, source paths, arguments or environment values.**
//!    Enforced *structurally*: **every label this module can emit is a
//!    `&'static str` that comes out of a `const` table in this file.** Each
//!    label-producing function ([`violation_label`], [`class_origin_label`],
//!    [`native_kind_label`], [`generator_label`], [`module_label`]) returns
//!    `Option<&'static str>` or `&'static str`. There is no code path from an
//!    owned `String` to a label, so a class name, a jar path, a command-line
//!    argument or an environment value *cannot* be emitted — not "is not", but
//!    "cannot be", which is the only version of this rule that survives
//!    somebody adding a call site later.
//! 5. **Versioned.** [`JDK_ONLY_TELEMETRY_SCHEMA_VERSION`] is emitted as
//!    `counter_schema_version` in every JSON snapshot, and the label set is
//!    fixed: a counter with no observations still emits every one of its
//!    labels with value `0`, so two CI artefacts from different runs have the
//!    same shape and diff cleanly.
//!
//! ## Label cardinality
//!
//! A label whose cardinality is attacker- or workload-controlled turns an
//! aggregate counter into a data leak: `{class="com.acme.PatientRecordDao"}` is
//! a class-name dump wearing a metric's clothes. Every label vocabulary here is
//! therefore closed and small:
//!
//!  * `kind`, `origin`, `generator` come from closed Rust enums
//!    (`JdkOnlyViolation`, `NativeKind`, `ClassOrigin`) — 7, 3, 10 and 4 values.
//!  * `module` is the one input that arrives as a free `String`. It is bounded
//!    by the fixed [`JDK_MODULES`] allow-list; anything not on it becomes
//!    [`MODULE_LABEL_OTHER`], and an absent module becomes
//!    [`MODULE_LABEL_UNKNOWN`]. A module named after a customer's internal
//!    package therefore cannot appear. Cardinality is `JDK_MODULES.len() + 2`,
//!    forever.
//!  * A tag that matches no vocabulary entry is **never echoed**. It is counted
//!    into [`JdkOnlyCounters::unrecognised_tags`], a number with no string
//!    attached, so a vocabulary drift is visible without leaking the drifting
//!    value.
//!
//! # Accuracy caveats that are part of the data, not the docs
//!
//! JDK-only mode is at the internal-diagnostic stage and enforcement is
//! partial, so two of these counters are honest lower bounds rather than
//! totals. That fact is emitted *in the JSON*, as
//! [`JdkOnlyCounters::lower_bound_labels`], because a bridge count that looks
//! complete but is not is worse than one labelled as a floor:
//!
//!  * `cratonvm_native_registration_total{kind="bridge"}` and
//!    `cratonvm_native_invocation_total{kind="bridge"}` **under-count**. Genuine
//!    JNI `RegisterNatives` bridges are held in the JNI layer's own function
//!    table, not in `NativeMethodRegistry`, so they are invisible to the census
//!    this module folds.
//!  * `synthetic-stub` is exact in both counters: a refused stub can never be
//!    in the registry, so there is no second table it could be hiding in.
//!  * `cratonvm_native_registration_total` counts registrations the registry
//!    **accepted**. Under `JdkOnly` a refused `SyntheticStub` never enters the
//!    census; it is counted once, as
//!    `cratonvm_jdk_only_violation_total{kind="synthetic-native-registered"}`.
//!    Attempted registrations are the sum of the two, which is why they are
//!    kept as separate counters rather than merged.

use cratonvm_types::error::JdkOnlyViolation;

// ───────────────────────────────────────────────────────────────────────────
// Versioning
// ───────────────────────────────────────────────────────────────────────────

/// Schema version of the counter block, emitted as `counter_schema_version`.
///
/// Bump when a counter is added, removed or renamed, when a label vocabulary
/// changes, or when the meaning of an existing number changes. Do **not** bump
/// for a value change — the whole point is that CI artefacts from two runs of
/// the same version are comparable.
///
/// Independent of the `schema_version: 1` on the `--jdk-only-report` envelope
/// (contract §9) and of the native census's `schema_version: 2`: this block can
/// be nested inside either, and three independently-evolving schemas must not
/// share one integer.
pub const JDK_ONLY_TELEMETRY_SCHEMA_VERSION: u32 = 1;

// ───────────────────────────────────────────────────────────────────────────
// Counter names
// ───────────────────────────────────────────────────────────────────────────

/// `cratonvm_jdk_only_violation_total{kind}`.
pub const COUNTER_VIOLATION_TOTAL: &str = "cratonvm_jdk_only_violation_total";
/// `cratonvm_class_origin_total{origin}`.
pub const COUNTER_CLASS_ORIGIN_TOTAL: &str = "cratonvm_class_origin_total";
/// `cratonvm_native_registration_total{kind}`.
pub const COUNTER_NATIVE_REGISTRATION_TOTAL: &str = "cratonvm_native_registration_total";
/// `cratonvm_native_invocation_total{kind}`.
pub const COUNTER_NATIVE_INVOCATION_TOTAL: &str = "cratonvm_native_invocation_total";
/// `cratonvm_real_bytecode_shadow_attempt_total` — unlabelled scalar.
pub const COUNTER_REAL_BYTECODE_SHADOW_ATTEMPT_TOTAL: &str =
    "cratonvm_real_bytecode_shadow_attempt_total";
/// `cratonvm_missing_native_total{module}`.
pub const COUNTER_MISSING_NATIVE_TOTAL: &str = "cratonvm_missing_native_total";
/// `cratonvm_generated_class_total{generator}`.
pub const COUNTER_GENERATED_CLASS_TOTAL: &str = "cratonvm_generated_class_total";

/// Every counter name, in emission order.
pub const COUNTER_NAMES: &[&str] = &[
    COUNTER_VIOLATION_TOTAL,
    COUNTER_CLASS_ORIGIN_TOTAL,
    COUNTER_NATIVE_REGISTRATION_TOTAL,
    COUNTER_NATIVE_INVOCATION_TOTAL,
    COUNTER_REAL_BYTECODE_SHADOW_ATTEMPT_TOTAL,
    COUNTER_MISSING_NATIVE_TOTAL,
    COUNTER_GENERATED_CLASS_TOTAL,
];

// ───────────────────────────────────────────────────────────────────────────
// Label vocabularies — closed, `&'static`, and asserted against the enums
// ───────────────────────────────────────────────────────────────────────────

/// `JdkOnlyViolation::kind()` spellings, in declaration order.
///
/// Mirrored rather than imported because `cratonvm_types` exposes the tag as a
/// method on a value, and a counter needs the *set*. `tests::violation_kind_
/// vocabulary_is_exactly_the_enum` constructs one violation of each variant and
/// asserts this array equals their `kind()`s, so a new variant upstream fails
/// this crate's tests instead of silently landing in `unrecognised_tags`.
pub const VIOLATION_KINDS: &[&str] = &[
    "compatibility-class-requested",
    "synthetic-native-registered",
    "synthetic-native-invocation",
    "missing-native",
    "native-shadows-bytecode",
    "missing-boot-class",
    "missing-implementation",
];

/// `ClassOrigin::as_str()` spellings, in declaration order.
///
/// `classloading` is not a dependency of this crate — it pulls in zip, mmap,
/// x509 and a trust-store parser, none of which belong under a counter — so the
/// vocabulary is mirrored here and the *caller* passes `ClassOriginEntry::origin`
/// strings in. Drift shows up as `unrecognised_tags`, never as a leaked string.
pub const CLASS_ORIGINS: &[&str] = &[
    "boot-image",
    "application-class-path",
    "user-defined",
    "vm-array",
    "hidden-class",
    "generated-lambda",
    "generated-proxy",
    "reflection-accessor",
    "vm-internal",
    "compatibility-stub",
];

/// `NativeKind::as_str()` spellings, in declaration order.
pub const NATIVE_KINDS: &[&str] = &["intrinsic", "bridge", "synthetic-stub"];

/// `cratonvm_generated_class_total{generator}` labels.
///
/// **These are the `ClassOrigin::as_str()` spellings, verbatim**, not a second
/// vocabulary. The plan names the four generators informally ("lambda, proxy,
/// reflection accessor, hidden class"); spelling them the way the enum already
/// spells them means `cratonvm_generated_class_total` labels are a strict
/// subset of `cratonvm_class_origin_total` labels, and the two counters can be
/// cross-checked by a consumer with no translation table.
///
/// `vm-array` and `vm-internal` are **not** here. They are VM-created classes,
/// but nothing *generated* them from a program's shape: an array class is
/// synthesised from its component type (JVMS §5.3.3) and a `vm-internal` type
/// has no class file at all. Note this is a narrower set than the
/// `generated_classes` bucket of the contract §9 `counts` block, which folds
/// all six non-class-file origins together; see [`ContractCounts`].
pub const GENERATORS: &[&str] = &[
    "hidden-class",
    "generated-lambda",
    "generated-proxy",
    "reflection-accessor",
];

/// `cratonvm_missing_native_total{module}` label for a violation that did not
/// name a module.
///
/// Today this is *every* such violation: the only producer of
/// `JdkOnlyViolation::MissingNative` builds it with `module: None`. See the
/// crate-level note in `JDK-ONLY-NOTE` form at the bottom of this file.
pub const MODULE_LABEL_UNKNOWN: &str = "unknown";

/// `cratonvm_missing_native_total{module}` label for a module that is not on
/// the [`JDK_MODULES`] allow-list.
///
/// This is the cardinality bound. A module name is the one label input that
/// arrives as a workload-controlled `String`; collapsing everything off the
/// allow-list into one bucket means an application module — whose *name* may
/// itself be sensitive — can never become a label.
pub const MODULE_LABEL_OTHER: &str = "other";

/// The JDK's own module names, sorted, for [`module_label`]'s binary search.
///
/// A closed allow-list rather than a `java.`/`jdk.` prefix test: a prefix test
/// still admits an unbounded set of names, and "starts with jdk." is not a
/// property anyone is prevented from choosing for their own module. Covering
/// JDK 17 through 25 with a few names that a given image will not contain costs
/// one unused `u64` each and keeps the label set stable when the runtime image
/// changes underneath a CI baseline.
///
/// A module the JDK adds later lands in [`MODULE_LABEL_OTHER`] and
/// under-attributes rather than leaking; add it here when that matters.
pub const JDK_MODULES: &[&str] = &[
    "java.base",
    "java.compiler",
    "java.datatransfer",
    "java.desktop",
    "java.instrument",
    "java.logging",
    "java.management",
    "java.management.rmi",
    "java.naming",
    "java.net.http",
    "java.prefs",
    "java.rmi",
    "java.scripting",
    "java.se",
    "java.security.jgss",
    "java.security.sasl",
    "java.smartcardio",
    "java.sql",
    "java.sql.rowset",
    "java.transaction.xa",
    "java.xml",
    "java.xml.crypto",
    "jdk.accessibility",
    "jdk.attach",
    "jdk.charsets",
    "jdk.compiler",
    "jdk.crypto.cryptoki",
    "jdk.crypto.ec",
    "jdk.dynalink",
    "jdk.editpad",
    "jdk.hotspot.agent",
    "jdk.httpserver",
    "jdk.incubator.vector",
    "jdk.internal.vm.ci",
    "jdk.internal.vm.compiler",
    "jdk.jartool",
    "jdk.javadoc",
    "jdk.jcmd",
    "jdk.jconsole",
    "jdk.jdeps",
    "jdk.jdi",
    "jdk.jdwp.agent",
    "jdk.jfr",
    "jdk.jlink",
    "jdk.jpackage",
    "jdk.jshell",
    "jdk.jsobject",
    "jdk.jstatd",
    "jdk.localedata",
    "jdk.management",
    "jdk.management.agent",
    "jdk.management.jfr",
    "jdk.naming.dns",
    "jdk.naming.rmi",
    "jdk.net",
    "jdk.nio.mapmode",
    "jdk.random",
    "jdk.sctp",
    "jdk.security.auth",
    "jdk.security.jgss",
    "jdk.unsupported",
    "jdk.unsupported.desktop",
    "jdk.xml.dom",
    "jdk.zipfs",
];

const N_VIOLATION: usize = 7;
const N_ORIGIN: usize = 10;
const N_NATIVE_KIND: usize = 3;
const N_GENERATOR: usize = 4;
/// [`JDK_MODULES`] plus [`MODULE_LABEL_OTHER`] plus [`MODULE_LABEL_UNKNOWN`].
const N_MODULE: usize = 66;

// ───────────────────────────────────────────────────────────────────────────
// Label functions — the privacy boundary
//
// Every one of these returns `&'static str`. That return type is the rule
// "no classpath contents, source paths, arguments or environment values in
// any emitted label", expressed in the type system: the borrow checker will
// not let a caller-owned `String` out of any of them.
// ───────────────────────────────────────────────────────────────────────────

/// Index of `tag` in `table`, or `None`. Linear over a <= 10-element table,
/// called once per input row at report time.
#[inline]
fn index_of(table: &[&'static str], tag: &str) -> Option<usize> {
    table.iter().position(|t| *t == tag)
}

/// The canonical `&'static` spelling of a violation kind, or `None` if the tag
/// is not one of [`VIOLATION_KINDS`].
pub fn violation_label(kind: &str) -> Option<&'static str> {
    index_of(VIOLATION_KINDS, kind).map(|i| VIOLATION_KINDS[i])
}

/// The canonical `&'static` spelling of a class origin, or `None`.
pub fn class_origin_label(origin: &str) -> Option<&'static str> {
    index_of(CLASS_ORIGINS, origin).map(|i| CLASS_ORIGINS[i])
}

/// The canonical `&'static` spelling of a native kind, or `None`.
pub fn native_kind_label(kind: &str) -> Option<&'static str> {
    index_of(NATIVE_KINDS, kind).map(|i| NATIVE_KINDS[i])
}

/// The `cratonvm_generated_class_total{generator}` label for a class origin, or
/// `None` if that origin is not a generated one.
///
/// `vm-array` and `vm-internal` deliberately answer `None`; see [`GENERATORS`].
pub fn generator_label(origin: &str) -> Option<&'static str> {
    index_of(GENERATORS, origin).map(|i| GENERATORS[i])
}

/// The bounded `cratonvm_missing_native_total{module}` label for a
/// `JdkOnlyViolation::MissingNative`'s `module` field.
///
/// Total, never fallible, and never echoes its input:
///
///  * `None`                     -> [`MODULE_LABEL_UNKNOWN`]
///  * a name in [`JDK_MODULES`]  -> that name's `&'static` spelling
///  * anything else              -> [`MODULE_LABEL_OTHER`]
///
/// The third arm is the load-bearing one. It is why an application module named
/// after an internal system, a path that a caller mistakenly passed as a module,
/// or a `-D` value that leaked into one, all come out as the four characters
/// `other`.
pub fn module_label(module: Option<&str>) -> &'static str {
    let Some(name) = module else {
        return MODULE_LABEL_UNKNOWN;
    };
    match JDK_MODULES.binary_search(&name) {
        Ok(i) => JDK_MODULES[i],
        Err(_) => MODULE_LABEL_OTHER,
    }
}

/// Index into the `missing_native` array for a module label.
fn module_index(module: Option<&str>) -> usize {
    let Some(name) = module else {
        return JDK_MODULES.len() + 1;
    };
    match JDK_MODULES.binary_search(&name) {
        Ok(i) => i,
        Err(_) => JDK_MODULES.len(),
    }
}

/// The module label at `index`, in emission order: the allow-list in sorted
/// order, then `other`, then `unknown`.
fn module_label_at(index: usize) -> &'static str {
    if index < JDK_MODULES.len() {
        JDK_MODULES[index]
    } else if index == JDK_MODULES.len() {
        MODULE_LABEL_OTHER
    } else {
        MODULE_LABEL_UNKNOWN
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Opt-in
// ───────────────────────────────────────────────────────────────────────────

/// The command-line flags whose presence turns this telemetry on.
///
/// **There is no environment variable, by design.** `types::flag_groups` holds
/// the whole `CRATONVM_*` surface to fifteen names and asserts, in
/// `jdk_only_adds_no_environment_variable`, that JDK-only adds none: it is a
/// runtime policy chosen per invocation, not something a parent shell may set
/// behind an operator's back. Reusing the existing §9 flags as the gate means
/// telemetry is on exactly when the operator has already asked for a JDK-only
/// artefact, and off — costing nothing, holding nothing — otherwise.
///
/// `--jdk-only` itself is *not* here: selecting the policy is not the same as
/// asking to be measured, and `--jdk-only` with no report flag should behave
/// like a normal run.
///
/// `tests::enabling_flags_are_real_contract_flags` asserts every entry is a
/// real `JDK_ONLY_CLI_FLAGS` member, so a typo cannot create a gate that never
/// opens.
pub const TELEMETRY_ENABLING_FLAGS: &[&str] = &[
    "--jdk-only-report",
    "--dump-class-origins",
    "--trace-jdk-only",
];

/// Whether a single argument is one of [`TELEMETRY_ENABLING_FLAGS`].
///
/// Accepts the `--flag=value` spelling by comparing up to the first `=`, which
/// is the one place the launcher's two spellings would otherwise diverge.
pub fn enabled_by_flag(arg: &str) -> bool {
    let head = match arg.find('=') {
        Some(i) => &arg[..i],
        None => arg,
    };
    TELEMETRY_ENABLING_FLAGS.contains(&head)
}

/// Whether any argument in `args` enables telemetry.
///
/// Takes the arguments the launcher already parsed; it does **not** read
/// `std::env` itself, so an embedded VM cannot have telemetry switched on by
/// the process's command line.
pub fn enabled_by_args<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter().any(|a| enabled_by_flag(a.as_ref()))
}

// ───────────────────────────────────────────────────────────────────────────
// Inputs
// ───────────────────────────────────────────────────────────────────────────

/// One row of `NativeMethodRegistry::census()`, reduced to the two fields a
/// counter may see.
///
/// `class`, `name`, `descriptor`, `registered_by` and `overwrote` are
/// **deliberately absent**. The first three are program identity; `registered_by`
/// is a *source path* (`"native-builtins/src/lib.rs:1234"`), precisely the kind
/// of string the privacy rules forbid in a label. Omitting the fields, rather
/// than carrying and ignoring them, means no later edit to this module can
/// start emitting them by accident.
///
/// The caller's mapping is one line:
///
/// ```ignore
/// registry.census().iter().map(|e| NativeCensusSample {
///     kind: e.kind.as_str(),
///     invocations: e.invocations,
/// })
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeCensusSample<'a> {
    /// `NativeCensusEntry::kind`, as `NativeKind::as_str()`.
    pub kind: &'a str,
    /// `NativeCensusEntry::invocations` — dispatches recorded against the slot
    /// this registration currently owns. `0` on a superseded row, so summing
    /// this column over the census is exactly `invocations_of_kind`, with no
    /// double count.
    pub invocations: u64,
}

// ───────────────────────────────────────────────────────────────────────────
// Aggregation
// ───────────────────────────────────────────────────────────────────────────

/// Aggregator for the JDK-only counter set.
///
/// Build one at a reporting point, feed it the censuses, call [`Self::finish`],
/// drop it. It holds no locks, allocates nothing, and has no lifetime tied to
/// the VM, which is what lets a hang watchdog build one from whatever it can
/// reach without risking a second acquisition of a lock the hung thread holds.
///
/// [`Default`] is **disabled**.
#[derive(Debug, Clone)]
pub struct JdkOnlyTelemetry {
    enabled: bool,
    violation: [u64; N_VIOLATION],
    class_origin: [u64; N_ORIGIN],
    native_registration: [u64; N_NATIVE_KIND],
    native_invocation: [u64; N_NATIVE_KIND],
    real_bytecode_shadow_attempt: u64,
    missing_native: [u64; N_MODULE],
    generated_class: [u64; N_GENERATOR],
    unrecognised_tags: u64,
}

impl Default for JdkOnlyTelemetry {
    /// Disabled. Rule 1 of the privacy posture, as the type's own default.
    fn default() -> Self {
        Self::disabled()
    }
}

impl JdkOnlyTelemetry {
    /// The single constructor. All counters start at zero in both states; the
    /// only thing `enabled` changes is whether an `add_*` observes its input.
    const fn with_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            violation: [0; N_VIOLATION],
            class_origin: [0; N_ORIGIN],
            native_registration: [0; N_NATIVE_KIND],
            native_invocation: [0; N_NATIVE_KIND],
            real_bytecode_shadow_attempt: 0,
            missing_native: [0; N_MODULE],
            generated_class: [0; N_GENERATOR],
            unrecognised_tags: 0,
        }
    }

    /// A disabled aggregate. Every `add_*` is a no-op and [`Self::finish`]
    /// yields all zeroes.
    pub const fn disabled() -> Self {
        Self::with_enabled(false)
    }

    /// An enabled aggregate. Call this only behind
    /// [`TELEMETRY_ENABLING_FLAGS`]; [`Self::new`] does that test for you.
    pub const fn enabled() -> Self {
        Self::with_enabled(true)
    }

    /// Enabled iff `enabled`. The launcher's spelling is
    /// `JdkOnlyTelemetry::new(enabled_by_args(&argv))`.
    pub const fn new(enabled: bool) -> Self {
        Self::with_enabled(enabled)
    }

    /// Whether this aggregate records anything.
    #[inline]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Fold one violation into `cratonvm_jdk_only_violation_total{kind}`, and
    /// into the two counters derived from specific variants.
    ///
    /// Reads `kind()` and — for `MissingNative` only — the `module` field,
    /// which [`module_label`] immediately reduces to a bounded `&'static str`.
    /// Nothing else about the violation is observed, so class, method,
    /// descriptor, requester, loader, call site and registration site cannot
    /// reach a counter.
    pub fn add_violation(&mut self, violation: &JdkOnlyViolation) {
        if !self.enabled {
            return;
        }
        match violation_label(violation.kind()).and_then(|l| index_of(VIOLATION_KINDS, l)) {
            Some(i) => self.violation[i] += 1,
            // Vocabulary drift: count it, never echo it.
            None => {
                self.unrecognised_tags += 1;
                return;
            }
        }
        match violation {
            JdkOnlyViolation::NativeShadowsBytecode { .. } => {
                self.real_bytecode_shadow_attempt += 1;
            }
            JdkOnlyViolation::MissingNative { module, .. } => {
                self.missing_native[module_index(module.as_deref())] += 1;
            }
            _ => {}
        }
    }

    /// Fold a whole violation slice.
    ///
    /// The three producers are `NativeMethodRegistry::refused_registrations()`,
    /// `ClassManager::origin_violations()` and the JIT's own recorded-violation
    /// buffer. Feed each once; they are disjoint.
    pub fn add_violations<'a, I>(&mut self, violations: I)
    where
        I: IntoIterator<Item = &'a JdkOnlyViolation>,
    {
        if !self.enabled {
            return;
        }
        for v in violations {
            self.add_violation(v);
        }
    }

    /// Fold the class-origin census into `cratonvm_class_origin_total{origin}`
    /// and `cratonvm_generated_class_total{generator}`.
    ///
    /// Each item is one `ClassOriginEntry::origin` — the `ClassOrigin::as_str()`
    /// tag, and nothing else from the row. `name`, `reason`, `requested_by` and
    /// `loader_id` are not passed in, because none of them may be a label.
    ///
    /// ```ignore
    /// telemetry.add_class_origins(class_manager.dump_class_origins().iter().map(|r| r.origin.as_str()));
    /// ```
    pub fn add_class_origins<I, S>(&mut self, origins: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        if !self.enabled {
            return;
        }
        for origin in origins {
            let origin = origin.as_ref();
            match index_of(CLASS_ORIGINS, origin) {
                Some(i) => self.class_origin[i] += 1,
                None => {
                    self.unrecognised_tags += 1;
                    continue;
                }
            }
            if let Some(g) = generator_label(origin) {
                if let Some(gi) = index_of(GENERATORS, g) {
                    self.generated_class[gi] += 1;
                }
            }
        }
    }

    /// Fold the native census into `cratonvm_native_registration_total{kind}`
    /// and `cratonvm_native_invocation_total{kind}`.
    ///
    /// Both counters come from the *same* pass over the *same* rows, which is
    /// the point: a registration and its dispatch count can never disagree
    /// about which kind they belong to, the way two independently maintained
    /// tallies eventually would.
    pub fn add_native_census<'a, I>(&mut self, rows: I)
    where
        I: IntoIterator<Item = NativeCensusSample<'a>>,
    {
        if !self.enabled {
            return;
        }
        for row in rows {
            match index_of(NATIVE_KINDS, row.kind) {
                Some(i) => {
                    self.native_registration[i] += 1;
                    self.native_invocation[i] += row.invocations;
                }
                None => self.unrecognised_tags += 1,
            }
        }
    }

    /// Freeze the aggregate.
    pub fn finish(self) -> JdkOnlyCounters {
        JdkOnlyCounters {
            schema_version: JDK_ONLY_TELEMETRY_SCHEMA_VERSION,
            enabled: self.enabled,
            violation: self.violation,
            class_origin: self.class_origin,
            native_registration: self.native_registration,
            native_invocation: self.native_invocation,
            real_bytecode_shadow_attempt: self.real_bytecode_shadow_attempt,
            missing_native: self.missing_native,
            generated_class: self.generated_class,
            unrecognised_tags: self.unrecognised_tags,
        }
    }

    /// Freeze without consuming, for a watchdog that wants to flush mid-run and
    /// keep aggregating afterwards.
    pub fn snapshot(&self) -> JdkOnlyCounters {
        self.clone().finish()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Snapshot
// ───────────────────────────────────────────────────────────────────────────

/// A frozen reading of the seven counters.
///
/// Every label of every counter is present with a value, including zeroes: two
/// artefacts of the same [`JDK_ONLY_TELEMETRY_SCHEMA_VERSION`] have identical
/// shape and diff on values alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JdkOnlyCounters {
    schema_version: u32,
    enabled: bool,
    violation: [u64; N_VIOLATION],
    class_origin: [u64; N_ORIGIN],
    native_registration: [u64; N_NATIVE_KIND],
    native_invocation: [u64; N_NATIVE_KIND],
    real_bytecode_shadow_attempt: u64,
    missing_native: [u64; N_MODULE],
    generated_class: [u64; N_GENERATOR],
    unrecognised_tags: u64,
}

impl Default for JdkOnlyCounters {
    fn default() -> Self {
        JdkOnlyTelemetry::disabled().finish()
    }
}

impl JdkOnlyCounters {
    /// [`JDK_ONLY_TELEMETRY_SCHEMA_VERSION`] as of the run that produced this.
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Whether the aggregate that produced this was enabled. `false` means
    /// every number below is zero because nothing was measured — not because
    /// nothing happened.
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Tags seen that matched no vocabulary. A number, never a string: a drift
    /// between this module's mirrored vocabularies and the upstream enums is
    /// visible without the drifting value being echoed into an artefact.
    pub const fn unrecognised_tags(&self) -> u64 {
        self.unrecognised_tags
    }

    /// `cratonvm_jdk_only_violation_total{kind}`; `0` for an unknown kind.
    pub fn violation_total(&self, kind: &str) -> u64 {
        index_of(VIOLATION_KINDS, kind).map_or(0, |i| self.violation[i])
    }

    /// `cratonvm_class_origin_total{origin}`.
    pub fn class_origin_total(&self, origin: &str) -> u64 {
        index_of(CLASS_ORIGINS, origin).map_or(0, |i| self.class_origin[i])
    }

    /// `cratonvm_native_registration_total{kind}` — registrations the registry
    /// **accepted**. Refusals are counted as violations; see the module docs.
    pub fn native_registration_total(&self, kind: &str) -> u64 {
        index_of(NATIVE_KINDS, kind).map_or(0, |i| self.native_registration[i])
    }

    /// `cratonvm_native_invocation_total{kind}`. A **lower bound** for
    /// `bridge`; exact for `intrinsic` and `synthetic-stub`.
    pub fn native_invocation_total(&self, kind: &str) -> u64 {
        index_of(NATIVE_KINDS, kind).map_or(0, |i| self.native_invocation[i])
    }

    /// `cratonvm_real_bytecode_shadow_attempt_total`.
    pub const fn real_bytecode_shadow_attempt_total(&self) -> u64 {
        self.real_bytecode_shadow_attempt
    }

    /// `cratonvm_missing_native_total{module}` for a bounded label. Ask with
    /// [`module_label`]'s output, or with `"unknown"` / `"other"`.
    pub fn missing_native_total(&self, label: &str) -> u64 {
        if label == MODULE_LABEL_UNKNOWN {
            return self.missing_native[JDK_MODULES.len() + 1];
        }
        if label == MODULE_LABEL_OTHER {
            return self.missing_native[JDK_MODULES.len()];
        }
        JDK_MODULES
            .binary_search(&label)
            .map_or(0, |i| self.missing_native[i])
    }

    /// `cratonvm_missing_native_total` summed over every module label.
    pub fn missing_native_grand_total(&self) -> u64 {
        self.missing_native.iter().sum()
    }

    /// `cratonvm_generated_class_total{generator}`.
    pub fn generated_class_total(&self, generator: &str) -> u64 {
        index_of(GENERATORS, generator).map_or(0, |i| self.generated_class[i])
    }

    /// `cratonvm_generated_class_total` summed over every generator.
    pub fn generated_class_grand_total(&self) -> u64 {
        self.generated_class.iter().sum()
    }

    /// Every `(label, value)` of `cratonvm_jdk_only_violation_total`, in
    /// vocabulary order, zeroes included.
    pub fn violations(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        VIOLATION_KINDS.iter().copied().zip(self.violation)
    }

    /// Every `(label, value)` of `cratonvm_class_origin_total`.
    pub fn class_origins(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        CLASS_ORIGINS.iter().copied().zip(self.class_origin)
    }

    /// Every `(label, value)` of `cratonvm_native_registration_total`.
    pub fn native_registrations(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        NATIVE_KINDS.iter().copied().zip(self.native_registration)
    }

    /// Every `(label, value)` of `cratonvm_native_invocation_total`.
    pub fn native_invocations(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        NATIVE_KINDS.iter().copied().zip(self.native_invocation)
    }

    /// Every `(label, value)` of `cratonvm_missing_native_total`, allow-list
    /// first in sorted order, then `other`, then `unknown`.
    pub fn missing_natives(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        (0..N_MODULE).map(move |i| (module_label_at(i), self.missing_native[i]))
    }

    /// Every `(label, value)` of `cratonvm_generated_class_total`.
    pub fn generated_classes(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        GENERATORS.iter().copied().zip(self.generated_class)
    }

    /// The counter series that are lower bounds rather than totals, in
    /// `name{label="value"}` form.
    ///
    /// Emitted into the JSON so the caveat travels with the data. See the
    /// module docs for why `bridge` under-counts and `synthetic-stub` does not.
    pub fn lower_bound_labels(&self) -> Vec<String> {
        vec![
            format!("{COUNTER_NATIVE_REGISTRATION_TOTAL}{{kind=\"bridge\"}}"),
            format!("{COUNTER_NATIVE_INVOCATION_TOTAL}{{kind=\"bridge\"}}"),
        ]
    }

    /// Whether `counter{label}` is a lower bound rather than a total.
    ///
    /// Machine-readable form of the same fact, for a consumer that would rather
    /// ask than parse [`Self::lower_bound_labels`].
    pub fn is_lower_bound(&self, counter: &str, label: &str) -> bool {
        matches!(
            (counter, label),
            (COUNTER_NATIVE_REGISTRATION_TOTAL, "bridge")
                | (COUNTER_NATIVE_INVOCATION_TOTAL, "bridge")
        )
    }

    /// The contract §9 `counts` block, derived from these counters.
    ///
    /// The §9 report is written by two places today (the launcher and the
    /// embedded path) which must not disagree about arithmetic. This is that
    /// arithmetic, once, so a third writer does not have to re-derive it.
    ///
    /// The fold is §9's, verbatim: `user-defined` counts as an application
    /// class (application bytes, whoever called `defineClass`), and
    /// `generated_classes` is *all six* non-class-file origins — including
    /// `vm-array` and `vm-internal`, which
    /// `cratonvm_generated_class_total{generator}` excludes. The two answer
    /// different questions and are both right; see [`GENERATORS`].
    pub fn contract_counts(&self) -> ContractCounts {
        ContractCounts {
            boot_image_classes: self.class_origin_total("boot-image"),
            application_classes: self.class_origin_total("application-class-path")
                + self.class_origin_total("user-defined"),
            generated_classes: self.class_origin_total("vm-array")
                + self.class_origin_total("hidden-class")
                + self.class_origin_total("generated-lambda")
                + self.class_origin_total("generated-proxy")
                + self.class_origin_total("reflection-accessor")
                + self.class_origin_total("vm-internal"),
            compatibility_classes: self.class_origin_total("compatibility-stub"),
            bridge_invocations: self.native_invocation_total("bridge"),
            intrinsic_invocations: self.native_invocation_total("intrinsic"),
            synthetic_stub_invocations: self.native_invocation_total("synthetic-stub"),
        }
    }

    /// The counter block as JSON, two-space indented to nest inside the §9
    /// report.
    ///
    /// No escaping is performed and none is needed: **every key and every label
    /// in the output is a `&'static str` from a `const` table in this file**, so
    /// there is no string here that a workload could have chosen. That is the
    /// same property that makes the privacy rules hold, seen from the
    /// serialiser's side.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(4096);
        out.push_str("{\n");
        out.push_str(&format!(
            "  \"counter_schema_version\": {},\n",
            self.schema_version
        ));
        out.push_str(&format!("  \"enabled\": {},\n", self.enabled));
        out.push_str(&format!(
            "  \"unrecognised_tags\": {},\n",
            self.unrecognised_tags
        ));
        out.push_str("  \"counters\": {\n");

        write_labelled(&mut out, COUNTER_VIOLATION_TOTAL, self.violations(), true);
        write_labelled(
            &mut out,
            COUNTER_CLASS_ORIGIN_TOTAL,
            self.class_origins(),
            true,
        );
        write_labelled(
            &mut out,
            COUNTER_NATIVE_REGISTRATION_TOTAL,
            self.native_registrations(),
            true,
        );
        write_labelled(
            &mut out,
            COUNTER_NATIVE_INVOCATION_TOTAL,
            self.native_invocations(),
            true,
        );
        out.push_str(&format!(
            "    \"{COUNTER_REAL_BYTECODE_SHADOW_ATTEMPT_TOTAL}\": {},\n",
            self.real_bytecode_shadow_attempt
        ));
        write_labelled(
            &mut out,
            COUNTER_MISSING_NATIVE_TOTAL,
            self.missing_natives(),
            true,
        );
        write_labelled(
            &mut out,
            COUNTER_GENERATED_CLASS_TOTAL,
            self.generated_classes(),
            false,
        );

        out.push_str("  },\n");
        out.push_str("  \"lower_bound\": [");
        for (i, series) in self.lower_bound_labels().iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("\n    \"");
            out.push_str(&series.replace('"', "\\\""));
            out.push('"');
        }
        out.push_str("\n  ],\n");
        out.push_str(
            "  \"lower_bound_reason\": \"JNI RegisterNatives bridges are held in the \
             JNI layer's own function table, not in the native method registry these \
             counters fold, so bridge registrations and bridge invocations are floors \
             rather than totals. The synthetic-stub series is exact: a refused stub is \
             never in the registry.\"\n",
        );
        out.push_str("}\n");
        out
    }
}

/// Append `"name": { "label": value, ... },` at two-space nesting.
fn write_labelled<I>(out: &mut String, name: &str, series: I, trailing_comma: bool)
where
    I: Iterator<Item = (&'static str, u64)>,
{
    out.push_str(&format!("    \"{name}\": {{"));
    for (i, (label, value)) in series.enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!("\n      \"{label}\": {value}"));
    }
    out.push_str("\n    }");
    if trailing_comma {
        out.push(',');
    }
    out.push('\n');
}

/// The contract §9 `counts` block, derived from [`JdkOnlyCounters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContractCounts {
    pub boot_image_classes: u64,
    pub application_classes: u64,
    pub generated_classes: u64,
    pub compatibility_classes: u64,
    /// A **lower bound** — see [`JdkOnlyCounters::lower_bound_labels`].
    pub bridge_invocations: u64,
    pub intrinsic_invocations: u64,
    /// Exact. A refused stub can never be in the registry.
    pub synthetic_stub_invocations: u64,
}

// JDK-ONLY-NOTE: `cratonvm_missing_native_total{module}` can be derived from
// `JdkOnlyViolation::MissingNative`, but its module label is `unknown` for
// every violation today, because the only producer —
// `vm/src/vm/vm_exec.rs::reject_missing_native` — constructs the violation with
// `module: None`. The counter is correct and inert rather than wrong; to make
// it informative, that function needs the defining class's JPMS module name
// threaded in (available from `ClassOrigin::BootImage { module, .. }` on the
// declaring class). `vm/**` is not this agent's to edit.

#[cfg(test)]
mod tests {
    use super::*;

    fn violation_of_each_kind() -> Vec<JdkOnlyViolation> {
        vec![
            JdkOnlyViolation::CompatibilityClassRequested {
                class: "org/jboss/Secret".into(),
                initiating_loader: Some("app".into()),
                requester: Some("com/acme/Boot.main([Ljava/lang/String;)V".into()),
                reason: "enterprise-prefix fallback".into(),
            },
            JdkOnlyViolation::SyntheticNativeRegistered {
                class: "java/lang/Foo".into(),
                method: "bar".into(),
                descriptor: "()V".into(),
                registered_by: Some("native-builtins/src/lib.rs:1234".into()),
                survivor: None,
            },
            JdkOnlyViolation::SyntheticNativeInvocation {
                class: "java/lang/Foo".into(),
                method: "bar".into(),
                descriptor: "()V".into(),
                call_site: Some("com/acme/App.run()V".into()),
            },
            JdkOnlyViolation::MissingNative {
                class: "java/lang/Foo".into(),
                method: "baz".into(),
                descriptor: "()V".into(),
                module: Some("java.base".into()),
            },
            JdkOnlyViolation::NativeShadowsBytecode {
                class: "java/lang/Foo".into(),
                method: "qux".into(),
                descriptor: "()V".into(),
                native_kind: "bridge",
            },
            JdkOnlyViolation::MissingBootClass {
                class: "java/lang/Object".into(),
                searched_image: "C:\\jdk-25\\lib\\modules".into(),
            },
            JdkOnlyViolation::MissingImplementation {
                class: "java/lang/Foo".into(),
                method: "quux".into(),
                descriptor: "()V".into(),
            },
        ]
    }

    // ── Vocabularies are exactly the enum spellings ───────────────────────

    #[test]
    fn violation_kind_vocabulary_is_exactly_the_enum() {
        let kinds: Vec<&str> = violation_of_each_kind().iter().map(|v| v.kind()).collect();
        assert_eq!(
            kinds, VIOLATION_KINDS,
            "VIOLATION_KINDS must mirror JdkOnlyViolation::kind(), in declaration order"
        );
        assert_eq!(VIOLATION_KINDS.len(), N_VIOLATION);
    }

    /// `ClassOrigin::as_str()` lives in `classloading`, which this crate does
    /// not depend on, so the mirror is asserted against the contract's own
    /// list (design doc §5) rather than against the enum. The tags are a wire
    /// format; re-spelling one is the failure this guards.
    #[test]
    fn class_origin_vocabulary_is_the_ten_contract_tags() {
        assert_eq!(
            CLASS_ORIGINS,
            &[
                "boot-image",
                "application-class-path",
                "user-defined",
                "vm-array",
                "hidden-class",
                "generated-lambda",
                "generated-proxy",
                "reflection-accessor",
                "vm-internal",
                "compatibility-stub",
            ]
        );
        assert_eq!(CLASS_ORIGINS.len(), N_ORIGIN);
    }

    #[test]
    fn native_kind_vocabulary_is_the_three_contract_tags() {
        assert_eq!(NATIVE_KINDS, &["intrinsic", "bridge", "synthetic-stub"]);
        assert_eq!(NATIVE_KINDS.len(), N_NATIVE_KIND);
    }

    #[test]
    fn generators_are_a_subset_of_class_origins() {
        for g in GENERATORS {
            assert!(
                CLASS_ORIGINS.contains(g),
                "{g} is not a ClassOrigin tag — the generator vocabulary must not fork"
            );
        }
        assert_eq!(GENERATORS.len(), N_GENERATOR);
        // The two VM-created-but-not-generated origins stay out.
        assert!(generator_label("vm-array").is_none());
        assert!(generator_label("vm-internal").is_none());
        assert!(generator_label("boot-image").is_none());
    }

    #[test]
    fn module_allow_list_is_sorted_and_sized() {
        let mut sorted = JDK_MODULES.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted, JDK_MODULES,
            "JDK_MODULES must be sorted for binary_search"
        );
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            JDK_MODULES.len(),
            "JDK_MODULES must be unique"
        );
        assert_eq!(JDK_MODULES.len() + 2, N_MODULE);
    }

    #[test]
    fn every_counter_name_is_the_planned_spelling() {
        assert_eq!(
            COUNTER_NAMES,
            &[
                "cratonvm_jdk_only_violation_total",
                "cratonvm_class_origin_total",
                "cratonvm_native_registration_total",
                "cratonvm_native_invocation_total",
                "cratonvm_real_bytecode_shadow_attempt_total",
                "cratonvm_missing_native_total",
                "cratonvm_generated_class_total",
            ]
        );
    }

    // ── Disabled by default, and inert when disabled ──────────────────────

    #[test]
    fn default_is_disabled() {
        assert!(!JdkOnlyTelemetry::default().is_enabled());
        assert!(!JdkOnlyCounters::default().is_enabled());
    }

    #[test]
    fn disabled_counters_are_zero_and_inert() {
        let mut t = JdkOnlyTelemetry::default();
        t.add_violations(violation_of_each_kind().iter());
        t.add_class_origins(CLASS_ORIGINS.iter().copied());
        t.add_native_census(NATIVE_KINDS.iter().copied().map(|k| NativeCensusSample {
            kind: k,
            invocations: 99,
        }));
        let c = t.finish();

        assert!(!c.is_enabled());
        assert_eq!(c.unrecognised_tags(), 0);
        assert_eq!(c.real_bytecode_shadow_attempt_total(), 0);
        assert_eq!(c.missing_native_grand_total(), 0);
        assert_eq!(c.generated_class_grand_total(), 0);
        for k in VIOLATION_KINDS {
            assert_eq!(c.violation_total(k), 0, "{k}");
        }
        for o in CLASS_ORIGINS {
            assert_eq!(c.class_origin_total(o), 0, "{o}");
        }
        for k in NATIVE_KINDS {
            assert_eq!(c.native_registration_total(k), 0, "{k}");
            assert_eq!(c.native_invocation_total(k), 0, "{k}");
        }
        assert_eq!(c, JdkOnlyCounters::default());
    }

    #[test]
    fn disabled_json_is_all_zero_but_full_shape() {
        let json = JdkOnlyCounters::default().to_json();
        assert!(json.contains("\"enabled\": false"));
        // Shape is stable even with nothing measured: every label present.
        for k in VIOLATION_KINDS {
            assert!(json.contains(&format!("\"{k}\": 0")), "missing label {k}");
        }
        for m in JDK_MODULES {
            assert!(json.contains(&format!("\"{m}\": 0")), "missing module {m}");
        }
        assert!(json.contains("\"unknown\": 0"));
        assert!(json.contains("\"other\": 0"));
    }

    // ── Enabled aggregation ───────────────────────────────────────────────

    #[test]
    fn violations_fold_into_kind_shadow_and_module_counters() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_violations(violation_of_each_kind().iter());
        let c = t.finish();

        for k in VIOLATION_KINDS {
            assert_eq!(c.violation_total(k), 1, "{k}");
        }
        assert_eq!(c.real_bytecode_shadow_attempt_total(), 1);
        assert_eq!(c.missing_native_total("java.base"), 1);
        assert_eq!(c.missing_native_grand_total(), 1);
        assert_eq!(c.unrecognised_tags(), 0);
    }

    #[test]
    fn class_origins_fold_into_origin_and_generator_counters() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_class_origins([
            "boot-image",
            "boot-image",
            "generated-lambda",
            "generated-proxy",
            "hidden-class",
            "reflection-accessor",
            "vm-array",
            "vm-internal",
            "compatibility-stub",
        ]);
        let c = t.finish();

        assert_eq!(c.class_origin_total("boot-image"), 2);
        assert_eq!(c.class_origin_total("compatibility-stub"), 1);
        // Four generators, one each; vm-array and vm-internal excluded.
        assert_eq!(c.generated_class_grand_total(), 4);
        assert_eq!(c.generated_class_total("generated-lambda"), 1);
        assert_eq!(c.generated_class_total("hidden-class"), 1);
        // …but the §9 fold does include them.
        assert_eq!(c.contract_counts().generated_classes, 6);
    }

    #[test]
    fn native_census_folds_registrations_and_invocations_in_one_pass() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_native_census([
            NativeCensusSample {
                kind: "bridge",
                invocations: 100,
            },
            NativeCensusSample {
                kind: "bridge",
                // A superseded row reports 0; it still counts as a registration.
                invocations: 0,
            },
            NativeCensusSample {
                kind: "intrinsic",
                invocations: 7,
            },
            NativeCensusSample {
                kind: "synthetic-stub",
                invocations: 0,
            },
        ]);
        let c = t.finish();

        assert_eq!(c.native_registration_total("bridge"), 2);
        assert_eq!(c.native_invocation_total("bridge"), 100);
        assert_eq!(c.native_registration_total("intrinsic"), 1);
        assert_eq!(c.native_invocation_total("intrinsic"), 7);
        assert_eq!(c.native_registration_total("synthetic-stub"), 1);
        assert_eq!(c.native_invocation_total("synthetic-stub"), 0);
    }

    #[test]
    fn class_bucket_fold_is_a_partition_of_the_origin_counter() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_class_origins(CLASS_ORIGINS.iter().copied());
        let c = t.finish();
        let counts = c.contract_counts();
        let bucketed = counts.boot_image_classes
            + counts.application_classes
            + counts.generated_classes
            + counts.compatibility_classes;
        let total: u64 = c.class_origins().map(|(_, v)| v).sum();
        assert_eq!(
            bucketed, total,
            "the four §9 buckets must partition the ten origin tags"
        );
    }

    #[test]
    fn snapshot_does_not_consume_and_does_not_double_count() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_class_origins(["boot-image"]);
        let first = t.snapshot();
        t.add_class_origins(["boot-image"]);
        let second = t.snapshot();
        assert_eq!(first.class_origin_total("boot-image"), 1);
        assert_eq!(second.class_origin_total("boot-image"), 2);
    }

    // ── Label bounding ────────────────────────────────────────────────────

    #[test]
    fn unknown_module_is_bounded_to_other_and_absent_to_unknown() {
        assert_eq!(module_label(None), MODULE_LABEL_UNKNOWN);
        assert_eq!(module_label(Some("java.base")), "java.base");
        assert_eq!(module_label(Some("com.acme.internal")), MODULE_LABEL_OTHER);
        assert_eq!(
            module_label(Some("/home/victor/secret/app.jar")),
            MODULE_LABEL_OTHER
        );
        assert_eq!(
            module_label(Some("C:\\Users\\Victor\\keys")),
            MODULE_LABEL_OTHER
        );
        assert_eq!(module_label(Some("-Dpassword=hunter2")), MODULE_LABEL_OTHER);
        assert_eq!(module_label(Some("")), MODULE_LABEL_OTHER);
    }

    #[test]
    fn missing_native_label_cardinality_is_bounded() {
        let mut t = JdkOnlyTelemetry::enabled();
        for i in 0..5_000u32 {
            t.add_violation(&JdkOnlyViolation::MissingNative {
                class: "x/Y".into(),
                method: "m".into(),
                descriptor: "()V".into(),
                module: Some(format!("attacker.module.{i}")),
            });
        }
        let c = t.finish();
        // 5000 distinct inputs, one label.
        assert_eq!(c.missing_native_total(MODULE_LABEL_OTHER), 5_000);
        assert_eq!(c.missing_native_grand_total(), 5_000);
        let labels: Vec<&str> = c.missing_natives().map(|(l, _)| l).collect();
        assert_eq!(labels.len(), N_MODULE);
        assert!(!c.to_json().contains("attacker"));
    }

    /// The privacy rule, stated as a test: feed violations whose every string
    /// field is a path, a class name, a command-line argument or an
    /// environment value, and assert that none of them reaches the JSON.
    #[test]
    fn no_path_or_argument_like_string_can_reach_a_label() {
        let secrets = [
            "/home/victor/apps/spring-boot.jar",
            "C:\\craton\\wt-jdk-only\\target\\release\\cratonvm.exe",
            "native-builtins/src/lib.rs:1234",
            "-Djavax.net.ssl.trustStorePassword=hunter2",
            "com/acme/PatientRecordDao",
            "AWS_SECRET_ACCESS_KEY=AKIA0000",
        ];
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_violation(&JdkOnlyViolation::CompatibilityClassRequested {
            class: secrets[4].into(),
            initiating_loader: Some(secrets[0].into()),
            requester: Some(secrets[1].into()),
            reason: secrets[3].into(),
        });
        t.add_violation(&JdkOnlyViolation::SyntheticNativeRegistered {
            class: secrets[4].into(),
            method: "m".into(),
            descriptor: "()V".into(),
            registered_by: Some(secrets[2].into()),
            survivor: None,
        });
        t.add_violation(&JdkOnlyViolation::MissingNative {
            class: secrets[4].into(),
            method: "m".into(),
            descriptor: "()V".into(),
            module: Some(secrets[5].into()),
        });
        t.add_violation(&JdkOnlyViolation::MissingBootClass {
            class: secrets[4].into(),
            searched_image: secrets[1].into(),
        });
        t.add_class_origins([secrets[0], secrets[4]]);
        t.add_native_census([NativeCensusSample {
            kind: secrets[2],
            invocations: 1,
        }]);

        let counters = t.finish();
        let json = counters.to_json();
        for s in secrets {
            assert!(
                !json.contains(s),
                "{s} leaked into the counter block:\n{json}"
            );
        }
        // Nothing path-like at all: no separators, no drive letters, no
        // property assignments. (A single `\` does occur, in the escaped
        // `kind=\"bridge\"` of the lower-bound series, so the check is for the
        // two-character drive prefix rather than for the backslash itself.)
        assert!(!json.contains('/'), "a path separator reached the JSON");
        assert!(!json.contains(":\\"), "a drive letter reached the JSON");
        assert!(!json.contains("-D"), "a -D property reached the JSON");

        // Stronger than "the secrets are absent": every label the snapshot can
        // emit is a member of one of the closed vocabularies.
        let known = |l: &str| {
            VIOLATION_KINDS.contains(&l)
                || CLASS_ORIGINS.contains(&l)
                || NATIVE_KINDS.contains(&l)
                || GENERATORS.contains(&l)
                || JDK_MODULES.contains(&l)
                || l == MODULE_LABEL_OTHER
                || l == MODULE_LABEL_UNKNOWN
        };
        let emitted = counters
            .violations()
            .chain(counters.class_origins())
            .chain(counters.native_registrations())
            .chain(counters.native_invocations())
            .chain(counters.missing_natives())
            .chain(counters.generated_classes());
        for (label, _) in emitted {
            assert!(known(label), "{label} is not in any closed vocabulary");
        }
    }

    #[test]
    fn unrecognised_tags_are_counted_never_echoed() {
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_class_origins(["not-a-real-origin", "/etc/passwd"]);
        t.add_native_census([NativeCensusSample {
            kind: "not-a-real-kind",
            invocations: 42,
        }]);
        let c = t.finish();
        assert_eq!(c.unrecognised_tags(), 3);
        // The unknown native row contributes no invocations anywhere.
        let total: u64 = c.native_invocations().map(|(_, v)| v).sum();
        assert_eq!(total, 0);
        let json = c.to_json();
        assert!(!json.contains("not-a-real"));
        assert!(!json.contains("passwd"));
    }

    // ── Versioning and JSON shape ─────────────────────────────────────────

    #[test]
    fn version_is_present_and_stable() {
        assert_eq!(JDK_ONLY_TELEMETRY_SCHEMA_VERSION, 1);
        let c = JdkOnlyTelemetry::enabled().finish();
        assert_eq!(c.schema_version(), JDK_ONLY_TELEMETRY_SCHEMA_VERSION);
        assert!(c.to_json().contains(&format!(
            "\"counter_schema_version\": {JDK_ONLY_TELEMETRY_SCHEMA_VERSION}"
        )));
    }

    #[test]
    fn json_shape_is_run_to_run_stable() {
        let empty = JdkOnlyTelemetry::enabled().finish().to_json();
        let mut t = JdkOnlyTelemetry::enabled();
        t.add_violations(violation_of_each_kind().iter());
        t.add_class_origins(CLASS_ORIGINS.iter().copied());
        let busy = t.finish().to_json();
        // Same number of lines and the same keys in the same order: two CI
        // artefacts differ on values only.
        assert_eq!(empty.lines().count(), busy.lines().count());
        let keys = |s: &str| -> Vec<String> {
            s.lines()
                .filter_map(|l| l.split(':').next().map(|k| k.trim().to_string()))
                .collect()
        };
        assert_eq!(keys(&empty), keys(&busy));
    }

    #[test]
    fn json_names_every_counter() {
        let json = JdkOnlyTelemetry::enabled().finish().to_json();
        for name in COUNTER_NAMES {
            assert!(json.contains(&format!("\"{name}\"")), "missing {name}");
        }
    }

    #[test]
    fn lower_bound_is_emitted_as_data() {
        let c = JdkOnlyTelemetry::enabled().finish();
        assert!(c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "bridge"));
        assert!(c.is_lower_bound(COUNTER_NATIVE_REGISTRATION_TOTAL, "bridge"));
        // The exact ones must not be labelled as floors.
        assert!(!c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "synthetic-stub"));
        assert!(!c.is_lower_bound(COUNTER_NATIVE_INVOCATION_TOTAL, "intrinsic"));
        assert!(!c.is_lower_bound(COUNTER_VIOLATION_TOTAL, "missing-native"));

        let json = c.to_json();
        assert!(json.contains("\"lower_bound\""));
        assert!(json.contains("kind=\\\"bridge\\\""));
        assert!(json.contains("lower_bound_reason"));
        assert_eq!(c.lower_bound_labels().len(), 2);
    }

    // ── Opt-in ────────────────────────────────────────────────────────────

    /// The gate is the existing §9 command-line surface, not a new environment
    /// variable. If someone adds one, `types`'
    /// `jdk_only_adds_no_environment_variable` fails too — this asserts the
    /// other half: that the flags we gate on are real.
    #[test]
    fn enabling_flags_are_real_contract_flags() {
        for f in TELEMETRY_ENABLING_FLAGS {
            assert!(
                cratonvm_types::flag_groups::jdk_only_flag(f).is_some(),
                "{f} is not one of JDK_ONLY_CLI_FLAGS"
            );
            assert!(!f.starts_with("CRATONVM"), "{f} looks like an env var");
        }
    }

    #[test]
    fn telemetry_is_off_unless_an_artefact_flag_is_present() {
        assert!(!enabled_by_args(["-cp", "app.jar", "com.acme.Main"]));
        // Selecting the policy is not asking to be measured.
        assert!(!enabled_by_args(["--jdk-only", "com.acme.Main"]));
        assert!(!enabled_by_args(["--explain-jdk-only"]));

        assert!(enabled_by_args([
            "--jdk-only",
            "--jdk-only-report",
            "r.json"
        ]));
        assert!(enabled_by_args(["--dump-class-origins=o.json"]));
        assert!(enabled_by_args(["--trace-jdk-only"]));

        assert!(!JdkOnlyTelemetry::new(false).is_enabled());
        assert!(JdkOnlyTelemetry::new(enabled_by_args(["--trace-jdk-only"])).is_enabled());
    }

    #[test]
    fn near_miss_flags_do_not_open_the_gate() {
        assert!(!enabled_by_flag("--jdk-only-reports"));
        assert!(!enabled_by_flag("-jdk-only-report"));
        assert!(!enabled_by_flag(""));
        assert!(!enabled_by_flag("--dump-class-origin"));
    }
}
