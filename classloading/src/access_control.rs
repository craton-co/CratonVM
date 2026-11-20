// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Access control enforcement (JVM spec 5.4.4).
//!
//! Checks whether a class, field, or method is accessible from a given context.
//! Returns `IllegalAccessError` when access is denied.
//!
//! # STATUS (audited 2026-07-26) - member access is NOT enforced at runtime
//!
//! The question this audit set out to answer was "does a resolution cache skip
//! the access check?". The answer is no, and for an uncomfortable reason:
//! **on the bytecode resolution path there is no member access check to skip.**
//!
//! What the caching does right, so it is not re-litigated later: every live
//! resolution cache is keyed on the *referencing* class, not just the resolved
//! member --
//!
//! * `resolution::ResolutionKey` = `(ClassId, u16)`, i.e. (referring class, cp
//!   index);
//! * `resolution::InvokeCacheKey` = `(caller class, cp index, is_special)`,
//!   plus the receiver class for the polymorphic tier;
//! * `lockfree_resolve::PromotedInvokeKey` adds the receiver class to the same
//!   caller-keyed tuple.
//!
//! JVMS 5.4.4 resolution is defined per (referencing class, symbolic
//! reference), so a cache hit is by construction the *same* accessor that the
//! check was performed for, and re-running it would be redundant. The bug
//! shape to watch for -- a globally-keyed resolved member reused from a
//! *different* referencing class without re-checking -- is not present. The one
//! globally-keyed member cache, [`super::resolution::LinkResolver`], stores no
//! access verdict and serves only JNI `GetMethodID`/`GetFieldID`, where the
//! spec does not mandate the check. (`lockfree_resolve`'s
//! `SharedResolutionState::global_methods`/`global_fields` DO use an
//! accessor-free key; they have no production callers today, and that key shape
//! must not be adopted for anything carrying an access verdict.)
//!
//! The actual gap is upstream of caching:
//!
//! | function | production call sites |
//! |---|---|
//! | [`check_class_access`] | `new` (every mode); since interpreter round i1 wave 9 also `anewarray`, `multianewarray`, `checkcast`, `instanceof` and `ldc` of a class, through `vm/src/runtime/interpreter/constants.rs`'s `check_class_constant_access` (every mode since wave 10; wave 9 enforced it under `--real-jdk` and traced `--compatible` for a census that read zero) |
//! | [`check_module_access_by_id`] | field + method resolution (JPMS) |
//! | [`check_field_access`] | field resolution misses, through `MemberResolver::check_member_access` with `AccessPolicy::Full` (interpreter round i1 wave 26, lane L5): enforced under `--real-jdk`, counted under `--compatible` — see the wave-26 section below |
//! | [`check_method_access`] | method resolution misses, the same way |
//! | [`class_export_denial`] | the same `check_class_constant_access` path (i11-L2): enforced for every accessor in every mode (`--real-jdk` since wave 12, `--compatible` since wave 14; each after a census that read zero) |
//! | [`check_class_access_with_modules`] | **none** |
//! | [`check_field_access_with_modules`] | **none** |
//! | [`check_method_access_with_modules`] | **none** |
//! | [`are_nestmates`] | the two member checks above |
//!
//! # Wave 26 (interpreter round i1, lane L5): member access at resolution
//!
//! The consequence paragraph below described the state until wave 26. Since
//! then every field / method resolution MISS (the interpreter's
//! `field_access.rs` `resolve_field_in_class`, `resolve_field_ref*`;
//! `invoke.rs` `resolve_method_metadata`) also asks JVMS §5.4.4 for the member it resolved
//! to ([`check_field_access`] / [`check_method_access`], the protected
//! clause's `T` being the class the reference NAMES, as HotSpot's
//! `resolved_class`) and for the class the reference names (the class-constant
//! verdict). `--real-jdk` throws `IllegalAccessError` (HotSpot's wording:
//! [`field_access_denied_message`], [`method_access_denied_message`]);
//! `--compatible` admits and counts (`ClassRealm::member_access_refusals`,
//! `CRATONVM_DBG=access`). Exempt: VM-made classes on either side (lambda
//! proxies, `$ProxyN`, reflection accessors, VM-internal shapes, compatibility
//! stubs), `SerializationConstructorAccessorImpl` subclasses, HotSpot's
//! pre-Java-8 relaxation, and a private access whose common nest host is not
//! loaded (HotSpot loads it; this check cannot).
//!
//! Consequence (before wave 26): at `getfield` / `putfield` / `getstatic` / `putstatic` /
//! `invoke*`, a hand-written class file that names another class's `private`
//! or package-private member resolves and executes. The verifier does not
//! compensate -- `IllegalAccessError` is constructed nowhere outside this
//! module. Class-level access is enforced in every mode for `new`,
//! `checkcast`, `instanceof`, `ldc`, `anewarray` and `multianewarray` (waves
//! 9-10) -- not for the owner class of a field/method ref. The JPMS export
//! clause ([`class_export_denial`]) is enforced for every accessor in every
//! mode (wave 14); [`check_class_access_with_modules`] has
//! no caller.
//! Reflection is a separate subsystem with its own (correctly wired) check in
//! `native-builtins/src/lang_class.rs`.
//!
//! Wiring this up requires edits in `vm/src/runtime/interpreter.rs`, which this
//! module's owner does not own; the exact insertion points are recorded under
//! "cross-owner requests" in
//! `classloading-verify-and-resolve.md` and in
//! `access-control-and-map-coverage.md`.
//!
//! # 2026-07-26: the two false-denial defects are FIXED; the checks are safe
//! to wire
//!
//! An earlier attempt to wire these checks into the interpreter was correctly
//! abandoned, because as written they would have rejected *correct* programs.
//! Two provable false denials were identified and have now been fixed here:
//!
//! * **1a — hidden classes could never be nestmates.** [`confirmed_nest_host`]
//!   demanded that the claimed host list the member in its `NestMembers`.
//!   `NestMembers` is a class-file attribute, and a hidden class is minted at
//!   run time under a mangled name, so the confirmation was unsatisfiable by
//!   construction and `are_nestmates(hidden, host)` was *always* false. Every
//!   lambda body's `invokestatic host.lambda$foo$0` would have thrown
//!   `IllegalAccessError`. Hidden classes are now exempt (their `nest_host` is
//!   set by the defining `Lookup`, not read from attacker bytes). Pinned by
//!   `hidden_class_is_nestmate_of_its_lambda_host` and three controls.
//!
//! * **1b — [`receiver_ok_for_protected`] was stricter than the spec.** It
//!   implemented only `T <: D` of the four JVMS §5.4.4 / HotSpot
//!   `verify_field_access` disjuncts (`T == C`, `T == D`, `D <: T`, `T <: D`),
//!   so the ordinary javac-emitted `this.inheritedProtectedField`
//!   (`getfield p/C.f` from `q/S`) was denied. All four are now implemented,
//!   one test per disjunct plus a sibling-receiver control proving the clause
//!   was widened and not neutered.
//!
//! One live trap remains for whoever does the wiring: the cross-package
//! protected receiver clause is satisfied **vacuously** by `receiver: None`.
//! Plumbing the static receiver type through `getfield`/`invokevirtual` is
//! part of the job, not an optional refinement -- see
//! `protected_receiver_none_is_vacuous_not_a_check`.

use cratonvm_reader::class_access_flags::{FieldAccessFlags, MethodAccessFlags};
#[cfg(test)]
use std::sync::Arc;

use super::class::{Class, ClassStore};
use crate::loader_flags;
use crate::module::{package_of as module_pkg_of, ModuleIdent, ModuleRegistry, UNNAMED_MODULE};
pub use cratonvm_native_api::ModuleQuestion;
use cratonvm_types::error::LinkageError;
use cratonvm_types::ClassLoaderId;

/// Check whether `accessor` can access `target` class.
///
/// Per JVM spec 5.4.4:
/// - Public classes are accessible from any class.
/// - Non-public (package-private) classes are only accessible from the same runtime package.
#[inline]
pub fn check_class_access(accessor: &Class, target: &Class) -> Result<(), LinkageError> {
    check_class_access_in(accessor, target, None)
}

/// [`check_class_access`] whose refusal names a platform-module class's
/// loader `'platform'` when `registry` holds the JDK's platform-module table
/// ([`message_loader_of`]).
#[inline]
pub fn check_class_access_in(
    accessor: &Class,
    target: &Class,
    registry: Option<&ModuleRegistry>,
) -> Result<(), LinkageError> {
    if target.is_public() {
        return Ok(());
    }

    // Package-private: same runtime package required (loader-aware, JVMS §5.3)
    if same_runtime_package(accessor, target) {
        return Ok(());
    }

    if loader_flags().dbg_access {
        eprintln!(
            "[ACCESS-DBG] DENY accessor={} accessor.loader_id={:?} target={} target.loader_id={:?}",
            accessor.name, accessor.loader_id, target.name, target.loader_id
        );
    }
    // HotSpot's wording (`class_access_denied_message`), which the
    // interpreter's `new` and the JIT's CP-indexed helpers now share; the old
    // `class X cannot access class Y (not public, different package)` was a
    // CratonVM invention, and the JIT's copy of it carried an extra
    // `illegal access: ` prefix (interpreter round i1, wave 9, lane L2).
    Err(LinkageError::IllegalAccessError {
        message: class_access_denied_message(accessor, target, registry),
    })
}

