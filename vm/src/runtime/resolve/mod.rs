// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The single method- and field-resolution API.
//!
//! C2 review P0 — *"Centralize method and field resolution"*. The exit
//! criterion is:
//!
//! > No direct metadata-table bypass remains; a repository check rejects new
//! > bypasses.
//!
//! The repository check is [`guard`]. This module is the thing it points every
//! bypass at.
//!
//! # Why a façade and not "just call `find_method_recursive`"
//!
//! Before this module there were **six** independent answers to "what member
//! does this symbolic reference name", each with its own cache, its own
//! failure encoding, and its own (usually absent) access check:
//!
//! | consumer | entry point | cache | access check |
//! |---|---|---|---|
//! | interpreter, constant-pool method ref | `interpreter::invoke::resolve_method_metadata` | `ResolutionCache` (per-VM) | JPMS module only |
//! | interpreter, constant-pool field ref | `interpreter::field_access::resolve_field_ref` | `ResolutionCache` (per-VM) | JPMS module only |
//! | interpreter, inline-cache miss | `JvmThread::invoke_cache` → `SharedResolutionState::promoted_invokes` | per-VM | none (inherits the CP entry's) |
//! | JIT, call-site resolution | `vm/src/jit/helpers.rs` `find_method_recursive` | none | `check_class_access` for `new` only |
//! | JNI `Get{Method,Field}ID` | `vm/src/native/jni.rs` | `LinkResolver` (per-VM) | none (JNI does not mandate it) |
//! | reflection / method handles | `native-builtins/src/lang_class.rs` | `LinkResolver` via `NativeContext` | a **second**, reflection-only implementation |
//!
//! Two of those rows are defects rather than design:
//!
//! * **Two access-control implementations.** `classloading::access_control`
//!   implements JVMS §5.4.4 in full and — as its own module docs state — has
//!   **zero production callers** for [`check_field_access`] and
//!   [`check_method_access`]. The only live member check is
//!   `native-builtins/src/lang_class.rs::check_field_access`, a separate
//!   reflection-only implementation. So bytecode member access is not enforced
//!   and reflection member access is enforced by different code.
//! * **`None` means two things.** `NativeContextImpl::link_resolver_get_method`
//!   (`vm/src/vm/vm_exec.rs:6776`) returns `None` for *both* "cache cold" and
//!   "cache hit: this member does not exist", and its own comment admits the
//!   caller cannot tell them apart. That is the ambiguous-`None` hazard this
//!   codebase has been bitten by before.
//!
//! # What this pass changes and what it deliberately does not
//!
//! **Behaviour is unchanged.** Nothing here starts enforcing an access check
//! that was not being enforced, and nothing changes which member a given
//! symbolic reference resolves to. [`AccessPolicy::ModuleOnly`] exists
//! precisely so the bytecode path can keep doing what it does today while the
//! call being made is *named* rather than implied. Wiring
//! [`AccessPolicy::Full`] into the interpreter is a separate, behaviour-changing
//! change; the trap that makes it non-trivial (the cross-package protected
//! receiver clause is satisfied vacuously by `receiver: None`) is recorded in
//! `classloading::access_control`'s module docs and repeated on
//! [`MemberResolver::check_member_access`].
//!
//! # The three properties this API is built to hold
//!
//! 1. **A cross-VM answer is unrepresentable.** `ClassId`s are allocated
//!    per VM (`docs/architecture/per-vm-state.md`, Fact 1), so `ClassId(7)`
//!    names a different class in every VM and any bare-`ClassId` key aliases
//!    across VMs. Every input to and output from this module is a
//!    [`VmScoped<T>`], and the payload cannot be read out without naming a
//!    [`VmId`] that matches. See [`VmScoped`] for the exact strength of that
//!    claim — it is a real check, not a type-level proof, and the difference
//!    is stated there rather than glossed.
//! 2. **Failure is structured.** [`ResolveError`] distinguishes
//!    `NoSuchMethod` / `NoSuchField` / `IllegalAccess` / `NoClassDefFound`
//!    from each other, and [`CacheProbe`] is a *separate* type so
//!    "not cached" can never be spelled the same way as "does not exist".
//! 3. **Access control is applied exactly once, by one implementation.**
//!    [`MemberResolver::check_member_access`] is the only place in
//!    `vm/src/runtime/` that calls `classloading::access_control`, it bumps
//!    [`access_checks_run`], and it hands back a `#[must_use]`
//!    [`AccessGrant`] so a caller that resolves without checking is visible at
//!    the call site.
//!
//! # Migration status
//!
//! `docs/architecture/member-resolution.md` carries the full site inventory
//! and the ordered migration list. [`guard::ALLOWED`] is the machine-readable
//! half of it: every not-yet-migrated bypass has a row with the reason and the
//! migration step number, and a bypass that is *not* on that list fails the
//! build.

#[cfg(test)]
pub mod guard;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};

pub use cratonvm_native_api::VmId;

use crate::classloading::resolution::{ResolvedField, ResolvedMember, ResolvedMethod};
use crate::classloading::{find_field_recursive, find_method_recursive, ClassId, ClassManager};
use crate::error::{LinkageError, MethodCallFailed, VmError};
use crate::types::{ObjectRef, Value};
use crate::vm::SharedVm;

// ===========================================================================
// VM scoping
// ===========================================================================

/// A value that is only meaningful inside one VM.
///
/// # What this actually guarantees
///
/// Stated plainly, because the useful version of this claim is the narrow one:
/// [`VmId`] is constructible from a raw `usize` ([`VmId::from_raw`], which
/// `native-api` keeps public for the boot path), so this is **not** a
/// type-level proof that no code can ever fabricate a matching tag. What it is:
///
/// * There is no way to obtain the `T` out of a `VmScoped<T>` without naming a
///   `VmId`, and no accessor that takes "whatever VM happens to be current".
///   A cross-VM read is therefore never *accidental* — it requires writing down
///   a VM identity that is not the one you hold.
/// * Every entry point on [`MemberResolver`] takes and returns `VmScoped`
///   values tagged with the resolver's own VM, so a `ClassId` resolved in VM A
///   cannot be fed to VM B's resolver without a [`ResolveError::ForeignVm`].
/// * `grep VmScoped` enumerates every value in this subsystem whose meaning
///   depends on which VM produced it — the same reason `native-api` made
///   [`VmId`] a newtype rather than a `usize`.
///
/// That is the property the per-VM audit actually needs: it found four caches
/// keyed on a bare `ClassId` that could serve one VM's answer to another, and
/// all four were reachable *by accident* — a plain map probe with a plain key.
/// None of them would have compiled against this type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct VmScoped<T> {
    vm: VmId,
    value: T,
}

impl<T> VmScoped<T> {
    /// Tag `value` as belonging to `vm`.
    ///
    /// Prefer [`MemberResolver::scope`], which cannot name the wrong VM
    /// because it takes the identity from the resolver it is called on.
    pub const fn new(vm: VmId, value: T) -> Self {
        VmScoped { vm, value }
    }

