// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! VM error types and the two-layer exception model.
//!
//! Defines [`MethodCallFailed`] — the result of a failed Java method call,
//! split into non-catchable internal VM errors and catchable Java exceptions —
//! along with the [`VmError`] hierarchy ([`ClassFileError`], [`LinkageError`],
//! [`RuntimeError`]) and the [`MethodCallResult`] alias used throughout the VM.
//!
//! Also home to [`JdkOnlyViolation`], the structured `--jdk-only` refusal
//! defined by `docs/feature-designs/jdk-only-mode.md` §3. It lives here rather
//! than beside [`crate::compat::CompatibilityMode`] because it is an *error*
//! that `VmError` carries, and because every crate that can raise one already
//! depends on this module.

use std::borrow::Cow;
use std::fmt;

use thiserror::Error;

use crate::{ObjectRef, Value};

// ---------------------------------------------------------------------------
// MethodCallFailed -- the two-layer exception model
// ---------------------------------------------------------------------------

/// The result of executing a Java method.
///
/// - `Ok(Some(value))` -- method returned a value (non-void)
/// - `Ok(None)` -- method returned void
/// - `Err(MethodCallFailed)` -- method failed (either internal error or Java exception)
pub type MethodCallResult = Result<Option<Value>, MethodCallFailed>;

/// How a method call can fail.
///
/// This is the core of the exception model:
/// - **`InternalError`**: a Rust-level VM bug or fatal error. These are **not**
///   catchable by Java `catch` blocks. They abort execution entirely.
/// - **`ExceptionThrown`**: a Java exception was thrown. The `ObjectRef` points
///   to a heap-allocated `Throwable` object that can be caught by Java exception
///   handlers.
#[derive(Debug)]
pub enum MethodCallFailed {
    /// Internal VM error (not catchable by Java code).
    InternalError(VmError),

    /// A Java exception was thrown (can be caught by exception handlers).
    /// The `ObjectRef` points to the `Throwable` object on the heap.
    ExceptionThrown(ObjectRef),
}

impl fmt::Display for MethodCallFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // `VmError` is already a self-describing error: every variant's
            // `Display` carries its own category prefix ("class file error: ",
            // "linkage error: ", "runtime error: ", "internal error: ").
            // Prepending another "internal error: " here doubled the prefix
            // for `VmError::Internal` (yielding "internal error: internal
            // error: ...") and mislabeled the other variants. Delegate to the
            // inner error's `Display` so the category appears exactly once.
            MethodCallFailed::InternalError(err) => write!(f, "{err}"),
            MethodCallFailed::ExceptionThrown(obj_ref) => {
                write!(f, "exception thrown: ref({:p})", obj_ref.as_ptr())
            }
        }
    }
}

impl From<VmError> for MethodCallFailed {
    fn from(err: VmError) -> Self {
        MethodCallFailed::InternalError(err)
    }
}

impl From<ClassFileError> for MethodCallFailed {
    fn from(err: ClassFileError) -> Self {
        MethodCallFailed::InternalError(VmError::ClassFile(err))
    }
}

impl From<LinkageError> for MethodCallFailed {
    fn from(err: LinkageError) -> Self {
        MethodCallFailed::InternalError(VmError::Linkage(err))
    }
}

impl From<RuntimeError> for MethodCallFailed {
    fn from(err: RuntimeError) -> Self {
        MethodCallFailed::InternalError(VmError::Runtime(err))
    }
}

// ---------------------------------------------------------------------------
// VmError -- the existing error hierarchy
// ---------------------------------------------------------------------------

/// Top-level VM error categories.
#[derive(Debug, Error)]
pub enum VmError {
    /// Error reading or parsing a `.class` file.
    #[error("class file error: {0}")]
    ClassFile(#[from] ClassFileError),

    /// Error during class linking (verification, preparation, resolution).
    #[error("linkage error: {0}")]
    Linkage(#[from] LinkageError),

    /// Runtime error during bytecode execution.
    #[error("runtime error: {0}")]
    Runtime(#[from] RuntimeError),

    /// Internal VM error (bug in the implementation).
    #[error("internal error: {message}")]
    Internal { message: String },

    /// The requested configuration is self-contradictory and the VM refused to
    /// start. `--jdk-only` together with `--synthetic-jdk` is the case this was
    /// added for (`VmConfig::validate_compatibility`), but the variant is
    /// deliberately general: it is the "you asked for two things that cannot
    /// both be true" error, distinct from `Internal`, which means *we* have a
    /// bug.
    #[error("invalid configuration: {0}")]
    InvalidConfiguration(String),

    /// A `--jdk-only` policy violation ([`JdkOnlyViolation`]).
    ///
    /// Deliberately **not** `#[from]`. thiserror's `#[from]` also makes the
    /// field the error's `source()`, which requires
    /// `JdkOnlyViolation: std::error::Error`; the contract gives the violation
    /// `Display` only (it is a report, not a chained cause), so the conversion
    /// is hand-written below instead.
    #[error("jdk-only violation: {0}")]
    JdkOnly(JdkOnlyViolation),

    /// Non-failure scheduler control transfer used to unmount an unpinned
    /// virtual thread. It is wrapped in `MethodCallFailed::InternalError`
    /// solely to travel through native/interpreter return types and is
    /// intercepted before any Java exception boundary.
    #[error("virtual-thread continuation yielded for {wake_after_nanos}ns")]
    ContinuationYield { wake_after_nanos: u64 },
}

impl From<JdkOnlyViolation> for VmError {
    /// Hand-written rather than `#[from]`.
    ///
    /// `#[from]` would additionally register the field as the error's
    /// `source()`, which makes thiserror require
    /// `JdkOnlyViolation: std::error::Error`. The contract
    /// (`docs/feature-designs/jdk-only-mode.md` §3) gives the violation
    /// `Display` only — it is a structured *report* the launcher renders, not a
    /// cause in an error chain — so the bound cannot be satisfied without
    /// widening the contract. This impl gives callers the same `?`/`.into()`
    /// ergonomics with none of that.
    fn from(violation: JdkOnlyViolation) -> Self {
        VmError::JdkOnly(violation)
    }
}

// ---------------------------------------------------------------------------
// JdkOnlyViolation -- the `--jdk-only` structured refusal
// ---------------------------------------------------------------------------

/// The stand-in for any value the VM could not determine.
const UNKNOWN: &str = "<unknown>";

/// What an absolute path is replaced with when `verbose` is false.
const REDACTED: &str = "<redacted>";

/// Offered only on a report that actually redacted something.
const REMEDIATION_EXPLAIN: &str =
    "re-run with --explain-jdk-only to print absolute paths unredacted";

/// The penultimate line of every [`JdkOnlyViolation::render`]. Pinned: §1.7
/// requires every strict-mode failure to name the fallback, and an operator
/// scrolled to the bottom of a wall of diagnostics must find it in the same
/// place every time.
const REMEDIATION_FALLBACK: &str =
    "re-run with --real-jdk to restore the current compatibility behaviour";

/// The final line of every [`JdkOnlyViolation::render`]. Pinned for the same
/// reason, and because a violation an operator cannot capture is a violation
/// that arrives in a bug report as a screenshot.
const REMEDIATION_CAPTURE: &str =
    "capture the full machine-readable report with --jdk-only-report <FILE>";

/// The one `native_kind` spelling on a
/// [`JdkOnlyViolation::NativeShadowsBytecode`] row that means **the native
/// won** — it dispatched in front of real class bytes and the bytecode never
/// ran.
///
/// # Why this constant is here and not in `vm`
///
/// It was in `vm/src/vm/vm_exec.rs` (`JDK_ONLY_SHADOW_UNENFORCED_TAG`, which now
/// aliases this), and being there is what let the report ship a row whose
/// human-readable text said the opposite of what the row meant. `types` owns
/// `summary()`, `detail()` and `to_json()` for this variant, so `types` is where
/// the outcome has to be decidable — otherwise every consumer re-derives it, and
/// the first one to get it wrong does so silently.
///
/// # The reading this exists to make impossible
///
/// `jdk-only/G60-1-what-jdk-only-still-overrides-RESOLVED-20260817.md`
/// §1 split one `RJdkReflBox` census into
/// `58 bridge-ran-over-bytecode` and `21 bridge`, and glossed the second group
/// as *"registered over bytecode; not observed running"* — then nominated all 21
/// for measurement as *"neither safe nor unsafe today — they are unmeasured"*.
/// The 21 are the opposite of unmeasured: each one is a triple where strict mode
/// sent a dispatch to the REAL bytecode and the bridge lost, recorded from the
/// yield path itself. The row said `bridge` because that is
/// `NativeKind::as_str()`, and nothing in the row said which side won.
///
/// So the tag is a discriminator, not a kind, and it is the ONLY one: every
/// other spelling (`bridge`, `synthetic-stub`, `jit-thin-direct-helper`) is
/// recorded where the native yielded. See [`JdkOnlyViolation::shadow_outcome`].
pub const NATIVE_SHADOW_RAN_TAG: &str = "bridge-ran-over-bytecode";

/// `"outcome"` for a row where the registered native ran and the real bytes did
/// not — the §1.4 violation that actually took effect.
pub const SHADOW_OUTCOME_NATIVE_WON: &str = "native-won";

/// `"outcome"` for a row where the registered native lost to the real bytes —
/// §1.4 enforced. Still a mis-tagged registration worth removing, and **not** a
/// behavioural defect in this run.
pub const SHADOW_OUTCOME_BYTECODE_WON: &str = "bytecode-won";

/// A JDK-only policy violation. All fields are owned/plain so `types` needs no
/// dependency on `native-api` or `classloading` — `NativeKind` and
/// `ClassOrigin` are carried as their own `as_str()` spellings, never as the
/// enums, which is what keeps the crate layering in §2 acyclic.
///
/// `PartialEq` is load-bearing: `jit/src/lib.rs` de-duplicates recorded
/// violations with `Vec::contains` before pushing, so that a hot refused call
/// site cannot fill the bounded buffer with one repeated entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JdkOnlyViolation {
    /// A class with no real bytes anywhere was about to be fabricated (§1.2).
    CompatibilityClassRequested {
        class: String,
        initiating_loader: Option<String>,
        /// `"owner/Class.method(Desc)"` — the site that asked for the class.
        requester: Option<String>,
        reason: String,
    },
    /// A `NativeKind::SyntheticStub` was offered to the registry (§1.3).
    SyntheticNativeRegistered {
        class: String,
        method: String,
        descriptor: String,
        /// Registration site, captured by `#[track_caller]` in the registry.
        registered_by: Option<String>,
    },
    /// A `NativeKind::SyntheticStub` was about to be dispatched (§1.3).
    SyntheticNativeInvocation {
        class: String,
        method: String,
        descriptor: String,
        call_site: Option<String>,
    },
    /// An `ACC_NATIVE` method has no bridge and no reviewed intrinsic (§1.5).
    /// Never answered with a stub.
    MissingNative {
        class: String,
        method: String,
        descriptor: String,
        module: Option<String>,
    },
    /// A registered native stood in front of concrete Java bytecode for the
    /// same method, and is not a reviewed intrinsic (§1.4).
    ///
    /// **One variant, two OPPOSITE outcomes**, and [`native_shadow_outcome`] is
    /// the only thing that tells them apart. Every producer records the same
    /// §1.4 shape, but three of the four record it on the *yield* path — the
    /// native lost and the real bytes ran — while one records it on the path
    /// where the native WON. Read [`Self::shadow_outcome`] before drawing any
    /// conclusion from a row of this kind; the field below cannot be read as a
    /// kind alone.
    NativeShadowsBytecode {
        class: String,
        method: String,
        descriptor: String,
        /// `NativeKind::as_str()`, or the VM mechanism when the reporting crate
        /// cannot see that enum (the JIT's thin direct helpers) — plus the one
        /// value that is neither, [`NATIVE_SHADOW_RAN_TAG`], which is how the
        /// "the native won" outcome is spelled.
        native_kind: &'static str,
    },
    /// A boot class was not present in the real runtime image (§1.1).
    MissingBootClass {
        class: String,
        searched_image: String,
    },
    /// A method has neither a `Code` attribute nor an admissible native, so
    /// there is nothing to run (§7 step 4).
    MissingImplementation {
        class: String,
        method: String,
        descriptor: String,
    },
}

impl JdkOnlyViolation {
    /// Stable kind tag for counters and JSON.
    ///
    /// **Wire format.** It is the `"kind"` field of every `--jdk-only-report`
    /// violation row (`difftest/src/census.rs` tallies by exactly this string)
    /// and the label the dispatch tests assert on. Do not re-spell these.
    pub fn kind(&self) -> &'static str {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested { .. } => "compatibility-class-requested",
            JdkOnlyViolation::SyntheticNativeRegistered { .. } => "synthetic-native-registered",
            JdkOnlyViolation::SyntheticNativeInvocation { .. } => "synthetic-native-invocation",
            JdkOnlyViolation::MissingNative { .. } => "missing-native",
            JdkOnlyViolation::NativeShadowsBytecode { .. } => "native-shadows-bytecode",
            JdkOnlyViolation::MissingBootClass { .. } => "missing-boot-class",
            JdkOnlyViolation::MissingImplementation { .. } => "missing-implementation",
        }
    }