/// Check whether `accessor` can access a field in `declaring` class with given flags.
///
/// Per JVM spec 5.4.4:
/// - `PUBLIC` → accessible from anywhere
/// - `PRIVATE` → accessible only from declaring class
/// - `PROTECTED` → accessible from same package OR subclasses (with the
///   additional receiver-subtype requirement below for cross-package access)
/// - Package-private (no access modifier) → accessible from same package only
///
/// `receiver` is the *static type* of the object/expression through which the
/// member is being accessed (`None` for a static field, or when the caller does
/// not track it). It is only consulted for the JVMS §5.4.4 cross-package
/// protected receiver-subtype check (see [`receiver_ok_for_protected`]).
#[inline]
pub fn check_field_access(
    accessor: &Class,
    declaring: &Class,
    flags: FieldAccessFlags,
    store: &ClassStore,
    receiver: Option<&Class>,
) -> Result<(), LinkageError> {
    // Public fields are always accessible
    if flags.contains(FieldAccessFlags::PUBLIC) {
        return Ok(());
    }

    // Private: only from the declaring class itself or a nestmate (JEP 181)
    if flags.contains(FieldAccessFlags::PRIVATE) {
        if accessor.id == declaring.id || are_nestmates(accessor, declaring, store) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access private field in {}",
                accessor.name, declaring.name
            ),
        });
    }

    // Protected: same package OR subclass
    if flags.contains(FieldAccessFlags::PROTECTED) {
        if same_runtime_package(accessor, declaring) {
            return Ok(());
        }
        // Cross-package protected access: the accessor C must be a subclass of
        // the class D declaring the member (JVMS §5.4.4). An INTERFACE is never
        // one here: HotSpot's `Reflection::verify_member_access` skips the
        // protected clause for an interface accessor ("An interface may not
        // access protected members of j.l.Object"), whose superclass entry
        // names `java/lang/Object` (JIT round 20 wave 15, lane huntLink;
        // probe `R20HuntLinkAccess`).
        if !accessor.is_interface() && accessor.is_subclass_of(declaring.id, store) {
            // ...AND the access must be *through a receiver* whose static type
            // T satisfies one of the four JVMS §5.4.4 disjuncts (see
            // `receiver_ok_for_protected`). A protected member of a superclass
            // in another package is NOT reachable through an unrelated
            // sibling-type receiver. When the caller does not supply a receiver
            // (e.g. a static field, or a context that does not track it) the
            // receiver clause does not apply and access is permitted.
            if receiver_ok_for_protected(accessor, declaring, receiver, store) {
                return Ok(());
            }
            return Err(LinkageError::IllegalAccessError {
                message: format!(
                    "class {} cannot access protected field in {} \
                     (different package; receiver is not a subtype of {})",
                    accessor.name, declaring.name, accessor.name
                ),
            });
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access protected field in {} (different package, not subclass)",
                accessor.name, declaring.name
            ),
        });
    }

    // Package-private (no access modifier): same package only
    if same_runtime_package(accessor, declaring) {
        return Ok(());
    }

    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access package-private field in {} (different package)",
            accessor.name, declaring.name
        ),
    })
}

/// Check whether `accessor` can access a method in `declaring` class with given flags.
///
/// Same rules as field access (JVM spec 5.4.4), including the cross-package
/// protected receiver-subtype requirement. `receiver` is the static type of the
/// object through which the method is invoked (`None` for a static method, or
/// when the caller does not track it).
#[inline]
pub fn check_method_access(
    accessor: &Class,
    declaring: &Class,
    flags: MethodAccessFlags,
    store: &ClassStore,
    receiver: Option<&Class>,
) -> Result<(), LinkageError> {
    // Public methods are always accessible
    if flags.contains(MethodAccessFlags::PUBLIC) {
        return Ok(());
    }

    // Private: only from the declaring class itself or a nestmate (JEP 181)
    if flags.contains(MethodAccessFlags::PRIVATE) {
        if accessor.id == declaring.id || are_nestmates(accessor, declaring, store) {
            return Ok(());
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access private method in {}",
                accessor.name, declaring.name
            ),
        });
    }

    // Protected: same package OR subclass
    if flags.contains(MethodAccessFlags::PROTECTED) {
        if same_runtime_package(accessor, declaring) {
            return Ok(());
        }
        // Cross-package protected access: the accessor C must be a subclass of
        // the class D declaring the member (JVMS §5.4.4), and not an interface
        // (see `check_field_access`).
        if !accessor.is_interface() && accessor.is_subclass_of(declaring.id, store) {
            // ...AND the access must be *through a receiver* whose static type
            // T satisfies one of the four JVMS §5.4.4 disjuncts. See
            // `receiver_ok_for_protected` and `check_field_access` for the
            // rationale; `None` receiver (static invocation / untracked) skips
            // the receiver clause.
            if receiver_ok_for_protected(accessor, declaring, receiver, store) {
                return Ok(());
            }
            return Err(LinkageError::IllegalAccessError {
                message: format!(
                    "class {} cannot access protected method in {} \
                     (different package; receiver is not a subtype of {})",
                    accessor.name, declaring.name, accessor.name
                ),
            });
        }
        return Err(LinkageError::IllegalAccessError {
            message: format!(
                "class {} cannot access protected method in {} (different package, not subclass)",
                accessor.name, declaring.name
            ),
        });
    }

    // Package-private: same package only
    if same_runtime_package(accessor, declaring) {
        return Ok(());
    }

    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access package-private method in {} (different package)",
            accessor.name, declaring.name
        ),
    })
}

/// Enforce the JVMS §5.4.4 *receiver-subtype* clause for cross-package
/// `protected` access.
///
/// When code in class `C` (the `accessor`) accesses a `protected` member that
/// is declared in a class `D` belonging to a *different* run-time package, the
/// access is only permitted if `C` is a subclass of `D` **and** the access is
/// performed through a reference whose static type is `C` or a subclass of `C`.
///
/// The classic example (JLS §6.6.2): given `package p; public class C` and a
/// subclass `package q; class S extends C` with a `protected` member `m`
/// inherited from `C` — actually declared in `p` — code in `S` may use
/// `this.m` or `((S) other).m`, but may NOT reach `m` through a bare `C`
/// receiver (`someC.m`) because `C` is in a different package. This stops a
/// subclass from using its inherited access to reach a *sibling's* protected
/// state.
///
/// `receiver` is the static type of the expression the member is accessed
/// through. `None` means there is no receiver subject to this clause (a static
/// member, or a caller that does not model the receiver type); in that case the
/// clause is vacuously satisfied — the preceding subclass check already gated
/// access. When a receiver *is* supplied, it must satisfy one of the four
/// disjuncts below.
///
/// # The four disjuncts (FALSE-DENIAL FIX, arch-2026-07-26, defect 1b)
///
/// Naming follows JVMS §5.4.4 (**not** the parameter names): `C` is the class
/// that *declares* the member (`declaring`), `D` is the class attempting the
/// access (`accessor`), and `T` is the class named by the symbolic reference,
/// i.e. the static type of the receiver.
///
/// JVMS §5.4.4 requires `T` to be "either a subclass of `D`, a superclass of
/// `D`, or `D` itself". HotSpot's `Reflection::verify_field_access` implements
/// that as four disjuncts (`current_class` = `D`, `resolved_class` = `T`,
/// `field_class` = `C`):
///
/// ```text
///     current_class == resolved_class              // T == D
///  || field_class   == resolved_class              // T == C
///  || current_class->is_subclass_of(resolved_class) // D <: T
///  || resolved_class->is_subclass_of(current_class) // T <: D
/// ```
///
/// This function previously implemented **only** `T <: D`. That denied the
/// single most common shape javac emits: `this.protectedInheritedField` from a
/// subclass in another package. Given `package p; public class C { protected
/// int f; }` and `package q; class S extends C`, javac compiles `this.f`
/// inside `S` to `getfield p/C.f` — so `T = p/C`, `D = q/S`, `C = p/C`.
/// `T <: D` is false (`p/C` is a *super*class of `q/S`), so a spec-legal,
/// javac-generated access was rejected. `T == C` and `D <: T` both cover it.
///
/// `T == C` is in fact implied by `D <: T` whenever the caller's preceding
/// `D <: C` subclass gate held; it is kept as an explicit disjunct anyway so
/// this function reads as the spec rule rather than as a derived one, and so a
/// hierarchy walk that cannot reach `C` (e.g. a not-yet-linked super link)
/// cannot turn a legal access into an `IllegalAccessError`.
#[inline]
fn receiver_ok_for_protected(
    accessor: &Class,
    declaring: &Class,
    receiver: Option<&Class>,
    store: &ClassStore,
) -> bool {
    match receiver {
        // No tracked receiver (static access, or untracked) → clause N/A.
        None => true,
        Some(t) => {
            // T == D  — access through the accessor's own type.
            t.id == accessor.id
                // T == C  — the symbolic reference names the declaring class,
                // which is what javac emits for `this.inheritedProtected`.
                || t.id == declaring.id
                // D <: T  — receiver typed as some supertype of the accessor
                // that is still at or below the declaring class.
                || accessor.is_subclass_of(t.id, store)
                // T <: D  — receiver typed as the accessor or a subclass.
                || t.is_subclass_of(accessor.id, store)
        }
    }
}

/// Check if two classes are nestmates (JEP 181, Java 11+).
///
/// Two classes are nestmates if they have the same nest host. The nest host is:
/// - The class named by the `NestHost` attribute, if present.
/// - Otherwise, the class itself (it is its own nest host).
///
/// Per JVMS §5.4.4, nest membership must be confirmed *bidirectionally*:
/// a self-declared `NestHost` attribute is not sufficient. The claimed host
/// class must actually be loadable and must list the claiming class in its
/// `NestMembers` attribute. Without this confirmation a hostile class file
/// could spoof its `NestHost` to gain `private` access to a victim's
/// nestmates. If a claimed host cannot be resolved in the [`ClassStore`],
/// or does not list the claiming member, that class is treated as its own
/// nest host (so the spoof simply fails to grant access).
///
/// **Exception: hidden classes** (JEP 371). A `NestMembers` attribute is
/// parsed out of a *class file*, so it can only ever name classes that
/// existed when the host was compiled. A hidden class is minted at run time
/// under a synthetic name (`com/foo/Host/0x2a`), so no host's `NestMembers`
/// can list it and the bidirectional confirmation is unsatisfiable *by
/// construction* rather than because anything is wrong. See
/// [`confirmed_nest_host`] for why trusting the hidden class's declared
/// `NestHost` is nevertheless safe.
///
/// # One loader, one run-time package (interpreter round i1 wave 27, lane L5)
///
/// A nest lies within one run-time package: HotSpot resolves a member's
/// claimed host through the MEMBER's defining loader and accepts it only in
/// the member's run-time package (`InstanceKlass::nest_host`), else the member
/// is its own host. So two classes of different defining loaders are never
/// nestmates, and a claim is confirmed only by the host class of the member's
/// own loader, in its package. Before wave 27 the hosts were compared by NAME
/// and a claim was confirmed through the loader-blind (and O(classes))
/// `ClassStore::find_by_name`, so a loader's `Outer$In` that another loader's
/// `Outer` listed was admitted to that `Outer`'s private members (probe
/// `tools/probes/interp/L5/L5W27NestmateEdges.java`, `cross-loader nest
/// claim`).
///
/// The common pair — a nested class and the top-level host it names, either
/// way round — is answered from the two classes in hand ([`hosted_by`]); only
/// siblings and hidden classes whose declared host is itself nested search the
/// store.
#[inline]
pub fn are_nestmates(a: &Class, b: &Class, store: &ClassStore) -> bool {
    if a.id == b.id {
        return true;
    }
    if a.loader_id != b.loader_id {
        return false;
    }
    if let Some(verdict) = hosted_by(a, b).or_else(|| hosted_by(b, a)) {
        return verdict;
    }
    let host_a = confirmed_nest_host(a, store);
    let host_b = confirmed_nest_host(b, store);
    host_a == host_b
}

