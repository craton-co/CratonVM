// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Class-identity answers a native method can act on.
//!
//! # Why this module exists
//!
//! A runtime class is identified by `(binary name, defining loader)`, never by
//! name alone. Every by-name entry point on [`NativeContext`] takes only the
//! name, so each of them has to say something about the case where the name is
//! carried by **two distinct classes**. Until this module they all said the
//! same thing they say for a name nobody has ever defined:
//!
//! * [`NativeClassAccess::class_id_by_name`] returns `None` — indistinguishable
//!   from absent;
//! * [`NativeSystemAccess::ensure_synthetic_class`] returns a `ClassId` — and
//!   the class manager behind it used to *mint a stub under the ambiguous
//!   name*, filed under the bootstrap loader, which is probed first and
//!   therefore outranks both real classes from that moment on.
//!
//! Collapsing *ambiguous* onto *absent* is how a second (then a third) copy of
//! a class gets minted. Two genuinely distinct classes treated as one is type
//! confusion: it defeats the verifier and produces machine code that reads the
//! wrong object layout.
//!
//! So this module gives natives the two things they were missing: a way to
//! **ask** ([`NameLookup`]) and a way to be **refused** ([`ClassIdentityError`]).
//!
//! # Relationship to the class manager
//!
//! [`NameLookup`] mirrors `classloading::class_manager::NameResolution`. It is
//! a separate type on purpose: `native-api` sits *below* `classloading` in the
//! crate graph and cannot depend on it. The VM's `NativeContextImpl`
//! (`vm/src/vm/vm_exec.rs`) is the single translation point between the two,
//! and the variants are deliberately one-to-one so that translation cannot
//! quietly lose a case.
//!
//! [`NativeContext`]: crate::registry::NativeContext
//! [`NativeClassAccess::class_id_by_name`]: crate::registry::NativeClassAccess::class_id_by_name
//! [`NativeSystemAccess::ensure_synthetic_class`]: crate::registry::NativeSystemAccess::ensure_synthetic_class

use core::fmt;

use cratonvm_types::error::{ClassFileError, LinkageError, MethodCallFailed, VmError};
use cratonvm_types::ClassId;

/// What the VM's class-name index knows about a name, with the "no single
/// answer" outcomes kept apart.
///
/// The distinction is only useful to a caller that would otherwise **act** on
/// a miss — load the class, or fabricate a stand-in for it. `Absent` permits
/// that; `Ambiguous` forbids it, because acting produces one more class under
/// a name that already has too many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameLookup {
    /// No loader has a class registered under this name. Loading or
    /// fabricating one is safe.
    Absent,
    /// Exactly one class carries this name. Several loaders may *name* it (a
    /// delegation alias), which is still one class and still unique.
    Unique(ClassId),
    /// Two or more **distinct** classes carry this name. There is no correct
    /// context-free answer: re-ask with an initiating loader
    /// ([`class_id_by_name_and_loader`], [`class_id_by_name_via_referencing_class`])
    /// or fail.
    ///
    /// [`class_id_by_name_and_loader`]: crate::registry::NativeClassAccess::class_id_by_name_and_loader
    /// [`class_id_by_name_via_referencing_class`]: crate::registry::NativeClassAccess::class_id_by_name_via_referencing_class
    Ambiguous {
        /// How many distinct classes carry the name.
        definitions: usize,
    },
}

impl NameLookup {
    /// The class id when the name resolves uniquely, `None` for both `Absent`
    /// and `Ambiguous`.
    ///
    /// This reproduces the old `Option`-returning answer exactly, for call
    /// sites that genuinely do not care *why* there is no answer. Anything
    /// that would load or fabricate on a miss must match on the variants
    /// instead — that is the whole point of the type.
    #[inline]
    pub fn unique(self) -> Option<ClassId> {
        match self {
            NameLookup::Unique(id) => Some(id),
            NameLookup::Absent | NameLookup::Ambiguous { .. } => None,
        }
    }

    /// True when more than one distinct class carries the name.
    #[inline]
    pub fn is_ambiguous(self) -> bool {
        matches!(self, NameLookup::Ambiguous { .. })
    }
}

/// Why the VM refused to hand back a class for a name.
///
/// Two variants, because a native's correct response differs: an ambiguous
/// name can be retried with an initiating loader, a policy refusal cannot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassIdentityError {
    /// The name is already carried by `definitions` distinct classes, so any
    /// stand-in minted under it would shadow all of them.
    ///
    /// **What a native should do:** refuse in turn. Re-ask with an initiating
    /// loader if one is in hand ([`class_id_by_name_and_loader`],
    /// [`class_id_by_name_via_referencing_class`]); otherwise propagate. Do not
    /// substitute another class, and do not retry the same question — it has
    /// the same answer.
    ///
    /// [`class_id_by_name_and_loader`]: crate::registry::NativeClassAccess::class_id_by_name_and_loader
    /// [`class_id_by_name_via_referencing_class`]: crate::registry::NativeClassAccess::class_id_by_name_via_referencing_class
    AmbiguousName {
        /// The requested binary name.
        name: String,
        /// How many distinct classes already carry it.
        definitions: usize,
    },
    /// The VM declined to fabricate a stand-in as a matter of policy — today,
    /// `--jdk-only`, which forbids compatibility stubs outright.
    ///
    /// **What a native should do:** propagate. Retrying cannot help.
    Refused {
        /// The requested binary name.
        name: String,
        /// Operator-facing explanation, straight from the VM.
        reason: String,
    },
}