    /// Which VM this value belongs to.
    pub const fn vm(&self) -> VmId {
        self.vm
    }

    /// Take the payload, proving it belongs to `vm`.
    pub fn get(self, vm: VmId) -> Result<T, ResolveError> {
        if self.vm == vm {
            Ok(self.value)
        } else {
            Err(ResolveError::ForeignVm {
                expected: vm,
                found: self.vm,
            })
        }
    }

    /// Borrow the payload, proving it belongs to `vm`.
    pub fn peek(&self, vm: VmId) -> Result<&T, ResolveError> {
        if self.vm == vm {
            Ok(&self.value)
        } else {
            Err(ResolveError::ForeignVm {
                expected: vm,
                found: self.vm,
            })
        }
    }

    /// Transform the payload, keeping the VM tag.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> VmScoped<U> {
        VmScoped {
            vm: self.vm,
            value: f(self.value),
        }
    }
}

// ===========================================================================
// Structured failure
// ===========================================================================

/// Why a resolution did not produce a member.
///
/// Every variant is a *different* answer to a caller. The reason this is an
/// enum and not `Option` is the failure mode this codebase has hit repeatedly:
/// a caller that receives `None` treats it as "absent" when the producer meant
/// "not cached", "you may not see it", or "the class is not there at all".
///
/// [`CacheProbe`] deliberately does **not** appear here. "Not cached" is not a
/// resolution failure and must not be spellable as one.
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// JVMS §5.4.3.3 — the hierarchy walk completed and no such method exists.
    NoSuchMethod {
        /// Class the search started from.
        class_name: String,
        /// Method name searched for.
        method_name: String,
        /// Method descriptor searched for.
        descriptor: String,
    },
    /// JVMS §5.4.3.2 — the hierarchy walk completed and no such field exists.
    NoSuchField {
        /// Class the search started from.
        class_name: String,
        /// Field name searched for.
        field_name: String,
    },
    /// JVMS §5.4.4 — the member exists but the accessor may not see it.
    IllegalAccess {
        /// Human-readable denial reason, as produced by
        /// `classloading::access_control`.
        message: String,
    },
    /// The owner class named by the symbolic reference is not loadable.
    NoClassDefFound {
        /// Internal name of the missing class.
        class_name: String,
    },
    /// The member exists but with an incompatible shape (static/instance
    /// mismatch, interface/class mismatch).
    IncompatibleClassChange {
        /// Human-readable reason.
        message: String,
    },
    /// A [`VmScoped`] value from one VM was presented to another.
    ///
    /// This is the failure the per-VM audit's four aliasing caches would have
    /// produced instead of a silent wrong answer.
    ForeignVm {
        /// The VM the reader holds.
        expected: VmId,
        /// The VM the value came from.
        found: VmId,
    },
    /// The process-global vtable manager belongs to a different VM.
    ///
    /// `docs/architecture/per-vm-state.md` item V1: `GLOBAL_VTABLE_MANAGER` is
    /// a `OnceLock` the first VM latches, and the classloading install hook is
    /// a captureless `fn`, so a second VM's vtables land in the first VM's
    /// index under colliding `ClassId`s. The fix needs a signature change in
    /// `cratonvm_classloading`; until then
    /// [`MemberResolver::vtable_manager`] reports the situation instead of
    /// dispatching through a foreign table.
    ForeignVtableManager {
        /// The VM that asked.
        vm: VmId,
    },
    /// A Java exception is already pending; the caller must propagate it.
    Pending(ObjectRef),
    /// A genuine VM-internal inconsistency (malformed constant pool, missing
    /// current class). Never a Java-visible condition.
    Internal {
        /// Diagnostic detail.
        message: String,
    },
}

/// The discriminant of a [`ResolveError`], for tests and telemetry that care
/// which *kind* of failure occurred but not about the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResolveErrorKind {
    /// [`ResolveError::NoSuchMethod`].
    NoSuchMethod,
    /// [`ResolveError::NoSuchField`].
    NoSuchField,
    /// [`ResolveError::IllegalAccess`].
    IllegalAccess,
    /// [`ResolveError::NoClassDefFound`].
    NoClassDefFound,
    /// [`ResolveError::IncompatibleClassChange`].
    IncompatibleClassChange,
    /// [`ResolveError::ForeignVm`].
    ForeignVm,
    /// [`ResolveError::ForeignVtableManager`].
    ForeignVtableManager,
    /// [`ResolveError::Pending`].
    Pending,
    /// [`ResolveError::Internal`].
    Internal,
}