/// `Some(verdict)` when `host` is its own nest host (it names no other host)
/// and so the question "are `member` and `host` nestmates" is "is `host`
/// `member`'s confirmed host", decidable from the two classes alone (same
/// loader, checked by the caller): a non-hidden `member` must name `host`, be
/// listed in `host`'s `NestMembers` and share its run-time package; a hidden
/// `member` must have been given `host` by its defining `Lookup`. `None` when
/// `host` is itself nested or hidden, or when a hidden `member`'s declared host
/// is another class (it may be a nested class of `host`'s nest: the store
/// walk in [`confirmed_nest_host`] follows it).
#[inline]
fn hosted_by(member: &Class, host: &Class) -> Option<bool> {
    if host.hidden || host.nest_host.as_deref().is_some_and(|h| h != &*host.name) {
        return None;
    }
    let declared = match member.nest_host.as_deref() {
        None => return Some(false),
        Some(h) if h == &*member.name => return Some(false),
        Some(h) => h,
    };
    if member.hidden {
        return (declared == &*host.name).then_some(true);
    }
    Some(
        declared == &*host.name
            && runtime_package_name(member) == runtime_package_name(host)
            && host.nest_members.iter().any(|m| m == &*member.name),
    )
}

/// The class named `name` that `loader` defined, if it is in the store. A
/// linear scan: [`are_nestmates`] reaches it only for the pairs [`hosted_by`]
/// cannot decide.
fn class_of_loader_named<'s>(
    store: &'s ClassStore,
    name: &str,
    loader: ClassLoaderId,
) -> Option<&'s Class> {
    store
        .iter()
        .find(|c| c.loader_id == loader && &*c.name == name)
}

/// Resolve the *confirmed* nest host of `class`.
///
/// If `class` declares a `NestHost` attribute, the named host must be
/// loadable and must list `class` in its own `NestMembers` attribute for
/// the claim to be honored (JVMS §5.4.4). When the claim cannot be
/// confirmed, `class` is its own nest host.
///
/// # Hidden classes are exempt from the bidirectional confirmation
///
/// FALSE-DENIAL FIX (arch-2026-07-26, defect 1a). `NestMembers` is a
/// *class-file* attribute: it is fixed at compile time and can only name
/// classes that the compiler knew about. A hidden class (JEP 371) is created
/// at run time by `Lookup.defineHiddenClass(..., NESTMATE)` under a mangled,
/// per-instance name (`native-builtins/src/lookup_define.rs` mints
/// `"{lookup_or_its_host}/0x{counter:x}"`), so **no** host's `NestMembers`
/// list can ever contain it. Requiring confirmation therefore made
/// `are_nestmates(hidden, host)` unconditionally false, which in turn made
/// every lambda body's `invokestatic host.lambda$foo$0` (private, in the
/// host) an `IllegalAccessError` the moment [`check_method_access`] is wired
/// into the interpreter — i.e. it would have broken every lambda in every
/// correct program.
///
/// Trusting the hidden class's declared `NestHost` is not a spoofing hole,
/// because — unlike a `NestHost` attribute read out of attacker-supplied
/// bytes — a hidden class's `nest_host` is **not** read from its class file
/// at all. `class_manager::define_class_with_options` overwrites whatever the
/// class file said with `options.nest_host_class_name`, and the only
/// producers of that option (`lookup_define.rs`,
/// `native-builtins/src/classloader.rs`, `native-builtins/src/lang_system.rs`)
/// derive it from the *`MethodHandles.Lookup`'s own class*, which the caller
/// must already have had nest-level access to obtain. The claim is
/// authoritative because only the defining call could have made it.
///
/// The exemption is deliberately one-directional and deliberately narrow: it
/// applies only when `class.hidden` is set, and a *non-hidden* class claiming
/// a hidden class's name as its host still goes through full confirmation
/// (and fails, since a hidden class is not in `find_by_name`'s index under a
/// name any classfile could spell).
///
/// # W3-2 complement: the exemption only ever sees a claim it can trust
///
/// The `class.hidden` arm below trusts `class.nest_host` because it was
/// supplied by the defining `Lookup` rather than read from the class file. For
/// that premise to hold, a hidden class defined WITHOUT `ClassOption::NESTMATE`
/// must not be carrying a class-file `NestHost` attribute at all — and javac
/// emits one for every nested class, so re-defining already-compiled bytes as
/// a hidden class used to smuggle exactly such a claim in here.
/// `class_manager::hidden_class_drops_class_file_nest_host` now discards it at
/// definition time, so a non-NESTMATE hidden class reaches the `None` arm and
/// is its own nest host — no private access to the class it was compiled
/// inside, which is the JEP 371 contract and what
/// `regression-suite/src/RJdkHidden.java:151` asserts.
fn confirmed_nest_host<'a>(class: &'a Class, store: &'a ClassStore) -> &'a str {
    match class.nest_host.as_deref() {
        // A class that names itself as its NestHost is its own host.
        Some(host) if host == &*class.name => &class.name,
        // JEP 371 hidden class: the declared host is authoritative by
        // construction (see the doc comment above). No `NestMembers`
        // round-trip is possible or required. It is the Lookup class the
        // defining call named, and JEP 371 puts the hidden class in THAT
        // class's nest: when the Lookup class is itself nested
        // (`Outer$Inner`), the nest host is its host (`Outer`), as
        // `Class.getNestHost()` answers on HotSpot. Followed here, whichever
        // producer recorded the Lookup class rather than its host (interpreter
        // round i1 wave 27, lane L5; `L5W27NestmateEdges`, `hidden -> outer
        // private`).
        Some(host) if class.hidden => match class_of_loader_named(store, host, class.loader_id) {
            Some(lookup_class) if !lookup_class.hidden => confirmed_nest_host(lookup_class, store),
            _ => host,
        },
        Some(host) => {
            // The host of THIS class's loader must exist, share its run-time
            // package and explicitly list this class as a member. Otherwise
            // the NestHost claim is unconfirmed (spoofed, or another loader's
            // nest), and the class is its own host.
            match class_of_loader_named(store, host, class.loader_id) {
                Some(host_class)
                    if runtime_package_name(host_class) == runtime_package_name(class)
                        && host_class.nest_members.iter().any(|m| m == &*class.name) =>
                {
                    host
                }
                _ => &class.name,
            }
        }
        // No NestHost attribute: the class is its own nest host.
        None => &class.name,
    }
}

/// Check if two classes are in the same *runtime* package.
///
/// Per JVMS §5.3, a runtime package is identified by the tuple
/// `(defining class loader, package name)` — NOT by the package-name
/// string alone. Two classes named `java/lang/Xxx` are only in the same
/// runtime package if they were *defined by the same class loader*.
///
/// This matters for security (finding H5): a user-defined class loader
/// can define a class literally named `java/lang/Evil`. If package
/// identity were computed from the name string alone, that class would
/// be treated as a package-mate of the real bootstrap `java.lang.*`
/// classes and could reach their package-private members. Comparing the
/// defining-loader id as well closes that spoofing hole: the attacker's
/// class lives in `(user-defined-loader, "java/lang")`, which is a
/// distinct runtime package from `(bootstrap, "java/lang")`.
///
/// The package name itself is still the prefix of the fully-qualified
/// internal name. For example:
/// - `"java/lang/Object"` → package `"java/lang"`
/// - `"java/lang/String"` → package `"java/lang"` (same name)
/// - `"java/util/List"` → package `"java/util"` (different name)
/// - `"Foo"` → default package `""` (no `/`)
#[inline]
pub fn same_runtime_package(a: &Class, b: &Class) -> bool {
    // Runtime package identity = (defining loader, package name).
    a.loader_id == b.loader_id && runtime_package_name(a) == runtime_package_name(b)
}

/// The package a class's runtime package is named by.
///
/// A hidden class is stored under `<class-file name>/0x<hex>` (every mint
/// site, and `ClassManager::define_class_with_options`'s collision suffix,
/// builds that shape), and the `/` before `0x` is part of the NAME, not a
/// package separator: JEP 371 puts a hidden class in its lookup class's
/// runtime package, which is the package of the class-file name. Taking the
/// last `/` of the stored name put `p/Host$$Lambda/0x1f` in package
/// `p/Host$$Lambda`, so the class-access check denied a lambda proxy every
/// package-private class of its own host's package (interpreter round i1,
/// wave 9, lane L2).
pub(crate) fn runtime_package_name(class: &Class) -> &str {
    let name: &str = &class.name;
    if class.hidden {
        if let Some((base, hex)) = name.rsplit_once("/0x") {
            if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return package_of(base);
            }
        }
    }
    package_of(name)
}

/// `Class.getName()`'s spelling of a class for a message: dotted, with a
/// hidden class's `/0x<hex>` tail kept verbatim (HotSpot's `external_name()`).
pub(crate) fn external_class_name(class: &Class) -> String {
    let name: &str = &class.name;
    if class.hidden {
        if let Some((base, hex)) = name.rsplit_once("/0x") {
            if !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return format!("{}/0x{hex}", base.replace('/', "."));
            }
        }
    }
    name.replace('/', ".")
}