impl ClassIdentityError {
    /// The binary name the caller asked about.
    pub fn name(&self) -> &str {
        match self {
            ClassIdentityError::AmbiguousName { name, .. }
            | ClassIdentityError::Refused { name, .. } => name,
        }
    }

    /// True for the identity conflict, false for the policy refusal.
    ///
    /// The predicate exists so a caller can pick the loader-aware retry
    /// without matching on a variant it would otherwise not care about.
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, ClassIdentityError::AmbiguousName { .. })
    }
}

impl fmt::Display for ClassIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClassIdentityError::AmbiguousName { name, definitions } => write!(
                f,
                "ambiguous class name {name}: {definitions} distinct classes are already \
                 defined under it, so no context-free answer exists; refusing to guess",
            ),
            ClassIdentityError::Refused { name, reason } => {
                write!(f, "refused to supply a class for {name}: {reason}")
            }
        }
    }
}

impl std::error::Error for ClassIdentityError {}

impl From<ClassIdentityError> for VmError {
    /// Keeps the two refusals in different `VmError` families.
    ///
    /// The ambiguity conflict becomes a `LinkageError` — the family HotSpot
    /// uses for loader-constraint violations, which is the same disease (one
    /// name, two runtime types). The policy refusal becomes `ClassNotFound`,
    /// which is what `--jdk-only` already reports. Mapping both onto
    /// `ClassNotFound` would put ambiguity back on top of absence one layer
    /// further out, which is the collapse this module exists to undo.
    fn from(err: ClassIdentityError) -> Self {
        let message = err.to_string();
        match err {
            ClassIdentityError::AmbiguousName { .. } => {
                VmError::Linkage(LinkageError::IncompatibleClassChangeError { message })
            }
            ClassIdentityError::Refused { name, .. } => {
                VmError::ClassFile(ClassFileError::ClassNotFound { class_name: name })
            }
        }
    }
}

impl From<ClassIdentityError> for MethodCallFailed {
    /// Lets a migrated native write `ctx.try_ensure_synthetic_class(n, f)?`
    /// in any function returning `MethodCallResult`, with no intermediate
    /// mapping — which is what makes the ~40 call-site migration mechanical.
    ///
    /// **This produces the UNCATCHABLE form.** `MethodCallFailed::InternalError`
    /// is documented as "not catchable by Java code — aborts execution
    /// entirely", which is the right shape for a VM invariant and the wrong one
    /// for a policy refusal: contract §5 asks for "the specification-appropriate
    /// `ClassNotFoundException` / `NoClassDefFoundError`", and an abort is
    /// neither. A native that can reach a live heap should call
    /// [`refusal_to_java_failure`] instead; this impl remains for the `?`
    /// ergonomics and as the fallback when the throwable itself cannot be built.
    fn from(err: ClassIdentityError) -> Self {
        MethodCallFailed::InternalError(err.into())
    }
}