    /// Which side of §1.4 actually ran, for the one variant where that is a
    /// question — `None` for every other variant.
    ///
    /// [`SHADOW_OUTCOME_NATIVE_WON`] is the violation that took effect;
    /// [`SHADOW_OUTCOME_BYTECODE_WON`] is §1.4 being enforced, recorded because
    /// the registration is still over-tagged and worth removing. **Both wear
    /// `kind() == "native-shadows-bytecode"`**, which is why a consumer that
    /// tallies by `kind()` alone — `difftest/src/census.rs` does — is counting
    /// two opposite facts as one, and why this accessor exists rather than a
    /// comment telling readers to compare strings themselves.
    ///
    /// The discriminator is [`NATIVE_SHADOW_RAN_TAG`], and the polarity is
    /// deliberate: the "native won" spelling is a single closed value, so a NEW
    /// producer that forgets about outcomes is classified `bytecode-won` — the
    /// reading that under-states a violation rather than inventing one. A new
    /// native-won producer must therefore say so explicitly, which is the whole
    /// point.
    pub fn shadow_outcome(&self) -> Option<&'static str> {
        match self {
            JdkOnlyViolation::NativeShadowsBytecode { native_kind, .. } => {
                Some(if *native_kind == NATIVE_SHADOW_RAN_TAG {
                    SHADOW_OUTCOME_NATIVE_WON
                } else {
                    SHADOW_OUTCOME_BYTECODE_WON
                })
            }
            _ => None,
        }
    }

    /// The class every variant names.
    pub fn class(&self) -> &str {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested { class, .. }
            | JdkOnlyViolation::SyntheticNativeRegistered { class, .. }
            | JdkOnlyViolation::SyntheticNativeInvocation { class, .. }
            | JdkOnlyViolation::MissingNative { class, .. }
            | JdkOnlyViolation::NativeShadowsBytecode { class, .. }
            | JdkOnlyViolation::MissingBootClass { class, .. }
            | JdkOnlyViolation::MissingImplementation { class, .. } => class.as_str(),
        }
    }

    /// `(method, descriptor)` for the six variants that name a member.
    ///
    /// The descriptor is not decoration: it is the only thing that
    /// distinguishes an overload, and a refusal that cannot be turned back into
    /// a single method is not actionable (§1.7).
    pub fn member(&self) -> Option<(&str, &str)> {
        match self {
            JdkOnlyViolation::SyntheticNativeRegistered {
                method, descriptor, ..
            }
            | JdkOnlyViolation::SyntheticNativeInvocation {
                method, descriptor, ..
            }
            | JdkOnlyViolation::MissingNative {
                method, descriptor, ..
            }
            | JdkOnlyViolation::NativeShadowsBytecode {
                method, descriptor, ..
            }
            | JdkOnlyViolation::MissingImplementation {
                method, descriptor, ..
            } => Some((method.as_str(), descriptor.as_str())),
            JdkOnlyViolation::CompatibilityClassRequested { .. }
            | JdkOnlyViolation::MissingBootClass { .. } => None,
        }
    }

    /// Whoever asked: the requesting method, the registrar's source line, or
    /// the call site, depending on the variant.
    fn requested_from(&self) -> Option<&str> {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested { requester, .. } => requester.as_deref(),
            JdkOnlyViolation::SyntheticNativeRegistered { registered_by, .. } => {
                registered_by.as_deref()
            }
            JdkOnlyViolation::SyntheticNativeInvocation { call_site, .. } => call_site.as_deref(),
            _ => None,
        }
    }

    /// The module the member belongs to, when the reporter knew it.
    fn module(&self) -> Option<&str> {
        match self {
            JdkOnlyViolation::MissingNative { module, .. } => module.as_deref(),
            _ => None,
        }
    }

    /// Why the policy refused. Only `CompatibilityClassRequested` carries a
    /// caller-supplied reason; the rest are a property of the kind itself.
    fn reason(&self) -> String {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested { reason, .. } => reason.clone(),
            JdkOnlyViolation::SyntheticNativeRegistered { .. } => {
                "a synthetic-stub native may not be registered under --jdk-only".to_string()
            }
            JdkOnlyViolation::SyntheticNativeInvocation { .. } => {
                "a synthetic-stub native may not be invoked under --jdk-only".to_string()
            }
            JdkOnlyViolation::MissingNative { .. } => {
                "the method is ACC_NATIVE and no bridge or reviewed intrinsic is bound".to_string()
            }
            // Two outcomes, two sentences. This line used to end "concrete
            // bytecode wins under --jdk-only" for BOTH, which is the promise
            // §1.4 makes and not what happened on the rows tagged
            // `NATIVE_SHADOW_RAN_TAG` — there the native won and the real bytes
            // never ran. A `reason` that states policy where it should state
            // measurement is how a report gets read backwards.
            JdkOnlyViolation::NativeShadowsBytecode { native_kind, .. }
                if *native_kind == NATIVE_SHADOW_RAN_TAG =>
            {
                "a registered bridge stood in front of the real class bytes and RAN; \
                 §1.4 was observed but not enforced for this dispatch"
                    .to_string()
            }
            JdkOnlyViolation::NativeShadowsBytecode { native_kind, .. } => format!(
                "a registered {native_kind} native stands in front of the real class \
                 bytes; concrete bytecode won this dispatch under --jdk-only"
            ),
            JdkOnlyViolation::MissingBootClass { searched_image, .. } => {
                format!("no real class bytes for this boot class in {searched_image}")
            }
            JdkOnlyViolation::MissingImplementation { .. } => {
                "the method has no Code attribute and no admissible native".to_string()
            }
        }
    }

    /// The variant-specific remediation lines, in order. The two fixed lines
    /// are appended by [`Self::render`] and are not repeated here.
    fn remediation(&self) -> &'static [&'static str] {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested { .. } => &[
                "put the real class on the class path, or drop the dependency that needs it",
                "list every fabrication this run wanted with --dump-class-origins <FILE>",
            ],
            JdkOnlyViolation::SyntheticNativeRegistered { .. } => {
                &["reclassify the registration as a Bridge or a reviewed Intrinsic, or delete it"]
            }
            JdkOnlyViolation::SyntheticNativeInvocation { .. } => {
                &["the real JDK implements this method; check why its bytes were not loaded"]
            }
            JdkOnlyViolation::MissingNative { .. } => {
                &["implement the method as a NativeKind::Bridge and register it at VM init"]
            }
            // The native-won rows need the extra line, and it is not advice:
            // retirement through `native-api`'s `retired_shadow` table is the
            // mechanism that already exists for exactly this, and a reader who
            // does not know that reaches for a dispatch-time allow-list instead
            // — which AGENTS.md forbids and which this tree has several
            // disagreeing copies of already.
            JdkOnlyViolation::NativeShadowsBytecode { native_kind, .. }
                if *native_kind == NATIVE_SHADOW_RAN_TAG =>
            {
                &[
                    "unregister the native, or have it reviewed and reclassified as an Intrinsic",
                    "to retire it for strict mode only, add the triple to \
                     native-api's retired_shadow table — do not add a dispatch-time \
                     class-name allow-list",
                ]
            }
            JdkOnlyViolation::NativeShadowsBytecode { .. } => {
                &["unregister the native, or have it reviewed and reclassified as an Intrinsic"]
            }
            JdkOnlyViolation::MissingBootClass { .. } => {
                &["point --jdk-home at a complete JDK runtime image (one with lib/modules)"]
            }
            JdkOnlyViolation::MissingImplementation { .. } => &[
                "the resolved method is abstract or bodiless; check the dispatch that reached it",
            ],
        }
    }

    /// One-line form for logs and for the message of the Java-visible error the
    /// JNI layer raises.
    ///
    /// Deliberately **not** redacted: it is a log line, and the caller that
    /// wants redaction wants [`Self::render`].
    pub fn summary(&self) -> String {
        match self {
            JdkOnlyViolation::CompatibilityClassRequested {
                class, requester, ..
            } => match requester {
                Some(from) => format!("compatibility class requested: {class} (from {from})"),
                None => format!("compatibility class requested: {class}"),
            },
            JdkOnlyViolation::SyntheticNativeRegistered {
                class,
                method,
                descriptor,
                ..
            } => format!("synthetic-stub native registered: {class}.{method}{descriptor}"),
            JdkOnlyViolation::SyntheticNativeInvocation {
                class,
                method,
                descriptor,
                ..
            } => format!("synthetic-stub native invoked: {class}.{method}{descriptor}"),
            JdkOnlyViolation::MissingNative {
                class,
                method,
                descriptor,
                ..
            } => format!("no native bound for {class}.{method}{descriptor}"),
            // The outcome is APPENDED rather than woven in, on purpose: the
            // leading `"{native_kind} native shadows bytecode of {triple}"` is
            // what every existing grep, log-scrape and sort key in the tree
            // matches on, and this is a summary line, not a place to break them.
            // What it adds is the half a reader cannot otherwise get — G60-1 §1
            // read 21 of these rows as "not observed running" when each one is a
            // recorded dispatch that went to the real bytes.
            JdkOnlyViolation::NativeShadowsBytecode {
                class,
                method,
                descriptor,
                native_kind,
            } => format!(
                "{native_kind} native shadows bytecode of {class}.{method}{descriptor} \
                 [{}]",
                self.shadow_outcome().unwrap_or(SHADOW_OUTCOME_BYTECODE_WON)
            ),
            JdkOnlyViolation::MissingBootClass {
                class,
                searched_image,
            } => format!("boot class not found: {class} (searched {searched_image})"),
            JdkOnlyViolation::MissingImplementation {
                class,
                method,
                descriptor,
            } => format!("no implementation for {class}.{method}{descriptor}"),
        }
    }

    /// Multi-line operator-facing report.
    ///
    /// Layout: the requested class (and member), who asked, why it was refused,
    /// a JDK block, then a `Remediation:` block whose **last two lines are
    /// fixed** — the `--real-jdk` fallback, then the `--jdk-only-report`
    /// capture hint.
    ///
    /// Absolute paths are replaced with `<redacted>` unless `verbose` (which
    /// `--explain-jdk-only` sets). Relative paths — the shape of every
    /// `#[track_caller]` provenance string, e.g.
    /// `native-builtins/src/lib.rs:1234` — pass through untouched, because
    /// those are the ones a reader actually needs and they leak nothing about
    /// the machine the run happened on.
    /// The loader that initiated the request, for the variants that record one.
    ///
    /// Only `CompatibilityClassRequested` carries it: it is the only refusal
    /// where "who asked" is a loader rather than a call site.
    pub fn initiating_loader(&self) -> Option<&str> {
        match self {
            Self::CompatibilityClassRequested {
                initiating_loader, ..
            } => initiating_loader.as_deref(),
            _ => None,
        }
    }

    pub fn render(&self, jdk_feature: Option<u32>, verbose: bool) -> String {
        let mut out = String::new();
        // The headline names the mode that refused, not just the rule. A report
        // pasted into a tracker has to say "this VM was run with --jdk-only"
        // before anything else, or the first reply is always "run it normally
        // then" — which is exactly the fallback the last line already offers.
        // `docs/jdk-only-migration.md` shows the same shape.
        out.push_str(&format!(
            "CratonVM --jdk-only: policy violation [{}]\n",
            self.kind()
        ));
        out.push_str(&format!(
            "  requested class:   {}\n",
            redact_paths(self.class(), verbose)
        ));
        if let Some((method, descriptor)) = self.member() {
            out.push_str(&format!("  requested member:  {method}{descriptor}\n"));
        }
        out.push_str(&format!(
            "  requested from:    {}\n",
            match self.requested_from() {
                Some(from) => redact_paths(from, verbose),
                None => UNKNOWN.to_string(),
            }
        ));
        // The initiating loader is half the answer to "why did this class not
        // resolve": the same name resolves differently through the app loader
        // and through a module layer, and a fabrication refusal is nearly
        // always a story about which one asked. `to_json` has always carried
        // it; the long form dropped it, which is the wrong way round — the
        // long form is the one a human reads.
        if let Some(loader) = self.initiating_loader() {
            out.push_str(&format!(
                "  initiating loader: {}\n",
                redact_paths(loader, verbose)
            ));
        }
        out.push_str(&format!(
            "  reason:            {}\n",
            redact_paths(&self.reason(), verbose)
        ));
        out.push_str("  JDK:\n");
        out.push_str(&format!(
            "    feature version: {}\n",
            match jdk_feature {
                Some(v) => v.to_string(),
                None => UNKNOWN.to_string(),
            }
        ));
        out.push_str(&format!(
            "    java.home:       {}\n",
            match java_home() {
                Some(home) => redact_paths(&home, verbose),
                None => UNKNOWN.to_string(),
            }
        ));
        out.push_str(&format!(
            "    module:          {}\n",
            self.module().unwrap_or(UNKNOWN)
        ));
        out.push_str("  Remediation:\n");
        for line in self.remediation() {
            out.push_str(&format!("    {line}\n"));
        }
        // Only when something was actually redacted: a reader who can see the
        // paths does not need to be told how to see them, and an unconditional
        // line trains people to ignore it.
        if !verbose && out.contains(REDACTED) {
            out.push_str(&format!("    {REMEDIATION_EXPLAIN}\n"));
        }
        out.push_str(&format!("    {REMEDIATION_FALLBACK}\n"));
        out.push_str(&format!("    {REMEDIATION_CAPTURE}\n"));
        out
    }

    /// One `violations[]` element of the `--jdk-only-report` JSON.
    ///
    /// Hand-rolled to match the existing dump style (`types` has no serde). The
    /// object is internally tagged: `kind` first, then `summary`, then the
    /// variant's own fields in declaration order. Absent optionals are emitted
    /// as `null` rather than omitted, so every row of a given kind has the same
    /// shape and a consumer can index columns without probing.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        let mut first = true;
        let summary = self.summary();
        out.push('{');
        json_field(&mut out, &mut first, "kind", Some(self.kind()));
        json_field(&mut out, &mut first, "summary", Some(summary.as_str()));
        match self {
            JdkOnlyViolation::CompatibilityClassRequested {
                class,
                initiating_loader,
                requester,
                reason,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "initiating_loader",
                    initiating_loader.as_deref(),
                );
                json_field(&mut out, &mut first, "requester", requester.as_deref());
                json_field(&mut out, &mut first, "reason", Some(reason.as_str()));
            }
            JdkOnlyViolation::SyntheticNativeRegistered {
                class,
                method,
                descriptor,
                registered_by,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(&mut out, &mut first, "method", Some(method.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "descriptor",
                    Some(descriptor.as_str()),
                );
                json_field(
                    &mut out,
                    &mut first,
                    "registered_by",
                    registered_by.as_deref(),
                );
            }
            JdkOnlyViolation::SyntheticNativeInvocation {
                class,
                method,
                descriptor,
                call_site,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(&mut out, &mut first, "method", Some(method.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "descriptor",
                    Some(descriptor.as_str()),
                );
                json_field(&mut out, &mut first, "call_site", call_site.as_deref());
            }
            JdkOnlyViolation::MissingNative {
                class,
                method,
                descriptor,
                module,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(&mut out, &mut first, "method", Some(method.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "descriptor",
                    Some(descriptor.as_str()),
                );
                json_field(&mut out, &mut first, "module", module.as_deref());
            }
            JdkOnlyViolation::NativeShadowsBytecode {
                class,
                method,
                descriptor,
                native_kind,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(&mut out, &mut first, "method", Some(method.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "descriptor",
                    Some(descriptor.as_str()),
                );
                json_field(&mut out, &mut first, "native_kind", Some(*native_kind));
                // The field a machine consumer needs and could not compute:
                // `native_kind` is a kind on three of the four producers and a
                // discriminator on the fourth, so deriving the outcome from it
                // means hard-coding `NATIVE_SHADOW_RAN_TAG` in every reader.
                // Emitted for every row of this kind, never conditionally — an
                // absent `outcome` would be indistinguishable from
                // `bytecode-won`, which is the direction that hides a violation.
                json_field(&mut out, &mut first, "outcome", self.shadow_outcome());
            }
            JdkOnlyViolation::MissingBootClass {
                class,
                searched_image,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "searched_image",
                    Some(searched_image.as_str()),
                );
            }
            JdkOnlyViolation::MissingImplementation {
                class,
                method,
                descriptor,
            } => {
                json_field(&mut out, &mut first, "class", Some(class.as_str()));
                json_field(&mut out, &mut first, "method", Some(method.as_str()));
                json_field(
                    &mut out,
                    &mut first,
                    "descriptor",
                    Some(descriptor.as_str()),
                );
            }
        }
        out.push('}');
        out
    }
}

