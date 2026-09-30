// SPDX-License-Identifier: Apache-2.0
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
//! | [`check_class_access`] | `new` (every mode); since interpreter round i1 wave 9 also `anewarray`, `multianewarray`, `checkcast`, `instanceof` and `ldc` of a class, through `vm/src/runtime/interpreter/constants.rs`'s `check_class_constant_access` (every mode since wave 10; wave 9 enforced it under `--jdk-only` and traced `--compatible` for a census that read zero) |
//! | [`check_module_access_by_id`] | field + method resolution (JPMS) |
//! | [`check_field_access`] | field resolution misses, through `MemberResolver::check_member_access` with `AccessPolicy::Full` (interpreter round i1 wave 26, lane L5): enforced under `--jdk-only`, counted under `--compatible` — see the wave-26 section below |
//! | [`check_method_access`] | method resolution misses, the same way |
//! | [`class_export_denial`] | the same `check_class_constant_access` path (i11-L2): enforced for every accessor in every mode (`--jdk-only` since wave 12, `--compatible` since wave 14; each after a census that read zero) |
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
//! verdict). `--jdk-only` throws `IllegalAccessError` (HotSpot's wording:
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
use cratonvm_types::error::LinkageError;
use cratonvm_types::ClassLoaderId;

/// Check whether `accessor` can access `target` class.
///
/// Per JVM spec 5.4.4:
/// - Public classes are accessible from any class.
/// - Non-public (package-private) classes are only accessible from the same runtime package.
#[inline]
pub fn check_class_access(accessor: &Class, target: &Class) -> Result<(), LinkageError> {
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
        message: class_access_denied_message(accessor, target),
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
        // the class D declaring the member (JVMS §5.4.4).
        if accessor.is_subclass_of(declaring.id, store) {
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
        // the class D declaring the member (JVMS §5.4.4).
        if accessor.is_subclass_of(declaring.id, store) {
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
fn runtime_package_name(class: &Class) -> &str {
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
fn external_class_name(class: &Class) -> String {
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
/// user-defined loader: HotSpot prints `<loader class> @<identity hash>`, a
/// value this VM cannot reproduce, so the message leaves the clause out
/// rather than fabricate one (the same rule `runtime::exceptions` applies to
/// `ClassCastException` messages).
fn builtin_loader_name_and_id(loader: ClassLoaderId) -> Option<&'static str> {
    match loader {
        ClassLoaderId::Bootstrap => Some("'bootstrap'"),
        ClassLoaderId::Extension => Some("'platform'"),
        ClassLoaderId::Application => Some("'app'"),
        ClassLoaderId::UserDefined(_) => None,
    }
}

/// HotSpot's `Klass::class_in_module_of_loader` (no module `@version`: JDK 25
/// prints none for the JDK's own modules, and this VM records none).
fn class_in_module_of_loader(display: &str, class: &Class, loader: &str, use_are: bool) -> String {
    let verb = if use_are { "are" } else { "is" };
    match class.module_name.as_deref() {
        Some(module) => format!("{display} {verb} in module {module} of loader {loader}"),
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
pub fn class_access_denied_message(accessor: &Class, target: &Class) -> String {
    let t = external_class_name(target);
    let a = external_class_name(accessor);
    match module_clause(target, accessor) {
        Some(detail) => format!("failed to access class {t} from class {a} ({detail})"),
        None => format!("failed to access class {t} from class {a}"),
    }
}

/// The parenthetical HotSpot appends to an access error naming `first` and
/// `second` (`Klass::joint_in_module_of_loader` when both are in one module,
/// else `first->class_in_module_of_loader()` and `second`'s joined by `"; "`),
/// or `None` when either loader is user-defined (see
/// [`builtin_loader_name_and_id`]). "Same module" compares module identity,
/// so two unnamed modules are one only when the loaders agree.
fn module_clause(first: &Class, second: &Class) -> Option<String> {
    let first_loader = builtin_loader_name_and_id(first.loader_id)?;
    let second_loader = builtin_loader_name_and_id(second.loader_id)?;
    let f = external_class_name(first);
    let s = external_class_name(second);
    let same_module = first.module_name == second.module_name
        && (first.module_name.is_some() || first.loader_id == second.loader_id);
    Some(if same_module {
        format!(
            "{f} and {}",
            class_in_module_of_loader(&s, second, second_loader, true)
        )
    } else {
        format!(
            "{}; {}",
            class_in_module_of_loader(&f, first, first_loader, false),
            class_in_module_of_loader(&s, second, second_loader, false)
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
    member_access_denied_message(accessor, holder, &member)
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
    member_access_denied_message(accessor, holder, &member)
}

fn member_access_denied_message(accessor: &Class, holder: &Class, member: &str) -> String {
    let a = external_class_name(accessor);
    match module_clause(accessor, holder) {
        Some(detail) => format!("class {a} tried to access {member} ({detail})"),
        None => format!("class {a} tried to access {member}"),
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
    check_class_access(accessor, target)?;
    check_module_access(accessor, target, registry)
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
    // A class of a non-boot layer's module (`--jdk-only`): its module is
    // known by identity, not by the name map (interpreter round i1 wave 44,
    // lane L5).
    if let Some(verdict) = layer_module_export_verdict(accessor, target, registry) {
        return verdict.err();
    }
    let target_module = named_module_of(target, registry)?;
    // The accessor by identity: a class of a non-boot layer's module
    // (`--jdk-only`) is in a NAMED module the name map does not hold, which
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
    let exported = match accessor_ident {
        ModuleIdent::Layer { name, .. } => {
            registry.is_package_exported_to_layer_module(target_module, package, name)
        }
        _ => registry.is_package_exported_to(
            target_module,
            package,
            accessor_module.unwrap_or(UNNAMED_MODULE),
        ),
    };
    if exported {
        return None;
    }
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    let p = package.replace('/', ".");
    let message = match accessor_module {
        Some(from) => format!(
            "class {a} (in module {from}) cannot access class {t} (in module {target_module}) \
             because module {target_module} does not export {p} to module {from}"
        ),
        None => format!(
            "class {a} (in unnamed module) cannot access class {t} (in module {target_module}) \
             because module {target_module} does not export {p} to unnamed module"
        ),
    };
    Some(ExportDenial {
        message,
        accessor_named: accessor_module.is_some(),
    })
}

/// The layer module `class` is a member of: a class a user-defined loader
/// defined in a package a module of a non-boot `ModuleLayer` holds for that
/// loader (`Module.defineModule0`, recorded by
/// [`ModuleRegistry::define_layer_module`]; `--jdk-only` only). HotSpot files
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
/// layer's module (`--jdk-only`) and a `target` of another NAMED module (a
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
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    Some(ExportDenial {
        message: format!(
            "class {a} (in module {name}) cannot access class {t} (in module {provider_name}) \
             because module {name} does not read module {provider_name}"
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
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    Some(ExportDenial {
        message: format!(
            "class {a} (in module {name}) cannot access class {t} (in unnamed module) \
             because module {name} does not read unnamed module"
        ),
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
    let target_mod = target.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    let target_pkg = module_pkg_of(&target.name);
    let verb = match question {
        ReflectiveModuleQuestion::Export => "exports",
        ReflectiveModuleQuestion::Open => "opens",
    };
    if let Some((_, layer)) = layer_module_of(accessor, registry) {
        let reached = target_mod == UNNAMED_MODULE
            || match question {
                ReflectiveModuleQuestion::Export => {
                    registry.is_package_exported_to_layer_module(target_mod, target_pkg, layer)
                }
                ReflectiveModuleQuestion::Open => {
                    registry.is_package_open_to_layer_module(target_mod, target_pkg, layer)
                }
            };
        if reached {
            return Ok(());
        }
        return Err(format!(
            "module {target_mod} does not \"{verb} {}\" to module {layer}",
            target_pkg.replace('/', ".")
        ));
    }
    let accessor_mod = accessor.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    // `--jdk-only`: an edge to the accessor's own loader's unnamed module
    // (`Instrumentation.redefineModule` / `Module.addOpens` to
    // `loader.getUnnamedModule()`), recorded under
    // `unnamed_module_of_loader_target` since wave 46 (lane L5). None exists
    // under `--compatible`.
    let to_own_unnamed = |open: bool| {
        accessor_mod == UNNAMED_MODULE && {
            let own = crate::module::unnamed_module_of_loader_target(
                accessor.loader_id.to_native_id(),
            );
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
            Err(format!(
                "module {target_mod} does not \"{verb} {}\" to {to}",
                target_pkg.replace('/', ".")
            ))
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
/// a non-boot layer's module (`--jdk-only`): `None` when `target` is not such
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
    let a = external_class_name(accessor);
    let t = external_class_name(target);
    let p = package.replace('/', ".");
    let from = accessor_module.describe();
    Some(Err(ExportDenial {
        message: format!(
            "class {a} (in {from}) cannot access class {t} (in module {target_module}) \
             because module {target_module} does not export {p} to {from}"
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
mod tests {
    use super::*;
    use crate::class::{Class, ClassId, ClassLoaderId, ClassState, ClassStore};
    use cratonvm_reader::class_access_flags::ClassAccessFlags;
    use cratonvm_reader::class_file_version::ClassFileVersion;
    use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};

    fn empty_cp() -> ConstantPool {
        ConstantPool::new(vec![ConstantPoolEntry::Tombstone])
    }

    fn make_class(
        store: &mut ClassStore,
        name: &str,
        superclass: Option<ClassId>,
        flags: ClassAccessFlags,
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: flags,
            superclass,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    }

    // --- The four JVMS §5.4.4 protected disjuncts (defect 1b) ---

    /// Build the cross-package `protected` fixture used by the four-disjunct
    /// tests.
    ///
    /// ```text
    ///   package p:  C  <-  Mid          Other
    ///                       ^             ^
    ///   package q:          D             |
    ///                       ^             |
    ///                      Sub        (extends C, unrelated to D)
    /// ```
    ///
    /// `C` declares the `protected` member; `D` (a different runtime package)
    /// is the accessor. Returns `(C, Mid, D, Sub, Other)`.
    fn protected_fixture(store: &mut ClassStore) -> (ClassId, ClassId, ClassId, ClassId, ClassId) {
        let c = make_class(store, "p/C", None, ClassAccessFlags::PUBLIC);
        let mid = make_class(store, "p/Mid", Some(c), ClassAccessFlags::PUBLIC);
        let d = make_class(store, "q/D", Some(mid), ClassAccessFlags::PUBLIC);
        let sub = make_class(store, "q/Sub", Some(d), ClassAccessFlags::PUBLIC);
        let other = make_class(store, "p/Other", Some(c), ClassAccessFlags::PUBLIC);
        (c, mid, d, sub, other)
    }

    /// Disjunct `T == C`: the receiver's static type is the **declaring**
    /// class. This is what javac emits for `this.inheritedProtectedField` in a
    /// cross-package subclass: `getfield p/C.f` from inside `q/D`, so
    /// `T = p/C`. FAILS BEFORE THE FIX — the old single-clause implementation
    /// asked only `T <: D`, and `p/C` is a *super*class of `q/D`.
    #[test]
    fn protected_disjunct_t_equals_declaring_class() {
        let mut store = ClassStore::new();
        let (c, _mid, d, _sub, _other) = protected_fixture(&mut store);
        let accessor = store.get(d).unwrap();
        let declaring = store.get(c).unwrap();
        let receiver = store.get(c).unwrap();
        assert!(
            check_field_access(
                accessor,
                declaring,
                FieldAccessFlags::PROTECTED,
                &store,
                Some(receiver)
            )
            .is_ok(),
            "T == C: javac's `this.inheritedProtected` shape must be permitted"
        );
        assert!(
            check_method_access(
                accessor,
                declaring,
                MethodAccessFlags::PROTECTED,
                &store,
                Some(receiver)
            )
            .is_ok(),
            "T == C: the method half must agree with the field half"
        );
    }

    /// Disjunct `T == D`: the receiver's static type is the accessor itself.
    #[test]
    fn protected_disjunct_t_equals_accessor() {
        let mut store = ClassStore::new();
        let (c, _mid, d, _sub, _other) = protected_fixture(&mut store);
        let accessor = store.get(d).unwrap();
        let declaring = store.get(c).unwrap();
        let receiver = store.get(d).unwrap();
        assert!(check_field_access(
            accessor,
            declaring,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok());
        assert!(check_method_access(
            accessor,
            declaring,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok());
    }

    /// Disjunct `D <: T`: the receiver is typed as a *superclass* of the
    /// accessor that is still at or below the declaring class (`p/Mid`).
    /// FAILS BEFORE THE FIX for the same reason as `T == C`.
    #[test]
    fn protected_disjunct_accessor_subclass_of_receiver_type() {
        let mut store = ClassStore::new();
        let (c, mid, d, _sub, _other) = protected_fixture(&mut store);
        let accessor = store.get(d).unwrap();
        let declaring = store.get(c).unwrap();
        let receiver = store.get(mid).unwrap();
        assert!(
            check_field_access(
                accessor,
                declaring,
                FieldAccessFlags::PROTECTED,
                &store,
                Some(receiver)
            )
            .is_ok(),
            "D <: T: a receiver typed as an intermediate superclass is legal"
        );
        assert!(check_method_access(
            accessor,
            declaring,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok());
    }

    /// Disjunct `T <: D`: the receiver is typed as a subclass of the accessor.
    /// This is the one clause the pre-fix implementation had.
    #[test]
    fn protected_disjunct_receiver_type_subclass_of_accessor() {
        let mut store = ClassStore::new();
        let (c, _mid, d, sub, _other) = protected_fixture(&mut store);
        let accessor = store.get(d).unwrap();
        let declaring = store.get(c).unwrap();
        let receiver = store.get(sub).unwrap();
        assert!(check_field_access(
            accessor,
            declaring,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok());
        assert!(check_method_access(
            accessor,
            declaring,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_ok());
    }

    /// CONTROL: widening to four disjuncts must not neuter the clause. A
    /// *sibling* receiver — `p/Other extends p/C`, unrelated to `q/D` in both
    /// directions — satisfies none of the four and is still denied. This is
    /// the case the clause exists for: it stops a subclass from using its
    /// inherited access to reach a sibling's protected state.
    #[test]
    fn protected_sibling_receiver_still_denied_after_widening() {
        let mut store = ClassStore::new();
        let (c, _mid, d, _sub, other) = protected_fixture(&mut store);
        let accessor = store.get(d).unwrap();
        let declaring = store.get(c).unwrap();
        let receiver = store.get(other).unwrap();
        assert!(check_field_access(
            accessor,
            declaring,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_err());
        assert!(check_method_access(
            accessor,
            declaring,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(receiver)
        )
        .is_err());
    }

    /// CONTROL: the receiver clause is only reached once clause 1 (the
    /// accessor is a subclass of the declaring class) holds. A cross-package
    /// non-subclass is denied no matter how the receiver is typed.
    #[test]
    fn protected_non_subclass_accessor_denied_regardless_of_receiver() {
        let mut store = ClassStore::new();
        let (c, _mid, _d, _sub, other) = protected_fixture(&mut store);
        let stranger = make_class(&mut store, "r/Stranger", None, ClassAccessFlags::PUBLIC);
        let accessor = store.get(stranger).unwrap();
        let declaring = store.get(c).unwrap();
        for recv in [
            None,
            Some(store.get(c).unwrap()),
            Some(store.get(other).unwrap()),
        ] {
            assert!(
                check_field_access(
                    accessor,
                    declaring,
                    FieldAccessFlags::PROTECTED,
                    &store,
                    recv
                )
                .is_err(),
                "clause 1 gates the receiver clause, not the other way round"
            );
        }
    }

    // --- The protected receiver-subtype trap (audit 2026-07-26) ---

    /// PINS A TRAP, does not assert desired behaviour.
    ///
    /// The JVMS 5.4.4 cross-package protected rule has two clauses: the
    /// accessor must be a subclass of the declaring class, AND the receiver's
    /// static type must be the accessor or a subclass of it. The second clause
    /// is implemented, but `receiver: None` satisfies it **vacuously** -- so a
    /// caller that has not plumbed the static receiver type through gets clause
    /// 1 only, and cross-package protected access that JVMS forbids is allowed.
    ///
    /// Today nothing calls `check_field_access` at all (see the module STATUS
    /// section), so this is latent rather than live. It becomes live the moment
    /// someone wires the check up at `getfield`/`invokevirtual` and passes
    /// `None` because the receiver type is inconvenient to thread through. This
    /// test exists so that reviewer sees the divergence spelled out.
    #[test]
    fn protected_receiver_none_is_vacuous_not_a_check() {
        let mut store = ClassStore::new();
        // p/Declaring  <- q/Accessor (subclass, different package)
        //                 p/Sibling  (unrelated to Accessor)
        let declaring = make_class(&mut store, "p/Declaring", None, ClassAccessFlags::PUBLIC);
        let accessor = make_class(
            &mut store,
            "q/Accessor",
            Some(declaring),
            ClassAccessFlags::PUBLIC,
        );
        let sibling = make_class(
            &mut store,
            "p/Sibling",
            Some(declaring),
            ClassAccessFlags::PUBLIC,
        );

        let acc = store.get(accessor).unwrap();
        let dec = store.get(declaring).unwrap();
        let sib = store.get(sibling).unwrap();

        // With the receiver supplied, clause 2 does its job: `p/Sibling` is not
        // a subtype of `q/Accessor`, so cross-package protected access through
        // it is denied.
        assert!(
            check_field_access(acc, dec, FieldAccessFlags::PROTECTED, &store, Some(sib)).is_err(),
            "clause 2 must reject a sibling receiver"
        );

        // With `None`, the SAME access is allowed. This is the trap.
        assert!(
            check_field_access(acc, dec, FieldAccessFlags::PROTECTED, &store, None).is_ok(),
            "an omitted receiver satisfies clause 2 vacuously - a wirer that \
             passes None gets clause 1 only"
        );
    }

    // --- package_of ---

    #[test]
    fn package_of_standard_class() {
        assert_eq!(package_of("java/lang/Object"), "java/lang");
    }

    #[test]
    fn package_of_nested() {
        assert_eq!(package_of("com/example/foo/Bar"), "com/example/foo");
    }

    #[test]
    fn package_of_default_package() {
        assert_eq!(package_of("Foo"), "");
    }

    // --- same_package_name (loader-unaware string comparison) ---

    #[test]
    fn same_package_java_lang() {
        assert!(same_package_name("java/lang/Object", "java/lang/String"));
    }

    #[test]
    fn different_packages() {
        assert!(!same_package_name("java/lang/Object", "java/util/List"));
    }

    #[test]
    fn same_default_package() {
        assert!(same_package_name("Foo", "Bar"));
    }

    #[test]
    fn default_vs_named_package() {
        assert!(!same_package_name("Foo", "com/example/Bar"));
    }

    // --- same_runtime_package (loader-aware, JVMS §5.3) ---

    /// Build a class with an explicit defining loader for the
    /// loader-aware runtime-package tests.
    fn make_class_with_loader(
        store: &mut ClassStore,
        name: &str,
        loader_id: ClassLoaderId,
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_8,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: ClassAccessFlags::SUPER, // package-private
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: None,
            nest_members: Vec::new(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    }

    #[test]
    fn same_loader_same_package_is_same_runtime_package() {
        let mut store = ClassStore::new();
        let a = make_class_with_loader(&mut store, "java/lang/A", ClassLoaderId::Bootstrap);
        let b = make_class_with_loader(&mut store, "java/lang/B", ClassLoaderId::Bootstrap);
        assert!(same_runtime_package(
            store.get(a).unwrap(),
            store.get(b).unwrap()
        ));
    }

    #[test]
    fn different_loader_same_package_name_is_distinct_runtime_package() {
        // H5: a user-defined loader's `java/lang/Evil` must NOT be in the
        // same runtime package as a bootstrap-defined `java/lang/Object`.
        let mut store = ClassStore::new();
        let boot = make_class_with_loader(&mut store, "java/lang/Object", ClassLoaderId::Bootstrap);
        let evil =
            make_class_with_loader(&mut store, "java/lang/Evil", ClassLoaderId::UserDefined(7));
        assert!(!same_runtime_package(
            store.get(boot).unwrap(),
            store.get(evil).unwrap()
        ));
    }

    #[test]
    fn spoofed_java_lang_class_cannot_reach_package_private_member() {
        // End-to-end: a user-defined-loader class named `java/lang/Evil`
        // is denied package-private field/method access to a
        // bootstrap-defined `java/lang` class.
        let mut store = ClassStore::new();
        let victim =
            make_class_with_loader(&mut store, "java/lang/Object", ClassLoaderId::Bootstrap);
        let evil =
            make_class_with_loader(&mut store, "java/lang/Evil", ClassLoaderId::UserDefined(7));
        let victim_c = store.get(victim).unwrap();
        let evil_c = store.get(evil).unwrap();

        // Package-private (no modifier) and protected non-subclass both denied.
        assert!(
            check_field_access(evil_c, victim_c, FieldAccessFlags::empty(), &store, None).is_err()
        );
        assert!(
            check_method_access(evil_c, victim_c, MethodAccessFlags::empty(), &store, None)
                .is_err()
        );
        assert!(check_class_access(evil_c, victim_c).is_err());
    }

    // --- check_class_access ---

    #[test]
    fn public_class_always_accessible() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/bar/Target",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_ok());
    }

    #[test]
    fn package_private_class_same_package() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/foo/Target",
            None,
            ClassAccessFlags::SUPER, // no PUBLIC
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_ok());
    }

    #[test]
    fn package_private_class_different_package() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/Accessor",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let target_id = make_class(
            &mut store,
            "com/bar/Target",
            None,
            ClassAccessFlags::SUPER, // no PUBLIC
        );

        let accessor = store.get(accessor_id).unwrap();
        let target = store.get(target_id).unwrap();
        assert!(check_class_access(accessor, target).is_err());
    }

    /// i9-L2: a hidden class's `/0x<hex>` tail is not a package segment. A
    /// lambda proxy stored as `p/Host$$Lambda/0x1f` is in its host's runtime
    /// package `p`, so it may access `p`'s package-private classes; a
    /// non-hidden class whose name merely looks like that is not rewritten.
    #[test]
    fn a_hidden_class_is_in_its_class_file_names_runtime_package() {
        let mut store = ClassStore::new();
        let proxy = make_class_with_loader(
            &mut store,
            "p/Host$$Lambda/0x1f",
            ClassLoaderId::Application,
        );
        let target = make_class_with_loader(&mut store, "p/PkgPrivate", ClassLoaderId::Application);
        assert!(
            check_class_access(store.get(proxy).unwrap(), store.get(target).unwrap()).is_err(),
            "control: a non-hidden `p/Host$$Lambda/0x1f` is in package `p/Host$$Lambda`"
        );
        store.get_mut(proxy).unwrap().hidden = true;
        let (proxy_c, target_c) = (store.get(proxy).unwrap(), store.get(target).unwrap());
        assert!(same_runtime_package(proxy_c, target_c));
        assert!(check_class_access(proxy_c, target_c).is_ok());
        // Another loader's `p` is still another runtime package.
        let foreign = make_class_with_loader(&mut store, "p/Other", ClassLoaderId::UserDefined(9));
        let (proxy_c, foreign_c) = (store.get(proxy).unwrap(), store.get(foreign).unwrap());
        assert!(check_class_access(proxy_c, foreign_c).is_err());
    }

    /// i9-L2: the denial carries HotSpot 25's `check_klass_accessibility`
    /// text, split and joint forms, and leaves the loader clause out for a
    /// user-defined loader rather than invent an identity hash.
    #[test]
    fn a_class_access_denial_carries_hotspots_message() {
        let mut store = ClassStore::new();
        let target = make_class_with_loader(
            &mut store,
            "java/util/ImmutableCollections$ListN",
            ClassLoaderId::Bootstrap,
        );
        store.get_mut(target).unwrap().module_name = Some("java.base".to_string());
        let app = make_class_with_loader(&mut store, "q/X", ClassLoaderId::Application);
        let err = check_class_access(store.get(app).unwrap(), store.get(target).unwrap())
            .expect_err("package-private, other package");
        let LinkageError::IllegalAccessError { message } = err else {
            panic!("expected IllegalAccessError, got {err:?}");
        };
        assert_eq!(
            message,
            "failed to access class java.util.ImmutableCollections$ListN from class q.X \
             (java.util.ImmutableCollections$ListN is in module java.base of loader \
             'bootstrap'; q.X is in unnamed module of loader 'app')"
        );

        let same = make_class_with_loader(&mut store, "p/Hidden", ClassLoaderId::Application);
        assert_eq!(
            class_access_denied_message(store.get(app).unwrap(), store.get(same).unwrap()),
            "failed to access class p.Hidden from class q.X \
             (p.Hidden and q.X are in unnamed module of loader 'app')"
        );

        let custom = make_class_with_loader(&mut store, "r/Custom", ClassLoaderId::UserDefined(3));
        assert_eq!(
            class_access_denied_message(store.get(custom).unwrap(), store.get(same).unwrap()),
            "failed to access class p.Hidden from class r.Custom"
        );
    }

    /// Interpreter round i1 wave 26 (lane L5): a refused MEMBER carries HotSpot
    /// 25's `check_field_accessability` / `check_method_accessability` text —
    /// the referencing class first in the joint module clause — and a
    /// user-defined loader's clause is left out (probe
    /// `tools/probes/interp/L5/L5W25MemberAccessProbe.java` printed the full
    /// messages on HotSpot).
    #[test]
    fn a_member_access_denial_carries_hotspots_message() {
        let mut store = ClassStore::new();
        let accessor = make_class_with_loader(&mut store, "q/Peek", ClassLoaderId::Application);
        let holder = make_class_with_loader(&mut store, "p/Secret", ClassLoaderId::Application);
        let (a, h) = (store.get(accessor).unwrap(), store.get(holder).unwrap());
        assert_eq!(
            field_access_denied_message(
                a,
                h,
                FieldAccessFlags::PRIVATE | FieldAccessFlags::STATIC,
                "P"
            ),
            "class q.Peek tried to access private field p.Secret.P \
             (q.Peek and p.Secret are in unnamed module of loader 'app')"
        );
        assert_eq!(
            field_access_denied_message(a, h, FieldAccessFlags::STATIC, "Q"),
            "class q.Peek tried to access field p.Secret.Q \
             (q.Peek and p.Secret are in unnamed module of loader 'app')"
        );
        assert_eq!(
            method_access_denied_message(
                a,
                h,
                MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC,
                "int p.Secret.p()"
            ),
            "class q.Peek tried to access private method 'int p.Secret.p()' \
             (q.Peek and p.Secret are in unnamed module of loader 'app')"
        );
        assert_eq!(
            method_access_denied_message(
                a,
                h,
                MethodAccessFlags::PROTECTED | MethodAccessFlags::ABSTRACT,
                "void p.Secret.m(int, java.lang.String)"
            ),
            "class q.Peek tried to access abstract protected method \
             'void p.Secret.m(int, java.lang.String)' \
             (q.Peek and p.Secret are in unnamed module of loader 'app')"
        );

        let boot = make_class_with_loader(&mut store, "java/lang/Thing", ClassLoaderId::Bootstrap);
        store.get_mut(boot).unwrap().module_name = Some("java.base".to_string());
        assert_eq!(
            field_access_denied_message(
                store.get(accessor).unwrap(),
                store.get(boot).unwrap(),
                FieldAccessFlags::PROTECTED,
                "x"
            ),
            "class q.Peek tried to access protected field java.lang.Thing.x \
             (q.Peek is in unnamed module of loader 'app'; java.lang.Thing is in \
             module java.base of loader 'bootstrap')"
        );

        let custom = make_class_with_loader(&mut store, "r/Custom", ClassLoaderId::UserDefined(3));
        assert_eq!(
            field_access_denied_message(
                store.get(custom).unwrap(),
                store.get(holder).unwrap(),
                FieldAccessFlags::PRIVATE,
                "f"
            ),
            "class r.Custom tried to access private field p.Secret.f"
        );
    }

    // --- check_field_access ---

    #[test]
    fn public_field_always_accessible() {
        let mut store = ClassStore::new();
        let accessor_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let declaring_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let accessor = store.get(accessor_id).unwrap();
        let declaring = store.get(declaring_id).unwrap();
        assert!(
            check_field_access(accessor, declaring, FieldAccessFlags::PUBLIC, &store, None).is_ok()
        );
    }

    #[test]
    fn private_field_same_class() {
        let mut store = ClassStore::new();
        let class_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let class = store.get(class_id).unwrap();
        assert!(check_field_access(class, class, FieldAccessFlags::PRIVATE, &store, None).is_ok());
    }

    #[test]
    fn private_field_different_class() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_err());
    }

    #[test]
    fn protected_field_subclass() {
        let mut store = ClassStore::new();
        let parent_id = make_class(
            &mut store,
            "com/foo/Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let child_id = make_class(
            &mut store,
            "com/bar/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let child = store.get(child_id).unwrap();
        let parent = store.get(parent_id).unwrap();
        // Cross-package protected access through a receiver of the accessor's
        // own type (`child`) is permitted (JVMS §5.4.4 receiver-subtype clause
        // satisfied). `None` (untracked receiver) is likewise permitted.
        assert!(check_field_access(
            child,
            parent,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(child)
        )
        .is_ok());
        assert!(
            check_field_access(child, parent, FieldAccessFlags::PROTECTED, &store, None).is_ok()
        );
    }

    #[test]
    fn protected_field_non_subclass_different_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::PROTECTED, &store, None).is_err());
    }

    #[test]
    fn package_private_field_same_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store, None).is_ok());
    }

    #[test]
    fn package_private_field_different_package() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_field_access(a, b, FieldAccessFlags::empty(), &store, None).is_err());
    }

    // --- check_method_access ---

    #[test]
    fn public_method_always_accessible() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/bar/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_method_access(a, b, MethodAccessFlags::PUBLIC, &store, None).is_ok());
    }

    #[test]
    fn private_method_same_class() {
        let mut store = ClassStore::new();
        let class_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let class = store.get(class_id).unwrap();
        assert!(
            check_method_access(class, class, MethodAccessFlags::PRIVATE, &store, None).is_ok()
        );
    }

    #[test]
    fn private_method_different_class() {
        let mut store = ClassStore::new();
        let a_id = make_class(
            &mut store,
            "com/foo/A",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let b_id = make_class(
            &mut store,
            "com/foo/B",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store, None).is_err());
    }

    #[test]
    fn protected_method_subclass_different_package() {
        let mut store = ClassStore::new();
        let parent_id = make_class(
            &mut store,
            "com/foo/Parent",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let child_id = make_class(
            &mut store,
            "com/bar/Child",
            Some(parent_id),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );

        let child = store.get(child_id).unwrap();
        let parent = store.get(parent_id).unwrap();
        // Receiver of the accessor's own type satisfies the §5.4.4 clause;
        // `None` (untracked) is also permitted.
        assert!(check_method_access(
            child,
            parent,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(child)
        )
        .is_ok());
        assert!(
            check_method_access(child, parent, MethodAccessFlags::PROTECTED, &store, None).is_ok()
        );
    }

    // --- JVMS §5.4.4 cross-package protected receiver-subtype clause ---

    /// Builds the classic §5.4.4 / JLS §6.6.2 shape:
    /// `p/Base` (declares the protected member) and two subclasses in a
    /// *different* package `q`: `q/Sub` (the accessor C) and `q/Sibling`
    /// (an unrelated subtype of Base that is NOT a subtype of Sub).
    /// Returns `(store, base_id, sub_id, sibling_id, grandsub_id)` where
    /// `q/GrandSub extends q/Sub`.
    fn build_protected_hierarchy() -> (ClassStore, ClassId, ClassId, ClassId, ClassId) {
        let mut store = ClassStore::new();
        let base = make_class(
            &mut store,
            "p/Base",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let sub = make_class(
            &mut store,
            "q/Sub",
            Some(base),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let sibling = make_class(
            &mut store,
            "q/Sibling",
            Some(base),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let grandsub = make_class(
            &mut store,
            "q/GrandSub",
            Some(sub),
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        (store, base, sub, sibling, grandsub)
    }

    #[test]
    fn protected_cross_pkg_receiver_self_type_ok() {
        // Receiver static type == accessor C → allowed.
        let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
        let base_c = store.get(base).unwrap();
        let sub_c = store.get(sub).unwrap();
        assert!(check_field_access(
            sub_c,
            base_c,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(sub_c)
        )
        .is_ok());
        assert!(check_method_access(
            sub_c,
            base_c,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(sub_c)
        )
        .is_ok());
    }

    #[test]
    fn protected_cross_pkg_receiver_subtype_of_c_ok() {
        // Receiver static type is a subclass of accessor C → allowed.
        let (store, base, sub, _sibling, grand) = build_protected_hierarchy();
        let base_c = store.get(base).unwrap();
        let sub_c = store.get(sub).unwrap();
        let grand_c = store.get(grand).unwrap();
        assert!(check_field_access(
            sub_c,
            base_c,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(grand_c)
        )
        .is_ok());
    }

    #[test]
    fn protected_cross_pkg_receiver_sibling_denied() {
        // Receiver static type is a sibling (subtype of D=Base, but NOT of
        // C=Sub) → DENIED per §5.4.4 receiver-subtype clause. This is the
        // bug being fixed: previously the subclass check alone admitted it.
        let (store, base, sub, sibling, _grand) = build_protected_hierarchy();
        let base_c = store.get(base).unwrap();
        let sub_c = store.get(sub).unwrap();
        let sibling_c = store.get(sibling).unwrap();
        assert!(check_field_access(
            sub_c,
            base_c,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(sibling_c)
        )
        .is_err());
        assert!(check_method_access(
            sub_c,
            base_c,
            MethodAccessFlags::PROTECTED,
            &store,
            Some(sibling_c)
        )
        .is_err());
    }

    #[test]
    fn protected_cross_pkg_receiver_declaring_type_permitted_by_5_4_4() {
        // Receiver static type T is the declaring class C itself (`p/Base`),
        // accessed from D=`q/Sub` in another package. §5.4.4 PERMITS this.
        //
        // It is tempting to read the protected rule as "a subclass may not
        // reach its other-package superclass's protected member through a bare
        // superclass-typed receiver" and deny it. That conflates two different
        // checks:
        //
        //   * §5.4.4 (this function, resolution time) constrains T, the class
        //     named in the *symbolic reference* — "T is either a subclass of D,
        //     a superclass of D, or D itself". Reaching this branch already
        //     established D <: C, so T == C makes T a superclass of D and the
        //     clause is satisfied. Denying it would reject javac's ordinary
        //     `this.inheritedProtectedField`, which compiles to `getfield C.f`.
        //
        //   * §4.10.1.8 (the type checker, verification time) constrains the
        //     actual *operand-stack* type, which must be assignable to D. That
        //     is the check that stops a superclass-typed value from reaching a
        //     protected member, and it is strictly stronger. It is NOT
        //     implemented here and cannot be — this function never sees the
        //     stack. See `arch-2026-07-26/`.
        //
        // HotSpot draws the same line: `Reflection::verify_field_access` lists
        // `field_class == resolved_class` as an explicit disjunct, while
        // `ClassVerifier::verify_field_instructions` separately requires the
        // stack type to be assignable to the current class.
        let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
        let base_c = store.get(base).unwrap();
        let sub_c = store.get(sub).unwrap();
        assert!(
            check_field_access(
                sub_c,
                base_c,
                FieldAccessFlags::PROTECTED,
                &store,
                Some(base_c)
            )
            .is_ok(),
            "T == C is a §5.4.4 disjunct (T is a superclass of D); the strict \
             receiver rule is §4.10.1.8's and operates on the operand stack"
        );
    }

    #[test]
    fn protected_cross_pkg_untracked_receiver_allowed() {
        // No receiver tracked (None) → receiver clause N/A; the subclass
        // check governs. Preserves behavior for callers that don't model the
        // receiver type and for static members.
        let (store, base, sub, _sibling, _grand) = build_protected_hierarchy();
        let base_c = store.get(base).unwrap();
        let sub_c = store.get(sub).unwrap();
        assert!(
            check_field_access(sub_c, base_c, FieldAccessFlags::PROTECTED, &store, None).is_ok()
        );
    }

    #[test]
    fn protected_same_pkg_ignores_receiver() {
        // Same-package protected access is granted regardless of receiver
        // type (the §5.4.4 receiver clause only applies cross-package).
        let mut store = ClassStore::new();
        let base = make_class(
            &mut store,
            "p/Base",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        // Accessor in the SAME package as the declaring class, not a subclass.
        let peer = make_class(
            &mut store,
            "p/Peer",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let unrelated = make_class(
            &mut store,
            "p/Unrelated",
            None,
            ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
        );
        let base_c = store.get(base).unwrap();
        let peer_c = store.get(peer).unwrap();
        let unrelated_c = store.get(unrelated).unwrap();
        // Even with an unrelated receiver, same-package access is allowed.
        assert!(check_field_access(
            peer_c,
            base_c,
            FieldAccessFlags::PROTECTED,
            &store,
            Some(unrelated_c)
        )
        .is_ok());
    }

    // --- Nest-based access control (JEP 181) ---

    fn make_nest_class(
        store: &mut ClassStore,
        name: &str,
        nest_host: Option<&str>,
        nest_members: &[&str],
    ) -> ClassId {
        let id = store.next_id();
        store.add(Class {
            id,
            loader_id: ClassLoaderId::Application,
            name: Arc::from(name),
            source_file: None,
            version: ClassFileVersion::JAVA_11,
            state: ClassState::Loaded,
            initializing_thread: None,
            constant_pool: empty_cp(),
            access_flags: ClassAccessFlags::PUBLIC | ClassAccessFlags::SUPER,
            superclass: None,
            interfaces: vec![],
            fields: vec![],
            methods: vec![],
            first_field_index: 0,
            num_total_fields: 0,
            bootstrap_methods: vec![],
            annotations: Vec::new(),
            nest_host: nest_host.map(|s| s.to_string()),
            nest_members: nest_members.iter().map(|s| s.to_string()).collect(),
            record_components: Vec::new(),
            permitted_subclasses: Vec::new(),
            inner_classes: Vec::new(),
            enclosing_method: None,
            hidden: false,
            module_name: None,
            origin: crate::class_origin::ClassOrigin::VmInternal,
            signature: None,
            has_finalizer: false,
            code_source: None,
            array_info: None,
            init_state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0)),
            record_object_methods: std::sync::atomic::AtomicU8::new(0),
        });
        id
    }

    #[test]
    fn nestmates_same_host_can_access_private() {
        let mut store = ClassStore::new();
        // Outer is the nest host, Inner has NestHost pointing to Outer
        let _outer_id =
            make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$Inner"]);
        let inner_id = make_nest_class(
            &mut store,
            "com/foo/Outer$Inner",
            Some("com/foo/Outer"),
            &[],
        );

        let outer = store.get(_outer_id).unwrap();
        let inner = store.get(inner_id).unwrap();

        // Inner can access Outer's private fields
        assert!(check_field_access(inner, outer, FieldAccessFlags::PRIVATE, &store, None).is_ok());
        // Outer can access Inner's private methods
        assert!(
            check_method_access(outer, inner, MethodAccessFlags::PRIVATE, &store, None).is_ok()
        );
    }

    #[test]
    fn nestmates_both_inner_classes() {
        let mut store = ClassStore::new();
        // The nest host must exist and must list both members for the
        // bidirectional confirmation (JVMS §5.4.4) to succeed.
        let _outer_id = make_nest_class(
            &mut store,
            "com/foo/Outer",
            None,
            &["com/foo/Outer$A", "com/foo/Outer$B"],
        );
        // Two inner classes with the same nest host
        let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
        let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();

        // A and B are nestmates, can access each other's private members
        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_ok());
        assert!(check_field_access(b, a, FieldAccessFlags::PRIVATE, &store, None).is_ok());
    }

    #[test]
    fn non_nestmates_cannot_access_private() {
        let mut store = ClassStore::new();
        let a_id = make_nest_class(
            &mut store,
            "com/foo/A",
            None, // its own host
            &[],
        );
        let b_id = make_nest_class(
            &mut store,
            "com/foo/B",
            None, // its own host (different nest)
            &[],
        );

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();

        assert!(check_field_access(a, b, FieldAccessFlags::PRIVATE, &store, None).is_err());
        assert!(check_method_access(a, b, MethodAccessFlags::PRIVATE, &store, None).is_err());
    }

    #[test]
    fn are_nestmates_same_class() {
        let mut store = ClassStore::new();
        let id = make_nest_class(&mut store, "com/foo/A", None, &[]);
        let class = store.get(id).unwrap();
        assert!(are_nestmates(class, class, &store));
    }

    #[test]
    fn spoofed_nest_host_is_rejected() {
        let mut store = ClassStore::new();
        // Victim nest: Host explicitly lists only its legitimate member.
        let _host_id = make_nest_class(
            &mut store,
            "com/victim/Host",
            None,
            &["com/victim/Host$Member"],
        );
        let member_id = make_nest_class(
            &mut store,
            "com/victim/Host$Member",
            Some("com/victim/Host"),
            &[],
        );
        // Attacker self-declares the victim's host as its NestHost but is
        // NOT listed in Host's NestMembers.
        let attacker_id = make_nest_class(
            &mut store,
            "com/attacker/Evil",
            Some("com/victim/Host"),
            &[],
        );

        let member = store.get(member_id).unwrap();
        let attacker = store.get(attacker_id).unwrap();

        // Spoof must fail: attacker is not a confirmed nestmate of member.
        assert!(!are_nestmates(attacker, member, &store));
        assert!(
            check_field_access(attacker, member, FieldAccessFlags::PRIVATE, &store, None).is_err()
        );
        assert!(
            check_method_access(attacker, member, MethodAccessFlags::PRIVATE, &store, None)
                .is_err()
        );
    }

    #[test]
    fn unconfirmed_nest_host_when_host_missing() {
        let mut store = ClassStore::new();
        // Two classes claim the same host, but the host class is not loaded.
        let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
        let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);
        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        // Host unresolvable → claim unconfirmed → not nestmates.
        assert!(!are_nestmates(a, b, &store));
    }

    // --- Hidden classes are nestmates of their declared host (defect 1a) ---

    /// A JEP 371 hidden class, minted the way
    /// `native-builtins/src/lookup_define.rs` mints one: a mangled
    /// `"{host}/0x{n:x}"` name that no class file's `NestMembers` could ever
    /// spell, plus a `nest_host` supplied by the defining `Lookup`.
    fn make_hidden_nest_class(store: &mut ClassStore, name: &str, nest_host: &str) -> ClassId {
        let id = make_nest_class(store, name, Some(nest_host), &[]);
        store.get_mut(id).unwrap().hidden = true;
        id
    }

    /// THE LAMBDA SHAPE. FAILS BEFORE THE FIX.
    ///
    /// `LambdaMetafactory` spins the lambda body's implementation class as a
    /// hidden class in the capturing class's nest; its bytecode then does
    /// `invokestatic com/foo/Host.lambda$run$0`, which is `private static`.
    /// Because the host's `NestMembers` is a compile-time attribute it can
    /// never list the runtime-generated hidden class, so the bidirectional
    /// confirmation was unsatisfiable and this access — present in essentially
    /// every modern Java program — would have thrown `IllegalAccessError` the
    /// moment `check_method_access` was wired into the interpreter.
    #[test]
    fn hidden_class_is_nestmate_of_its_lambda_host() {
        let mut store = ClassStore::new();
        // A top-level class with a lambda has NO NestMembers attribute at all
        // (there are no compile-time nest members to list) — which is exactly
        // why the confirmation could never succeed.
        let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
        let hidden_id = make_hidden_nest_class(&mut store, "com/foo/Host/0x2a", "com/foo/Host");

        let host = store.get(host_id).unwrap();
        let hidden = store.get(hidden_id).unwrap();

        assert!(
            are_nestmates(hidden, host, &store),
            "a NESTMATE hidden class must be a nestmate of its declared host"
        );
        assert!(
            are_nestmates(host, hidden, &store),
            "nest membership is symmetric"
        );
        assert!(
            check_method_access(
                hidden,
                host,
                MethodAccessFlags::PRIVATE | MethodAccessFlags::STATIC,
                &store,
                None,
            )
            .is_ok(),
            "the lambda body's `invokestatic host.lambda$run$0` must be permitted"
        );
        assert!(
            check_field_access(hidden, host, FieldAccessFlags::PRIVATE, &store, None).is_ok(),
            "a captured private field read from the lambda body must be permitted"
        );
    }

    /// A hidden class defined into a *nested* host's nest (the `Lookup` was on
    /// `Outer$Inner`, so `resolve_lookup_nest_host` hands back `Outer`) must be
    /// a nestmate of every other member of that nest, not just of the lookup
    /// class.
    #[test]
    fn hidden_class_joins_the_whole_nest_not_just_the_lookup_class() {
        let mut store = ClassStore::new();
        let _outer = make_nest_class(
            &mut store,
            "com/foo/Outer",
            None,
            &["com/foo/Outer$A", "com/foo/Outer$B"],
        );
        let a_id = make_nest_class(&mut store, "com/foo/Outer$A", Some("com/foo/Outer"), &[]);
        let b_id = make_nest_class(&mut store, "com/foo/Outer$B", Some("com/foo/Outer"), &[]);
        let hidden_id = make_hidden_nest_class(&mut store, "com/foo/Outer/0x7", "com/foo/Outer");

        let a = store.get(a_id).unwrap();
        let b = store.get(b_id).unwrap();
        let hidden = store.get(hidden_id).unwrap();

        assert!(are_nestmates(hidden, a, &store));
        assert!(are_nestmates(hidden, b, &store));
        assert!(
            check_field_access(hidden, b, FieldAccessFlags::PRIVATE, &store, None).is_ok(),
            "a lambda captured in Outer$A may still touch Outer$B's private state"
        );
    }

    /// Interpreter round i1 wave 27 (lane L5): a nest lies within one loader.
    /// A user loader's `Outer$In` that names `Outer` as its host, while the
    /// `Outer` that lists it was defined by ANOTHER loader, is its own nest
    /// host (HotSpot: the host resolved through the member's loader is in a
    /// different run-time package), whether the pair is asked directly or
    /// through the store walk (probe `L5W27NestmateEdges`).
    #[test]
    fn a_nest_claim_on_another_loaders_host_is_not_confirmed() {
        let mut store = ClassStore::new();
        let outer_id = make_nest_class(&mut store, "l5n2/Outer", None, &["l5n2/Outer$In"]);
        let in_id = make_nest_class(&mut store, "l5n2/Outer$In", Some("l5n2/Outer"), &[]);
        let sib_id = make_nest_class(&mut store, "l5n2/Outer$Sib", Some("l5n2/Outer"), &[]);
        store.get_mut(outer_id).unwrap().loader_id = ClassLoaderId::UserDefined(2);
        store.get_mut(in_id).unwrap().loader_id = ClassLoaderId::UserDefined(1);
        store.get_mut(sib_id).unwrap().loader_id = ClassLoaderId::UserDefined(1);
        let outer = store.get(outer_id).unwrap();
        let inner = store.get(in_id).unwrap();
        let sib = store.get(sib_id).unwrap();
        assert!(!are_nestmates(inner, outer, &store));
        assert!(!are_nestmates(outer, inner, &store));
        assert!(
            check_field_access(inner, outer, FieldAccessFlags::PRIVATE, &store, None).is_err()
        );
        // Two members of loader 1 claiming a host only loader 2 defined are
        // each their own host, not nestmates through loader 2's list.
        assert!(!are_nestmates(inner, sib, &store));
    }

    /// Wave 27 (lane L5), the fast path's two halves: a nested class and the
    /// top-level host that lists it are nestmates either way round, and a
    /// host of another package is not the member's host even when it lists it.
    #[test]
    fn a_member_and_its_listing_host_are_nestmates_only_in_one_package() {
        let mut store = ClassStore::new();
        let outer_id = make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$In"]);
        let in_id = make_nest_class(&mut store, "com/foo/Outer$In", Some("com/foo/Outer"), &[]);
        let far_id = make_nest_class(&mut store, "com/bar/Far", None, &["com/foo/Stray"]);
        let stray_id = make_nest_class(&mut store, "com/foo/Stray", Some("com/bar/Far"), &[]);
        let outer = store.get(outer_id).unwrap();
        let inner = store.get(in_id).unwrap();
        assert!(are_nestmates(inner, outer, &store));
        assert!(are_nestmates(outer, inner, &store));
        let far = store.get(far_id).unwrap();
        let stray = store.get(stray_id).unwrap();
        assert!(!are_nestmates(stray, far, &store));
        assert!(!are_nestmates(far, stray, &store));
    }

    /// Wave 27 (lane L5): a NESTMATE hidden class whose defining Lookup was on
    /// a NESTED class records that class; JEP 371 puts it in that class's nest,
    /// hosted by the outer class, so it is a nestmate of the outer class and
    /// of the lookup class alike.
    #[test]
    fn a_hidden_class_of_a_nested_lookup_joins_the_outer_nest() {
        let mut store = ClassStore::new();
        let outer_id = make_nest_class(&mut store, "com/foo/Outer", None, &["com/foo/Outer$In"]);
        let in_id = make_nest_class(&mut store, "com/foo/Outer$In", Some("com/foo/Outer"), &[]);
        let hidden_id =
            make_hidden_nest_class(&mut store, "com/foo/Outer$In$H/0x9", "com/foo/Outer$In");
        let outer = store.get(outer_id).unwrap();
        let inner = store.get(in_id).unwrap();
        let hidden = store.get(hidden_id).unwrap();
        assert!(are_nestmates(hidden, inner, &store));
        assert!(are_nestmates(hidden, outer, &store));
        assert!(are_nestmates(outer, hidden, &store));
    }

    /// CONTROL: the exemption is keyed on `Class::hidden`, nothing else. An
    /// ordinary class file that claims `NestHost com/foo/Host` while the host
    /// does not list it is still an unconfirmed (spoofed) claim and is still
    /// denied — the identical shape to the test above minus the hidden flag.
    #[test]
    fn non_hidden_class_with_same_shape_is_still_denied() {
        let mut store = ClassStore::new();
        let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
        let spoof_id = make_nest_class(
            &mut store,
            "com/evil/Spoof",
            Some("com/foo/Host"),
            &[], // host does not list it back
        );
        let host = store.get(host_id).unwrap();
        let spoof = store.get(spoof_id).unwrap();
        assert!(
            !are_nestmates(spoof, host, &store),
            "only `hidden` classes are exempt from bidirectional confirmation"
        );
        assert!(
            check_method_access(spoof, host, MethodAccessFlags::PRIVATE, &store, None).is_err()
        );
    }

    /// CONTROL: a hidden class with no `NestHost` (a non-NESTMATE
    /// `defineHiddenClass`) is its own nest host and gets no private access to
    /// anyone.
    #[test]
    fn hidden_class_without_nest_host_is_its_own_nest() {
        let mut store = ClassStore::new();
        let host_id = make_nest_class(&mut store, "com/foo/Host", None, &[]);
        let lone_id = make_nest_class(&mut store, "com/foo/Host/0x3", None, &[]);
        store.get_mut(lone_id).unwrap().hidden = true;
        let host = store.get(host_id).unwrap();
        let lone = store.get(lone_id).unwrap();
        assert!(!are_nestmates(lone, host, &store));
        assert!(check_field_access(lone, host, FieldAccessFlags::PRIVATE, &store, None).is_err());
    }

    // --- JPMS module access: array / empty-package targets ---

    use crate::module::ModuleDescriptor;

    fn make_class_in_module(store: &mut ClassStore, name: &str, module: Option<&str>) -> ClassId {
        let id = make_class(store, name, None, ClassAccessFlags::PUBLIC);
        store.get_mut(id).unwrap().module_name = module.map(|m| m.to_string());
        id
    }

    /// A non-empty registry where `java.xml` reads `java.base` but `java.base`
    /// exports nothing — so a *real* cross-module access would be denied. This
    /// lets the array / empty-package exemptions be tested against a registry
    /// that would otherwise reject.
    fn strict_registry() -> ModuleRegistry {
        let desc = |name: &str| ModuleDescriptor {
            name: name.to_string(),
            version: None,
            is_open: false,
            requires: vec![],
            exports: vec![],
            opens: vec![],
            uses: vec![],
            provides: vec![],
            automatic: false,
            main_class: None,
        };
        let mut reg = ModuleRegistry::new();
        reg.register(desc("java.base"), vec![]);
        reg.register(desc("java.xml"), vec![]);
        reg.build_readability_graph();
        reg.add_reads("java.xml", "java.base");
        reg
    }

    #[test]
    fn module_access_array_target_is_exempt() {
        // Regression: java.xml's `XMLSecurityManager` references `int[]`, which
        // CratonVM synthesises as `[I` under module `java.base` with an empty
        // package. The JPMS check must not deny access to an array class.
        let mut store = ClassStore::new();
        let accessor = make_class_in_module(
            &mut store,
            "jdk/xml/internal/XMLSecurityManager",
            Some("java.xml"),
        );
        let prim_arr = make_class_in_module(&mut store, "[I", Some("java.base"));
        let ref_arr = make_class_in_module(&mut store, "[Ljava/lang/Object;", Some("java.base"));
        let reg = strict_registry();
        assert!(check_module_access(
            store.get(accessor).unwrap(),
            store.get(prim_arr).unwrap(),
            &reg
        )
        .is_ok());
        assert!(check_module_access(
            store.get(accessor).unwrap(),
            store.get(ref_arr).unwrap(),
            &reg
        )
        .is_ok());
    }

    #[test]
    fn module_access_empty_package_named_module_is_exempt() {
        // A default-(empty-)package class labelled into a named module is a
        // CratonVM artifact (e.g. a synthetic/hidden class), not a real export
        // boundary — don't deny on it.
        let mut store = ClassStore::new();
        let accessor = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
        let target = make_class_in_module(&mut store, "DefaultPkgClass", Some("java.base"));
        let reg = strict_registry();
        assert!(check_module_access(
            store.get(accessor).unwrap(),
            store.get(target).unwrap(),
            &reg
        )
        .is_ok());
    }

    #[test]
    fn module_access_real_unexported_package_still_denied() {
        // Control: a genuine cross-module access to an un-exported package of a
        // named module is still denied — the exemptions above don't neuter the
        // check for real classes.
        let mut store = ClassStore::new();
        let accessor = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
        let target = make_class_in_module(&mut store, "java/lang/invoke/Hidden", Some("java.base"));
        let reg = strict_registry();
        assert!(check_module_access(
            store.get(accessor).unwrap(),
            store.get(target).unwrap(),
            &reg
        )
        .is_err());
    }

    // --- i11-L2: the JPMS export clause of a CONSTANT_Class resolution ---

    use crate::module::{ModuleExportsEntry, ALL_UNNAMED_TARGET};

    /// `java.base` exporting `java/lang` unqualified and `jdk/internal/access`
    /// to `java.xml` only; `java.xml` exporting nothing; `app.cp`, a module
    /// known only from a class-path jar; `java.open`, an open module.
    fn export_registry() -> ModuleRegistry {
        let desc =
            |name: &str, exports: Vec<ModuleExportsEntry>, is_open: bool, automatic: bool| {
                ModuleDescriptor {
                    name: name.to_string(),
                    version: None,
                    is_open,
                    requires: vec![],
                    exports,
                    opens: vec![],
                    uses: vec![],
                    provides: vec![],
                    automatic,
                    main_class: None,
                }
            };
        let export = |pkg: &str, to: &[&str]| ModuleExportsEntry {
            package_name: pkg.to_string(),
            to_modules: to.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        };
        let mut reg = ModuleRegistry::new();
        reg.register(
            desc(
                "java.base",
                vec![
                    export("java/lang", &[]),
                    export("jdk/internal/access", &["java.xml"]),
                ],
                false,
                false,
            ),
            vec![],
        );
        reg.register(desc("java.xml", vec![], false, false), vec![]);
        reg.register(desc("app.cp", vec![], false, true), vec![]);
        reg.register(desc("java.open", vec![], true, false), vec![]);
        reg.build_readability_graph();
        reg
    }

    #[test]
    fn the_export_clause_refuses_only_an_unexported_package_of_a_named_module() {
        let mut store = ClassStore::new();
        let unnamed = make_class_in_module(&mut store, "p/App", None);
        let class_path = make_class_in_module(&mut store, "q/Jar", Some("app.cp"));
        let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
        let unsafe_ =
            make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
        let access = make_class_in_module(
            &mut store,
            "jdk/internal/access/SharedSecrets",
            Some("java.base"),
        );
        let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
        let opened = make_class_in_module(&mut store, "o/Inside", Some("java.open"));
        let arr =
            make_class_in_module(&mut store, "[Ljdk/internal/misc/Unsafe;", Some("java.base"));
        let reg = export_registry();
        let c = |id| store.get(id).unwrap();
        let denied = |a, t| class_export_denial(c(a), c(t), &reg);

        // Unqualified export: everyone.
        for accessor in [unnamed, class_path, xml] {
            assert_eq!(denied(accessor, string), None);
        }
        // Not exported: an unnamed accessor (and a class-path-only module,
        // which HotSpot treats as unnamed) is refused but NOT named...
        for accessor in [unnamed, class_path] {
            let d = denied(accessor, unsafe_).expect("jdk.internal.misc is not exported");
            assert!(!d.accessor_named);
            assert!(
                d.message.ends_with(
                    "because module java.base does not export jdk.internal.misc to unnamed module"
                ),
                "{}",
                d.message
            );
        }
        // ...and a named accessor is refused and named, with HotSpot's text.
        let d = denied(xml, unsafe_).expect("not exported to java.xml either");
        assert!(d.accessor_named);
        assert_eq!(
            d.message,
            "class jdk.xml.internal.Foo (in module java.xml) cannot access class \
             jdk.internal.misc.Unsafe (in module java.base) because module java.base \
             does not export jdk.internal.misc to module java.xml"
        );
        // A qualified export reaches its target module only.
        assert_eq!(denied(xml, access), None);
        assert!(denied(unnamed, access).is_some());
        // Same module, an open module, the unnamed module and a
        // class-path-only module as targets, and an array name: nothing to
        // refuse.
        assert_eq!(denied(string, unsafe_), None);
        assert_eq!(denied(xml, opened), None);
        assert_eq!(denied(xml, unnamed), None);
        assert_eq!(denied(xml, class_path), None);
        assert_eq!(denied(unnamed, arr), None);
        // An empty registry decides nothing.
        assert_eq!(
            class_export_denial(c(xml), c(unsafe_), &ModuleRegistry::new()),
            None
        );
    }

    /// Interpreter round i1 wave 26, lane L5b: a class a USER-DEFINED loader
    /// defined in a JDK package is labelled with the platform module by
    /// package name, but it is a member of its loader's unnamed module, so no
    /// export clause applies to it — as target or as accessor. The bootstrap
    /// class of the same shape is still refused (control).
    #[test]
    fn a_user_loaders_class_in_a_jdk_package_is_not_a_platform_module_member() {
        let mut store = ClassStore::new();
        let unnamed = make_class_in_module(&mut store, "p/App", None);
        store.get_mut(unnamed).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let spoof =
            make_class_in_module(&mut store, "jdk/internal/misc/L5Helper", Some("java.base"));
        store.get_mut(spoof).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let boot =
            make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
        store.get_mut(boot).unwrap().loader_id = ClassLoaderId::Bootstrap;
        let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
        store.get_mut(xml).unwrap().loader_id = ClassLoaderId::Bootstrap;
        let reg = export_registry();
        let c = |id| store.get(id).unwrap();
        assert_eq!(class_export_denial(c(unnamed), c(spoof), &reg), None);
        assert!(
            class_export_denial(c(unnamed), c(boot), &reg).is_some(),
            "control: the bootstrap class is java.base's"
        );
        assert!(check_module_access(c(spoof), c(xml), &reg).is_ok());
        assert!(check_module_access(c(xml), c(spoof), &reg).is_ok());
    }

    #[test]
    fn a_dynamic_export_admits_its_target() {
        let mut store = ClassStore::new();
        let unnamed = make_class_in_module(&mut store, "p/App", None);
        let xml = make_class_in_module(&mut store, "jdk/xml/internal/Foo", Some("java.xml"));
        let unsafe_ =
            make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
        let mut reg = export_registry();
        // `--add-exports java.base/jdk.internal.misc=ALL-UNNAMED`: the
        // unnamed module only, not every named module.
        reg.add_exports("java.base", "jdk/internal/misc", ALL_UNNAMED_TARGET);
        let c = |id| store.get(id).unwrap();
        assert_eq!(class_export_denial(c(unnamed), c(unsafe_), &reg), None);
        assert!(class_export_denial(c(xml), c(unsafe_), &reg).is_some());
        // `Module.addExports(pn, java.xml)`.
        reg.add_exports("java.base", "jdk/internal/misc", "java.xml");
        assert_eq!(class_export_denial(c(xml), c(unsafe_), &reg), None);
    }

    #[test]
    fn a_layer_modules_exports_are_judged_by_its_identity() {
        use crate::module::LayerExportTarget;
        let mut store = ClassStore::new();
        // Two layers, loaders 7 and 8, each with a module `m` holding `p`
        // (exported) and `q`; only loader 7's `m` exports `q`.
        let mut reg = export_registry();
        let pkgs = vec!["p".to_string(), "q".to_string()];
        reg.define_layer_module(7, "m", false, &pkgs);
        reg.define_layer_module(8, "m", false, &pkgs);
        for ns in [7, 8] {
            reg.add_layer_export(ns, "m", "p", LayerExportTarget::Everyone);
        }
        reg.add_layer_export(7, "m", "q", LayerExportTarget::Everyone);
        // `exports r to n` within loader 7's layer.
        reg.define_layer_module(7, "n", false, &["s".to_string()]);
        reg.define_layer_module(7, "k", false, &["r".to_string()]);
        reg.add_layer_export(
            7,
            "k",
            "r",
            LayerExportTarget::Layer {
                loader_ns: 7,
                name: "n".to_string(),
            },
        );
        let mk = |store: &mut ClassStore, name: &str, loader: u32| {
            let id = make_class_in_module(store, name, None);
            store.get_mut(id).unwrap().loader_id = ClassLoaderId::UserDefined(loader);
            id
        };
        let q7 = mk(&mut store, "q/Other", 7);
        let q8 = mk(&mut store, "q/Other", 8);
        let p8 = mk(&mut store, "p/Hello", 8);
        let r7 = mk(&mut store, "r/Other", 7);
        let s7 = mk(&mut store, "s/Acc", 7);
        let child = mk(&mut store, "acc/Acc", 9);
        let c = |id| store.get(id).unwrap();
        assert_eq!(class_export_denial(c(child), c(q7), &reg), None);
        let denial = class_export_denial(c(child), c(q8), &reg).expect("q of loader 8 is not exported");
        assert_eq!(
            denial.message,
            "class acc.Acc (in unnamed module) cannot access class q.Other (in module m) \
             because module m does not export q to unnamed module"
        );
        assert_eq!(class_export_denial(c(child), c(p8), &reg), None);
        // The module's own class and a qualified target.
        assert_eq!(class_export_denial(c(p8), c(q8), &reg), None);
        assert_eq!(class_export_denial(c(s7), c(r7), &reg), None);
        let named = class_export_denial(c(q7), c(r7), &reg).expect("r is exported to n only");
        assert!(named.accessor_named);
        assert!(class_export_denial(c(child), c(r7), &reg).is_some());
        // A loader's unnamed module as the target.
        reg.add_layer_export(7, "k", "r", LayerExportTarget::Unnamed { loader_ns: 9 });
        assert_eq!(class_export_denial(c(child), c(r7), &reg), None);
        // An open module exports every package.
        reg.define_layer_module(8, "m", true, &pkgs);
        assert_eq!(class_export_denial(c(child), c(q8), &reg), None);
    }

    /// Interpreter round i1 wave 45, lane L5: a layer module's class as the
    /// ACCESSOR of a name-map module is in a named module, so an export to
    /// `ALL-UNNAMED` does not reach it; an unqualified export and a run-time
    /// export to the layer module's name do.
    #[test]
    fn a_layer_class_is_a_named_accessor_of_a_name_map_module() {
        let mut store = ClassStore::new();
        let mut reg = export_registry();
        reg.define_layer_module(7, "m", false, &["p".to_string()]);
        reg.add_exports("java.base", "jdk/internal/misc", "ALL-UNNAMED");
        let layer = make_class_in_module(&mut store, "p/Acc", None);
        store.get_mut(layer).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        // The same loader's class outside the module's packages is unnamed.
        let outside = make_class_in_module(&mut store, "o/Acc", None);
        store.get_mut(outside).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let unsafe_ =
            make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
        let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
        let access = make_class_in_module(
            &mut store,
            "jdk/internal/access/SharedSecrets",
            Some("java.base"),
        );
        let c = |id| store.get(id).unwrap();
        assert_eq!(class_export_denial(c(outside), c(unsafe_), &reg), None);
        let d = class_export_denial(c(layer), c(unsafe_), &reg).expect("ALL-UNNAMED is not m");
        assert!(d.accessor_named);
        assert_eq!(
            d.message,
            "class p.Acc (in module m) cannot access class jdk.internal.misc.Unsafe \
             (in module java.base) because module java.base does not export \
             jdk.internal.misc to module m"
        );
        assert_eq!(class_export_denial(c(layer), c(string), &reg), None);
        // A descriptor's qualified export names a module of its own
        // configuration, never a layer module.
        assert!(class_export_denial(c(layer), c(access), &reg).is_some());
        // A run-time export to the layer module's name reaches it.
        reg.add_exports("java.base", "jdk/internal/misc", "m");
        assert_eq!(class_export_denial(c(layer), c(unsafe_), &reg), None);
    }

    /// Interpreter round i1 wave 45, lane L5: a layer module's class must
    /// read the target's module (HotSpot's `MODULE_NOT_READABLE`), from the
    /// reads `addReads0` recorded; no recorded read decides nothing.
    #[test]
    fn a_layer_class_must_read_the_module_it_names() {
        use crate::module::LayerExportTarget;
        let mut store = ClassStore::new();
        let mut reg = export_registry();
        reg.define_layer_module(7, "m", false, &["p".to_string()]);
        reg.define_layer_module(7, "n", false, &["q".to_string()]);
        reg.add_layer_export(7, "n", "q", LayerExportTarget::Everyone);
        let acc = make_class_in_module(&mut store, "p/Acc", None);
        store.get_mut(acc).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let other = make_class_in_module(&mut store, "q/Other", None);
        store.get_mut(other).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let xml = make_class_in_module(&mut store, "javax/xml/X", Some("java.xml"));
        let string = make_class_in_module(&mut store, "java/lang/String", Some("java.base"));
        reg.add_exports("java.xml", "javax/xml", "");
        let c = |id| store.get(id).unwrap();
        // Nothing recorded: permissive.
        assert_eq!(class_export_denial(c(acc), c(xml), &reg), None);
        assert_eq!(class_export_denial(c(acc), c(other), &reg), None);
        reg.add_layer_read(7, "m", LayerExportTarget::Named("java.base".to_string()));
        assert_eq!(class_export_denial(c(acc), c(string), &reg), None);
        let d = class_export_denial(c(acc), c(xml), &reg).expect("m does not read java.xml");
        assert_eq!(
            d.message,
            "class p.Acc (in module m) cannot access class javax.xml.X (in module java.xml) \
             because module m does not read module java.xml"
        );
        let d = class_export_denial(c(acc), c(other), &reg).expect("m does not read n");
        assert!(d.message.ends_with("because module m does not read module n"), "{}", d.message);
        reg.add_layer_read(7, "m", LayerExportTarget::Named("java.xml".to_string()));
        reg.add_layer_read(
            7,
            "m",
            LayerExportTarget::Layer {
                loader_ns: 7,
                name: "n".to_string(),
            },
        );
        assert_eq!(class_export_denial(c(acc), c(xml), &reg), None);
        assert_eq!(class_export_denial(c(acc), c(other), &reg), None);
    }

    /// Interpreter round i1 wave 46, lane L5: the reflective gates' one
    /// module question answers a layer accessor by its module's identity
    /// and everyone else from the name map.
    #[test]
    fn the_reflective_module_question_is_asked_by_identity() {
        use super::ReflectiveModuleQuestion::{Export, Open};
        let mut store = ClassStore::new();
        let mut reg = export_registry();
        reg.define_layer_module(7, "m", false, &["p".to_string()]);
        reg.add_exports("java.base", "jdk/internal/misc", "ALL-UNNAMED");
        let layer = make_class_in_module(&mut store, "p/Acc", None);
        store.get_mut(layer).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let outside = make_class_in_module(&mut store, "o/Acc", None);
        store.get_mut(outside).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let unsafe_ =
            make_class_in_module(&mut store, "jdk/internal/misc/Unsafe", Some("java.base"));
        let plain = make_class_in_module(&mut store, "q/Plain", None);
        let c = |id| store.get(id).unwrap();
        assert_eq!(reflective_module_access(c(outside), c(unsafe_), &reg, Export), Ok(()));
        assert_eq!(
            reflective_module_access(c(layer), c(unsafe_), &reg, Export),
            Err("module java.base does not \"exports jdk.internal.misc\" to module m".to_string())
        );
        assert_eq!(
            reflective_module_access(c(layer), c(unsafe_), &reg, Open),
            Err("module java.base does not \"opens jdk.internal.misc\" to module m".to_string())
        );
        // An unnamed target is open to a layer accessor.
        assert_eq!(reflective_module_access(c(layer), c(plain), &reg, Open), Ok(()));
        // No module registered: class-path-only mode admits everything.
        let empty = ModuleRegistry::new();
        assert_eq!(reflective_module_access(c(layer), c(unsafe_), &empty, Open), Ok(()));
    }

    /// Interpreter round i1 wave 46, lane L5: a layer module's class reads
    /// an UNNAMED module only through an `ALL-UNNAMED` edge or that loader's
    /// unnamed module; a JDK name in an unnamed module is not asked.
    #[test]
    fn a_layer_class_must_read_the_unnamed_module_it_names() {
        use crate::module::LayerExportTarget;
        let mut store = ClassStore::new();
        let mut reg = export_registry();
        reg.define_layer_module(7, "m", false, &["p".to_string()]);
        let acc = make_class_in_module(&mut store, "p/Acc", None);
        store.get_mut(acc).unwrap().loader_id = ClassLoaderId::UserDefined(7);
        let app = make_class_in_module(&mut store, "Target", None);
        store.get_mut(app).unwrap().loader_id = ClassLoaderId::Application;
        let user = make_class_in_module(&mut store, "u/Other", None);
        store.get_mut(user).unwrap().loader_id = ClassLoaderId::UserDefined(9);
        let jdk = make_class_in_module(&mut store, "jdk/internal/Gen", None);
        store.get_mut(jdk).unwrap().loader_id = ClassLoaderId::Application;
        let c = |id| store.get(id).unwrap();
        // Nothing recorded: permissive.
        assert_eq!(class_export_denial(c(acc), c(app), &reg), None);
        reg.add_layer_read(7, "m", LayerExportTarget::Named("java.base".to_string()));
        let d = class_export_denial(c(acc), c(app), &reg).expect("m does not read unnamed");
        assert_eq!(
            d.message,
            "class p.Acc (in module m) cannot access class Target (in unnamed module) \
             because module m does not read unnamed module"
        );
        assert!(class_export_denial(c(acc), c(user), &reg).is_some());
        assert_eq!(class_export_denial(c(acc), c(jdk), &reg), None);
        // One loader's unnamed module.
        reg.add_layer_read(7, "m", LayerExportTarget::Unnamed { loader_ns: 2 });
        assert_eq!(class_export_denial(c(acc), c(app), &reg), None);
        assert!(class_export_denial(c(acc), c(user), &reg).is_some());
        // Every unnamed module.
        reg.add_layer_read(7, "m", LayerExportTarget::AllUnnamed);
        assert_eq!(class_export_denial(c(acc), c(user), &reg), None);
    }
}