/// HotSpot's `ClassLoaderData::loader_name_and_id()` for the three built-in
/// loaders (JDK 25: `'bootstrap'`, `'platform'`, `'app'`). `None` for a
/// user-defined loader, whose `<loader class> @<identity hash>` text this
/// crate cannot read (it has no loader object): the caller of
/// [`class_access_denied_message_naming`] hands in the loader's own
/// `ClassLoader.nameAndId` (JIT round 17 wave 5, lane mh5), and only without
/// one is the clause left out. (j27 wave 3 lane BACKLOG, MH517-3: the old
/// "hash cannot be reproduced" wording was stale.)
fn builtin_loader_name_and_id(loader: ClassLoaderId) -> Option<&'static str> {
    match loader {
        ClassLoaderId::Bootstrap => Some("'bootstrap'"),
        ClassLoaderId::Extension => Some("'platform'"),
        ClassLoaderId::Application => Some("'app'"),
        ClassLoaderId::UserDefined(_) => None,
    }
}

/// The loader a HotSpot-worded message names for `class`: its own, except a
/// boot-namespace class of a named module other than `java.base` that the
/// JDK's table defines to the PLATFORM loader
/// ([`ModuleRegistry::is_platform_loader_module`]: `java.sql`,
/// `java.net.http`, ...), which HotSpot's platform loader defines and names
/// `'platform'`. This VM defines every image class in its boot namespace.
/// Without the table (`None`, or not handed in yet) the class's own loader.
/// The `ClassCastException` funnel's twin is `runtime::exceptions`'
/// `message_loader_id` (interpreter round i1 wave 47, lane L4); this one
/// serves the access and loader-constraint texts (wave 48, lane L5).
pub fn message_loader_of(class: &Class, registry: Option<&ModuleRegistry>) -> ClassLoaderId {
    match (class.loader_id, class.module_name.as_deref(), registry) {
        (ClassLoaderId::Bootstrap, Some(module), Some(registry))
            if module != "java.base" && registry.is_platform_loader_module(module) =>
        {
            ClassLoaderId::Extension
        }
        (loader, ..) => loader,
    }
}

/// [`message_loader_of`] for a class known by its internal `name` alone (one
/// being defined): its package's module from the registry's name map.
pub fn message_loader_for_name(
    loader: ClassLoaderId,
    name: &str,
    registry: Option<&ModuleRegistry>,
) -> ClassLoaderId {
    let Some(registry) = registry else {
        return loader;
    };
    if loader != ClassLoaderId::Bootstrap {
        return loader;
    }
    match registry.module_for_package(package_of(name)) {
        Some(module) if module != "java.base" && registry.is_platform_loader_module(module) => {
            ClassLoaderId::Extension
        }
        _ => loader,
    }
}

/// The `@version` HotSpot's `Klass::class_in_module_of_loader` appends to the
/// name of `class`'s module (`ModuleEntry::should_show_version()`): a
/// non-boot layer's module's descriptor version (a module of a user-defined
/// loader, whose `ModuleFinder` location this VM does not record and takes to
/// be present, as `ModuleFinder.of` gives one), or a module-path module's
/// ([`ModuleRegistry::shown_module_version`]; the application loader defines
/// them). `None` for every other class, the JDK's own modules included.
/// Interpreter round i4 wave 99, lane L5
/// (`i98-orch-module-clauses-omit-version-of-named-application-modules`).
pub fn message_module_version<'r>(class: &Class, registry: &'r ModuleRegistry) -> Option<&'r str> {
    if let Some((loader_ns, name)) = layer_module_of(class, registry) {
        return registry
            .layer_module_version(loader_ns, name)
            .filter(|v| !v.is_empty());
    }
    if class.loader_id != ClassLoaderId::Application {
        return None;
    }
    let name = class.module_name.as_deref().filter(|m| !m.is_empty())?;
    registry.shown_module_version(name)
}

/// `module` with HotSpot's `@version` suffix when `version` is `Some`.
pub fn module_with_version(module: &str, version: Option<&str>) -> String {
    match version {
        Some(version) => format!("{module}@{version}"),
        None => module.to_string(),
    }
}

/// HotSpot's `Klass::class_in_module_of_loader`; the module's `@version` per
/// [`message_module_version`] (wave 99, lane L5).
fn class_in_module_of_loader(
    display: &str,
    class: &Class,
    loader: &str,
    use_are: bool,
    registry: Option<&ModuleRegistry>,
) -> String {
    let verb = if use_are { "are" } else { "is" };
    match class.module_name.as_deref() {
        Some(module) => {
            let version = registry.and_then(|r| message_module_version(class, r));
            let module = module_with_version(module, version);
            format!("{display} {verb} in module {module} of loader {loader}")
        }
        None => format!("{display} {verb} in unnamed module of loader {loader}"),
    }
}

/// The `IllegalAccessError` message HotSpot raises when `accessor` may not
/// access `target` (`LinkResolver::check_klass_accessibility`, JDK 25):
///
/// `failed to access class p.T from class q.A (p.T is in module java.base of
/// loader 'bootstrap'; q.A is in unnamed module of loader 'app')`
///
/// or, when both are in the same module, the joint clause
/// `(p.T and q.A are in unnamed module of loader 'app')`. "Same module"
/// compares module identity, so two unnamed modules are one only when the
/// loaders agree. When either loader is user-defined the parenthetical is left
/// out (see [`builtin_loader_name_and_id`]).
pub fn class_access_denied_message(
    accessor: &Class,
    target: &Class,
    registry: Option<&ModuleRegistry>,
) -> String {
    class_access_denied_message_naming(accessor, target, registry, &|_| None)
}