impl ResolveError {
    /// Which kind of failure this is.
    pub fn kind(&self) -> ResolveErrorKind {
        match self {
            ResolveError::NoSuchMethod { .. } => ResolveErrorKind::NoSuchMethod,
            ResolveError::NoSuchField { .. } => ResolveErrorKind::NoSuchField,
            ResolveError::IllegalAccess { .. } => ResolveErrorKind::IllegalAccess,
            ResolveError::NoClassDefFound { .. } => ResolveErrorKind::NoClassDefFound,
            ResolveError::IncompatibleClassChange { .. } => {
                ResolveErrorKind::IncompatibleClassChange
            }
            ResolveError::ForeignVm { .. } => ResolveErrorKind::ForeignVm,
            ResolveError::ForeignVtableManager { .. } => ResolveErrorKind::ForeignVtableManager,
            ResolveError::Pending(_) => ResolveErrorKind::Pending,
            ResolveError::Internal { .. } => ResolveErrorKind::Internal,
        }
    }
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NoSuchMethod {
                class_name,
                method_name,
                descriptor,
            } => write!(f, "no such method: {class_name}.{method_name}{descriptor}"),
            ResolveError::NoSuchField {
                class_name,
                field_name,
            } => write!(f, "no such field: {class_name}.{field_name}"),
            ResolveError::IllegalAccess { message } => write!(f, "illegal access: {message}"),
            ResolveError::NoClassDefFound { class_name } => {
                write!(f, "no class def found: {class_name}")
            }
            ResolveError::IncompatibleClassChange { message } => {
                write!(f, "incompatible class change: {message}")
            }
            ResolveError::ForeignVm { expected, found } => write!(
                f,
                "resolution result from {found} presented to {expected}; \
                 ClassIds are allocated per VM and do not name the same class \
                 in both"
            ),
            ResolveError::ForeignVtableManager { vm } => write!(
                f,
                "the process-global vtable manager was installed by a \
                 different VM than {vm} (per-vm-state.md V1)"
            ),
            ResolveError::Pending(obj) => {
                write!(f, "exception already pending: ref({:p})", obj.as_ptr())
            }
            ResolveError::Internal { message } => write!(f, "internal error: {message}"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<LinkageError> for ResolveError {
    fn from(err: LinkageError) -> Self {
        match err {
            LinkageError::NoSuchMethodError {
                class_name,
                method_name,
                method_descriptor,
            } => ResolveError::NoSuchMethod {
                class_name,
                method_name,
                descriptor: method_descriptor,
            },
            LinkageError::NoSuchFieldError {
                class_name,
                field_name,
            } => ResolveError::NoSuchField {
                class_name,
                field_name,
            },
            LinkageError::IllegalAccessError { message } => ResolveError::IllegalAccess { message },
            LinkageError::NoClassDefFoundError { class_name } => {
                ResolveError::NoClassDefFound { class_name }
            }
            LinkageError::IncompatibleClassChangeError { message } => {
                ResolveError::IncompatibleClassChange { message }
            }
            other => ResolveError::Internal {
                message: other.to_string(),
            },
        }
    }
}

impl From<ResolveError> for LinkageError {
    fn from(err: ResolveError) -> Self {
        match err {
            ResolveError::NoSuchMethod {
                class_name,
                method_name,
                descriptor,
            } => LinkageError::NoSuchMethodError {
                class_name,
                method_name,
                method_descriptor: descriptor,
            },
            ResolveError::NoSuchField {
                class_name,
                field_name,
            } => LinkageError::NoSuchFieldError {
                class_name,
                field_name,
            },
            ResolveError::IllegalAccess { message } => LinkageError::IllegalAccessError { message },
            ResolveError::NoClassDefFound { class_name } => {
                LinkageError::NoClassDefFoundError { class_name }
            }
            ResolveError::IncompatibleClassChange { message } => {
                LinkageError::IncompatibleClassChangeError { message }
            }
            // The four remaining variants — `ForeignVm`, `ForeignVtableManager`,
            // `Pending` and `Internal` — have no JVMS linkage analogue: they
            // are VM-internal conditions, not linkage errors. Rendering them
            // through `Display` keeps the detail rather than collapsing them
            // onto an unrelated Java exception class. Note this direction is
            // lossy for them; `From<ResolveError> for MethodCallFailed` is the
            // one to use when a Java-visible outcome is needed, because it
            // routes `Pending` back to the live throwable and `Internal` to
            // `VmError::Internal` instead of coming through here.
            other => LinkageError::IncompatibleClassChangeError {
                message: other.to_string(),
            },
        }
    }
}

impl From<ResolveError> for MethodCallFailed {
    fn from(err: ResolveError) -> Self {
        match err {
            // Already a live Java exception — hand it back untouched so the
            // interpreter's handler search sees the original throwable.
            ResolveError::Pending(obj) => MethodCallFailed::ExceptionThrown(obj),
            ResolveError::Internal { message } => {
                MethodCallFailed::InternalError(VmError::Internal { message })
            }
            other => MethodCallFailed::InternalError(VmError::Linkage(other.into())),
        }
    }
}

impl From<MethodCallFailed> for ResolveError {
    fn from(err: MethodCallFailed) -> Self {
        match err {
            MethodCallFailed::ExceptionThrown(obj) => ResolveError::Pending(obj),
            MethodCallFailed::InternalError(VmError::Linkage(link)) => link.into(),
            MethodCallFailed::InternalError(other) => ResolveError::Internal {
                message: other.to_string(),
            },
        }
    }
}

impl From<VmError> for ResolveError {
    fn from(err: VmError) -> Self {
        match err {
            VmError::Linkage(link) => link.into(),
            other => ResolveError::Internal {
                message: other.to_string(),
            },
        }
    }
}

/// The outcome of asking a cache — as distinct from asking the class store.
///
/// This exists so that a cache probe and a resolution failure cannot share a
/// spelling. `Option<T>` collapses them, and the collapse is how
/// `NativeContextImpl::link_resolver_get_method` ended up returning `None`
/// for both "nothing cached yet" and "cached: this member does not exist" —
/// its own comment (`vm/src/vm/vm_exec.rs:6791`) documents the caller having
/// to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheProbe<T> {
    /// The cache has an answer. The answer may itself be "no such member" —
    /// see [`ResolvedMember::NotFound`], which is a *hit*.
    Hit(T),
    /// Nothing is cached for this key. The caller must resolve.
    Miss,
}

impl<T> CacheProbe<T> {
    /// Whether the cache had an answer of any kind.
    pub const fn is_hit(&self) -> bool {
        matches!(self, CacheProbe::Hit(_))
    }

    /// Collapse to `Option`, discarding the hit/miss distinction.
    ///
    /// Named `into_hit` rather than `ok` or `into_option` so that discarding
    /// the distinction is a deliberate, greppable act at the call site.
    pub fn into_hit(self) -> Option<T> {
        match self {
            CacheProbe::Hit(value) => Some(value),
            CacheProbe::Miss => None,
        }
    }
}

// ===========================================================================
// Access control — the one implementation
// ===========================================================================

/// How much of JVMS §5.4.4 to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessPolicy {
    /// JPMS module readability only (`check_module_access_by_id`).
    ///
    /// This is what the bytecode resolution path does **today**, and naming it
    /// is the point: before this module the interpreter simply called
    /// `check_module_access_by_id` and the absence of the member check was
    /// invisible at the call site.
    ModuleOnly,
    /// JPMS module readability *plus* the full member check
    /// (`check_field_access` / `check_method_access`).
    ///
    /// Currently used by no production caller — see
    /// [`MemberResolver::check_member_access`] for why turning it on is a
    /// separate, behaviour-changing change.
    Full,
}

/// Which member is being checked, and with which access flags.
#[derive(Clone, Copy, Debug)]
pub enum MemberFlags {
    /// A field, with its `ClassFileField::access_flags`.
    Field(FieldAccessFlags),
    /// A method, with its `ClassFileMethod::access_flags`.
    Method(MethodAccessFlags),
    /// The member's access flags are not available at this call site — only
    /// the owner class named by the symbolic reference is known.
    ///
    /// This is the honest description of the constant-pool method path: it
    /// checks module readability against the *owner class* before it has
    /// walked the hierarchy to find the declaring method, so there are no
    /// method flags to check yet. Valid with [`AccessPolicy::ModuleOnly`]
    /// only; pairing it with [`AccessPolicy::Full`] is a programming error and
    /// is reported as [`ResolveError::Internal`] rather than silently
    /// degrading to a module-only check.
    OwnerOnly,
}

/// Proof that [`MemberResolver::check_member_access`] ran.
///
/// `#[must_use]`: a resolution path that produces a grant and drops it without
/// binding is a path that ran the check for nothing, which is exactly as wrong
/// as not running it — it means the author intended a check somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an AccessGrant is the evidence the check ran; bind it or explain \
              why the check was requested"]
pub struct AccessGrant {
    policy: AccessPolicy,
    accessor: ClassId,
    declaring: ClassId,
}

impl AccessGrant {
    /// Which policy produced this grant.
    pub const fn policy(&self) -> AccessPolicy {
        self.policy
    }