/// Turn a [`ClassIdentityError`] into a **catchable Java** failure.
///
/// The `From` impl above yields `MethodCallFailed::InternalError`, which the
/// exception model defines as uncatchable and fatal. That is wrong for both of
/// these refusals: each is a linkage-level answer the program is entitled to
/// see and handle, and contract §5 names the exceptions by type.
///
/// * [`ClassIdentityError::Refused`] → `NoClassDefFoundError`, message = the
///   requested internal (slash-form) name. Same shape and same message form as
///   the VM-side `runtime::exceptions::raise_no_class_def_found`, so a refusal
///   reaching Java from a native and one reaching it from constant-pool
///   resolution are indistinguishable to a `catch` block — which they should
///   be, since they mean the same thing.
/// * [`ClassIdentityError::AmbiguousName`] → `IncompatibleClassChangeError`,
///   message = the full explanation. This is the family HotSpot uses for
///   loader-constraint violations, which is the same disease.
///
/// If the throwable cannot be constructed — no real bytes for it, or the heap
/// is exhausted — this falls back to the uncatchable form rather than
/// pretending the fabrication succeeded. The class name survives either way,
/// which is the part an operator needs.
pub fn refusal_to_java_failure(
    ctx: &mut dyn crate::registry::NativeContext,
    err: ClassIdentityError,
) -> MethodCallFailed {
    let (exception, message) = match &err {
        ClassIdentityError::AmbiguousName { .. } => {
            ("java/lang/IncompatibleClassChangeError", err.to_string())
        }
        ClassIdentityError::Refused { name, .. } => {
            ("java/lang/NoClassDefFoundError", name.clone())
        }
    };
    let message = ctx.create_string(&message);
    match ctx.new_object_initialized(
        exception,
        "(Ljava/lang/String;)V",
        &[cratonvm_types::Value::Object(Some(message))],
    ) {
        Ok(Some(cratonvm_types::Value::Object(Some(throwable)))) => {
            MethodCallFailed::ExceptionThrown(throwable)
        }
        _ => MethodCallFailed::from(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ambiguous_is_neither_unique_nor_absent() {
        let ambiguous = NameLookup::Ambiguous { definitions: 2 };
        assert!(ambiguous.is_ambiguous());
        assert_eq!(ambiguous.unique(), None);

        let absent = NameLookup::Absent;
        assert!(!absent.is_ambiguous());
        assert_eq!(absent.unique(), None);
        assert_ne!(
            absent, ambiguous,
            "the two must not compare equal — collapsing them is the bug",
        );

        let unique = NameLookup::Unique(ClassId::new(7));
        assert!(!unique.is_ambiguous());
        assert_eq!(unique.unique(), Some(ClassId::new(7)));
    }

    #[test]
    fn the_message_names_the_class_and_the_count() {
        let err = ClassIdentityError::AmbiguousName {
            name: "p/X".to_string(),
            definitions: 3,
        };
        let rendered = err.to_string();
        assert!(rendered.contains("p/X"), "{rendered}");
        assert!(rendered.contains('3'), "{rendered}");
        assert_eq!(err.name(), "p/X");
        assert!(err.is_ambiguous());
    }

    /// The two refusals must stay in different `VmError` families: a caller
    /// that unwraps the conversion has to be able to tell "there is no such
    /// class" from "there are two of them".
    #[test]
    fn the_two_refusals_convert_to_different_vm_errors() {
        let ambiguous: VmError = ClassIdentityError::AmbiguousName {
            name: "p/X".to_string(),
            definitions: 2,
        }
        .into();
        assert!(
            matches!(
                ambiguous,
                VmError::Linkage(LinkageError::IncompatibleClassChangeError { .. })
            ),
            "{ambiguous:?}",
        );
        assert!(ambiguous.to_string().contains("p/X"));

        let refused: VmError = ClassIdentityError::Refused {
            name: "p/X".to_string(),
            reason: "--jdk-only".to_string(),
        }
        .into();
        assert!(
            matches!(
                refused,
                VmError::ClassFile(ClassFileError::ClassNotFound { .. })
            ),
            "{refused:?}",
        );
    }

    /// The trait methods must be invisible to a context that does not override
    /// them: the fabrication default keeps answering `ClassId::new(0)`, the
    /// VM-internal door answers the same, and the classifier reports what
    /// `class_id_by_name` reports.
    ///
    /// The infallible `ensure_synthetic_class` this also covered was deleted by
    /// JDK-only wave 2 step 3 (2026-08-10); the remaining two are what a mock
    /// or non-VM context now sees.
    #[test]
    fn the_trait_defaults_preserve_the_previous_answers() {
        use crate::registry::{NativeClassAccess, NativeSystemAccess};
        use crate::test_mock::MockNativeContext;

        let mut ctx = MockNativeContext::new();

        assert_eq!(
            ctx.try_ensure_synthetic_class("p/Whatever", 4),
            Ok(ClassId::new(0)),
        );
        assert_eq!(
            ctx.ensure_vm_internal_class("p/Whatever", 4),
            ClassId::new(0),
        );

        // The mock has no name index, so every name reads absent — the same
        // answer its `class_id_by_name` gives. A context that cannot detect
        // ambiguity must not claim to.
        assert_eq!(ctx.class_id_by_name("p/Whatever"), None);
        assert_eq!(ctx.classify_class_name("p/Whatever"), NameLookup::Absent);
    }

    #[test]
    fn the_error_travels_through_a_native_return_type() {
        // The shape every migrated call site takes: the VM answers with a
        // `ClassIdentityError`, the native propagates it with `?`.
        fn vm_answer() -> Result<ClassId, ClassIdentityError> {
            Err(ClassIdentityError::AmbiguousName {
                name: "p/X".to_string(),
                definitions: 2,
            })
        }
        fn migrated_native() -> Result<ClassId, MethodCallFailed> {
            Ok(vm_answer()?)
        }
        let failed = migrated_native().expect_err("the refusal propagates");
        assert!(matches!(failed, MethodCallFailed::InternalError(_)));
        assert!(failed.to_string().contains("p/X"));
    }
}