/// [`class_access_denied_message`] that names a user-defined loader too:
/// `user_loader` answers HotSpot's `ClassLoaderData::loader_name_and_id()`
/// for a class whose (message) loader is user-defined -- the loader's
/// `ClassLoader.nameAndId`, `R17Mh4Loaders$ChildFirst @1dbd16a6` or
/// `'name' @1dbd16a6` -- or `None` when it cannot, and the parenthetical is
/// left out as before. The classloading crate cannot read a loader object;
/// the VM's class-constant verdict hands the reader in (JIT round 17 wave 5,
/// lane mh5).
pub fn class_access_denied_message_naming(
    accessor: &Class,
    target: &Class,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> String {
    let t = external_class_name(target);
    let a = external_class_name(accessor);
    match module_clause_naming(target, accessor, registry, user_loader) {
        Some(detail) => format!("failed to access class {t} from class {a} ({detail})"),
        None => format!("failed to access class {t} from class {a}"),
    }
}

/// HotSpot `ClassFileParser::check_super_class_access` /
/// `check_super_interface_access`'s `IllegalAccessError` message when the
/// class being defined (`this`) may not access its direct supertype `sup`
/// for want of a shared run-time package (`Reflection::verify_class_access`
/// answering `OTHER_PROBLEM`, i.e. `sup` is not public):
///
/// `class <C> cannot access its [abstract ]superclass <S> (<clause>)` or
/// `class <C> cannot access its superinterface <I> (<clause>)`
///
/// where the clause is `Klass::joint_in_module_of_loader` when both are in one
/// module (`C and S are in unnamed module of loader 'app'`), else
/// `class_in_module_of_loader` of each joined by `"; "`, no parent loader
/// either way. `user_loader` names a user-defined loader (see
/// [`class_access_denied_message_naming`]); the clause is left out when it
/// cannot. Interpreter round i4 wave 85, lane L5 (probe `L5W85SuperAccess`).
pub fn super_access_denied_message(
    this: &Class,
    sup: &Class,
    interface: bool,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> String {
    let c = external_class_name(this);
    let s = external_class_name(sup);
    let what = if interface {
        "superinterface"
    } else if sup.is_abstract() {
        "abstract superclass"
    } else {
        "superclass"
    };
    match module_clause_naming(this, sup, registry, user_loader) {
        Some(detail) => format!("class {c} cannot access its {what} {s} ({detail})"),
        None => format!("class {c} cannot access its {what} {s}"),
    }
}

/// The parenthetical HotSpot appends to an access error naming `first` and
/// `second` (`Klass::joint_in_module_of_loader` when both are in one module,
/// else `first->class_in_module_of_loader()` and `second`'s joined by `"; "`),
/// or `None` when either loader is user-defined (see
/// [`builtin_loader_name_and_id`]). "Same module" compares module identity,
/// so two unnamed modules are one only when the loaders agree. A
/// user-defined loader is named by `user_loader` (see
/// [`class_access_denied_message_naming`]); `None` when either loader is
/// user-defined and `user_loader` cannot name it.
fn module_clause_naming(
    first: &Class,
    second: &Class,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> Option<String> {
    let loader_text = |class: &Class| -> Option<std::borrow::Cow<'static, str>> {
        match builtin_loader_name_and_id(message_loader_of(class, registry)) {
            Some(builtin) => Some(std::borrow::Cow::Borrowed(builtin)),
            None => user_loader(class).map(std::borrow::Cow::Owned),
        }
    };
    let first_loader = loader_text(first)?;
    let second_loader = loader_text(second)?;
    let f = external_class_name(first);
    let s = external_class_name(second);
    let same_module = first.module_name == second.module_name
        && (first.module_name.is_some() || first.loader_id == second.loader_id);
    Some(if same_module {
        format!(
            "{f} and {}",
            class_in_module_of_loader(&s, second, &second_loader, true, registry)
        )
    } else {
        format!(
            "{}; {}",
            class_in_module_of_loader(&f, first, &first_loader, false, registry),
            class_in_module_of_loader(&s, second, &second_loader, false, registry)
        )
    })
}

/// HotSpot's `IllegalAccessError` message when `accessor` may not access the
/// field `name` declared in `holder` (`LinkResolver::check_field_accessability`,
/// JDK 25):
///
/// `class q.A tried to access private field p.T.f (q.A and p.T are in unnamed
/// module of loader 'app')`
///
/// The parenthetical is left out when either loader is user-defined (HotSpot
/// prints the loader's class and identity hash there; see
/// [`builtin_loader_name_and_id`]).
pub fn field_access_denied_message(
    accessor: &Class,
    holder: &Class,
    flags: FieldAccessFlags,
    name: &str,
    registry: Option<&ModuleRegistry>,
) -> String {
    field_access_denied_message_naming(accessor, holder, flags, name, registry, &|_| None)
}

/// [`field_access_denied_message`] that names a user-defined loader as
/// HotSpot does (`R20HuntLinkAccess$Loader @1dbd16a6`), through
/// `user_loader` (see [`class_access_denied_message_naming`]); the
/// parenthetical is still left out when `user_loader` cannot name one (JIT
/// round 20 wave 15, lane huntLink: the member texts dropped it for every
/// user-loader class, where the class text already named the loader).
pub fn field_access_denied_message_naming(
    accessor: &Class,
    holder: &Class,
    flags: FieldAccessFlags,
    name: &str,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> String {
    let member = format!(
        "{}{}field {}.{name}",
        if flags.contains(FieldAccessFlags::PROTECTED) {
            "protected "
        } else {
            ""
        },
        if flags.contains(FieldAccessFlags::PRIVATE) {
            "private "
        } else {
            ""
        },
        external_class_name(holder),
    );
    member_access_denied_message(accessor, holder, &member, registry, user_loader)
}

/// HotSpot's `IllegalAccessError` message when `accessor` may not access a
/// method of `holder` (`LinkResolver::check_method_accessability`, JDK 25):
///
/// `class q.A tried to access private method 'int p.T.m()' (q.A and p.T are
/// in unnamed module of loader 'app')`
///
/// `external_method` is `Method::print_external_name`'s spelling of the
/// method (`int p.T.m()`), which the caller builds from the descriptor.
pub fn method_access_denied_message(
    accessor: &Class,
    holder: &Class,
    flags: MethodAccessFlags,
    external_method: &str,
    registry: Option<&ModuleRegistry>,
) -> String {
    method_access_denied_message_naming(accessor, holder, flags, external_method, registry, &|_| {
        None
    })
}

/// [`method_access_denied_message`] that names a user-defined loader through
/// `user_loader` (see [`field_access_denied_message_naming`]).
pub fn method_access_denied_message_naming(
    accessor: &Class,
    holder: &Class,
    flags: MethodAccessFlags,
    external_method: &str,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> String {
    let member = format!(
        "{}{}{}method '{external_method}'",
        if flags.contains(MethodAccessFlags::ABSTRACT) {
            "abstract "
        } else {
            ""
        },
        if flags.contains(MethodAccessFlags::PROTECTED) {
            "protected "
        } else {
            ""
        },
        if flags.contains(MethodAccessFlags::PRIVATE) {
            "private "
        } else {
            ""
        },
    );
    member_access_denied_message(accessor, holder, &member, registry, user_loader)
}

fn member_access_denied_message(
    accessor: &Class,
    holder: &Class,
    member: &str,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> String {
    let a = external_class_name(accessor);
    match module_clause_naming(accessor, holder, registry, user_loader) {
        Some(detail) => format!("class {a} tried to access {member} ({detail})"),
        None => format!("class {a} tried to access {member}"),
    }
}

/// HotSpot's nest-host validation failure text for `member`, whose
/// `NestHost` attribute names a class that its loader resolves to `host`
/// (`InstanceKlass::nest_host`, JDK 25):
///
/// `Type g.N (loader: 'app') is not a nest member of type h.H (loader:
/// 'app'): types are in different packages`
///
/// or `...: current type is not listed as a nest member`. `None` when the
/// claim validates (same run-time package, listed in `host`'s
/// `NestMembers`), for a hidden class (its host is the defining `Lookup`'s,
/// never validated), for a class naming no host, and when a loader cannot
/// be named (see [`class_access_denied_message_naming`]). HotSpot appends the
/// accessor's then the holder's text to a refused PRIVATE access's
/// `IllegalAccessError` (`print_nest_host_error_on`: `, (<a>, <b>)`); JIT
/// round 20 wave 15, lane huntLink (`R20HuntLinkNest`).
pub fn nest_host_error_text(
    member: &Class,
    host: &Class,
    registry: Option<&ModuleRegistry>,
    user_loader: &dyn Fn(&Class) -> Option<String>,
) -> Option<String> {
    if member.hidden || member.nest_host.as_deref() != Some(&*host.name) {
        return None;
    }
    // JVMS §5.4.4 (and HotSpot) ask the package first.
    let error = if !same_runtime_package(member, host) {
        "types are in different packages"
    } else if host.nest_members.iter().any(|m| m == &*member.name) {
        return None;
    } else {
        "current type is not listed as a nest member"
    };
    let loader_text = |class: &Class| -> Option<String> {
        match builtin_loader_name_and_id(message_loader_of(class, registry)) {
            Some(builtin) => Some(builtin.to_string()),
            None => user_loader(class),
        }
    };
    Some(format!(
        "Type {} (loader: {}) is not a nest member of type {} (loader: {}): {error}",
        external_class_name(member),
        loader_text(member)?,
        external_class_name(host),
        loader_text(host)?,
    ))
}

/// HotSpot's `print_nest_host_error_on` suffix of a refused private access:
/// `, (<accessor's>, <holder's>)`, either alone when only one is set, or the
/// empty string when neither is.
pub fn nest_host_error_suffix(accessor_error: Option<&str>, holder_error: Option<&str>) -> String {
    match (accessor_error, holder_error) {
        (None, None) => String::new(),
        (Some(a), None) => format!(", ({a})"),
        (None, Some(b)) => format!(", ({b})"),
        (Some(a), Some(b)) => format!(", ({a}, {b})"),
    }
}

/// Compare only the package-*name* component of two internal class names.
///
/// This is the loader-unaware string comparison. It is NOT sufficient for
/// access control on its own (see [`same_runtime_package`]); it is used
/// where the defining loaders are already known to match (or are
/// irrelevant, e.g. the JPMS package-export check).
#[inline]
fn same_package_name(name_a: &str, name_b: &str) -> bool {
    package_of(name_a) == package_of(name_b)
}

/// Extract the package prefix from a fully-qualified internal name.
fn package_of(class_name: &str) -> &str {
    match class_name.rfind('/') {
        Some(pos) => &class_name[..pos],
        None => "", // default package
    }
}

// ---------------------------------------------------------------------------
// N2: Module-boundary access checks (JPMS, Java 9+)
// ---------------------------------------------------------------------------

/// Check JPMS module boundary rules when `accessor` accesses a public member
/// in `target`.
///
/// This is called **in addition** to the standard JVM 5.4.4 checks above.
/// It only fires when both classes belong to distinct *named* modules.
///
/// Rules (simplified from JVMS §5.4.4 with JPMS overlay):
/// 1. Same module → allowed.
/// 2. Either module is the unnamed module → allowed (classpath compat).
/// 3. `accessor_module` must *read* `target_module`.
/// 4. `target_module` must *export* the target package to `accessor_module`.
///
/// If the `ModuleRegistry` is empty (no modules registered), the check is
/// a no-op to preserve backward compatibility with classpath-only runs.
pub fn check_module_access(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    // No modules registered → classpath-only mode, skip enforcement.
    if registry.is_empty() {
        return Ok(());
    }

    // The module each class is a MEMBER of: see `member_module_of` for why a
    // user loader's class is never read as a JDK platform module's.
    let accessor_mod = member_module_of(accessor).unwrap_or(UNNAMED_MODULE);
    let target_mod = member_module_of(target).unwrap_or(UNNAMED_MODULE);

    // Same module or unnamed module involved → always allowed.
    if accessor_mod == target_mod || accessor_mod == UNNAMED_MODULE || target_mod == UNNAMED_MODULE
    {
        return Ok(());
    }

    // Array classes have no package of their own. In the JDK an array type
    // belongs to the run-time package of its element type, and primitive /
    // primitive-array types are always accessible. CratonVM synthesises every
    // array class under module `java.base` with a descriptor name (`[I`, `[C`,
    // `[Ljava/lang/Object;`), so `package_of` yields "" for primitive arrays
    // (no '/') and a bogus "[Ljava/lang" for reference arrays — either way the
    // package-export check is meaningless for an array target. Element-type
    // accessibility is enforced separately when the element type is itself
    // referenced. Skipping here matches the JDK (e.g. java.xml's
    // `XMLSecurityManager` legitimately uses `int[]`, which is `[I` in
    // `java.base` with an empty package).
    if target.name.starts_with('[') {
        return Ok(());
    }

    let target_pkg = module_pkg_of(&target.name);

    // A named module never legitimately contains a default-(empty-)package
    // class — the JLS forbids it. An empty package on a named-module target is
    // therefore a CratonVM labelling artifact (synthetic/hidden class), not a
    // real export boundary; don't deny on it.
    if target_pkg.is_empty() {
        return Ok(());
    }

    registry
        .check_module_access(accessor_mod, target_mod, target_pkg)
        .map_err(|reason| LinkageError::IllegalAccessError { message: reason })
}

/// Module-aware variant of [`check_class_access`].
///
/// Performs both the standard 5.4.4 check and the JPMS module-boundary check.
pub fn check_class_access_with_modules(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Result<(), LinkageError> {
    check_class_access_in(accessor, target, Some(registry))?;
    check_module_access(accessor, target, registry)
}

/// HotSpot's `Reflection::verify_class_access_msg` text for
/// `TYPE_NOT_EXPORTED`, the one source of it (interpreter round i45x, lane L5,
/// `i45-L5-proposal-one-module-access-question-by-identity` item 3):
/// `class A (in module m) cannot access class T (in module t) because module
/// t does not export p to module m`. `accessor_module` is the accessor's
/// module by identity; an unnamed one reads `unnamed module` (HotSpot appends
/// an identity hash this VM cannot reproduce). `package` is the target's
/// runtime package in internal form.
fn type_not_exported_message(
    accessor: &Class,
    accessor_module: ModuleIdent<'_>,
    target: &Class,
    target_module: &str,
    package: &str,
) -> String {
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    let p = package.replace('/', ".");
    let from = accessor_module.describe();
    format!(
        "class {a} (in {from}) cannot access class {t} (in module {target_module}) \
         because module {target_module} does not export {p} to {from}"
    )
}

/// HotSpot's text for `MODULE_NOT_READABLE`, the one source of it:
/// `class A (in module m) cannot access class T (in <provider>) because
/// module m does not read <provider>`. `provider` is `module n` or `unnamed
/// module`; the reader is always a named module.
fn module_not_readable_message(
    accessor: &Class,
    reader: &str,
    target: &Class,
    provider: &str,
) -> String {
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    format!(
        "class {a} (in module {reader}) cannot access class {t} (in {provider}) \
         because module {reader} does not read {provider}"
    )
}

/// The JDK's `InaccessibleObjectException` / `IllegalAccessException` reason
/// for a reflective module refusal, the one source of it: `module t does not
/// "exports|opens p" to <accessor>`. `accessor` is `Module.toString()`'s form
/// (`module m` / `unnamed module`); `package` is in internal form.
fn reflective_refusal_message(
    question: ReflectiveModuleQuestion,
    target_module: &str,
    package: &str,
    accessor: &str,
) -> String {
    let verb = match question {
        ReflectiveModuleQuestion::Export => "exports",
        ReflectiveModuleQuestion::Open => "opens",
    };
    format!(
        "module {target_module} does not \"{verb} {}\" to {accessor}",
        package.replace('/', ".")
    )
}

/// A refusal by the JPMS export clause of JVMS §5.4.4 ([`class_export_denial`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDenial {
    /// HotSpot's `Reflection::verify_class_access_msg` text (`TYPE_NOT_EXPORTED`).
    /// For an unnamed accessor HotSpot names the module by its identity hash
    /// (`unnamed module @0x…`), which this VM cannot reproduce; that form
    /// says `unnamed module` without it.
    pub message: String,
    /// Is the accessor in a NAMED module (a platform or `--module-path`
    /// module)? A class-path class — including one in a modular jar, whose
    /// descriptor the registry keeps but a real JVM ignores — is unnamed.
    pub accessor_named: bool,
}

/// A registered module name `class` belongs to, or `None` for the unnamed
/// module: no name, or a module the registry keeps only for a class-path jar
/// ([`ModuleRegistry::is_class_path_only`]; HotSpot puts such classes in the
/// unnamed module).
fn named_module_of<'a>(class: &'a Class, registry: &ModuleRegistry) -> Option<&'a str> {
    let module = member_module_of(class)?;
    (!registry.is_class_path_only(module)).then_some(module)
}

/// The named module `class` is a member of for access control, `None` for an
/// unnamed module.
///
/// `ClassManager::define_class_with_options` attributes membership by PACKAGE
/// name, whatever the defining loader, so a class a user-defined loader
/// defines in a JDK package (`jdk/internal/reflect/X`, `jdk/internal/misc/Y`
/// — only `java.*` is a prohibited package) is labelled `java.base`. A module
/// is defined to ONE loader, and the JDK's platform modules to the bootstrap
/// or platform loader, so on HotSpot such a class is in its own loader's
/// unnamed module: a same-loader reference to it is not an export question at
/// all. Reading the label made every such reference an `IllegalAccessError`
/// ("module java.base does not export jdk.internal.reflect to unnamed
/// module"; probe `tools/probes/interp/L5/L5W26SerializationAccessorSpoof.java`,
/// interpreter round i1 wave 26, lane L5b). A named module a user loader can
/// genuinely define (`ModuleLayer.defineModules`) keeps its name.
fn member_module_of(class: &Class) -> Option<&str> {
    let module = class.module_name.as_deref()?;
    if module == UNNAMED_MODULE {
        return None;
    }
    if matches!(class.loader_id, ClassLoaderId::UserDefined(_))
        && crate::module::is_platform_module_name(module)
    {
        return None;
    }
    Some(module)
}

/// The JPMS half of JVMS §5.4.4 for a `CONSTANT_Class` resolution: may
/// `accessor` reach the PUBLIC class `target` across a module boundary? The
/// package half ([`check_class_access`]) must already have admitted `target`,
/// and for an array type the caller passes its bottom element class.
///
/// `Some` exactly when HotSpot's `Reflection::verify_class_access` answers
/// `TYPE_NOT_EXPORTED`: `target` is in a named module that neither is open
/// nor exports `target`'s package — unqualified, to the accessor's module, or
/// (for an unnamed accessor) to `ALL-UNNAMED` — declared or dynamic
/// (`--add-exports`, `Module.addExports`). The unnamed module exports
/// everything and a class-path-only module is the unnamed module, so neither
/// is ever refused as a target.
///
/// Readability (`MODULE_NOT_READABLE`) is not asked: an unnamed module reads
/// every module, and a named accessor's reads are what this VM's registry
/// models least faithfully (automatic and synthetic modules), so this answers
/// the export clause alone. `None` as well when nothing can be decided: an
/// empty registry, an array or default-package target (a CratonVM labelling
/// artifact in a named module), a module the registry has no descriptor for.
pub fn class_export_denial(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Option<ExportDenial> {
    if registry.is_empty() || target.name.starts_with('[') {
        return None;
    }
    // A layer module's class as the accessor must READ the target's module
    // first (HotSpot's `MODULE_NOT_READABLE`, asked before the export).
    if let Some(denial) = layer_accessor_readability_denial(accessor, target, registry) {
        return Some(denial);
    }
    // A class of a non-boot layer's module (`--real-jdk`): its module is
    // known by identity, not by the name map (interpreter round i1 wave 44,
    // lane L5).
    if let Some(verdict) = layer_module_export_verdict(accessor, target, registry) {
        return verdict.err();
    }
    let target_module = named_module_of(target, registry)?;
    // The accessor by identity: a class of a non-boot layer's module
    // (`--real-jdk`) is in a NAMED module the name map does not hold, which
    // `named_module_of` answers as the unnamed module -- so an export to
    // `ALL-UNNAMED` (`--add-exports java.base/p=ALL-UNNAMED`) admitted it,
    // where HotSpot refuses a named module (interpreter round i1 wave 45,
    // lane L5). One test in a VM with no layer module.
    let accessor_ident = module_ident_of(accessor, registry);
    let accessor_module = match accessor_ident {
        ModuleIdent::Named(name) | ModuleIdent::Layer { name, .. } => Some(name),
        ModuleIdent::Unnamed { .. } => None,
    };
    // Same module: only a name-map module can be `target`'s (a layer module
    // of the same name is another module).
    if accessor_ident == ModuleIdent::Named(target_module) {
        return None;
    }
    let package = runtime_package_name(target);
    if package.is_empty() {
        return None;
    }
    registry.get(target_module)?;
    // `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` (default off; interpreter
    // round i6 wave 118, lane L5, `i45-L5-proposal-one-module-access-question-by-identity`
    // item 2): the one question by identity ([`module_access`]) is asked
    // first, and its yes admits. It differs from the name-map clause below
    // where HotSpot's own run time does: a package `opens`ed to the accessor
    // (the JDK's `Module.addOpens` tells the VM `addExports0` too, so a
    // linkage-time access to an opened package passes), and nothing else for
    // a name-map pair. Its no falls through to the clause below, which keeps
    // the refusal's text.
    if loader_flags().module_access_export_one_question
        && module_access(accessor, target, registry, ModuleQuestion::Export) == Some(true)
    {
        return None;
    }
    let exported = match accessor_ident {
        ModuleIdent::Layer { loader_ns, name } => registry.is_package_exported_to_layer_module(
            target_module,
            package,
            name,
            Some(loader_ns),
        ),
        _ => {
            registry.is_package_exported_to(
                target_module,
                package,
                accessor_module.unwrap_or(UNNAMED_MODULE),
            )
                // SBR5 lane jfr: an edge `Module.addExports(pkg, <unnamed module of
                // loader L>)` is recorded against that loader's unnamed module
                // (`unnamed_module_of_loader_target`); an accessor in L's unnamed
                // module must see it (JFR grants `jdk.jfr.internal.event` to each
                // event class's module this way).
                || match accessor_ident {
                    ModuleIdent::Unnamed { loader_ns } => registry.is_package_exported_to(
                        target_module,
                        package,
                        &crate::module::unnamed_module_of_loader_target(loader_ns),
                    ),
                    _ => false,
                }
        }
    };
    if exported {
        return None;
    }
    Some(ExportDenial {
        message: type_not_exported_message(
            accessor,
            accessor_ident,
            target,
            target_module,
            package,
        ),
        accessor_named: accessor_module.is_some(),
    })
}

/// The layer module `class` is a member of: a class a user-defined loader
/// defined in a package a module of a non-boot `ModuleLayer` holds for that
/// loader (`Module.defineModule0`, recorded by
/// [`ModuleRegistry::define_layer_module`]; both modes since wave 48). HotSpot files
/// the package in the loader's package-to-module map, and a class the loader
/// defines there is a member of that module, whatever its `module_name` says:
/// the define path decides `module_name` from the name map, which holds no
/// layer module. `None` for every other class, after one test in a VM with no
/// layer module.
pub fn layer_module_of<'r>(class: &Class, registry: &'r ModuleRegistry) -> Option<(u32, &'r str)> {
    if !registry.has_layer_modules() {
        return None;
    }
    let ClassLoaderId::UserDefined(loader_ns) = class.loader_id else {
        return None;
    };
    let name = registry.layer_module_for_package(loader_ns, runtime_package_name(class))?;
    Some((loader_ns, name))
}

/// The readability clause of JVMS §5.4.4 for an `accessor` of a non-boot
/// layer's module (`--real-jdk`) and a `target` of another NAMED module (a
/// layer module or a name-map module with a descriptor): `Some` with
/// HotSpot's `MODULE_NOT_READABLE` message when the accessor's module does
/// not read the target's ([`ModuleRegistry::layer_module_reads`], the reads
/// `addReads0` recorded). `None` for every other pair: an unnamed accessor
/// reads every module, and a name-map accessor's reads are what the registry
/// models least faithfully (see [`class_export_denial`]). Asked only for a
/// `CONSTANT_Class` resolution: core reflection assumes readability, as the
/// JDK's `Reflection.verifyModuleAccess` does. Interpreter round i1 wave 45,
/// lane L5; an unnamed TARGET since wave 46 (see
/// [`layer_accessor_unnamed_readability_denial`]).
fn layer_accessor_readability_denial(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Option<ExportDenial> {
    let (loader_ns, name) = layer_module_of(accessor, registry)?;
    let (provider, provider_name) = match layer_module_of(target, registry) {
        Some((target_ns, target_name)) => (
            ModuleIdent::Layer {
                loader_ns: target_ns,
                name: target_name,
            },
            target_name,
        ),
        None => match named_module_of(target, registry) {
            Some(module) => {
                registry.get(module)?;
                (ModuleIdent::Named(module), module)
            }
            None => {
                return layer_accessor_unnamed_readability_denial(
                    accessor, target, registry, loader_ns, name,
                );
            }
        },
    };
    if registry.layer_module_reads(loader_ns, name, provider) {
        return None;
    }
    Some(ExportDenial {
        message: module_not_readable_message(
            accessor,
            name,
            target,
            &format!("module {provider_name}"),
        ),
        accessor_named: true,
    })
}

/// [`layer_accessor_readability_denial`] for a `target` in an UNNAMED module:
/// the layer module `(loader_ns, name)` reads a loader's unnamed module only
/// through a recorded edge -- `ALL_UNNAMED_MODULE` (`addReads0(m, null)`,
/// which `Module.defineModules` makes for an automatic module and
/// `Module.addReads` / `Controller.addReads` for `ALL-UNNAMED`) or that
/// loader's unnamed `Module` (`addReads0(m, unnamed)`) -- as HotSpot's
/// `ModuleEntry::can_read` answers; otherwise HotSpot's `MODULE_NOT_READABLE`
/// message, whose identity hashes (`unnamed module @0x…`) this VM does not
/// reproduce.
///
/// Asked only for a class of the application loader or of a user-defined
/// loader under a name no JDK package holds: the VM defines JDK classes of
/// its own in unnamed modules (flat-store defines, generated helpers) that
/// HotSpot keeps in `java.base`, and refusing one of those would be an
/// `IllegalAccessError` HotSpot never throws. A module with no recorded
/// read decides nothing ([`ModuleRegistry::layer_module_reads`]).
/// Interpreter round i1 wave 46, lane L5 (probe
/// `tools/probes/interp/L5/L5W46LayerReadsUnnamed.java`).
fn layer_accessor_unnamed_readability_denial(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
    loader_ns: u32,
    name: &str,
) -> Option<ExportDenial> {
    if !matches!(
        target.loader_id,
        ClassLoaderId::Application | ClassLoaderId::UserDefined(_)
    ) {
        return None;
    }
    let target_name: &str = &target.name;
    if ["java/", "javax/", "jdk/", "sun/", "com/sun/"]
        .iter()
        .any(|prefix| target_name.starts_with(prefix))
    {
        return None;
    }
    let provider = ModuleIdent::Unnamed {
        loader_ns: target.loader_id.to_native_id(),
    };
    if registry.layer_module_reads(loader_ns, name, provider) {
        return None;
    }
    Some(ExportDenial {
        message: module_not_readable_message(accessor, name, target, "unnamed module"),
        accessor_named: true,
    })
}

/// Which module question a reflective gate asks
/// ([`reflective_module_access`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflectiveModuleQuestion {
    /// Does the target's module export its package to the accessor's
    /// (`Method.invoke`, `Constructor.newInstance`, `Field.get` of a public
    /// member)?
    Export,
    /// Does it open the package to the accessor's (`setAccessible`, deep
    /// reflection)?
    Open,
}

/// The module half of a reflective access check, by the modules' identities:
/// may `accessor` reach `target`'s package by `question`? `Ok(())` or `Err`
/// with the reason, in the words of the JDK's `InaccessibleObjectException`
/// ("module m does not \"opens p\" to …"). One answer for both reflective
/// gates of the native interface (`vm_exec.rs`
/// `check_deep_reflection_access` / `reflective_export_to_accessor`), which
/// carried two copies of the same three arms before interpreter round i1
/// wave 46, lane L5
/// (`i45-L5-proposal-one-module-access-question-by-identity`, stage 2's
/// first step):
///
/// 1. a class of a non-boot layer's module as the TARGET
///    ([`layer_module_export_verdict`]; the VM records a layer module's
///    exports only -- its opens are the JDK's, asked by the `setAccessible`
///    native after this gate);
/// 2. a class of a layer module as the ACCESSOR of a name-map module: an
///    unnamed target is open to it, and otherwise only an unqualified edge or
///    a run-time edge to its module's name reaches it (`ALL-UNNAMED` does
///    not);
/// 3. everything else from the name map: [`ModuleRegistry::check_deep_reflection_access`]
///    for `Open`, [`ModuleRegistry::is_package_exported_to`] for `Export`.
///
/// An empty registry (class-path-only mode) admits everything.
///
/// A layer target's `Open` is answered from its EXPORT record here (the
/// native `setAccessible` gates correct it afterwards); see
/// `i45x-L5-module-access-flip-gate-and-remaining-readers-20261102.md`
/// section 3, row 4.
pub fn reflective_module_access(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
    question: ReflectiveModuleQuestion,
) -> Result<(), String> {
    if registry.is_empty() {
        return Ok(());
    }
    if let Some(verdict) = layer_module_export_verdict(accessor, target, registry) {
        return verdict.map_err(|denial| denial.message);
    }
    // `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION` (default off; interpreter
    // round i6 wave 118, lane L5): the one question by identity answers when it
    // can. Where it differs from the name-label arms below: an accessor (or a
    // target) a user loader defined in a JDK package is in its loader's
    // unnamed module (`module_ident_of`, as HotSpot's `member_module_of`), not
    // the platform module its label names -- the arms below read the label,
    // so such an accessor was "in java.base" and reached every java.base
    // package. No answer (a layer module's opens) keeps the arms below.
    if loader_flags().module_access_export_one_question {
        let q = match question {
            ReflectiveModuleQuestion::Export => ModuleQuestion::Export,
            ReflectiveModuleQuestion::Open => ModuleQuestion::Open,
        };
        match module_access(accessor, target, registry, q) {
            Some(true) => return Ok(()),
            Some(false) => {
                let from = match module_ident_of(target, registry) {
                    ModuleIdent::Named(name) | ModuleIdent::Layer { name, .. } => name,
                    ModuleIdent::Unnamed { .. } => UNNAMED_MODULE,
                };
                return Err(reflective_refusal_message(
                    question,
                    from,
                    runtime_package_name(target),
                    &module_ident_of(accessor, registry).describe(),
                ));
            }
            None => {}
        }
    }
    let target_mod = target.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    let target_pkg = module_pkg_of(&target.name);
    if let Some((layer_ns, layer)) = layer_module_of(accessor, registry) {
        let reached = target_mod == UNNAMED_MODULE
            || match question {
                ReflectiveModuleQuestion::Export => registry.is_package_exported_to_layer_module(
                    target_mod,
                    target_pkg,
                    layer,
                    Some(layer_ns),
                ),
                ReflectiveModuleQuestion::Open => registry.is_package_open_to_layer_module(
                    target_mod,
                    target_pkg,
                    layer,
                    Some(layer_ns),
                ),
            };
        if reached {
            return Ok(());
        }
        return Err(reflective_refusal_message(
            question,
            target_mod,
            target_pkg,
            &format!("module {layer}"),
        ));
    }
    let accessor_mod = accessor.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    // An edge to the accessor's own loader's unnamed module
    // (`Instrumentation.redefineModule` / `Module.addOpens` to
    // `loader.getUnnamedModule()`), recorded under
    // `unnamed_module_of_loader_target` since wave 46 (lane L5; `--real-jdk`)
    // and in `--compatible` too since wave 47 (lane L6).
    let to_own_unnamed = |open: bool| {
        accessor_mod == UNNAMED_MODULE && {
            let own =
                crate::module::unnamed_module_of_loader_target(accessor.loader_id.to_native_id());
            registry.is_package_open_to(target_mod, target_pkg, &own)
                || (!open && registry.is_package_exported_to(target_mod, target_pkg, &own))
        }
    };
    match question {
        ReflectiveModuleQuestion::Open => {
            let verdict =
                registry.check_deep_reflection_access(accessor_mod, target_mod, target_pkg);
            if verdict.is_err() && to_own_unnamed(true) {
                return Ok(());
            }
            verdict
        }
        ReflectiveModuleQuestion::Export => {
            if registry.is_package_exported_to(target_mod, target_pkg, accessor_mod)
                || to_own_unnamed(false)
            {
                return Ok(());
            }
            let to = if accessor_mod == UNNAMED_MODULE {
                "unnamed module".to_string()
            } else {
                format!("module {accessor_mod}")
            };
            Err(reflective_refusal_message(
                question, target_mod, target_pkg, &to,
            ))
        }
    }
}

/// Is [`reflective_module_access`]'s `Export` answer `Ok` for `target` and
/// EVERY accessor? True when the registry is empty (class-path-only mode), and
/// when `target` is in an unnamed module and in no non-boot layer's module:
/// [`layer_module_export_verdict`] then has no verdict, a layer accessor is
/// reached by an unnamed target, and the name map's
/// [`ModuleRegistry::is_package_exported_to`] admits an unnamed exporter. The
/// served reflective doors skip their caller walk on `true` (interpreter round
/// i3 wave 74, lane L3, `CRATONVM_REFLECT_INVOKE_PLAN`). Not monotone in
/// principle (a layer could later claim the package), so callers ask per call.
pub fn reflective_export_reaches_every_accessor(target: &Class, registry: &ModuleRegistry) -> bool {
    if registry.is_empty() {
        return true;
    }
    if layer_module_of(target, registry).is_some() {
        return false;
    }
    target.module_name.as_deref().unwrap_or(UNNAMED_MODULE) == UNNAMED_MODULE
}

/// The one module question about `accessor`'s and `target`'s modules, by the
/// modules' identities ([`module_ident_of`]): the three questions JVMS
/// §5.4.4 and the JDK's `Module` ask about a pair of modules (READ, EXPORT,
/// OPEN; [`ModuleQuestion`]) and whether they are one module (`Same`), with
/// the name map's unqualified / qualified / `ALL-UNNAMED` / own-loader
/// unnamed-module rules and the layer record in one place. `Some(answer)`,
/// or `None` when the registry cannot answer and the caller must ask the
/// JDK's `Module` methods: an empty registry, whether a layer module's
/// package is OPEN (its opens live in the JDK's `Module` fields; only an
/// open layer module is answered), and whether a name-map module reads a
/// layer module.
///
/// The package is `target`'s runtime package. Asked by `privateLookupIn`
/// (`Same`, `Read`, `Open`; `native-builtins` `classloader.rs`
/// `private_lookup_in_module_refusal`) under
/// `CRATONVM_MODULE_ACCESS_BY_IDENTITY`. Interpreter round i1 wave 47, lane
/// L6 (`i45-L5-proposal-one-module-access-question-by-identity`, the
/// `privateLookupIn` caller of its item 2).
pub fn module_access(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
    question: ModuleQuestion,
) -> Option<bool> {
    if registry.is_empty() {
        return None;
    }
    if matches!(
        question,
        ModuleQuestion::ExportAll | ModuleQuestion::OpenAll
    ) {
        return module_access_to_all(target, registry, question == ModuleQuestion::OpenAll);
    }
    let a = module_ident_of(accessor, registry);
    let t = module_ident_of(target, registry);
    if a == t {
        return Some(true);
    }
    let pkg = runtime_package_name(target);
    match question {
        // Answered above, before the same-module shortcut (which does not
        // hold for them); repeated so the match stays exhaustive.
        ModuleQuestion::ExportAll | ModuleQuestion::OpenAll => {
            module_access_to_all(target, registry, question == ModuleQuestion::OpenAll)
        }
        ModuleQuestion::Same => Some(false),
        ModuleQuestion::Read => match a {
            // An unnamed module reads every module.
            ModuleIdent::Unnamed { .. } => Some(true),
            ModuleIdent::Layer { loader_ns, name } => {
                Some(registry.layer_module_reads(loader_ns, name, t))
            }
            ModuleIdent::Named(reader) => match t {
                ModuleIdent::Named(provider) => Some(registry.reads(reader, provider)),
                ModuleIdent::Unnamed { .. } => Some(registry.reads(reader, UNNAMED_MODULE)),
                ModuleIdent::Layer { .. } => None,
            },
        },
        ModuleQuestion::Export | ModuleQuestion::Open => {
            let open = question == ModuleQuestion::Open;
            match t {
                // An unnamed module exports and opens every package.
                ModuleIdent::Unnamed { .. } => Some(true),
                ModuleIdent::Layer { loader_ns, name } => {
                    if open {
                        registry
                            .is_layer_module_open(loader_ns, name)
                            .then_some(true)
                    } else {
                        Some(registry.is_layer_package_exported_to(loader_ns, name, pkg, a))
                    }
                }
                ModuleIdent::Named(module) => Some(match a {
                    ModuleIdent::Layer { loader_ns, name } => {
                        if open {
                            registry.is_package_open_to_layer_module(
                                module,
                                pkg,
                                name,
                                Some(loader_ns),
                            )
                        } else {
                            registry.is_package_exported_to_layer_module(
                                module,
                                pkg,
                                name,
                                Some(loader_ns),
                            )
                        }
                    }
                    ModuleIdent::Named(to) => {
                        registry.is_package_open_to(module, pkg, to)
                            || (!open && registry.is_package_exported_to(module, pkg, to))
                    }
                    ModuleIdent::Unnamed { loader_ns } => {
                        // `ALL-UNNAMED` (the name map's unnamed target), or an
                        // edge to this loader's own unnamed module
                        // (`Instrumentation.redefineModule` / `addOpens` to
                        // `loader.getUnnamedModule()`, wave 46).
                        let own = crate::module::unnamed_module_of_loader_target(loader_ns);
                        registry.is_package_open_to(module, pkg, UNNAMED_MODULE)
                            || registry.is_package_open_to(module, pkg, &own)
                            || (!open
                                && (registry.is_package_exported_to(module, pkg, UNNAMED_MODULE)
                                    || registry.is_package_exported_to(module, pkg, &own)))
                    }
                }),
            }
        }
    }
}

/// [`ModuleQuestion::ExportAll`] / [`ModuleQuestion::OpenAll`]: does
/// `target`'s module export (`open`: open) `target`'s runtime package to
/// every module (`Module.isExported(pn)` / `isOpen(pn)`), by the module's
/// identity. An unnamed module exports and opens everything; a name-map
/// module answers from its descriptor and the unqualified run-time edges; a
/// layer module is exported from its `addExportsToAll0` record, and is
/// answered open only when the whole module is (its opens live in the JDK's
/// `Module` fields: `None`, as for a layer module the VM holds no record of).
/// Interpreter round i1 wave 49, lane L6
/// (`interpreter-L6-proposal-an-unqualified-export-question-by-identity-FIXED-20261013.md`).
fn module_access_to_all(target: &Class, registry: &ModuleRegistry, open: bool) -> Option<bool> {
    let pkg = runtime_package_name(target);
    match module_ident_of(target, registry) {
        ModuleIdent::Unnamed { .. } => Some(true),
        ModuleIdent::Named(module) => Some(if open {
            registry.is_package_open_unqualified(module, pkg)
        } else {
            registry.is_package_exported_unqualified(module, pkg)
        }),
        ModuleIdent::Layer { loader_ns, name } => {
            if open {
                registry
                    .is_layer_module_open(loader_ns, name)
                    .then_some(true)
            } else {
                registry.is_layer_package_exported_to_all(loader_ns, name, pkg)
            }
        }
    }
}

/// The name of the non-boot layer's module `class` is a member of (see
/// [`layer_module_of`]), for a message that names the class's module
/// (HotSpot's `class_in_module_of_loader`): `class.module_name` is `None`
/// for such a class. `None` for every other class.
pub fn layer_module_name_of<'r>(class: &Class, registry: &'r ModuleRegistry) -> Option<&'r str> {
    layer_module_of(class, registry).map(|(_, name)| name)
}

/// The module `class` is a member of, by identity ([`ModuleIdent`]): its
/// layer module (see [`layer_module_of`]), else the named module of the name
/// map it belongs to ([`named_module_of`]), else its loader's unnamed module.
pub fn module_ident_of<'a>(class: &'a Class, registry: &'a ModuleRegistry) -> ModuleIdent<'a> {
    if let Some((loader_ns, name)) = layer_module_of(class, registry) {
        return ModuleIdent::Layer { loader_ns, name };
    }
    match named_module_of(class, registry) {
        Some(name) => ModuleIdent::Named(name),
        None => ModuleIdent::Unnamed {
            loader_ns: class.loader_id.to_native_id(),
        },
    }
}

/// The JPMS export clause of JVMS §5.4.4 for a `target` that is a member of
/// a non-boot layer's module (`--real-jdk`): `None` when `target` is not such
/// a class (the name map decides), else `Some(Ok(()))` when the module is
/// `accessor`'s own, is open, or exports `target`'s package to `accessor`'s
/// module -- by identity, so two layers' modules of one name keep their own
/// exports -- and `Some(Err(denial))` with HotSpot's `TYPE_NOT_EXPORTED`
/// message otherwise. Asked by the `CONSTANT_Class` resolution check
/// ([`class_export_denial`]) and by the reflective export gates.
pub fn layer_module_export_verdict(
    accessor: &Class,
    target: &Class,
    registry: &ModuleRegistry,
) -> Option<Result<(), ExportDenial>> {
    let (loader_ns, target_module) = layer_module_of(target, registry)?;
    let accessor_module = module_ident_of(accessor, registry);
    let package = runtime_package_name(target);
    if registry.is_layer_package_exported_to(loader_ns, target_module, package, accessor_module) {
        return Some(Ok(()));
    }
    Some(Err(ExportDenial {
        message: type_not_exported_message(
            accessor,
            accessor_module,
            target,
            target_module,
            package,
        ),
        accessor_named: !matches!(accessor_module, ModuleIdent::Unnamed { .. }),
    }))
}

/// Module-aware variant of [`check_field_access`].
///
/// `receiver` is the static type of the access receiver; see
/// [`check_field_access`] for the JVMS §5.4.4 cross-package protected rule.
pub fn check_field_access_with_modules(
    accessor: &Class,
    declaring: &Class,
    flags: FieldAccessFlags,
    store: &ClassStore,
    registry: &ModuleRegistry,
    receiver: Option<&Class>,
) -> Result<(), LinkageError> {
    check_field_access(accessor, declaring, flags, store, receiver)?;
    check_module_access(accessor, declaring, registry)
}

/// Module-aware variant of [`check_method_access`].
///
/// `receiver` is the static type of the access receiver; see
/// [`check_method_access`] for the JVMS §5.4.4 cross-package protected rule.
pub fn check_method_access_with_modules(
    accessor: &Class,
    declaring: &Class,
    flags: MethodAccessFlags,
    store: &ClassStore,
    registry: &ModuleRegistry,
    receiver: Option<&Class>,
) -> Result<(), LinkageError> {
    check_method_access(accessor, declaring, flags, store, receiver)?;
    check_module_access(accessor, declaring, registry)
}

/// Convenience: check JPMS module access between two classes identified by
/// ClassId, using a ClassManager reference (which holds both the class store
/// and the module registry).
///
/// Returns Ok(()) silently if either class is not found (defensive — the
/// missing class will be caught later by a more specific error path).
///
/// AUDIT (2026-07-26): the two early `Ok(())` returns below are **fail-open**.
/// The empty-registry one is correct (classpath-only mode has no modules, so
/// there is no boundary to cross). The lookup-failure one is a silent allow:
/// if either `ClassId` is absent from the store the JPMS check is skipped
/// rather than raised. Both classes are normally resident by the time
/// resolution reaches here, so this is not currently reachable in a way that
/// grants access it should not -- but it is a fail-open default, and it is
/// recorded here rather than left implicit. Changing it to fail-closed
/// requires a boot-path validation this session could not run; see the
/// arch doc.
pub fn check_module_access_by_id(
    accessor_id: super::class::ClassId,
    target_id: super::class::ClassId,
    cm: &super::class_manager::ClassManager,
) -> Result<(), LinkageError> {
    if cm.module_registry.is_empty() {
        return Ok(());
    }
    let (accessor, target) = match (cm.get_class(accessor_id), cm.get_class(target_id)) {
        (Some(a), Some(t)) => (a, t),
        _ => return Ok(()),
    };
    check_module_access(accessor, target, &cm.module_registry)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