    /// The class the check was performed *for*.
    ///
    /// JVMS §5.4.4 resolution is defined per (referencing class, symbolic
    /// reference), so a cached resolution is only reusable by this accessor.
    pub const fn accessor(&self) -> ClassId {
        self.accessor
    }

    /// The class that declares the member.
    pub const fn declaring(&self) -> ClassId {
        self.declaring
    }
}

/// Number of member access checks performed since process start.
///
/// The "applied exactly once" half of the P0 criterion is a counting property,
/// so it needs a counter. Tests read this around a resolution and assert the
/// delta.
static ACCESS_CHECKS: AtomicU64 = AtomicU64::new(0);

/// How many times [`MemberResolver::check_member_access`] has run.
pub fn access_checks_run() -> u64 {
    ACCESS_CHECKS.load(Ordering::Relaxed)
}

// ===========================================================================
// MemberResolver
// ===========================================================================

/// The single entry point for method and field resolution.
///
/// Cheap to construct (two words); construct one per resolution rather than
/// storing it, so the borrow of `SharedVm` stays short.
pub struct MemberResolver<'a> {
    shared: &'a SharedVm,
    vm: VmId,
}

/// How many field resolutions asked for a (name, descriptor) pair the hierarchy
/// does not contain and fell back to the name-only match.
///
/// This is the ENGAGEMENT counter for tightening `locate_field` to the strict
/// JVMS §5.4.3.2 answer (`NoSuchFieldError`). A run that reports `0` is a run in
/// which the fallback could be deleted with no behaviour change at all; a
/// non-zero count names the number of sites that would start throwing, and
/// `CRATONVM_DBG_FIELD_DESCRIPTOR=1` names them.
pub static FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Snapshot of [`FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS`].
pub fn field_resolution_descriptor_fallbacks() -> u64 {
    FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS.load(std::sync::atomic::Ordering::Relaxed)
}

impl<'a> MemberResolver<'a> {
    /// Bind a resolver to `shared`.
    ///
    /// There is deliberately no constructor that does not take a VM: a
    /// resolution "for no VM in particular" is the bug class this whole module
    /// exists to make unspellable.
    pub fn new(shared: &'a SharedVm) -> Self {
        MemberResolver {
            vm: VmId::from_raw(shared.vm_identity),
            shared,
        }
    }

    /// The VM this resolver answers for.
    pub const fn vm(&self) -> VmId {
        self.vm
    }