impl fmt::Display for JdkOnlyViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

/// The JDK image location to name in the report's JDK block.
///
/// `CRATONVM_JAVA_HOME` is the VM's own override and wins over the ambient
/// `JAVA_HOME`, matching the launcher's precedence. Both go through
/// [`crate::flags::runtime_var_os`], which is what makes the reported path the
/// one the VM was *configured* with: `CRATONVM_JAVA_HOME` is a declared scalar,
/// so it is served from the immutable snapshot — including the case where a
/// launcher supplied it via `flags::install` and never touched `environ` at
/// all, which a raw `std::env` read would miss entirely. `JAVA_HOME` is not
/// declared, so it keeps live `getenv` semantics through the same call.
///
/// This previously read `std::env` directly, on the grounds that binding a
/// diagnostic to a latching snapshot would make its text depend on who read a
/// flag first. That is the wrong way round: the snapshot is fixed for the life
/// of the process, whereas `environ` is rewritten in place by
/// [`crate::flag_groups::expand_process_env`], so the raw read is the one that
/// can disagree with the running configuration.
fn java_home() -> Option<String> {
    for key in ["CRATONVM_JAVA_HOME", "JAVA_HOME"] {
        if let Some(value) = crate::flags::runtime_var_os(key) {
            if let Ok(text) = value.into_string() {
                if !text.trim().is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Whether `token` names an absolute location on any platform this VM builds
/// for.
///
/// Three shapes, all of which leak the layout of the machine the run happened
/// on: POSIX (`/opt/jdk`), Windows drive-qualified (`C:\Program Files\jdk`,
/// `C:/jdk`) and UNC (`\\build\share\jdk`; the POSIX-spelled `//host/share`
/// falls out of the first rule). A drive-relative Windows root (`\Windows\x`)
/// counts too — it is still an absolute-from-the-root path.
///
/// Nothing else in a violation can collide with these: an internal class name
/// (`java/lang/Object`) and a descriptor (`(Ljava/lang/String;)V`) never begin
/// with a separator, and a `#[track_caller]` provenance string is relative to
/// the workspace root.
fn is_absolute_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    match bytes {
        [b'/', ..] | [b'\\', ..] => true,
        [drive, b':', sep, ..]
            if drive.is_ascii_alphabetic() && (*sep == b'/' || *sep == b'\\') =>
        {
            true
        }
        _ => false,
    }
}

/// Replace every whitespace-delimited absolute path in `value` with
/// `<redacted>`, unless `verbose`.
///
/// Returns `value` unchanged — not merely equal, but with its original spacing
/// intact — when nothing needs redacting, which is the common case and the one
/// where the exact text is what a reader is diffing against.
fn redact_paths(value: &str, verbose: bool) -> String {
    if verbose || !value.split_whitespace().any(is_absolute_path) {
        return value.to_string();
    }
    value
        .split_whitespace()
        .map(|token| {
            if is_absolute_path(token) {
                REDACTED
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Append `"key":<value>` to a JSON object body, inserting the separating comma
/// only when something precedes it — which is what keeps the object free of the
/// trailing comma that makes a hand-rolled dump unparseable.
fn json_field(out: &mut String, first: &mut bool, key: &str, value: Option<&str>) {
    if !*first {
        out.push(',');
    }
    *first = false;
    json_string(out, key);
    out.push(':');
    match value {
        Some(text) => json_string(out, text),
        None => out.push_str("null"),
    }
}

/// Append a JSON string literal, escaping per RFC 8259.
///
/// Class names and descriptors cannot contain a quote or a backslash, but a
/// `reason` is free text supplied by the class loader and a Windows path is
/// full of backslashes — either would otherwise produce a report no JSON
/// parser accepts.
fn json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Errors related to class file loading and parsing.
#[derive(Debug, Error)]
pub enum ClassFileError {
    #[error("class not found: {class_name}")]
    ClassNotFound { class_name: String },

    #[error("I/O error reading class {class_name}: {source}")]
    IoError {
        class_name: String,
        source: std::io::Error,
    },

    #[error("invalid class file {class_name}: {message}")]
    InvalidClassFile { class_name: String, message: String },

    #[error("unsupported class version {major}.{minor} for class {class_name}")]
    UnsupportedVersion {
        class_name: String,
        major: u16,
        minor: u16,
    },
}

/// Errors during class linking (JVM spec Chapter 5).
#[derive(Debug, Error)]
pub enum LinkageError {
    #[error("class format error in {class_name}: {message}")]
    ClassFormatError { class_name: String, message: String },

    /// JVMS §4.1: the class file's `major.minor` pair is not loadable — too
    /// old, too new, a non-zero minor below the preview encoding, a preview
    /// class file at the wrong major, or a preview class file without
    /// `--enable-preview`.
    ///
    /// Distinct from [`ClassFormatError`](Self::ClassFormatError) only in the
    /// throwable it becomes: `java.lang.UnsupportedClassVersionError` extends
    /// `ClassFormatError`, so an application `catch (ClassFormatError)` fires
    /// either way — what diverged before this variant existed was
    /// `e.getClass().getName()` and the text. See
    /// docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
    ///
    /// `message` is HotSpot's wording verbatim, built by
    /// `ClassReaderError::unsupported_class_version_message`. It already
    /// contains the class name, in internal (slash) form, in the middle of the
    /// sentence — so nothing downstream may prepend the name again the way the
    /// `ClassFormatError` arm does. `class_name` is carried alongside for
    /// callers that need it structurally, not for rendering.
    #[error("{message}")]
    UnsupportedClassVersionError { class_name: String, message: String },

    #[error("verification error in {class_name}.{method_name}: {message}")]
    VerifyError {
        class_name: String,
        method_name: String,
        message: String,
    },

    #[error("no class def found: {class_name}")]
    NoClassDefFoundError { class_name: String },

    #[error("incompatible class change: {message}")]
    IncompatibleClassChangeError { message: String },

    /// JVMS §5.3.5: a class loader that has already defined a class of this
    /// name must not define another one. HotSpot raises
    /// `java.lang.LinkageError` ITSELF here, not a subclass — measured on
    /// OpenJDK 25.0.4:
    ///
    /// ```text
    /// java.lang.LinkageError: loader DupProbe$L @1dbd16a6 attempted duplicate
    /// class definition for Dp1. (Dp1 is in unnamed module of loader
    /// DupProbe$L @1dbd16a6, parent loader 'bootstrap')
    /// ```
    ///
    /// Distinct from [`IncompatibleClassChangeError`](Self::IncompatibleClassChangeError),
    /// which the class-manager backend raises for the same underlying
    /// condition: that one is a VM-internal signal the `defineClass` natives
    /// interpret, and it can also fire when a name collides inside a namespace
    /// two DIFFERENT loaders share (CratonVM's flat store). Only this variant
    /// means "the same loader object, twice", which is the one shape HotSpot
    /// refuses.
    ///
    /// The parenthetical module tail is deliberately not reproduced: it carries
    /// an identity hash, so no test could assert it.
    #[error("loader {loader} attempted duplicate class definition for {class_name}")]
    DuplicateClassDefinition { class_name: String, loader: String },

    #[error("no such field: {class_name}.{field_name}")]
    NoSuchFieldError {
        class_name: String,
        field_name: String,
    },

    #[error("no such method: {class_name}.{method_name}{method_descriptor}")]
    NoSuchMethodError {
        class_name: String,
        method_name: String,
        method_descriptor: String,
    },

    #[error("illegal access: {message}")]
    IllegalAccessError { message: String },

    #[error("abstract method error: {class_name}.{method_name}")]
    AbstractMethodError {
        class_name: String,
        method_name: String,
    },

    /// JVMTI `RedefineClasses` / `RetransformClasses` rejected the new
    /// bytecode because it violates JEP 109's structural-equivalence
    /// constraints (class name, superclass, interfaces, field set, or
    /// method declarations changed). Surfaced to JVMTI agents as
    /// `JVMTI_ERROR_UNSUPPORTED_REDEFINITION_*` and to in-process Java
    /// callers as `UnsupportedClassRedefinitionException`.
    #[error("unsupported class redefinition: {class_name}: {message}")]
    UnsupportedClassRedefinitionError { class_name: String, message: String },
}

/// Runtime exceptions during bytecode execution.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("NullPointerException{}", format_optional_message(.message))]
    NullPointerException { message: Option<String> },

    /// `message` carries HotSpot's exact text, and there are two shapes of
    /// it: an array access says "Index 9 out of bounds for length 4" (the same
    /// wording `Preconditions.checkIndex` produces, hence
    /// [`out_of_bounds_message::check_index`]), and `System.arraycopy` says
    /// "arraycopy: last source index 9 out of bounds for int[4]" (see
    /// [`arraycopy_message`]). It was absent until 2026-08-06, which is why
    /// every AIOOBE this VM threw from the interpreter had a null message
    /// while the JIT tier's own bounds check already carried one — the two
    /// tiers disagreed about the same array access.
    ///
    /// `None` is not "unknown": it is the deliberate answer for a call site
    /// that cannot name the length (a native holding an index and nothing
    /// else) and for `java.lang.reflect.Array`, whose out-of-bounds throw has
    /// a null message on HotSpot too. Build it with the `aioobe*` constructors
    /// below rather than by hand, so the wording stays in one place.
    #[error("ArrayIndexOutOfBoundsException: index {index}")]
    ArrayIndexOutOfBoundsException { index: i32, message: Option<String> },
    /// Plain `java.lang.IndexOutOfBoundsException` -- the SUPERCLASS of the
    /// Array/String variants above, and not interchangeable with them.
    ///
    /// Added 2026-08-05. Code that needed this previously reached for
    /// `ArrayIndexOutOfBoundsException`, which is a *subclass*: a
    /// `catch (IndexOutOfBoundsException)` still catches it, but anything
    /// testing the class, and the JDK's own contracts, do not agree. Two
    /// callers need the exact class: `Preconditions.outOfBounds` with a null
    /// formatter, and `Matcher.appendReplacement`'s "No group N".
    #[error("IndexOutOfBoundsException: {message:?}")]
    IndexOutOfBoundsException { message: Option<String> },

    #[error("ArithmeticException: {message}")]
    ArithmeticException { message: String },

    #[error("ClassCastException: {message}")]
    ClassCastException { message: String },

    #[error("StackOverflowError")]
    StackOverflowError,

    #[error("OutOfMemoryError: {message}")]
    OutOfMemoryError { message: String },

    #[error("NegativeArraySizeException: {size}")]
    NegativeArraySizeException { size: i32 },

    #[error("ArrayStoreException: {message}")]
    ArrayStoreException { message: String },

    /// `message` carries HotSpot's exact text ("Index 3 out of bounds for
    /// length 2", "Range [0, 5) out of bounds for length 2"). It was absent
    /// until 2026-08-05, which is why every SIOOBE this VM threw had a null
    /// message -- 13 rows of `StringPolicyMatrixProbe` differed from HotSpot on
    /// nothing but that. Build it with the `sioobe_*` constructors below rather
    /// than by hand, so the wording stays in one place.
    #[error("StringIndexOutOfBoundsException: index {index}")]
    StringIndexOutOfBoundsException { index: i32, message: Option<String> },

    #[error("ClassNotFoundException: {class_name}")]
    ClassNotFoundException { class_name: String },

    #[error("UnsatisfiedLinkError: {message}")]
    UnsatisfiedLinkError { message: String },

    #[error("IllegalMonitorStateException: {message}")]
    IllegalMonitorStateException { message: String },

    #[error("NumberFormatException: {message}")]
    NumberFormatException { message: String },

    #[error("InterruptedException")]
    InterruptedException,

    #[error("NoSuchFieldException: {field_name}")]
    NoSuchFieldException { field_name: String },

    #[error("NoSuchMethodException: {message}")]
    NoSuchMethodException { message: String },

    #[error("IllegalAccessException: {message}")]
    IllegalAccessException { message: String },

    #[error("InaccessibleObjectException: {message}")]
    InaccessibleObjectException { message: String },

    #[error("IllegalArgumentException: {message}")]
    IllegalArgumentException { message: String },

    #[error("IOException: {message}")]
    IOException { message: String },

    #[error("EOFException: {message}")]
    EOFException { message: String },

    /// `java.net.UnknownHostException` — a host name could not be resolved or
    /// is malformed. A subclass of IOException; must be thrown as the concrete
    /// type because real code catches it specifically (e.g. Tomcat
    /// `NetMask` catches `UnknownHostException` to convert to
    /// IllegalArgumentException — a bare IOException escapes that catch).
    #[error("UnknownHostException: {message}")]
    UnknownHostException { message: String },

    /// `java.net.SocketTimeoutException` — a blocking socket operation timed
    /// out (e.g. a read exceeded `setSoTimeout`/`setReadTimeout`). A subclass
    /// of `InterruptedIOException`/`IOException`; must be thrown as the
    /// concrete type because real code catches it specifically (e.g. Tomcat's
    /// `TestConnector.testStop` does `catch (SocketTimeoutException)` to treat
    /// a post-stop read timeout as 503 — a bare IOException escapes that catch).
    #[error("SocketTimeoutException: {message}")]
    SocketTimeoutException { message: String },

    /// `java.net.ConnectException` — a connection attempt was actively
    /// refused (or otherwise failed to establish) by the remote host. A
    /// subclass of `SocketException`/`IOException`; must be thrown as the
    /// concrete type because real code catches it specifically (e.g. ES
    /// `RestClientMultipleHostsIntegTests.testNodeSelector` does
    /// `catch (ConnectException e)` around a request to a stopped host — a
    /// bare IOException whose message merely mentions "ConnectException"
    /// escapes that catch and fails the test).
    #[error("ConnectException: {message}")]
    ConnectException { message: String },

    /// `java.net.ProtocolException` — a subclass of `IOException`; real JDK's
    /// `HttpURLConnection.setRequestMethod` throws this (not
    /// `IllegalArgumentException`) for a method outside its fixed whitelist
    /// (e.g. "PATCH" — Spring's `SimpleClientHttpRequestFactoryTests`
    /// specifically asserts `ProtocolException.class` for it).
    #[error("ProtocolException: {message}")]
    ProtocolException { message: String },
    /// `java.net.BindException` — a `bind()` failed, typically because the
    /// requested address/port is already in use. A subclass of
    /// `SocketException`/`IOException`; must be thrown as the concrete type
    /// because real code catches it specifically (e.g. Spring Boot's
    /// `PortInUseException.throwIfPortBindingException` does
    /// `ifCausedBy(ex, BindException.class, ...)` walking the cause chain —
    /// a bare IOException whose message merely mentions "BindException" as a
    /// text prefix is invisible to that `instanceof`-based walk, so
    /// `NettyWebServer.start()` falls back to a generic `WebServerException`
    /// instead of the specific `PortInUseException` tests assert on).
    #[error("BindException: {message}")]
    BindException { message: String },

    #[error("FileNotFoundException: {path}")]
    FileNotFoundException { path: String },

    #[error("NoSuchFileException: {path}")]
    NoSuchFileException { path: String },

    /// `java.nio.file.NotDirectoryException` — a directory operation applied to
    /// something that is not one.
    ///
    /// Like [`Self::NoSuchFileException`] the message is the PATH alone, because
    /// `FileSystemException.getMessage` builds its text from the `file` field; a
    /// populated `detailMessage` renders as `<path>: <path>`.
    #[error("NotDirectoryException: {path}")]
    NotDirectoryException { path: String },

    #[error("UnsupportedOperationException: {message}")]
    UnsupportedOperationException { message: String },

    #[error("IllegalStateException: {message}")]
    IllegalStateException { message: String },

    #[error("IllegalThreadStateException: {message}")]
    IllegalThreadStateException { message: String },

    /// `java.util.concurrent.CancellationException` — what `Future.get()`,
    /// `ForkJoinTask.join()` and `ForkJoinTask.invoke()` raise for a task that
    /// was cancelled.
    ///
    /// It exists because the alternative was a PROXY: `fjp_state_get_checked`
    /// raised `IllegalStateException` with the string
    /// `"java.util.concurrent.CancellationException: task was cancelled"` in
    /// its message, and a proxy is exactly as good as the message and no
    /// better — `catch (CancellationException)`, which is the ONLY way a
    /// caller distinguishes "cancelled" from "failed", does not fire on it.
    /// MEASURED against HotSpot 25.0.4+7 in both modes
    /// (`probes/ForkJoinShadowSweep.java`): three rows, `join`, `get` and
    /// `invoke` on a cancelled task, all `IllegalStateException` here and
    /// `CancellationException` there.
    ///
    /// An EMPTY message is NO message, like `IllegalThreadStateException`
    /// above: HotSpot reaches `new CancellationException()` on every one of
    /// those three paths.
    #[error("CancellationException: {message}")]
    CancellationException { message: String },

    /// `java.util.concurrent.RejectedExecutionException` — what an executor
    /// raises for work submitted after `shutdown()`.
    ///
    /// Same empty-means-none convention. Added with
    /// [`RuntimeError::CancellationException`] because the ForkJoinPool
    /// submission natives run their task INLINE and so never consulted the
    /// pool's shutdown state at all: `submit`, `execute` and `invoke` each
    /// ran a task on a pool the caller had already shut down.
    #[error("RejectedExecutionException: {message}")]
    RejectedExecutionException { message: String },

    /// Thrown when a method is invoked by an unauthorized caller. Used by the
    /// Panama native-access gate when `--enable-native-access` has not been
    /// granted to the calling module — matches OpenJDK's
    /// `java.lang.IllegalCallerException` semantics.
    #[error("IllegalCallerException: {message}")]
    IllegalCallerException { message: String },

    #[error("ConcurrentModificationException")]
    ConcurrentModificationException,

    #[error("NoSuchElementException: {message}")]
    NoSuchElementException { message: String },

    /// `java.util.EmptyStackException`. NOT a `NoSuchElementException` — it
    /// extends `RuntimeException` **directly**, so a `catch (EmptyStackException)`
    /// in application code does not fire when the wrong one is raised, and the
    /// caller falls through to whatever handler comes next. `java.util.Stack`'s
    /// `pop`/`peek` are the only throwers in the JDK and the only ones here.
    ///
    /// Field-less because the real class declares only a no-arg constructor and
    /// sets no detail message; `getMessage()` is null on HotSpot.
    /// See W7-33-differential-dead-sections R2.
    #[error("EmptyStackException")]
    EmptyStackException,

    /// `java.nio.BufferUnderflowException` — a relative `get` was attempted on
    /// a buffer with no elements remaining. Distinct from IllegalStateException
    /// because real code catches it specifically (e.g. Tomcat
    /// `CharsetUtil.isAsciiSuperset`); folding it into IllegalStateException
    /// makes those `catch (BufferUnderflowException)` blocks miss.
    #[error("BufferUnderflowException")]
    BufferUnderflowException,

    /// `java.nio.BufferOverflowException` — a relative `put` was attempted on a
    /// buffer with no space remaining.
    #[error("BufferOverflowException")]
    BufferOverflowException,

    /// `java.nio.ReadOnlyBufferException` — a mutating operation (`put`,
    /// `compact`, `array()`) was attempted on a read-only buffer.
    #[error("ReadOnlyBufferException")]
    ReadOnlyBufferException,

    #[error("InputMismatchException: {message}")]
    InputMismatchException { message: String },

    #[error("SecurityException: {message}")]
    SecurityException { message: String },

    #[error("MatchException: {message}")]
    MatchException { message: String },

    /// `java.util.regex.PatternSyntaxException` — a regular expression did not
    /// compile. A subclass of `IllegalArgumentException`, but it must be thrown
    /// as the concrete type: validation code catches it specifically, and the
    /// alternative a native has when its own engine rejects a pattern is to
    /// fall back to a *literal* match and return a plausible wrong answer.
    /// That is exactly what `String.matches` / `replaceAll` / `replaceFirst`
    /// did until 2026-08-04 — `"Hello, World".matches("[")` returned `false`
    /// where HotSpot throws.
    #[error("PatternSyntaxException: {description} near index {index} in {pattern}")]
    /// The three fields `java.util.regex.PatternSyntaxException` actually
    /// stores. NOT a pre-formatted message: that class **overrides**
    /// `getMessage()` and builds its three-line report from `desc`, `pattern`
    /// and `index`, using `System.lineSeparator()` -- so formatting it here
    /// would hard-code `\n` where HotSpot emits `\r\n` on Windows, and would
    /// still leave `getDescription()` / `getPattern()` / `getIndex()` empty.
    /// The throw site sets the fields and lets the JDK's own bytecode format
    /// them. `index` is -1 when unknown.
    PatternSyntaxException {
        description: String,
        pattern: String,
        index: i32,
    },

    #[error("not implemented: {feature}")]
    NotImplemented { feature: String },

    /// `java.lang.InternalError` -- a JVM-internal invariant the caller cannot
    /// have violated, raised where HotSpot raises it.
    ///
    /// Distinct from [`MethodCallFailed::InternalError`], which is documented
    /// as the UNCATCHABLE form and is not a Java throwable at all. This one is
    /// an ordinary catchable `Error`, which is what HotSpot throws from
    /// `Unsafe.objectFieldOffset(Class, String)` when the class has no such
    /// field.
    #[error("internal error: {message}")]
    InternalError { message: String },
}

fn format_optional_message(message: &Option<String>) -> String {
    match message {
        Some(msg) => format!(": {msg}"),
        None => String::new(),
    }
}

/// `jdk.internal.util.Preconditions.outOfBoundsMessage`, reproduced verbatim.
///
/// The JDK builds these three shapes in `Preconditions.outOfBounds*` and real
/// code greps them, so the wording is behaviour, not decoration. They live here
/// — one crate below everything that raises a bounds exception — because there
/// are now three families of caller and a second copy is how they drift:
///
/// * `RuntimeError::sioobe_*` below, for the `java.lang.String` /
///   `AbstractStringBuilder` natives;
/// * `native-builtins`'s `Preconditions` natives, which format the same text
///   for whichever class the caller's exception formatter asks for;
/// * the `java.nio` buffer natives, which shadow the bytecode that would
///   otherwise have reached `Preconditions` at all.
///
/// Arguments are `i64` so the `int` and `long` `Preconditions` overloads share
/// one implementation; the JDK formats through `%s` on a boxed `Number`, which
/// is decimal either way.
pub mod out_of_bounds_message {
    /// `checkIndex(index, length)`.
    pub fn check_index(index: i64, length: i64) -> String {
        format!("Index {index} out of bounds for length {length}")
    }

    /// `checkFromToIndex(from, to, length)` — a half-open `[from, to)` range.
    pub fn check_from_to_index(from: i64, to: i64, length: i64) -> String {
        format!("Range [{from}, {to}) out of bounds for length {length}")
    }

    /// `checkFromIndexSize(from, size, length)` — `[from, from + size)`.
    ///
    /// The message prints the ADDITION unevaluated (`%<s` in the JDK's format
    /// string), so this is not the same text as
    /// `check_from_to_index(from, from + size, length)` — and `from + size` can
    /// overflow, which is exactly why the JDK does not evaluate it.
    pub fn check_from_index_size(from: i64, size: i64, length: i64) -> String {
        format!("Range [{from}, {from} + {size}) out of bounds for length {length}")
    }
}

/// `System.arraycopy`'s exception wordings, reproduced from HotSpot's
/// `TypeArrayKlass::copy_array` / `ObjArrayKlass::copy_array`.
///
/// `arraycopy` does not use [`out_of_bounds_message`] at all: it names the
/// array's *type* and *length* and says which of the five arguments failed,
/// because an index alone cannot distinguish "your source ran out" from "your
/// destination did". The five out-of-bounds shapes below become
/// `ArrayIndexOutOfBoundsException`s; the three type shapes become
/// `ArrayStoreException`s, and live here so both halves of one JDK method's
/// contract stay together.
///
/// `ty` is the element-type name HotSpot's `type2name_tab` prints — `"int"`,
/// `"byte"`, `"boolean"`, `"char"`, `"short"`, `"long"`, `"float"`,
/// `"double"` — or the literal `"object array"` for any reference array,
/// which is why the rendered text reads `object array[4]` and not
/// `java.lang.String[4]`. Use [`element_type_name`] rather than spelling one
/// out at a call site.
///
/// The two `last_*` shapes print `pos + length` **unsigned** (`%u` in
/// HotSpot), which is what makes an overflowing `pos + length` render as a
/// huge positive number instead of a negative one.
pub mod arraycopy_message {
    use crate::ArrayElementType;

    /// HotSpot's `type2name_tab` entry for an array's element type, which is
    /// the token every message below interpolates before `[len]`.
    ///
    /// Every reference array collapses to the single literal `"object array"`
    /// — HotSpot's `ObjArrayKlass::copy_array` never prints the component
    /// class — so `String[]`, `Object[]` and `int[][]` all render alike.
    pub fn element_type_name(element_type: ArrayElementType) -> &'static str {
        match element_type {
            ArrayElementType::Boolean => "boolean",
            ArrayElementType::Char => "char",
            ArrayElementType::Float => "float",
            ArrayElementType::Double => "double",
            ArrayElementType::Byte => "byte",
            ArrayElementType::Short => "short",
            ArrayElementType::Int => "int",
            ArrayElementType::Long => "long",
            ArrayElementType::Reference => "object array",
        }
    }

    /// `srcPos < 0`.
    pub fn source_index(src_pos: i32, ty: &str, src_len: i32) -> String {
        format!("arraycopy: source index {src_pos} out of bounds for {ty}[{src_len}]")
    }

    /// `destPos < 0` — reached only once `srcPos` has been cleared.
    pub fn destination_index(dest_pos: i32, ty: &str, dest_len: i32) -> String {
        format!("arraycopy: destination index {dest_pos} out of bounds for {ty}[{dest_len}]")
    }

    /// `length < 0` — reached only once both positions have been cleared.
    /// The one shape that names no array.
    pub fn negative_length(length: i32) -> String {
        format!("arraycopy: length {length} is negative")
    }

    /// `srcPos + length > src.length`, printed unsigned.
    pub fn last_source_index(src_pos: i32, length: i32, ty: &str, src_len: i32) -> String {
        let last = (src_pos as u32).wrapping_add(length as u32);
        format!("arraycopy: last source index {last} out of bounds for {ty}[{src_len}]")
    }

    /// `destPos + length > dest.length`, printed unsigned.
    pub fn last_destination_index(dest_pos: i32, length: i32, ty: &str, dest_len: i32) -> String {
        let last = (dest_pos as u32).wrapping_add(length as u32);
        format!("arraycopy: last destination index {last} out of bounds for {ty}[{dest_len}]")
    }

    /// `src` is not an array at all (an `ArrayStoreException`, not an AIOOBE).
    pub fn source_not_an_array(class_name: &str) -> String {
        format!("arraycopy: source type {class_name} is not an array")
    }

    /// `dest` is not an array at all (an `ArrayStoreException`).
    pub fn destination_not_an_array(class_name: &str) -> String {
        format!("arraycopy: destination type {class_name} is not an array")
    }

    /// Both are arrays but their element types differ (an
    /// `ArrayStoreException`). HotSpot renders each side as `{ty}[]`, so a
    /// reference array reads `object array[]`.
    pub fn type_mismatch(src_ty: &str, dest_ty: &str) -> String {
        format!("arraycopy: type mismatch: can not copy {src_ty}[] into {dest_ty}[]")
    }

    /// An internal class name or descriptor as HotSpot's `external_name()`
    /// prints it: `[Ljava/lang/String;` -> `java.lang.String[]`, `[I` ->
    /// `int[]`, `java/lang/String` -> `java.lang.String`.
    pub fn external_class_name(internal: &str) -> String {
        let mut dims = 0usize;
        let mut rest = internal;
        while let Some(stripped) = rest.strip_prefix('[') {
            dims += 1;
            rest = stripped;
        }
        let base = if dims == 0 {
            rest.replace('/', ".")
        } else {
            match rest.as_bytes().first() {
                Some(b'L') => rest
                    .trim_start_matches('L')
                    .trim_end_matches(';')
                    .replace('/', "."),
                Some(b'Z') => "boolean".to_string(),
                Some(b'B') => "byte".to_string(),
                Some(b'C') => "char".to_string(),
                Some(b'S') => "short".to_string(),
                Some(b'I') => "int".to_string(),
                Some(b'J') => "long".to_string(),
                Some(b'F') => "float".to_string(),
                Some(b'D') => "double".to_string(),
                _ => rest.replace('/', "."),
            }
        };
        format!("{base}{}", "[]".repeat(dims))
    }

    /// The PER-ELEMENT `ArrayStoreException` text: a reference copy whose
    /// source holds an element the destination component type cannot accept.
    ///
    /// Distinct from [`type_mismatch`] above, which is the BULK rejection when
    /// the two element KINDS differ and no element is ever examined.
    ///
    /// Both arguments are internal names: `src_array_component` is the source
    /// array's component (`java/lang/Object` for an `Object[]`), and
    /// `dst_component` is the destination array's component.
    ///
    /// # The two halves are rendered in different dialects, on purpose
    ///
    /// That is HotSpot's sentence, not an inconsistency to tidy up. The source
    /// is the array's `external_name()`; the destination component is the
    /// component Klass's own dotted NAME, which for an array class is a
    /// descriptor. MEASURED on 25.0.4+7 (`probes/DodArrayStoreSweep`):
    ///
    /// ```text
    /// ... elements of java.lang.Object[] ... destination array, java.lang.String
    /// ... elements of java.lang.Object[] ... destination array, [Ljava.lang.Integer;
    /// ... elements of java.lang.Object[] ... destination array, [I
    /// ```
    ///
    /// # Why it lives here rather than beside its callers
    ///
    /// Three natives in two crates must print it identically —
    /// `System.arraycopy` and `Arrays.copyOf(U[],int,Class)` in
    /// `native-builtins`, and `ArrayList.toArray(T[])` in
    /// `native-collections` — and `native-builtins` depends on
    /// `native-collections`, so no home in either crate can serve all three.
    /// It was private to `native-builtins` and that is precisely why the
    /// `toArray` store check went unwritten for a day.
    pub fn element_type_mismatch(src_array_component: &str, dst_component: &str) -> String {
        let dst_rendered = if dst_component.starts_with('[') {
            dst_component.replace('/', ".")
        } else {
            external_class_name(dst_component)
        };
        format!(
            "arraycopy: element type mismatch: can not cast one of the elements of {}[] to the type of the destination array, {dst_rendered}",
            external_class_name(src_array_component),
        )
    }
}

impl RuntimeError {
    /// `StringIndexOutOfBoundsException` with HotSpot's `checkIndex` wording.
    ///
    /// The three `sioobe_*` constructors carry
    /// [`out_of_bounds_message`]'s three shapes — the ones a `String`-domain
    /// caller gets because it passes `Preconditions.SIOOBE_FORMATTER`.
    pub fn sioobe_index(index: i32, length: i32) -> Self {
        RuntimeError::StringIndexOutOfBoundsException {
            index,
            message: Some(out_of_bounds_message::check_index(
                i64::from(index),
                i64::from(length),
            )),
        }
    }

    /// HotSpot's `checkFromToIndex` wording: a half-open `[from, to)` range.
    pub fn sioobe_range(from: i32, to: i32, length: i32) -> Self {
        RuntimeError::StringIndexOutOfBoundsException {
            index: if from < 0 { from } else { to },
            message: Some(out_of_bounds_message::check_from_to_index(
                i64::from(from),
                i64::from(to),
                i64::from(length),
            )),
        }
    }

    /// HotSpot's `checkFromIndexSize` wording: `[from, from + size)`.
    pub fn sioobe_range_size(from: i32, size: i32, length: i32) -> Self {
        RuntimeError::StringIndexOutOfBoundsException {
            index: if from < 0 { from } else { size },
            message: Some(out_of_bounds_message::check_from_index_size(
                i64::from(from),
                i64::from(size),
                i64::from(length),
            )),
        }
    }

    /// Plain `IndexOutOfBoundsException` with a message.
    ///
    /// Use where the JDK throws the SUPERCLASS -- `Preconditions` with no
    /// exception formatter, and `Matcher`'s "No group N". Reaching for
    /// `ArrayIndexOutOfBoundsException` there is wrong in the direction that
    /// breaks a `catch`.
    pub fn ioobe(message: impl Into<String>) -> Self {
        RuntimeError::IndexOutOfBoundsException {
            message: Some(message.into()),
        }
    }

    /// Plain `IndexOutOfBoundsException` with **no** detail message.
    ///
    /// `java.nio.Buffer` declares its own exception formatter whose entire body
    /// is `new IndexOutOfBoundsException()`, so every ABSOLUTE buffer accessor
    /// (`get(i)`, `put(i, v)`, `getInt(i)`, `CharBuffer.charAt(i)`, …) has a
    /// null `getMessage()` on HotSpot — unlike its `Objects.check*` and
    /// `slice(index, length)` neighbours, which carry
    /// [`out_of_bounds_message`] text. Both are contract; see
    /// `probes/PreconditionsFormatterProbe`'s `NIO contract neighbours` rows.
    pub fn ioobe_no_message() -> Self {
        RuntimeError::IndexOutOfBoundsException { message: None }
    }

    /// A SIOOBE whose call site does not know the length, so it cannot build
    /// HotSpot's text. Prefer one of the three above; this exists so the
    /// remaining sites say so explicitly rather than silently passing `None`.
    pub fn sioobe_no_length(index: i32) -> Self {
        RuntimeError::StringIndexOutOfBoundsException {
            index,
            message: None,
        }
    }

    /// An out-of-bounds **array access**: HotSpot's
    /// `InterpreterRuntime::throw_ArrayIndexOutOfBoundsException` wording,
    /// which is character-for-character `Preconditions.checkIndex`'s.
    ///
    /// This is the one every `aaload`/`aastore`/`iaload`/… reaches, in both
    /// the interpreter and the JIT, so both tiers must call it rather than
    /// formatting their own copy.
    pub fn aioobe(index: i32, length: i32) -> Self {
        RuntimeError::ArrayIndexOutOfBoundsException {
            index,
            message: Some(out_of_bounds_message::check_index(
                i64::from(index),
                i64::from(length),
            )),
        }
    }

    /// An AIOOBE carrying a message that is not the array-access shape —
    /// `System.arraycopy`'s five (see [`arraycopy_message`]), and
    /// `java.util.Arrays`' `"Array index out of range: N"`.
    ///
    /// `index` stays the machine-readable operand; the message is what
    /// `getMessage()` returns.
    pub fn aioobe_with_message(index: i32, message: impl Into<String>) -> Self {
        RuntimeError::ArrayIndexOutOfBoundsException {
            index,
            message: Some(message.into()),
        }
    }

    /// An AIOOBE whose call site knows the index but not the array's length,
    /// so it cannot build HotSpot's array-access text.
    ///
    /// It gets the JDK's own `ArrayIndexOutOfBoundsException(int)` wording,
    /// `"Array index out of range: N"` — which is what a `java.util.Arrays`
    /// range check produces on HotSpot, and what the great majority of the
    /// natives migrated in 2026-08-06 are standing in for. It is exact for
    /// what is known rather than a guess at what is not.
    ///
    /// Prefer [`RuntimeError::aioobe`] wherever the length is reachable; an
    /// array access must never come through here, because HotSpot's wording
    /// for one names the length.
    pub fn aioobe_index_only(index: i32) -> Self {
        RuntimeError::ArrayIndexOutOfBoundsException {
            index,
            message: Some(format!("Array index out of range: {index}")),
        }
    }

    /// An AIOOBE with a deliberately **null** `getMessage()`.
    ///
    /// Not an unfinished migration: HotSpot's `java.lang.reflect.Array`
    /// accessors raise the exception with no text at all, so
    /// `Array.get(new int[4], 9).getMessage()` is null there while a plain
    /// `a[9]` in bytecode says "Index 9 out of bounds for length 4". Use this
    /// only where a HotSpot control shows a null message; use
    /// [`RuntimeError::aioobe_index_only`] otherwise.
    pub fn aioobe_no_message(index: i32) -> Self {
        RuntimeError::ArrayIndexOutOfBoundsException {
            index,
            message: None,
        }
    }

    /// The Java throwable this error materialises as: `(internal class name,
    /// detail message)`, or `None` when it has no Java counterpart
    /// ([`RuntimeError::NotImplemented`], which must stay an internal error).
    ///
    /// `None` for the message means the throwable is constructed with its
    /// no-arg constructor, i.e. `getMessage()` must be null — not an empty
    /// string. Several variants depend on that distinction; see the comments
    /// on the individual arms.
    ///
    /// This lives on the error rather than in `vm::runtime::exceptions` because
    /// there are **two** consumers that must agree: the interpreter's throw
    /// site (`throw_runtime_error`) and the reflective-call wrapper
    /// (`native-builtins`'s `wrap_as_invocation_target_exception`, which has to
    /// materialise the same throwable in order to wrap it in an
    /// `InvocationTargetException`). It used to be a `match` private to the
    /// former, so the latter could only handle the one variant somebody had
    /// needed — which is how `Method.invoke` came to propagate a native
    /// `UnsupportedOperationException` raw instead of wrapping it (H2
    /// `TestMVStore.testIterate`).
    /// The detail message for a variant that carries a payload but no `String`
    /// to borrow, so it has to be built.
    ///
    /// Every other variant either owns a message the table below borrows, or
    /// genuinely has none. `NegativeArraySizeException` owns its payload as an
    /// `i32` and the table dropped it, so `getMessage()` came back **null**
    /// where HotSpot has text.
    ///
    /// `ArrayIndexOutOfBoundsException` was in the same position until
    /// 2026-08-06, and was answered here with the JDK's
    /// `ArrayIndexOutOfBoundsException(int)` wording, `"Array index out of
    /// range: N"`. That is right for a site that knows only an index, and
    /// wrong for the two that matter most: an array access says "Index 9 out
    /// of bounds for length 4", and `java.lang.reflect.Array` says nothing at
    /// all. Answering here made the choice unavailable to the call site, so
    /// the variant carries its own `message` now and the three
    /// `RuntimeError::aioobe*` constructors pick the wording — including
    /// [`RuntimeError::aioobe_index_only`], which is this text, still the
    /// right default for a native that holds an index and nothing else.
    fn synthesised_detail_message(&self) -> Option<String> {
        match self {
            // HotSpot's message is the size alone, with no prose (`new int[-1]`
            // reports `-1`).
            RuntimeError::NegativeArraySizeException { size } => Some(size.to_string()),
            _ => None,
        }
    }

    pub fn as_java_throwable(&self) -> Option<(&'static str, Option<Cow<'_, str>>)> {
        let pair = match self {
            RuntimeError::NullPointerException { message } => (
                "java/lang/NullPointerException",
                // Empty = the "no message" marker an implicit-dereference NPE
                // carries when `-XX:-ShowCodeDetailsInExceptionMessages` is
                // explicitly off (the throw sites must hand a `String` to
                // `pop_object_ref_ctx_with`, so they cannot pass `None`
                // themselves). HotSpot's `getMessage()` is null there. A
                // *deliberate* empty NPE message is not produced anywhere
                // Rust-side; a Java `new NullPointerException("")` never travels
                // through `RuntimeError`.
                match message.as_deref() {
                    Some("") => None,
                    other => other,
                },
            ),
            RuntimeError::ArithmeticException { message } => {
                ("java/lang/ArithmeticException", Some(message.as_str()))
            }
            // The variant now carries its own message, so this arm borrows
            // like every other one and `synthesised_detail_message` no longer
            // answers for it. `None` here means the no-arg constructor and a
            // null `getMessage()` — which is what HotSpot's
            // `java.lang.reflect.Array` accessors produce.
            RuntimeError::ArrayIndexOutOfBoundsException { message, .. } => (
                "java/lang/ArrayIndexOutOfBoundsException",
                message.as_deref(),
            ),
            RuntimeError::IndexOutOfBoundsException { message } => {
                ("java/lang/IndexOutOfBoundsException", message.as_deref())
            }
            RuntimeError::ClassCastException { message } => (
                "java/lang/ClassCastException",
                // An EMPTY message is NO message, the marker
                // `IllegalArgumentException` and `EOFException` carry. HotSpot's
                // `AtomicReferenceFieldUpdater.newUpdater` with a mismatched
                // `vclass` raises this with a null message (measured); a
                // `Some("")` would build it with a non-null empty string, which
                // a caller printing `getMessage()` can tell apart.
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            // As above: the message is synthesised from `size`, not absent.
            RuntimeError::NegativeArraySizeException { size: _ } => {
                ("java/lang/NegativeArraySizeException", None)
            }
            RuntimeError::StackOverflowError => ("java/lang/StackOverflowError", None),
            RuntimeError::OutOfMemoryError { message } => {
                ("java/lang/OutOfMemoryError", Some(message.as_str()))
            }
            RuntimeError::ArrayStoreException { message } => {
                ("java/lang/ArrayStoreException", Some(message.as_str()))
            }
            RuntimeError::ClassNotFoundException { class_name } => (
                "java/lang/ClassNotFoundException",
                Some(class_name.as_str()),
            ),
            RuntimeError::UnsatisfiedLinkError { message } => {
                ("java/lang/UnsatisfiedLinkError", Some(message.as_str()))
            }
            RuntimeError::IllegalMonitorStateException { message } => (
                "java/lang/IllegalMonitorStateException",
                // An EMPTY message is NO message, the same marker
                // `IllegalArgumentException` and `EOFException` carry above.
                // HotSpot's `StampedLock.unlock*(badStamp)` and
                // `Object.wait()`-off-monitor raise this with a null message;
                // `Some("")` would build it with a non-null empty string, which
                // a caller printing `getMessage()` can tell apart. No site in
                // the workspace passes a deliberate empty IMSE message.
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::StringIndexOutOfBoundsException { message, .. } => (
                "java/lang/StringIndexOutOfBoundsException",
                message.as_deref(),
            ),
            RuntimeError::NumberFormatException { message } => {
                ("java/lang/NumberFormatException", Some(message.as_str()))
            }
            RuntimeError::InterruptedException => ("java/lang/InterruptedException", None),
            RuntimeError::NoSuchFieldException { field_name } => {
                ("java/lang/NoSuchFieldException", Some(field_name.as_str()))
            }
            RuntimeError::NoSuchMethodException { message } => {
                ("java/lang/NoSuchMethodException", Some(message.as_str()))
            }
            RuntimeError::IllegalAccessException { message } => {
                ("java/lang/IllegalAccessException", Some(message.as_str()))
            }
            RuntimeError::InaccessibleObjectException { message } => (
                "java/lang/reflect/InaccessibleObjectException",
                Some(message.as_str()),
            ),
            RuntimeError::IllegalArgumentException { message } => (
                "java/lang/IllegalArgumentException",
                // Empty = "no message", the same marker `UnsupportedOperation`
                // Exception uses above and for the same reason: the variant
                // holds a `String`, so a site that needs a null `getMessage()`
                // cannot pass `None`. `Array.set(new int[4], 0, null)` is one --
                // HotSpot reaches `new IllegalArgumentException()` with no
                // argument there. No site produces a deliberate empty IAE
                // message (checked across the workspace), so the marker is
                // unambiguous; `Some("")` would build the exception with a
                // non-null empty string instead.
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::IOException { message } => {
                ("java/io/IOException", Some(message.as_str()))
            }
            RuntimeError::EOFException { message } => (
                "java/io/EOFException",
                // An EMPTY message is no message, not the empty string — the
                // same distinction `IllegalThreadStateException` needs above.
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::UnknownHostException { message } => {
                ("java/net/UnknownHostException", Some(message.as_str()))
            }
            RuntimeError::SocketTimeoutException { message } => {
                ("java/net/SocketTimeoutException", Some(message.as_str()))
            }
            RuntimeError::ConnectException { message } => {
                ("java/net/ConnectException", Some(message.as_str()))
            }
            RuntimeError::ProtocolException { message } => {
                ("java/net/ProtocolException", Some(message.as_str()))
            }
            RuntimeError::BindException { message } => {
                ("java/net/BindException", Some(message.as_str()))
            }
            RuntimeError::FileNotFoundException { path } => {
                ("java/io/FileNotFoundException", Some(path.as_str()))
            }
            RuntimeError::NoSuchFileException { path } => {
                ("java/nio/file/NoSuchFileException", Some(path.as_str()))
            }
            RuntimeError::NotDirectoryException { path } => {
                ("java/nio/file/NotDirectoryException", Some(path.as_str()))
            }
            RuntimeError::UnsupportedOperationException { message } => (
                "java/lang/UnsupportedOperationException",
                // An empty message means "no message" (e.g. the blocked-mutator
                // helper for Collections.unmodifiable*/List.of view wrappers,
                // matching the real JDK's `new UnsupportedOperationException()`
                // no-arg constructor) — must produce a null `getMessage()`, not a
                // non-null empty string. `Some("")` would call the
                // `(Ljava/lang/String;)V` ctor and set detailMessage to "".
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::IllegalStateException { message } => {
                ("java/lang/IllegalStateException", Some(message.as_str()))
            }
            RuntimeError::IllegalThreadStateException { message } => (
                "java/lang/IllegalThreadStateException",
                // G72-1: an EMPTY message is not the empty string, it is no
                // message. `Thread.start()` on a started thread throws the
                // no-arg constructor on HotSpot, so `getMessage()` is null --
                // and `Some("")` renders as `""`, which is a different value a
                // caller can see. The variant is a `String` rather than an
                // `Option<String>`, so this is where the distinction has to be
                // made.
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::CancellationException { message } => (
                "java/util/concurrent/CancellationException",
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::RejectedExecutionException { message } => (
                "java/util/concurrent/RejectedExecutionException",
                if message.is_empty() {
                    None
                } else {
                    Some(message.as_str())
                },
            ),
            RuntimeError::IllegalCallerException { message } => {
                // Task #57: route the new variant to `java.lang.IllegalCallerException`
                // so the Panama native-access gate raises the JDK-conventional class
                // instead of folding into IllegalStateException.
                ("java/lang/IllegalCallerException", Some(message.as_str()))
            }
            RuntimeError::ConcurrentModificationException => {
                ("java/util/ConcurrentModificationException", None)
            }
            RuntimeError::NoSuchElementException { message } => {
                // An EMPTY message means "no message", i.e. the no-arg
                // constructor and a null `getMessage()` -- not the (String)
                // constructor with "". HotSpot's own `Vector.firstElement()`,
                // `lastElement()` and the ArrayDeque/TreeMap family throw with
                // NO message (MEASURED 2026-08-13, scratchpad/orch/V2.java:
                // `NoSuchElementException: null`), and six sites in this tree
                // already pass `String::new()` intending exactly that. The
                // variant carries a non-optional String, so this is where the
                // distinction has to be made; converting all 83 construction
                // sites to Option<String> is the wider change it does not
                // need.
                if message.is_empty() {
                    ("java/util/NoSuchElementException", None)
                } else {
                    ("java/util/NoSuchElementException", Some(message.as_str()))
                }
            }
            // `None`, like `ConcurrentModificationException` above: the real
            // class has a no-arg constructor only.
            RuntimeError::EmptyStackException => ("java/util/EmptyStackException", None),
            RuntimeError::BufferUnderflowException => ("java/nio/BufferUnderflowException", None),
            RuntimeError::BufferOverflowException => ("java/nio/BufferOverflowException", None),
            RuntimeError::ReadOnlyBufferException => ("java/nio/ReadOnlyBufferException", None),
            RuntimeError::InputMismatchException { message } => {
                ("java/util/InputMismatchException", Some(message.as_str()))
            }
            RuntimeError::SecurityException { message } => {
                ("java/lang/SecurityException", Some(message.as_str()))
            }
            RuntimeError::MatchException { message } => {
                ("java/lang/MatchException", Some(message.as_str()))
            }
            // The concrete class, not its `IllegalArgumentException` parent: code
            // that validates a user-supplied regex catches
            // `PatternSyntaxException` by name, and a parent-class throw is
            // invisible to that catch. Note the real class declares only
            // `(String desc, String regex, int index)`, so
            // `create_exception_object`'s `<init>(String)` path does not populate
            // `getMessage()` — the same `msg=null` the real `Pattern.compile`
            // bridge already produces. Getting the class right is the part that
            // changes control flow; the description text is a separate gap.
            // `None`: the real class leaves `Throwable.detailMessage` null and
            // overrides `getMessage()`. The throw site fills `desc`/`pattern`/
            // `index` right after construction.
            RuntimeError::PatternSyntaxException { .. } => {
                ("java/util/regex/PatternSyntaxException", None)
            }
            RuntimeError::NotImplemented { feature: _ } => return None,
            RuntimeError::InternalError { message } => {
                ("java/lang/InternalError", Some(message.as_str()))
            }
        };
        let (class_name, borrowed) = pair;
        // A synthesised message wins over the table's `None`. The two are
        // mutually exclusive by construction — `synthesised_detail_message`
        // answers only for variants whose arm above has nothing to borrow — and
        // `or_else` keeps it that way if a third such variant is ever added:
        // the borrowed message stays authoritative wherever one exists.
        let message = self
            .synthesised_detail_message()
            .map(Cow::Owned)
            .or_else(|| borrowed.map(Cow::Borrowed));
        Some((class_name, message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- as_java_throwable detail messages --

    /// The two variants that carry an `i32` payload used to convert with a
    /// `None` message, so every VM-thrown AIOOBE reached Java as
    /// `java.lang.ArrayIndexOutOfBoundsException: null`. That is a behavioural
    /// divergence for anything reading `getMessage()`, and it is what made a
    /// Spring AOT failure undiagnosable.
    ///
    /// Expected text measured on HotSpot (Temurin jdk-25.0.3+9).
    #[test]
    fn payload_carrying_variants_synthesise_hotspots_message() {
        let err = RuntimeError::aioobe_index_only(7);
        let (cls, msg) = err.as_java_throwable().expect("AIOOBE is a Java throwable");
        assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
        // `new ArrayIndexOutOfBoundsException(7)` on HotSpot — the wording for
        // a site that knows the index and not the length. An array access
        // takes `aioobe` and says something else; see the test below.
        assert_eq!(msg.as_deref(), Some("Array index out of range: 7"));

        let (cls, msg) = RuntimeError::NegativeArraySizeException { size: -1 }
            .as_java_throwable()
            .expect("NegativeArraySizeException is a Java throwable");
        assert_eq!(cls, "java/lang/NegativeArraySizeException");
        // `new int[-1]` on HotSpot: the size alone, no prose.
        assert_eq!(msg.as_deref(), Some("-1"));
    }

    /// `None` must keep meaning "construct with the no-arg constructor, so
    /// `getMessage()` is null". Several variants depend on that distinction and
    /// the synthesising path must not have blurred it.
    #[test]
    fn variants_with_no_message_still_convert_to_none() {
        for err in [
            RuntimeError::StackOverflowError,
            RuntimeError::InterruptedException,
            RuntimeError::BufferUnderflowException,
            RuntimeError::BufferOverflowException,
            RuntimeError::ReadOnlyBufferException,
            RuntimeError::ConcurrentModificationException,
            RuntimeError::EmptyStackException,
        ] {
            let (_, msg) = err.as_java_throwable().expect("is a Java throwable");
            assert!(msg.is_none(), "{err:?} must have a null detail message");
        }

        // The empty-string markers are "no message" too, not an empty one.
        // Bound to a `let` because the returned `Cow` borrows from the error.
        let npe = RuntimeError::NullPointerException {
            message: Some(String::new()),
        };
        let (_, msg) = npe.as_java_throwable().unwrap();
        assert!(
            msg.is_none(),
            "an empty NPE marker means getMessage() == null"
        );

        let uoe = RuntimeError::UnsupportedOperationException {
            message: String::new(),
        };
        let (_, msg) = uoe.as_java_throwable().unwrap();
        assert!(
            msg.is_none(),
            "an empty UOE message means getMessage() == null"
        );

        // `Array.set(new int[4], 0, null)` on HotSpot: an IllegalArgumentException
        // with a null message, not an empty one.
        let iae = RuntimeError::IllegalArgumentException {
            message: String::new(),
        };
        let (_, msg) = iae.as_java_throwable().unwrap();
        assert!(
            msg.is_none(),
            "an empty IAE message means getMessage() == null"
        );

        let iae = RuntimeError::IllegalArgumentException {
            message: "argument type mismatch".to_string(),
        };
        let (_, msg) = iae.as_java_throwable().unwrap();
        assert_eq!(msg.as_deref(), Some("argument type mismatch"));
    }

    /// A variant that owns a message is still BORROWED, not copied — the `Cow`
    /// exists so only the two synthesising variants pay for an allocation.
    #[test]
    fn owned_messages_are_borrowed_not_copied() {
        let err = RuntimeError::IllegalStateException {
            message: "boom".to_string(),
        };
        let (_, msg) = err.as_java_throwable().unwrap();
        assert!(matches!(msg, Some(Cow::Borrowed("boom"))));

        // The AIOOBE variant owns its message since 2026-08-06, so it
        // borrows like the rest. `NegativeArraySizeException` is now the only
        // variant that still has to build one.
        let err = RuntimeError::aioobe(9, 4);
        let (_, msg) = err.as_java_throwable().unwrap();
        assert!(matches!(
            msg,
            Some(Cow::Borrowed("Index 9 out of bounds for length 4"))
        ));

        let err = RuntimeError::NegativeArraySizeException { size: -1 };
        let (_, msg) = err.as_java_throwable().unwrap();
        assert!(matches!(msg, Some(Cow::Owned(_))));
    }

    /// The three wordings are three different HotSpot behaviours, not three
    /// spellings of one. A single blanket message for the variant — which is
    /// what `synthesised_detail_message` used to do — cannot be right for all
    /// three at once, and that is why the call site chooses.
    #[test]
    fn the_three_aioobe_constructors_do_not_agree() {
        let access = RuntimeError::aioobe(9, 4);
        let index_only = RuntimeError::aioobe_index_only(9);
        let reflective = RuntimeError::aioobe_no_message(9);
        assert_eq!(
            access.as_java_throwable().and_then(|(_, m)| m).as_deref(),
            Some("Index 9 out of bounds for length 4")
        );
        assert_eq!(
            index_only
                .as_java_throwable()
                .and_then(|(_, m)| m)
                .as_deref(),
            Some("Array index out of range: 9")
        );
        assert_eq!(
            reflective
                .as_java_throwable()
                .and_then(|(_, m)| m)
                .as_deref(),
            None
        );
    }

    /// `NotImplemented` is a VM gap, not something Java can catch.
    #[test]
    fn not_implemented_is_not_a_java_throwable() {
        let err = RuntimeError::NotImplemented {
            feature: "whatever".to_string(),
        };
        assert!(err.as_java_throwable().is_none());
    }

    // -- VmError Display tests --

    #[test]
    fn vm_error_class_file_display() {
        let err = VmError::ClassFile(ClassFileError::ClassNotFound {
            class_name: "com/example/Foo".into(),
        });
        assert_eq!(
            format!("{err}"),
            "class file error: class not found: com/example/Foo"
        );
    }

    #[test]
    fn vm_error_linkage_display() {
        let err = VmError::Linkage(LinkageError::NoClassDefFoundError {
            class_name: "Bar".into(),
        });
        assert_eq!(format!("{err}"), "linkage error: no class def found: Bar");
    }

    #[test]
    fn vm_error_runtime_display() {
        let err = VmError::Runtime(RuntimeError::StackOverflowError);
        assert_eq!(format!("{err}"), "runtime error: StackOverflowError");
    }

    #[test]
    fn vm_error_internal_display() {
        let err = VmError::Internal {
            message: "something broke".into(),
        };
        assert_eq!(format!("{err}"), "internal error: something broke");
    }

    // -- VmError From conversions --

    #[test]
    fn vm_error_from_class_file_error() {
        let cfe = ClassFileError::ClassNotFound {
            class_name: "X".into(),
        };
        let vm_err: VmError = cfe.into();
        assert!(matches!(vm_err, VmError::ClassFile(_)));
    }

    #[test]
    fn vm_error_from_linkage_error() {
        let le = LinkageError::IllegalAccessError {
            message: "denied".into(),
        };
        let vm_err: VmError = le.into();
        assert!(matches!(vm_err, VmError::Linkage(_)));
    }

    #[test]
    fn vm_error_from_runtime_error() {
        let re = RuntimeError::StackOverflowError;
        let vm_err: VmError = re.into();
        assert!(matches!(vm_err, VmError::Runtime(_)));
    }

    // -- ClassFileError variants --

    #[test]
    fn class_file_error_class_not_found() {
        let err = ClassFileError::ClassNotFound {
            class_name: "java/lang/Object".into(),
        };
        assert_eq!(format!("{err}"), "class not found: java/lang/Object");
    }

    #[test]
    fn class_file_error_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err = ClassFileError::IoError {
            class_name: "Test".into(),
            source: io_err,
        };
        let display = format!("{err}");
        assert!(display.contains("I/O error reading class Test"));
        assert!(display.contains("file gone"));
    }

    #[test]
    fn class_file_error_invalid_class_file() {
        let err = ClassFileError::InvalidClassFile {
            class_name: "Bad".into(),
            message: "bad magic number".into(),
        };
        assert_eq!(format!("{err}"), "invalid class file Bad: bad magic number");
    }

    #[test]
    fn class_file_error_unsupported_version() {
        let err = ClassFileError::UnsupportedVersion {
            class_name: "Future".into(),
            major: 99,
            minor: 0,
        };
        assert_eq!(
            format!("{err}"),
            "unsupported class version 99.0 for class Future"
        );
    }

    // -- LinkageError variants --

    #[test]
    fn linkage_error_class_format_error() {
        let err = LinkageError::ClassFormatError {
            class_name: "X".into(),
            message: "corrupt".into(),
        };
        assert_eq!(format!("{err}"), "class format error in X: corrupt");
    }

    #[test]
    fn linkage_error_verify_error() {
        let err = LinkageError::VerifyError {
            class_name: "A".into(),
            method_name: "foo".into(),
            message: "bad stack".into(),
        };
        assert_eq!(format!("{err}"), "verification error in A.foo: bad stack");
    }

    #[test]
    fn linkage_error_no_such_field() {
        let err = LinkageError::NoSuchFieldError {
            class_name: "C".into(),
            field_name: "x".into(),
        };
        assert_eq!(format!("{err}"), "no such field: C.x");
    }

    #[test]
    fn linkage_error_no_such_method() {
        let err = LinkageError::NoSuchMethodError {
            class_name: "C".into(),
            method_name: "run".into(),
            method_descriptor: "(I)V".into(),
        };
        assert_eq!(format!("{err}"), "no such method: C.run(I)V");
    }

    #[test]
    fn linkage_error_incompatible_class_change() {
        let err = LinkageError::IncompatibleClassChangeError {
            message: "interface became class".into(),
        };
        assert_eq!(
            format!("{err}"),
            "incompatible class change: interface became class"
        );
    }

    #[test]
    fn linkage_error_illegal_access() {
        let err = LinkageError::IllegalAccessError {
            message: "private".into(),
        };
        assert_eq!(format!("{err}"), "illegal access: private");
    }

    #[test]
    fn linkage_error_abstract_method() {
        let err = LinkageError::AbstractMethodError {
            class_name: "I".into(),
            method_name: "doIt".into(),
        };
        assert_eq!(format!("{err}"), "abstract method error: I.doIt");
    }

    // -- RuntimeError variants --

    #[test]
    fn runtime_error_null_pointer_with_message() {
        let err = RuntimeError::NullPointerException {
            message: Some("field access".into()),
        };
        assert_eq!(format!("{err}"), "NullPointerException: field access");
    }

    #[test]
    fn runtime_error_null_pointer_without_message() {
        let err = RuntimeError::NullPointerException { message: None };
        assert_eq!(format!("{err}"), "NullPointerException");
    }

    #[test]
    fn runtime_error_array_index_out_of_bounds() {
        let err = RuntimeError::aioobe_no_message(-1);
        assert_eq!(format!("{err}"), "ArrayIndexOutOfBoundsException: index -1");
        assert_eq!(
            err.as_java_throwable().and_then(|(_, m)| m).as_deref(),
            None,
            "aioobe_no_message must produce a message-less throwable, i.e. the \
             no-arg constructor — HotSpot's reflect.Array behaviour"
        );
    }

    #[test]
    fn aioobe_carries_hotspots_array_access_wording() {
        let err = RuntimeError::aioobe(9, 4);
        let (cls, msg) = err.as_java_throwable().expect("is a Java throwable");
        assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
        assert_eq!(msg.as_deref(), Some("Index 9 out of bounds for length 4"));
        // The array-access wording IS `Preconditions.checkIndex`'s; if these
        // two ever diverge, one of them has been rewritten by hand.
        assert_eq!(
            out_of_bounds_message::check_index(9, 4),
            "Index 9 out of bounds for length 4"
        );
    }

    #[test]
    fn aioobe_negative_index_still_names_the_length() {
        // HotSpot prints the negative index verbatim rather than clamping.
        let err = RuntimeError::aioobe(-1, 4);
        assert_eq!(
            err.as_java_throwable().and_then(|(_, m)| m).as_deref(),
            Some("Index -1 out of bounds for length 4")
        );
    }

    #[test]
    fn arraycopy_wordings_match_hotspot() {
        use arraycopy_message as ac;
        assert_eq!(
            ac::last_source_index(0, 9, "int", 4),
            "arraycopy: last source index 9 out of bounds for int[4]"
        );
        assert_eq!(
            ac::last_destination_index(0, 9, "char", 4),
            "arraycopy: last destination index 9 out of bounds for char[4]"
        );
        assert_eq!(
            ac::source_index(-1, "short", 4),
            "arraycopy: source index -1 out of bounds for short[4]"
        );
        assert_eq!(
            ac::destination_index(-1, "float", 4),
            "arraycopy: destination index -1 out of bounds for float[4]"
        );
        assert_eq!(ac::negative_length(-1), "arraycopy: length -1 is negative");
        // A reference array is "object array", never its own class name.
        assert_eq!(
            ac::last_source_index(0, 9, "object array", 4),
            "arraycopy: last source index 9 out of bounds for object array[4]"
        );
        assert_eq!(
            ac::type_mismatch("int", "object array"),
            "arraycopy: type mismatch: can not copy int[] into object array[]"
        );
        assert_eq!(
            ac::source_not_an_array("java.lang.String"),
            "arraycopy: source type java.lang.String is not an array"
        );
    }

    #[test]
    fn arraycopy_element_type_names_are_hotspots() {
        use arraycopy_message::element_type_name as name;
        assert_eq!(name(crate::ArrayElementType::Int), "int");
        assert_eq!(name(crate::ArrayElementType::Boolean), "boolean");
        assert_eq!(name(crate::ArrayElementType::Char), "char");
        assert_eq!(name(crate::ArrayElementType::Byte), "byte");
        assert_eq!(name(crate::ArrayElementType::Short), "short");
        assert_eq!(name(crate::ArrayElementType::Long), "long");
        assert_eq!(name(crate::ArrayElementType::Float), "float");
        assert_eq!(name(crate::ArrayElementType::Double), "double");
        // Not "java.lang.String", not "Object" — HotSpot prints this literal
        // for every reference array, including an array of arrays.
        assert_eq!(name(crate::ArrayElementType::Reference), "object array");
    }

    #[test]
    fn arraycopy_element_type_mismatch_matches_hotspot() {
        use arraycopy_message::element_type_mismatch as msg;
        // MEASURED on HotSpot 25.0.4+7, `probes/DodArrayStoreSweep`. The source
        // half is an external name and the destination half is the component
        // Klass's dotted NAME -- two dialects in one sentence, which is
        // HotSpot's and not ours to normalise.
        assert_eq!(
            msg("java/lang/Object", "java/lang/String"),
            "arraycopy: element type mismatch: can not cast one of the elements \
             of java.lang.Object[] to the type of the destination array, java.lang.String"
        );
        assert_eq!(
            msg("java/lang/Object", "[Ljava/lang/Integer;"),
            "arraycopy: element type mismatch: can not cast one of the elements \
             of java.lang.Object[] to the type of the destination array, [Ljava.lang.Integer;"
        );
        assert_eq!(
            msg("java/lang/Object", "[I"),
            "arraycopy: element type mismatch: can not cast one of the elements \
             of java.lang.Object[] to the type of the destination array, [I"
        );
    }

    #[test]
    fn external_class_name_is_hotspots_external_name() {
        use arraycopy_message::external_class_name as ext;
        assert_eq!(ext("java/lang/String"), "java.lang.String");
        assert_eq!(ext("[Ljava/lang/String;"), "java.lang.String[]");
        assert_eq!(ext("[[Ljava/lang/String;"), "java.lang.String[][]");
        assert_eq!(ext("[I"), "int[]");
        assert_eq!(ext("[[D"), "double[][]");
    }

    #[test]
    fn arraycopy_last_index_is_printed_unsigned() {
        // HotSpot formats `pos + length` with `%u`, so an addition that
        // overflows `int` renders as a large positive number. Printing it
        // signed would produce a negative "last index", which no HotSpot
        // message ever shows.
        assert_eq!(
            arraycopy_message::last_source_index(i32::MAX, 1, "int", 4),
            "arraycopy: last source index 2147483648 out of bounds for int[4]"
        );
    }

    #[test]
    fn arraycopy_message_carries_through_to_the_throwable() {
        let err = RuntimeError::aioobe_with_message(
            9,
            arraycopy_message::last_source_index(0, 9, "int", 4),
        );
        let (cls, msg) = err.as_java_throwable().expect("is a Java throwable");
        assert_eq!(cls, "java/lang/ArrayIndexOutOfBoundsException");
        assert_eq!(
            msg.as_deref(),
            Some("arraycopy: last source index 9 out of bounds for int[4]")
        );
    }

    #[test]
    fn runtime_error_arithmetic() {
        let err = RuntimeError::ArithmeticException {
            message: "/ by zero".into(),
        };
        assert_eq!(format!("{err}"), "ArithmeticException: / by zero");
    }

    #[test]
    fn runtime_error_class_cast() {
        let err = RuntimeError::ClassCastException {
            message: "String cannot be cast to Integer".into(),
        };
        assert_eq!(
            format!("{err}"),
            "ClassCastException: String cannot be cast to Integer"
        );
    }

    #[test]
    fn runtime_error_stack_overflow() {
        let err = RuntimeError::StackOverflowError;
        assert_eq!(format!("{err}"), "StackOverflowError");
    }

    #[test]
    fn runtime_error_out_of_memory() {
        let err = RuntimeError::OutOfMemoryError {
            message: "heap full".into(),
        };
        assert_eq!(format!("{err}"), "OutOfMemoryError: heap full");
    }

    #[test]
    fn runtime_error_negative_array_size() {
        let err = RuntimeError::NegativeArraySizeException { size: -5 };
        assert_eq!(format!("{err}"), "NegativeArraySizeException: -5");
    }

    #[test]
    fn runtime_error_array_store() {
        let err = RuntimeError::ArrayStoreException {
            message: "wrong type".into(),
        };
        assert_eq!(format!("{err}"), "ArrayStoreException: wrong type");
    }

    #[test]
    fn runtime_error_string_index_out_of_bounds() {
        let err = RuntimeError::sioobe_no_length(99);
        assert_eq!(
            format!("{err}"),
            "StringIndexOutOfBoundsException: index 99"
        );
    }

    #[test]
    fn runtime_error_class_not_found() {
        let err = RuntimeError::ClassNotFoundException {
            class_name: "Missing".into(),
        };
        assert_eq!(format!("{err}"), "ClassNotFoundException: Missing");
    }

    #[test]
    fn runtime_error_unsatisfied_link() {
        let err = RuntimeError::UnsatisfiedLinkError {
            message: "native lib".into(),
        };
        assert_eq!(format!("{err}"), "UnsatisfiedLinkError: native lib");
    }

    #[test]
    fn runtime_error_illegal_monitor_state() {
        let err = RuntimeError::IllegalMonitorStateException {
            message: "not owner".into(),
        };
        assert_eq!(format!("{err}"), "IllegalMonitorStateException: not owner");
    }

    #[test]
    fn runtime_error_number_format() {
        let err = RuntimeError::NumberFormatException {
            message: "abc".into(),
        };
        assert_eq!(format!("{err}"), "NumberFormatException: abc");
    }

    #[test]
    fn runtime_error_interrupted() {
        let err = RuntimeError::InterruptedException;
        assert_eq!(format!("{err}"), "InterruptedException");
    }

    #[test]
    fn runtime_error_not_implemented() {
        let err = RuntimeError::NotImplemented {
            feature: "invokedynamic".into(),
        };
        assert_eq!(format!("{err}"), "not implemented: invokedynamic");
    }

    #[test]
    fn runtime_error_concurrent_modification() {
        let err = RuntimeError::ConcurrentModificationException;
        assert_eq!(format!("{err}"), "ConcurrentModificationException");
    }

    #[test]
    fn runtime_error_io_exception() {
        let err = RuntimeError::IOException {
            message: "broken pipe".into(),
        };
        assert_eq!(format!("{err}"), "IOException: broken pipe");
    }

    #[test]
    fn runtime_error_file_not_found() {
        let err = RuntimeError::FileNotFoundException {
            path: "/tmp/missing.txt".into(),
        };
        assert_eq!(format!("{err}"), "FileNotFoundException: /tmp/missing.txt");
    }

    #[test]
    fn runtime_error_unsupported_operation() {
        let err = RuntimeError::UnsupportedOperationException {
            message: "immutable".into(),
        };
        assert_eq!(format!("{err}"), "UnsupportedOperationException: immutable");
    }

    #[test]
    fn runtime_error_illegal_state() {
        let err = RuntimeError::IllegalStateException {
            message: "closed".into(),
        };
        assert_eq!(format!("{err}"), "IllegalStateException: closed");
    }

    #[test]
    fn runtime_error_illegal_caller_display() {
        // The new variant exists and formats consistently with its siblings —
        // bare class name followed by ": <message>", no double prefix.
        let err = RuntimeError::IllegalCallerException {
            message: "Native access is not enabled for this module".into(),
        };
        assert_eq!(
            format!("{err}"),
            "IllegalCallerException: Native access is not enabled for this module"
        );
    }

    #[test]
    fn runtime_error_illegal_caller_is_distinct_from_illegal_state() {
        // Regression guard for task #57: the Panama native-access gate used
        // to fold IllegalCallerException into IllegalStateException because
        // the variant did not exist. The two variants must remain distinct
        // at the Rust level so the exception-mapping table can route them
        // to different Java classes.
        let caller = RuntimeError::IllegalCallerException {
            message: "denied".into(),
        };
        let state = RuntimeError::IllegalStateException {
            message: "denied".into(),
        };
        assert!(matches!(
            caller,
            RuntimeError::IllegalCallerException { .. }
        ));
        assert!(matches!(state, RuntimeError::IllegalStateException { .. }));
        // Display strings must not collide.
        assert_ne!(format!("{caller}"), format!("{state}"));
    }

    #[test]
    fn runtime_error_no_such_element() {
        let err = RuntimeError::NoSuchElementException {
            message: "empty".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchElementException: empty");
    }

    #[test]
    fn runtime_error_input_mismatch() {
        let err = RuntimeError::InputMismatchException {
            message: "expected int".into(),
        };
        assert_eq!(format!("{err}"), "InputMismatchException: expected int");
    }

    #[test]
    fn runtime_error_no_such_field_exception() {
        let err = RuntimeError::NoSuchFieldException {
            field_name: "value".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchFieldException: value");
    }

    #[test]
    fn runtime_error_no_such_method_exception() {
        let err = RuntimeError::NoSuchMethodException {
            message: "run()".into(),
        };
        assert_eq!(format!("{err}"), "NoSuchMethodException: run()");
    }

    #[test]
    fn runtime_error_illegal_access_exception() {
        let err = RuntimeError::IllegalAccessException {
            message: "private method".into(),
        };
        assert_eq!(format!("{err}"), "IllegalAccessException: private method");
    }

    #[test]
    fn runtime_error_illegal_argument_exception() {
        let err = RuntimeError::IllegalArgumentException {
            message: "negative".into(),
        };
        assert_eq!(format!("{err}"), "IllegalArgumentException: negative");
    }

    // -- MethodCallFailed tests --

    #[test]
    fn method_call_failed_internal_error_display() {
        let err = MethodCallFailed::InternalError(VmError::Internal {
            message: "oops".into(),
        });
        // `MethodCallFailed::InternalError` delegates to the inner `VmError`'s
        // `Display`, which already supplies the "internal error: " prefix —
        // so the prefix must appear exactly once, not twice.
        assert_eq!(format!("{err}"), "internal error: oops");
    }

    #[test]
    fn method_call_failed_exception_thrown_display() {
        let fake_ptr = 0xDEAD_BEE0_u64 as *mut u8; // must be 8-byte aligned
        let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
        let err = MethodCallFailed::ExceptionThrown(obj);
        let display = format!("{err}");
        assert!(display.starts_with("exception thrown: ref(0x"));
    }

    #[test]
    fn method_call_failed_from_vm_error() {
        let vm_err = VmError::Internal {
            message: "bug".into(),
        };
        let mcf: MethodCallFailed = vm_err.into();
        assert!(matches!(mcf, MethodCallFailed::InternalError(_)));
    }

    #[test]
    fn method_call_failed_from_class_file_error() {
        let cfe = ClassFileError::ClassNotFound {
            class_name: "X".into(),
        };
        let mcf: MethodCallFailed = cfe.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::ClassFile(_))
        ));
    }

    #[test]
    fn method_call_failed_from_linkage_error() {
        let le = LinkageError::NoClassDefFoundError {
            class_name: "Y".into(),
        };
        let mcf: MethodCallFailed = le.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::Linkage(_))
        ));
    }

    #[test]
    fn method_call_failed_from_runtime_error() {
        let re = RuntimeError::StackOverflowError;
        let mcf: MethodCallFailed = re.into();
        assert!(matches!(
            mcf,
            MethodCallFailed::InternalError(VmError::Runtime(_))
        ));
    }

    // -- MethodCallResult pattern matching --

    #[test]
    fn method_call_result_ok_value() {
        let result: MethodCallResult = Ok(Some(Value::Int(42)));
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap().as_int(), Some(42));
    }

    #[test]
    fn method_call_result_ok_void() {
        let result: MethodCallResult = Ok(None);
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn method_call_result_err_internal() {
        let result: MethodCallResult = Err(MethodCallFailed::InternalError(VmError::Internal {
            message: "fail".into(),
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, MethodCallFailed::InternalError(_)));
    }

    // -- JdkOnlyViolation --

    /// One of each variant, fully populated, so a test can sweep all seven.
    fn all_variants() -> Vec<JdkOnlyViolation> {
        vec![
            JdkOnlyViolation::CompatibilityClassRequested {
                class: "org/jboss/Absent".into(),
                initiating_loader: Some("app".into()),
                requester: Some("com/example/Boot.start()V".into()),
                reason: "enterprise-prefix fallback".into(),
            },
            JdkOnlyViolation::SyntheticNativeRegistered {
                class: "com/example/Strict".into(),
                method: "fake".into(),
                descriptor: "(I)Ljava/lang/String;".into(),
                registered_by: Some("native-builtins/src/lib.rs:1234".into()),
            },
            JdkOnlyViolation::SyntheticNativeInvocation {
                class: "com/example/Strict".into(),
                method: "fake".into(),
                descriptor: "(Ljava/lang/Object;)Z".into(),
                call_site: None,
            },
            JdkOnlyViolation::MissingNative {
                class: "java/net/Socket".into(),
                method: "socket0".into(),
                descriptor: "(ZZZZ)I".into(),
                module: Some("java.base".into()),
            },
            JdkOnlyViolation::NativeShadowsBytecode {
                class: "java/util/HashMap".into(),
                method: "put".into(),
                descriptor: "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;".into(),
                native_kind: "jit-thin-direct-helper",
            },
            JdkOnlyViolation::MissingBootClass {
                class: "java/lang/Object".into(),
                searched_image: "jdk-25".into(),
            },
            JdkOnlyViolation::MissingImplementation {
                class: "com/example/Owner".into(),
                method: "compute".into(),
                descriptor: "()J".into(),
            },
        ]
    }

    /// The kind tags are a wire format: `difftest/src/census.rs` tallies
    /// `--jdk-only-report` rows by exactly these strings, and
    /// `vm/tests/jdk_only_dispatch.rs` asserts three of them literally. Pinning
    /// them here means a re-spelling fails in this crate rather than silently
    /// producing a report whose every row tallies as an unknown kind.
    #[test]
    fn kind_tags_are_pinned() {
        let expected = [
            "compatibility-class-requested",
            "synthetic-native-registered",
            "synthetic-native-invocation",
            "missing-native",
            "native-shadows-bytecode",
            "missing-boot-class",
            "missing-implementation",
        ];
        let actual: Vec<&str> = all_variants().iter().map(|v| v.kind()).collect();
        assert_eq!(actual, expected);
    }

    /// Two kinds sharing a tag would make the report unable to say which policy
    /// rule fired, and would silently merge two counters into one.
    #[test]
    fn kind_tags_are_distinct() {
        let mut tags: Vec<&str> = all_variants().iter().map(|v| v.kind()).collect();
        let before = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(before, tags.len(), "two variants share a kind tag");
        assert_eq!(before, 7, "the contract defines exactly seven variants");
    }

    /// Every method-bearing variant must be identifiable down to the overload.
    /// A refusal naming `HashMap.put` without the descriptor cannot be turned
    /// into a work item (§1.7), and both the dispatch and registry suites
    /// assert the descriptor is present in `summary()`.
    #[test]
    fn summary_names_the_class_and_the_overload() {
        for v in all_variants() {
            let summary = v.summary();
            assert!(
                summary.contains(v.class()),
                "{}: summary omits the class: {summary}",
                v.kind()
            );
            if let Some((method, descriptor)) = v.member() {
                assert!(
                    summary.contains(method) && summary.contains(descriptor),
                    "{}: summary omits the overload: {summary}",
                    v.kind()
                );
            }
        }
    }

    #[test]
    fn display_is_exactly_summary() {
        for v in all_variants() {
            assert_eq!(format!("{v}"), v.summary());
        }
    }

    /// `render` ends with the two fixed lines, in that order. An operator who
    /// scrolls to the bottom of a wall of diagnostics has to find the fallback
    /// and the capture hint in the same place every time; the ordering is also
    /// the one thing about the layout other agents were told to rely on.
    #[test]
    fn remediation_ends_with_the_two_fixed_lines() {
        for v in all_variants() {
            let rendered = v.render(Some(25), true);
            let lines: Vec<&str> = rendered.lines().collect();
            let last = lines[lines.len() - 1].trim();
            let penultimate = lines[lines.len() - 2].trim();
            assert_eq!(
                penultimate,
                REMEDIATION_FALLBACK,
                "{}: fallback is not the penultimate line\n{rendered}",
                v.kind()
            );
            assert_eq!(
                last,
                REMEDIATION_CAPTURE,
                "{}: capture hint is not the last line\n{rendered}",
                v.kind()
            );
            assert!(
                rendered.contains("--real-jdk"),
                "§1.7 requires the fallback to be named"
            );
            assert!(rendered.contains("--jdk-only-report <FILE>"));

            // The blocks the contract requires, in order.
            let remediation_at = rendered.find("  Remediation:").expect("remediation block");
            let jdk_at = rendered.find("  JDK:").expect("JDK block");
            let reason_at = rendered.find("  reason:").expect("reason line");
            let from_at = rendered
                .find("  requested from:")
                .expect("requested-from line");
            let class_at = rendered
                .find("  requested class:")
                .expect("requested-class line");
            assert!(
                class_at < from_at && from_at < reason_at && reason_at < jdk_at,
                "{}: block order is wrong\n{rendered}",
                v.kind()
            );
            assert!(jdk_at < remediation_at, "remediation must come last");
            assert!(rendered.contains("feature version: 25"));
        }
    }

    #[test]
    fn render_reports_an_unknown_feature_version_rather_than_guessing() {
        let v = JdkOnlyViolation::MissingNative {
            class: "java/net/Socket".into(),
            method: "socket0".into(),
            descriptor: "(ZZZZ)I".into(),
            module: Some("java.base".into()),
        };
        let rendered = v.render(None, true);
        assert!(
            rendered.contains("feature version: <unknown>"),
            "{rendered}"
        );
        // The module the reporter *did* know is still named.
        assert!(
            rendered.contains("module:          java.base"),
            "{rendered}"
        );
    }

    /// The three absolute forms leak the layout of the machine the run happened
    /// on and are redacted; a relative path is the useful, harmless one and
    /// passes through byte-for-byte.
    #[test]
    fn absolute_paths_are_redacted_unless_verbose() {
        let absolute = [
            "/opt/jdk-25/lib/modules",
            "C:\\Program\\jdk-25",
            "c:/jdk-25/lib",
            "\\\\build01\\share\\jdk-25",
            "//build01/share/jdk-25",
            "\\Windows\\jdk",
        ];
        for path in absolute {
            assert!(is_absolute_path(path), "{path} should count as absolute");
            assert_eq!(redact_paths(path, false), REDACTED, "{path} not redacted");
            assert_eq!(redact_paths(path, true), path, "verbose must not redact");
        }

        let relative = [
            "native-builtins/src/lib.rs:1234",
            "java/lang/Object",
            "(Ljava/lang/String;)V",
            "com/example/Boot.start()V",
            "jdk-25",
        ];
        for path in relative {
            assert!(!is_absolute_path(path), "{path} is not absolute");
            assert_eq!(
                redact_paths(path, false),
                path,
                "a relative path must pass through untouched"
            );
        }

        // Mixed text: only the absolute token goes.
        assert_eq!(
            redact_paths("searched /opt/jdk-25 and lib/modules", false),
            "searched <redacted> and lib/modules"
        );
    }

    #[test]
    fn render_redacts_the_searched_image_but_keeps_the_registrar_line() {
        let boot = JdkOnlyViolation::MissingBootClass {
            class: "java/lang/Object".into(),
            searched_image: "/opt/jdk-25/lib/modules".into(),
        };
        let quiet = boot.render(Some(25), false);
        assert!(!quiet.contains("/opt/jdk-25"), "{quiet}");
        assert!(quiet.contains(REDACTED), "{quiet}");
        assert!(boot
            .render(Some(25), true)
            .contains("/opt/jdk-25/lib/modules"));

        // `#[track_caller]` provenance is workspace-relative: redacting it
        // would delete the only actionable fact in the report.
        let registered = JdkOnlyViolation::SyntheticNativeRegistered {
            class: "com/example/Strict".into(),
            method: "fake".into(),
            descriptor: "(I)Ljava/lang/String;".into(),
            registered_by: Some("native-builtins/src/lib.rs:1234".into()),
        };
        assert!(registered
            .render(Some(25), false)
            .contains("native-builtins/src/lib.rs:1234"));
    }

    /// The JSON is hand-rolled, so the three ways a hand-rolled dump goes wrong
    /// — unbalanced braces, a trailing comma, an unescaped quote — are pinned
    /// here rather than discovered by a consumer that cannot parse the report.
    #[test]
    fn to_json_is_well_formed() {
        for v in all_variants() {
            let json = v.to_json();
            assert!(json.starts_with('{') && json.ends_with('}'), "{json}");
            assert!(!json.contains(",}"), "trailing comma in {json}");
            assert!(!json.contains("{,"), "leading comma in {json}");

            // Braces balance, counting only those outside string literals.
            let mut depth = 0i32;
            let mut in_string = false;
            let mut escaped = false;
            for ch in json.chars() {
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        in_string = false;
                    }
                    continue;
                }
                match ch {
                    '"' => in_string = true,
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                assert!(depth >= 0, "unbalanced braces in {json}");
            }
            assert_eq!(depth, 0, "unbalanced braces in {json}");
            assert!(!in_string, "unterminated string in {json}");

            assert!(
                json.contains(&format!("\"kind\":\"{}\"", v.kind())),
                "every row must be internally tagged: {json}"
            );
            assert!(json.contains("\"summary\":\""), "{json}");
            assert!(json.contains("\"class\":\""), "{json}");
        }
    }

    /// Absent optionals are `null`, not omitted: every row of a kind then has
    /// the same shape, so a consumer can index columns without probing.
    #[test]
    fn to_json_emits_null_for_absent_optionals() {
        let invocation = JdkOnlyViolation::SyntheticNativeInvocation {
            class: "C".into(),
            method: "m".into(),
            descriptor: "()V".into(),
            call_site: None,
        };
        assert!(
            invocation.to_json().contains("\"call_site\":null"),
            "{}",
            invocation.to_json()
        );

        let present = JdkOnlyViolation::SyntheticNativeInvocation {
            class: "C".into(),
            method: "m".into(),
            descriptor: "()V".into(),
            call_site: Some("D.n()V".into()),
        };
        assert!(present.to_json().contains("\"call_site\":\"D.n()V\""));

        let missing = JdkOnlyViolation::MissingNative {
            class: "C".into(),
            method: "m".into(),
            descriptor: "()V".into(),
            module: None,
        };
        assert!(missing.to_json().contains("\"module\":null"));
    }

    #[test]
    fn to_json_escapes_strings() {
        let v = JdkOnlyViolation::CompatibilityClassRequested {
            class: "org/x/Q".into(),
            initiating_loader: None,
            requester: None,
            reason: "said \"no\" at C:\\build\\x\tand\nstopped".into(),
        };
        let json = v.to_json();
        assert!(json.contains("\\\"no\\\""), "{json}");
        assert!(json.contains("C:\\\\build\\\\x"), "{json}");
        assert!(json.contains("\\t"), "{json}");
        assert!(json.contains("\\n"), "{json}");
        // The raw control characters must not survive into the output.
        assert!(!json.contains('\t'));
        assert!(!json.contains('\n'));
        assert!(json.contains("\"initiating_loader\":null"));

        let mut escaped = String::new();
        json_string(&mut escaped, "\u{1}");
        assert_eq!(escaped, "\"\\u0001\"");
    }

    // -- the two new VmError variants --

    #[test]
    fn vm_error_invalid_configuration_display() {
        let err =
            VmError::InvalidConfiguration("--jdk-only conflicts with --synthetic-jdk".to_string());
        assert_eq!(
            format!("{err}"),
            "invalid configuration: --jdk-only conflicts with --synthetic-jdk"
        );
        assert!(matches!(err, VmError::InvalidConfiguration(_)));
    }

    /// `From` is hand-written because `#[from]` would also make the violation
    /// the error's `source()`, which needs `JdkOnlyViolation: std::error::Error`
    /// — a bound the contract's `Display`-only type does not have. This pins
    /// both that the conversion exists and that the Display text is the
    /// violation's own summary behind one category prefix.
    #[test]
    fn vm_error_jdk_only_variant_and_conversion() {
        let violation = JdkOnlyViolation::MissingNative {
            class: "java/net/Socket".into(),
            method: "socket0".into(),
            descriptor: "(ZZZZ)I".into(),
            module: Some("java.base".into()),
        };
        let err: VmError = violation.clone().into();
        assert!(matches!(err, VmError::JdkOnly(_)));
        assert_eq!(format!("{err}"), format!("jdk-only violation: {violation}"));
        assert!(format!("{err}").contains("(ZZZZ)I"));

        // And it travels through the two-layer exception model unchanged: the
        // JNI and interpreter surfacing paths match on exactly this shape.
        let failed: MethodCallFailed = VmError::JdkOnly(violation.clone()).into();
        match failed {
            MethodCallFailed::InternalError(VmError::JdkOnly(inner)) => {
                assert_eq!(inner, violation);
            }
            other => panic!("expected InternalError(JdkOnly), got {other:?}"),
        }
    }

    // -- format_optional_message --

    #[test]
    fn format_optional_message_some() {
        let result = format_optional_message(&Some("detail".into()));
        assert_eq!(result, ": detail");
    }

    #[test]
    fn format_optional_message_none() {
        let result = format_optional_message(&None);
        assert_eq!(result, "");
    }
}
