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
    NativeShadowsBytecode {
        class: String,
        method: String,
        descriptor: String,
        /// `NativeKind::as_str()`, or the VM mechanism when the reporting crate
        /// cannot see that enum (the JIT's thin direct helpers).
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
            JdkOnlyViolation::NativeShadowsBytecode { native_kind, .. } => format!(
                "a registered {native_kind} native stands in front of the real class \
                 bytes; concrete bytecode wins under --jdk-only"
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
            JdkOnlyViolation::SyntheticNativeRegistered { .. } => &[
                "reclassify the registration as a Bridge or a reviewed Intrinsic, or delete it",
            ],
            JdkOnlyViolation::SyntheticNativeInvocation { .. } => &[
                "the real JDK implements this method; check why its bytes were not loaded",
            ],
            JdkOnlyViolation::MissingNative { .. } => {
                &["implement the method as a NativeKind::Bridge and register it at VM init"]
            }
            JdkOnlyViolation::NativeShadowsBytecode { .. } => &[
                "unregister the native, or have it reviewed and reclassified as an Intrinsic",
            ],
            JdkOnlyViolation::MissingBootClass { .. } => &[
                "point --jdk-home at a complete JDK runtime image (one with lib/modules)",
            ],
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
            JdkOnlyViolation::NativeShadowsBytecode {
                class,
                method,
                descriptor,
                native_kind,
            } => format!(
                "{native_kind} native shadows bytecode of {class}.{method}{descriptor}"
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
                json_field(&mut out, &mut first, "descriptor", Some(descriptor.as_str()));
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
                json_field(&mut out, &mut first, "descriptor", Some(descriptor.as_str()));
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
                json_field(&mut out, &mut first, "descriptor", Some(descriptor.as_str()));
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
                json_field(&mut out, &mut first, "descriptor", Some(descriptor.as_str()));
                json_field(&mut out, &mut first, "native_kind", Some(*native_kind));
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
                json_field(&mut out, &mut first, "descriptor", Some(descriptor.as_str()));
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
        [drive, b':', sep, ..] if drive.is_ascii_alphabetic() && (*sep == b'/' || *sep == b'\\') => {
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

    #[error("ArrayIndexOutOfBoundsException: index {index}")]
    ArrayIndexOutOfBoundsException { index: i32 },
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
    StringIndexOutOfBoundsException {
        index: i32,
        message: Option<String>,
    },

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

    #[error("UnsupportedOperationException: {message}")]
    UnsupportedOperationException { message: String },

    #[error("IllegalStateException: {message}")]
    IllegalStateException { message: String },

    #[error("IllegalThreadStateException: {message}")]
    IllegalThreadStateException { message: String },

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
    pub fn as_java_throwable(&self) -> Option<(&'static str, Option<&str>)> {
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
            RuntimeError::ArrayIndexOutOfBoundsException { index: _ } => {
                ("java/lang/ArrayIndexOutOfBoundsException", None)
            }
            RuntimeError::IndexOutOfBoundsException { message } => {
                ("java/lang/IndexOutOfBoundsException", message.as_deref())
            }
            RuntimeError::ClassCastException { message } => {
                ("java/lang/ClassCastException", Some(message.as_str()))
            }
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
                Some(message.as_str()),
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
            RuntimeError::IllegalArgumentException { message } => {
                ("java/lang/IllegalArgumentException", Some(message.as_str()))
            }
            RuntimeError::IOException { message } => ("java/io/IOException", Some(message.as_str())),
            RuntimeError::EOFException { message } => ("java/io/EOFException", Some(message.as_str())),
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
                Some(message.as_str()),
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
                ("java/util/NoSuchElementException", Some(message.as_str()))
            }
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
        };
        Some(pair)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let err = RuntimeError::ArrayIndexOutOfBoundsException { index: -1 };
        assert_eq!(format!("{err}"), "ArrayIndexOutOfBoundsException: index -1");
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
                penultimate, REMEDIATION_FALLBACK,
                "{}: fallback is not the penultimate line\n{rendered}",
                v.kind()
            );
            assert_eq!(
                last, REMEDIATION_CAPTURE,
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
        assert!(rendered.contains("feature version: <unknown>"), "{rendered}");
        // The module the reporter *did* know is still named.
        assert!(rendered.contains("module:          java.base"), "{rendered}");
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
        assert!(boot.render(Some(25), true).contains("/opt/jdk-25/lib/modules"));

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
        let err = VmError::InvalidConfiguration(
            "--jdk-only conflicts with --synthetic-jdk".to_string(),
        );
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