    /// The `SharedVm` this resolver is bound to.
    pub fn shared(&self) -> &'a SharedVm {
        self.shared
    }

    /// Tag a value as belonging to this resolver's VM.
    pub fn scope<T>(&self, value: T) -> VmScoped<T> {
        VmScoped::new(self.vm, value)
    }

    /// Read a [`VmScoped`] value, proving it belongs to this resolver's VM.
    pub fn adopt<T>(&self, scoped: VmScoped<T>) -> Result<T, ResolveError> {
        scoped.get(self.vm)
    }

    // -----------------------------------------------------------------
    // Access control
    // -----------------------------------------------------------------

    /// Apply access control to one member reference. **The only** call into
    /// `classloading::access_control` from `vm/src/runtime/`.
    ///
    /// `receiver` is the *static type* of the expression the member is reached
    /// through, `None` for a static member or a caller that does not track it.
    ///
    /// # Trap, if you are the one wiring [`AccessPolicy::Full`] in
    ///
    /// The cross-package protected clause of JVMS §5.4.4 is satisfied
    /// **vacuously** by `receiver: None` — `classloading::access_control` pins
    /// that with `protected_receiver_none_is_vacuous_not_a_check`. Passing
    /// `None` from `getfield` / `invokevirtual` does not "skip a refinement",
    /// it turns the clause off. Plumbing the static receiver type through is
    /// part of that job.
    pub fn check_member_access(
        &self,
        cm: &ClassManager,
        accessor: VmScoped<ClassId>,
        declaring: VmScoped<ClassId>,
        member: MemberFlags,
        receiver: Option<VmScoped<ClassId>>,
        policy: AccessPolicy,
    ) -> Result<AccessGrant, ResolveError> {
        let accessor_id = self.adopt(accessor)?;
        let declaring_id = self.adopt(declaring)?;
        let receiver_id = match receiver {
            Some(r) => Some(self.adopt(r)?),
            None => None,
        };

        ACCESS_CHECKS.fetch_add(1, Ordering::Relaxed);

        // JPMS readability (JVMS §5.4.4 module clause). Fail-open when either
        // class is absent from the store — that is `check_module_access_by_id`'s
        // documented behaviour, not something introduced here.
        crate::classloading::access_control::check_module_access_by_id(
            accessor_id,
            declaring_id,
            cm,
        )?;

        if policy == AccessPolicy::Full {
            let store = cm.class_store();
            let (Some(accessor_class), Some(declaring_class)) =
                (cm.get_class(accessor_id), cm.get_class(declaring_id))
            else {
                // Same fail-open rule as the module clause above: an absent
                // class is not a denial. Keeping the two clauses consistent
                // matters more than tightening one of them here.
                return Ok(AccessGrant {
                    policy,
                    accessor: accessor_id,
                    declaring: declaring_id,
                });
            };
            let receiver_class = receiver_id.and_then(|id| cm.get_class(id));
            match member {
                MemberFlags::Field(flags) => {
                    crate::classloading::access_control::check_field_access(
                        accessor_class,
                        declaring_class,
                        flags,
                        store,
                        receiver_class,
                    )?;
                }
                MemberFlags::Method(flags) => {
                    crate::classloading::access_control::check_method_access(
                        accessor_class,
                        declaring_class,
                        flags,
                        store,
                        receiver_class,
                    )?;
                }
                MemberFlags::OwnerOnly => {
                    return Err(ResolveError::Internal {
                        message: format!(
                            "AccessPolicy::Full requested for {accessor_id:?} -> \
                             {declaring_id:?} without member flags; \
                             MemberFlags::OwnerOnly can only satisfy \
                             AccessPolicy::ModuleOnly"
                        ),
                    });
                }
            }
        }

        Ok(AccessGrant {
            policy,
            accessor: accessor_id,
            declaring: declaring_id,
        })
    }

    // -----------------------------------------------------------------
    // Constant-pool references (the interpreter and both JIT paths)
    // -----------------------------------------------------------------

    /// Resolve a `CONSTANT_Methodref` / `CONSTANT_InterfaceMethodref`.
    ///
    /// Delegates to the interpreter's resolution core, which owns the
    /// `ResolutionCache` write and the loader-aware fast paths. That core is
    /// `pub(crate)` and marked "call through `runtime::resolve`"; [`guard`]
    /// enforces it.
    pub fn method_ref(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
    ) -> Result<VmScoped<ResolvedMethod>, ResolveError> {
        let caller_id = self.adopt(caller)?;
        let resolved = crate::runtime::interpreter::invoke::resolve_method_metadata(
            self.shared,
            caller_id,
            cp_index,
        )?;
        Ok(self.scope(resolved))
    }

    /// Resolve a `CONSTANT_Fieldref`.
    pub fn field_ref(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
    ) -> Result<VmScoped<ResolvedField>, ResolveError> {
        let caller_id = self.adopt(caller)?;
        let resolved = crate::runtime::interpreter::field_access::resolve_field_ref(
            self.shared,
            caller_id,
            cp_index,
        )?;
        Ok(self.scope(resolved))
    }

    /// Ask the per-`(class, cp-index)` resolution cache without resolving.
    ///
    /// Returns [`CacheProbe::Miss`] when nothing is cached. There is no
    /// encoding of "cached: absent" for constant-pool entries — the cache only
    /// ever holds successful resolutions — but the probe still returns a
    /// `CacheProbe` so that callers of *all* the probes in this module read the
    /// same way.
    pub fn probe_method_ref(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
    ) -> CacheProbe<VmScoped<ResolvedMethod>> {
        let Ok(caller_id) = self.adopt(caller) else {
            return CacheProbe::Miss;
        };
        match self
            .shared
            .classes
            .resolution_cache
            .read()
            .get_method(caller_id, cp_index)
        {
            Some(hit) => CacheProbe::Hit(self.scope(hit.clone())),
            None => CacheProbe::Miss,
        }
    }

    /// Field sibling of [`Self::probe_method_ref`].
    pub fn probe_field_ref(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
    ) -> CacheProbe<VmScoped<ResolvedField>> {
        let Ok(caller_id) = self.adopt(caller) else {
            return CacheProbe::Miss;
        };
        match self
            .shared
            .classes
            .resolution_cache
            .read()
            .get_field(caller_id, cp_index)
        {
            Some(hit) => CacheProbe::Hit(self.scope(hit.clone())),
            None => CacheProbe::Miss,
        }
    }

    // -----------------------------------------------------------------
    // Constant-pool constants (`CONSTANT_Dynamic` and friends)
    // -----------------------------------------------------------------

    /// Ask the per-`(class, cp-index)` record for an already-resolved
    /// `CONSTANT_Dynamic`, `CONSTANT_MethodType` or `CONSTANT_MethodHandle`.
    ///
    /// JVMS §5.4.3 resolves a symbolic reference **once** per constant-pool
    /// entry and records the result; these three tags share one store because a
    /// CP index has exactly one tag, so their keys cannot collide. This pair is
    /// the entry point the bypass allowlist's `constants.rs` row named as
    /// missing — before it, `ldc` reached `shared.classes.resolution_cache`
    /// directly, and each new constant tag that learned to cache added another
    /// raw reach (`CONSTANT_MethodType` and `CONSTANT_MethodHandle` took the
    /// row from 2 sites to 5 without anyone deciding to).
    ///
    /// Returns [`CacheProbe::Miss`] when nothing is recorded. As with
    /// [`Self::probe_method_ref`], there is no encoding of "recorded: absent" —
    /// only successful resolutions are stored.
    pub fn probe_constant(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
    ) -> CacheProbe<VmScoped<Value>> {
        let Ok(caller_id) = self.adopt(caller) else {
            return CacheProbe::Miss;
        };
        match self
            .shared
            .classes
            .resolution_cache
            .read()
            .get_condy(caller_id, cp_index)
        {
            Some(hit) => CacheProbe::Hit(self.scope(*hit)),
            None => CacheProbe::Miss,
        }
    }

    /// Record the result of resolving one constant-pool constant.
    ///
    /// The write half of [`Self::probe_constant`]. A `Value` here is a live
    /// heap reference for the object tags, which is why it is `VmScoped`: the
    /// collector scans and remaps this store (`for_each_condy_root` /
    /// `update_condy_refs`) as roots of **this** VM, so a value from another
    /// VM's heap recorded here would be remapped against the wrong heap.
    ///
    /// A caller that cannot prove the VM silently records nothing rather than
    /// recording it against the wrong one — the same fail-closed direction the
    /// probes take.
    pub fn record_constant(
        &self,
        caller: VmScoped<ClassId>,
        cp_index: u16,
        value: VmScoped<Value>,
    ) {
        let (Ok(caller_id), Ok(value)) = (self.adopt(caller), self.adopt(value)) else {
            return;
        };
        self.shared
            .classes
            .resolution_cache
            .write()
            .put_condy(caller_id, cp_index, value);
    }

    // -----------------------------------------------------------------
    // Reflective (class, name, descriptor) lookups
    // -----------------------------------------------------------------

    /// Resolve `(owner, name, descriptor)` to a declaring class and method
    /// index, populating the per-VM `LinkResolver`.
    ///
    /// This is the implementation JNI `GetMethodID`, `Class.getDeclaredMethod`
    /// and the `MethodHandles.Lookup` natives all reach for. It is written out
    /// here rather than delegated, because before this module each of those
    /// three call sites had its own copy of the walk-then-index dance and they
    /// did not agree on what a miss meant.
    ///
    /// `cm` is passed in rather than acquired here so the caller keeps
    /// ownership of the L10 guard and the lock order is whatever the call site
    /// already established.
    pub fn declared_method(
        &self,
        cm: &ClassManager,
        owner: VmScoped<ClassId>,
        name: &str,
        descriptor: &str,
    ) -> Result<VmScoped<(ClassId, u32)>, ResolveError> {
        let owner_id = self.adopt(owner)?;
        let resolved = self.shared.classes.link_resolver.resolve_or_compute(
            owner_id,
            name,
            descriptor,
            || {
                let found = find_method_recursive(owner_id, name, descriptor, cm.class_store())
                    .and_then(|(_, declaring)| {
                        let decl = cm.class_store().get(declaring)?;
                        let index = decl
                            .methods
                            .iter()
                            .position(|m| &*m.name == name && &*m.descriptor == descriptor)?;
                        Some((declaring, index as u32))
                    });
                let member = match found {
                    Some((declaring_class_id, index)) => ResolvedMember::Method {
                        declaring_class_id,
                        index,
                    },
                    None => ResolvedMember::NotFound,
                };
                (
                    cratonvm_types::intern_arc(name),
                    cratonvm_types::intern_arc(descriptor),
                    member,
                )
            },
        );
        match resolved {
            ResolvedMember::Method {
                declaring_class_id,
                index,
            } => Ok(self.scope((declaring_class_id, index))),
            // A `Field` answer under a method key means the key was reused
            // across member kinds. Report it as absent rather than as a field:
            // the caller asked for a method.
            ResolvedMember::NotFound | ResolvedMember::Field { .. } => {
                Err(ResolveError::NoSuchMethod {
                    class_name: self.class_name(cm, owner_id),
                    method_name: name.to_string(),
                    descriptor: descriptor.to_string(),
                })
            }
        }
    }

    /// Resolve `(owner, name)` to a declaring class, absolute field index and
    /// staticness, populating the per-VM `LinkResolver`.
    ///
    /// `descriptor` participates in the cache key only — `find_field_recursive`
    /// matches on name alone (JVMS §5.4.3.2 resolves fields by name and type,
    /// but this VM's store indexes by name). Pass the reference's descriptor so
    /// two same-named fields of different types do not share a cache entry.
    pub fn declared_field(
        &self,
        cm: &ClassManager,
        owner: VmScoped<ClassId>,
        name: &str,
        descriptor: &str,
    ) -> Result<VmScoped<(ClassId, u32, bool)>, ResolveError> {
        let owner_id = self.adopt(owner)?;
        let resolved = self.shared.classes.link_resolver.resolve_or_compute(
            owner_id,
            name,
            descriptor,
            || {
                let member = match find_field_recursive(owner_id, name, cm.class_store()) {
                    Some((absolute_index, field, declaring_class_id)) => ResolvedMember::Field {
                        declaring_class_id,
                        absolute_index: absolute_index as u32,
                        is_static: field.is_static(),
                    },
                    None => ResolvedMember::NotFound,
                };
                (
                    cratonvm_types::intern_arc(name),
                    cratonvm_types::intern_arc(descriptor),
                    member,
                )
            },
        );
        match resolved {
            ResolvedMember::Field {
                declaring_class_id,
                absolute_index,
                is_static,
            } => Ok(self.scope((declaring_class_id, absolute_index, is_static))),
            ResolvedMember::NotFound | ResolvedMember::Method { .. } => {
                Err(ResolveError::NoSuchField {
                    class_name: self.class_name(cm, owner_id),
                    field_name: name.to_string(),
                })
            }
        }
    }

    /// Ask the `LinkResolver` without resolving.
    ///
    /// A cached [`ResolvedMember::NotFound`] is a [`CacheProbe::Hit`] — that is
    /// the whole reason this returns `CacheProbe` and not `Option`. The
    /// existing `NativeContextImpl::link_resolver_get_method` bridge collapses
    /// exactly this distinction and says so in its own comment.
    pub fn probe_declared(
        &self,
        owner: VmScoped<ClassId>,
        name: &str,
        descriptor: &str,
    ) -> CacheProbe<VmScoped<ResolvedMember>> {
        let Ok(owner_id) = self.adopt(owner) else {
            return CacheProbe::Miss;
        };
        match self
            .shared
            .classes
            .link_resolver
            .get(owner_id, name, descriptor)
        {
            Some(member) => CacheProbe::Hit(self.scope(member)),
            None => CacheProbe::Miss,
        }
    }

    // -----------------------------------------------------------------
    // Member lookup against an already-known owner class
    // -----------------------------------------------------------------

    /// Locate `field_name` on `owner`, applying `policy` exactly once.
    ///
    /// This is the core of the interpreter's `getfield`/`putfield`/
    /// `getstatic`/`putstatic` resolution, lifted here so the interpreter,
    /// the JIT field-access fast path, and `Unsafe.objectFieldOffset` cannot
    /// drift apart on which slot a name maps to.
    ///
    /// Search order is own-fields-first then the superclass/superinterface
    /// walk, matching `find_field_recursive` and JVMS §5.4.3.2. The
    /// own-fields pass is kept separate because it is the one that computes the
    /// *static* slot index — `find_field_recursive` returns the absolute
    /// instance index and the static index is only derivable while walking the
    /// declaring class's own field list in order.
    ///
    /// # `descriptor` — the other half of the JVMS key
    ///
    /// JVMS §5.4.3.2 resolves a `CONSTANT_Fieldref` by name **and** descriptor,
    /// and JVMS §4.5 forbids only the pair from repeating: one class may
    /// legally declare several fields sharing a NAME. This function used to
    /// match on the name alone and return the first hit, so on such a class it
    /// returned a field of the wrong type — and handed the caller its slot
    /// index and its static/instance flag.
    ///
    /// For the interpreter that mostly degrades to a wrong value, because it
    /// reads and writes a dynamically tagged cell. For the JIT it does not: the
    /// slot index comes from this search and the type tag comes from the
    /// constant-pool descriptor, so the two halves of a compiled field access
    /// describe different fields, and a `putfield` writes one field's tag at
    /// another field's slot.
    ///
    /// `Some(descriptor)` applies the full key. `None` keeps the historical
    /// name-only behaviour for the callers that pass a name they own and know
    /// to be unique.
    ///
    /// **Fallback, and why there is one.** When the full key matches nothing,
    /// this does not fail — it retries name-only, counts the retry in
    /// [`FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS`] and returns that answer, which
    /// is exactly what it would have returned before. JVMS says a missing
    /// (name, descriptor) pair is a `NoSuchFieldError`, but this VM substitutes
    /// and synthesises JDK classes whose field descriptors need not match the
    /// classfile a caller was compiled against, and failing those closed would
    /// trade a rare wrong slot for a common hard error. The counter is what
    /// turns "we could tighten this" from a guess into a measurement.
    pub fn locate_field(
        &self,
        cm: &ClassManager,
        accessor: VmScoped<ClassId>,
        owner: VmScoped<ClassId>,
        owner_name: &str,
        field_name: &str,
        descriptor: Option<&str>,
        policy: AccessPolicy,
    ) -> Result<VmScoped<ResolvedField>, ResolveError> {
        let accessor_id = self.adopt(accessor)?;
        let owner_id = self.adopt(owner)?;

        if let Some(class) = cm.get_class(owner_id) {
            let mut static_idx = 0usize;
            let mut instance_idx = 0usize;
            for f in &class.fields {
                // JVMS §5.4.3.2 — name AND descriptor. `descriptor: None` is
                // the historical name-only key, kept for callers that own the
                // name; see this function's doc.
                if &*f.name == field_name
                    && descriptor.is_none_or(|d| &*f.descriptor == d)
                {
                    let (index, is_static) = if f.is_static() {
                        (static_idx, true)
                    } else {
                        (class.first_field_index + instance_idx, false)
                    };
                    let _grant = self.check_member_access(
                        cm,
                        self.scope(accessor_id),
                        self.scope(owner_id),
                        MemberFlags::Field(f.access_flags),
                        None,
                        policy,
                    )?;
                    if cratonvm_types::flags::runtime_var("CRATON_FIELD_TRACE").is_ok()
                        && !is_static
                        && index >= class.num_total_fields
                    {
                        eprintln!(
                            "[FIELD-TRACE] OOB resolve(own): decl={} field={} field_index={} \
                             num_total_fields={} first_field_index={}",
                            class.name,
                            field_name,
                            index,
                            class.num_total_fields,
                            class.first_field_index
                        );
                    }
                    return Ok(self.scope(ResolvedField {
                        declaring_class_id: owner_id,
                        field_index: index,
                        is_static,
                        is_volatile: f.is_volatile(),
                        is_reference: f.descriptor.starts_with('L')
                            || f.descriptor.starts_with('['),
                        desc_byte: f.descriptor.as_bytes().first().copied().unwrap_or(0),
                    }));
                }
                if f.is_static() {
                    static_idx += 1;
                } else {
                    instance_idx += 1;
                }
            }
        }

        // The full JVMS key first. Only when no field of this exact name AND
        // descriptor exists anywhere in the hierarchy does the name-only search
        // run, and that retry is counted — see this function's doc for why it
        // exists at all rather than raising `NoSuchFieldError`.
        let located = match descriptor {
            Some(d) => crate::classloading::find_field_recursive_by_descriptor(
                owner_id,
                field_name,
                d,
                cm.class_store(),
            )
            .or_else(|| {
                let name_only = find_field_recursive(owner_id, field_name, cm.class_store());
                if name_only.is_some() {
                    FIELD_RESOLUTION_DESCRIPTOR_FALLBACKS
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_FIELD_DESCRIPTOR")
                        .is_some()
                    {
                        tracing::warn!(
                            target: "cratonvm::resolve",
                            owner = owner_name,
                            field = field_name,
                            wanted_descriptor = d,
                            "no field of this name and descriptor exists in the hierarchy;                              falling back to the name-only match this VM used before                              2026-08-27",
                        );
                    }
                }
                name_only
            }),
            None => find_field_recursive(owner_id, field_name, cm.class_store()),
        };
        let Some((field_index, field, declaring_id)) = located else {
            return Err(ResolveError::NoSuchField {
                class_name: owner_name.to_string(),
                field_name: field_name.to_string(),
            });
        };

        let _grant = self.check_member_access(
            cm,
            self.scope(accessor_id),
            self.scope(declaring_id),
            MemberFlags::Field(field.access_flags),
            None,
            policy,
        )?;

        if cratonvm_types::flags::runtime_var("CRATON_FIELD_TRACE").is_ok() && !field.is_static() {
            let decl = cm.get_class(declaring_id);
            let ntf = decl.map(|c| c.num_total_fields).unwrap_or(0);
            let ffi = decl.map(|c| c.first_field_index).unwrap_or(0);
            let dname = decl.map(|c| c.name.to_string()).unwrap_or_default();
            if field_index >= ntf {
                eprintln!(
                    "[FIELD-TRACE] OOB resolve: decl={dname} field_index={field_index} \
                     num_total_fields={ntf} first_field_index={ffi}"
                );
            }
        }

        Ok(self.scope(ResolvedField {
            declaring_class_id: declaring_id,
            field_index,
            is_static: field.is_static(),
            is_volatile: field.is_volatile(),
            is_reference: field.descriptor.starts_with('L') || field.descriptor.starts_with('['),
            desc_byte: field.descriptor.as_bytes().first().copied().unwrap_or(0),
        }))
    }

    // -----------------------------------------------------------------
    // Virtual dispatch tables
    // -----------------------------------------------------------------

    /// This VM's vtable manager, refusing to hand back another VM's.
    ///
    /// `GLOBAL_VTABLE_MANAGER` (`runtime::vtable`) is a `OnceLock` the first VM
    /// latches, and the classloading install hook is a captureless `fn`, so in
    /// a two-VM process the second VM's vtables are written into the first
    /// VM's index under colliding `ClassId`s
    /// (`docs/architecture/per-vm-state.md` V1). The manager `Arc` this VM
    /// stored in `SharedVm::classes::vtable_manager` is the same allocation it
    /// handed to the installer, so `Arc::ptr_eq` decides ownership exactly and
    /// without needing the installer to record a `VmId`.
    ///
    /// Callers that get [`ResolveError::ForeignVtableManager`] must fall back
    /// to a class-store walk rather than dispatching through the foreign
    /// table. No caller does this yet — this pass adds the detector, not the
    /// fallback, because installing one would change dispatch behaviour.
    pub fn vtable_manager(
        &self,
    ) -> Result<Arc<parking_lot::RwLock<crate::runtime::vtable::VtableManager>>, ResolveError> {
        let mine = &self.shared.classes.vtable_manager;
        match crate::runtime::vtable::global_vtable_manager() {
            // No VM has installed one (isolated unit-test harness): this VM's
            // own manager is trivially the right answer.
            None => Ok(Arc::clone(mine)),
            Some(global) if Arc::ptr_eq(&global, mine) => Ok(Arc::clone(mine)),
            Some(_) => Err(ResolveError::ForeignVtableManager { vm: self.vm }),
        }
    }

    // -----------------------------------------------------------------

    fn class_name(&self, cm: &ClassManager, id: ClassId) -> String {
        cm.get_class(id)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| format!("<class id {}>", id.as_u32()))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_a() -> VmId {
        VmId::from_raw(0xA)
    }

    fn vm_b() -> VmId {
        VmId::from_raw(0xB)
    }

    /// The constant-pool record round-trips through the entry point that
    /// replaced `constants.rs`'s five direct reaches into
    /// `shared.classes.resolution_cache`.
    ///
    /// Asserted as a DIFFERENCE — miss before the write, hit with the exact
    /// value after — because a `probe_constant` that always answered `Miss`
    /// would leave `ldc` correct on every first execution and only wrong from
    /// the second, and a `record_constant` that silently dropped the write
    /// looks identical. Both are the failure modes the migration could
    /// introduce, and neither is visible from a single resolution.
    #[test]
    fn a_recorded_constant_reads_back_through_the_resolver() {
        let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let resolver = MemberResolver::new(&shared);
        let owner = resolver.scope(ClassId::new(41));
        const CP: u16 = 13;

        assert!(
            !resolver.probe_constant(owner, CP).is_hit(),
            "nothing has been recorded for this entry yet"
        );

        let owner = resolver.scope(ClassId::new(41));
        resolver.record_constant(owner, CP, resolver.scope(Value::Int(4242)));

        let owner = resolver.scope(ClassId::new(41));
        let hit = resolver
            .probe_constant(owner, CP)
            .into_hit()
            .expect("the value just recorded must read back");
        assert_eq!(resolver.adopt(hit).expect("same vm"), Value::Int(4242));

        // A different cp index in the same class is a different entry, not a
        // second name for this one.
        let owner = resolver.scope(ClassId::new(41));
        assert!(!resolver.probe_constant(owner, CP + 1).is_hit());
    }

    /// A key from another VM must not read this VM's record. `ClassId`s are
    /// allocated per VM, so `ClassId(41)` elsewhere is a different class, and
    /// the collector scans this store as roots of THIS heap.
    #[test]
    fn a_foreign_key_neither_reads_nor_writes_the_constant_record() {
        let shared = std::sync::Arc::new(crate::vm::SharedVm::new(crate::config::VmConfig::default()));
        let resolver = MemberResolver::new(&shared);
        const CP: u16 = 77;

        let foreign = VmScoped::new(VmId::from_raw(0xDEAD), ClassId::new(41));
        resolver.record_constant(foreign, CP, resolver.scope(Value::Int(1)));
        let owner = resolver.scope(ClassId::new(41));
        assert!(
            !resolver.probe_constant(owner, CP).is_hit(),
            "a write under a foreign key must not land in this VM's record"
        );

        resolver.record_constant(resolver.scope(ClassId::new(41)), CP, resolver.scope(Value::Int(2)));
        let foreign = VmScoped::new(VmId::from_raw(0xDEAD), ClassId::new(41));
        assert!(
            !resolver.probe_constant(foreign, CP).is_hit(),
            "a read under a foreign key must not see this VM's record"
        );
    }

    #[test]
    fn a_scoped_value_reads_back_only_in_its_own_vm() {
        let scoped = VmScoped::new(vm_a(), ClassId::new(7));
        assert_eq!(scoped.get(vm_a()).expect("same vm"), ClassId::new(7));
    }

    /// The property the per-VM audit needs: `ClassId(7)` from VM A must not be
    /// readable as `ClassId(7)` in VM B, because it is not the same class.
    #[test]
    fn a_scoped_value_is_not_readable_in_another_vm() {
        let scoped = VmScoped::new(vm_a(), ClassId::new(7));
        let err = scoped.get(vm_b()).expect_err("cross-VM read must fail");
        assert_eq!(err.kind(), ResolveErrorKind::ForeignVm);
        match err {
            ResolveError::ForeignVm { expected, found } => {
                assert_eq!(expected, vm_b());
                assert_eq!(found, vm_a());
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // The borrow form must agree with the owning form.
        let scoped = VmScoped::new(vm_a(), ClassId::new(7));
        assert!(scoped.peek(vm_b()).is_err());
        assert!(scoped.peek(vm_a()).is_ok());
    }

    #[test]
    fn mapping_a_scoped_value_keeps_the_vm_tag() {
        let scoped = VmScoped::new(vm_a(), 41u32).map(|n| n + 1);
        assert_eq!(scoped.vm(), vm_a());
        assert_eq!(scoped.get(vm_a()).expect("same vm"), 42);
    }

    /// Each failure kind is distinguishable — the whole point of not using
    /// `Option`.
    ///
    /// Eight of the nine kinds; `Pending` is excluded because constructing one
    /// needs a live heap `ObjectRef`, which a unit test here cannot mint. Its
    /// discriminant is covered by `ResolveError::kind`'s exhaustive match: the
    /// compiler fails that match if a variant is added without a kind.
    #[test]
    fn every_constructible_failure_kind_is_distinguishable() {
        let cases: Vec<(ResolveError, ResolveErrorKind)> = vec![
            (
                ResolveError::NoSuchMethod {
                    class_name: "C".into(),
                    method_name: "m".into(),
                    descriptor: "()V".into(),
                },
                ResolveErrorKind::NoSuchMethod,
            ),
            (
                ResolveError::NoSuchField {
                    class_name: "C".into(),
                    field_name: "f".into(),
                },
                ResolveErrorKind::NoSuchField,
            ),
            (
                ResolveError::IllegalAccess {
                    message: "no".into(),
                },
                ResolveErrorKind::IllegalAccess,
            ),
            (
                ResolveError::NoClassDefFound {
                    class_name: "C".into(),
                },
                ResolveErrorKind::NoClassDefFound,
            ),
            (
                ResolveError::IncompatibleClassChange {
                    message: "static/instance".into(),
                },
                ResolveErrorKind::IncompatibleClassChange,
            ),
            (
                ResolveError::ForeignVm {
                    expected: vm_a(),
                    found: vm_b(),
                },
                ResolveErrorKind::ForeignVm,
            ),
            (
                ResolveError::ForeignVtableManager { vm: vm_a() },
                ResolveErrorKind::ForeignVtableManager,
            ),
            (
                ResolveError::Internal {
                    message: "bad cp".into(),
                },
                ResolveErrorKind::Internal,
            ),
        ];
        let mut seen = std::collections::HashSet::new();
        for (err, kind) in cases {
            assert_eq!(err.kind(), kind, "kind mismatch for {err:?}");
            assert!(seen.insert(kind), "two cases claim kind {kind:?}");
            // Every kind must render something non-empty, so a log line can
            // never silently become "".
            assert!(!err.to_string().is_empty());
        }
        assert_eq!(seen.len(), 8);
    }

    /// `NoSuchMethod` and `NoSuchField` must survive the round trip through
    /// `LinkageError`, because that is how they reach a Java `catch`.
    #[test]
    fn linkage_round_trip_preserves_the_kind() {
        let err = ResolveError::NoSuchMethod {
            class_name: "java/lang/String".into(),
            method_name: "nope".into(),
            descriptor: "()V".into(),
        };
        let back: ResolveError = LinkageError::from(err).into();
        assert_eq!(back.kind(), ResolveErrorKind::NoSuchMethod);

        let err = ResolveError::NoSuchField {
            class_name: "java/lang/String".into(),
            field_name: "nope".into(),
        };
        let back: ResolveError = LinkageError::from(err).into();
        assert_eq!(back.kind(), ResolveErrorKind::NoSuchField);

        let err = ResolveError::IllegalAccess {
            message: "private".into(),
        };
        let back: ResolveError = LinkageError::from(err).into();
        assert_eq!(back.kind(), ResolveErrorKind::IllegalAccess);
    }

    /// The distinction `Option` cannot make.
    #[test]
    fn a_cached_absence_is_a_hit_not_a_miss() {
        let cached_absent: CacheProbe<ResolvedMember> = CacheProbe::Hit(ResolvedMember::NotFound);
        let nothing_cached: CacheProbe<ResolvedMember> = CacheProbe::Miss;

        assert!(cached_absent.is_hit());
        assert!(!nothing_cached.is_hit());

        // And the collapse to Option is exactly what loses it — which is why
        // `into_hit` is named for the thing being discarded.
        assert!(cached_absent.into_hit().is_some());
        assert!(nothing_cached.into_hit().is_none());
    }

    #[test]
    fn the_access_counter_moves_monotonically() {
        let before = access_checks_run();
        ACCESS_CHECKS.fetch_add(1, Ordering::Relaxed);
        assert_eq!(access_checks_run(), before + 1);
    }

    /// `AccessGrant` records *which* accessor the check was performed for.
    /// JVMS §5.4.4 resolution is per (referencing class, symbolic reference),
    /// so a grant is not transferable to a different accessor; keeping the
    /// accessor on the token is what makes that checkable.
    #[test]
    fn an_access_grant_names_its_accessor_and_policy() {
        let grant = AccessGrant {
            policy: AccessPolicy::ModuleOnly,
            accessor: ClassId::new(3),
            declaring: ClassId::new(4),
        };
        assert_eq!(grant.policy(), AccessPolicy::ModuleOnly);
        assert_eq!(grant.accessor(), ClassId::new(3));
        assert_eq!(grant.declaring(), ClassId::new(4));
        assert_ne!(AccessPolicy::ModuleOnly, AccessPolicy::Full);
    }
}
