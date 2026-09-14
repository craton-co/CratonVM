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
//! | [`check_class_access`] | `vm/src/runtime/interpreter.rs` -- `new` only |
//! | [`check_module_access_by_id`] | field + method resolution (JPMS only) |
//! | [`check_field_access`] | **none** |
//! | [`check_method_access`] | **none** |
//! | [`check_class_access_with_modules`] | **none** |
//! | [`check_field_access_with_modules`] | **none** |
//! | [`check_method_access_with_modules`] | **none** |
//! | [`are_nestmates`] | none (reachable only via the two dead member checks) |
//!
//! Consequence: at `getfield` / `putfield` / `getstatic` / `putstatic` /
//! `invoke*`, a hand-written class file that names another class's `private`
//! or package-private member resolves and executes. The verifier does not
//! compensate -- `IllegalAccessError` is constructed nowhere outside this
//! module. Class-level access is enforced only for `new`, not for `checkcast`,
//! `instanceof`, `ldc`, `anewarray`, or the owner class of a field/method ref.
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
use crate::module::{package_of as module_pkg_of, ModuleRegistry, UNNAMED_MODULE};
use cratonvm_types::error::LinkageError;

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
    Err(LinkageError::IllegalAccessError {
        message: format!(
            "class {} cannot access class {} (not public, different package)",
            accessor.name, target.name
        ),
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
#[inline]
pub fn are_nestmates(a: &Class, b: &Class, store: &ClassStore) -> bool {
    if a.id == b.id {
        return true;
    }
    let host_a = confirmed_nest_host(a, store);
    let host_b = confirmed_nest_host(b, store);
    host_a == host_b
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
fn confirmed_nest_host<'a>(class: &'a Class, store: &ClassStore) -> &'a str {
    match class.nest_host.as_deref() {
        // A class that names itself as its NestHost is its own host.
        Some(host) if host == &*class.name => &class.name,
        // JEP 371 hidden class: the declared host is authoritative by
        // construction (see the doc comment above). No `NestMembers`
        // round-trip is possible or required.
        Some(host) if class.hidden => host,
        Some(host) => {
            // The host must exist and must explicitly list this class as a
            // member. Otherwise the NestHost claim is unconfirmed (spoofed).
            match store.find_by_name(host) {
                Some(host_class) if host_class.nest_members.iter().any(|m| m == &*class.name) => {
                    // Confirmed: return the claiming class's view of the
                    // host name (byte-identical to the resolved host, and
                    // borrowed from `class` so it satisfies the `'a`
                    // lifetime without tying it to `store`).
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
    a.loader_id == b.loader_id && same_package_name(&a.name, &b.name)
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

    let accessor_mod = accessor.module_name.as_deref().unwrap_or(UNNAMED_MODULE);
    let target_mod = target.module_name.as_deref().unwrap_or(UNNAMED_MODULE);

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
}
