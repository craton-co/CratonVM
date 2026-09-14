// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! INHERITANCE-INTERCEPTION GATE — a registration-time refusal for the
//! "a native attached to a base class silently answers for every subclass"
//! defect family.
//!
//! ## Why a gate is possible at all
//!
//! `NativeMethodRegistry::find` is an EXACT `(class, method, descriptor)`
//! lookup — it does no hierarchy walk of its own. The interception comes from
//! the *caller*: `try_stackless_invoke`
//! (`vm/src/runtime/interpreter/invoke.rs`) passes the RECEIVER's class name,
//! and when that class does not declare the method itself it climbs the
//! receiver's **superclass** chain. At each ancestor it asks the registry for
//! a native **before** it asks whether that ancestor has bytecode:
//!
//! ```text
//!     let parent = cm.get_class(parent_id)?;
//!     let has_bytecode = parent.find_method(method_name, descriptor).is_some();
//!     if let Some(cb) = registry.find(&parent.name, method_name, descriptor) {
//!         return Some(cb);          // <-- native wins over the parent's bytecode
//!     }
//!     if has_bytecode { return None; }
//! ```
//!
//! Two consequences, and both are what this file encodes:
//!
//! 1. A native on an **abstract class** (or any class that is always
//!    subclassed) is inherited by every subclass that does not override the
//!    method — including third-party and user subclasses the shim author never
//!    saw — and it **wins over that base class's real bytecode**. In the
//!    default (`Compatible`) dispatch mode a registered native wins
//!    unconditionally; see `resolve_step1_native`, whose `compat_native_wins`
//!    argument is a hard-coded `true`.
//! 2. A native on an **interface** is NOT inherited this way — the walk climbs
//!    `superclass` only. It fires solely for a receiver whose runtime class IS
//!    the interface, i.e. a synthetic stand-in minted by
//!    `ensure_synthetic_class`. That is a much smaller blast radius, so
//!    interfaces are deliberately out of this gate's scope.
//!
//! ## What the gate refuses
//!
//! Registrations of methods with **observable identity / equality / rendering
//! semantics** (`equals`, `hashCode`, `toString`, `compareTo`, `clone`) on a
//! curated set of inheritance-intercepting base classes. These are the ones
//! where "wins over bytecode and is wrong" is not a cosmetic bug: an `equals`
//! that answers by identity breaks every collection the object is put in, and a
//! `hashCode` derived from a raw heap address changes under a moving young
//! collection.
//!
//! Adding such a registration now fails CI. Removing one only requires deleting
//! its allowlist row. See `native-builtins-shim-audit.md`
//! for the full risk-ordered census, including the base classes deliberately
//! left out of the automated gate and why.
//!
//! Scope note: this gate can only see natives registered by **this crate**
//! (`register_essential_natives` / `register_synthetic_overrides`).
//! `native-collections` and `native-io` register into the same registry at VM
//! boot and have their own instances of this defect family — those are recorded
//! in the audit doc as cross-crate findings, not gated here.

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_native_builtins::register_essential_natives;

/// Base classes where a native registration is inherited by every subclass that
/// does not override the method, AND where this crate's registration set has
/// been fully enumerated (so the allowlist below is exhaustive rather than
/// hopeful).
///
/// Deliberately EXCLUDED, each for a stated reason — they are in the audit
/// doc's census table with an explicit verdict instead:
///
/// * `java/lang/Object` — universal base. Its `equals`/`hashCode`/`toString`
///   natives are the CORRECT fallback that the refusals in this sweep rely on
///   (`Object.hashCode` -> `ctx.identity_hash_code`, stable across relocation).
///   Gating it would forbid the fix.
/// * `java/lang/Throwable`, `java/lang/AbstractStringBuilder`,
///   `java/net/InetAddress`, `java/security/Provider`, `java/lang/ClassLoader`,
///   `javax/net/ssl/*` — these are registered through loops over a class-name
///   list rather than a literal, so a purely static enumeration of their rows
///   is not trustworthy enough to freeze. Listing them here would risk a
///   false CI failure, which is worse than no gate.
const INHERITANCE_INTERCEPTING_BASES: &[&str] = &[
    "java/util/AbstractMap",
    "java/util/AbstractList",
    "java/util/AbstractSet",
    "java/util/AbstractCollection",
    "java/util/AbstractSequentialList",
    "java/util/AbstractQueue",
    "java/lang/Enum",
    "java/lang/Number",
    "java/lang/Record",
    "java/nio/Buffer",
    "java/nio/ByteBuffer",
    "java/nio/CharBuffer",
    "java/util/concurrent/AbstractExecutorService",
    "java/util/concurrent/locks/AbstractOwnableSynchronizer",
    "java/util/concurrent/locks/AbstractQueuedSynchronizer",
    "java/util/concurrent/locks/AbstractQueuedLongSynchronizer",
    "java/util/prefs/AbstractPreferences",
    "java/io/FilterInputStream",
    "java/io/FilterOutputStream",
    "java/io/FilterReader",
    "java/io/FilterWriter",
];

/// Methods whose answer is *observable identity or ordering*, i.e. the ones
/// where a subclass inheriting a base-class shim produces a semantically wrong
/// program rather than a cosmetically odd one.
const IDENTITY_METHODS: &[&str] = &["equals", "hashCode", "toString", "compareTo", "clone"];

/// `(class, method)` pairs allowed to exist despite the rule above, each with
/// the reason it is safe. Keyed WITHOUT the descriptor on purpose: a descriptor
/// typo should not be able to smuggle a new shim past the gate.
///
/// Every row here was read and judged during the 2026-08-01 shim audit.
const ALLOWLIST: &[(&str, &str, &str)] = &[
    // `Enum` is abstract and every user enum inherits these, but the JDK's own
    // `Enum.equals`/`hashCode` ARE identity (they are `final` in the real JDK
    // precisely so nobody can change that), so answering by identity is the
    // specified behaviour rather than a guess. `toString` returns the `name`
    // field and `compareTo` the ordinal difference — both read real state.
    (
        "java/lang/Enum",
        "toString",
        "reads the real `name` field; matches JDK",
    ),
    (
        "java/lang/Enum",
        "equals",
        "JDK `Enum.equals` is final identity",
    ),
    (
        "java/lang/Enum",
        "hashCode",
        "JDK `Enum.hashCode` is final identity",
    ),
    (
        "java/lang/Enum",
        "compareTo",
        "ordinal difference, reads real state",
    ),
    // `Record` is abstract and its `equals`/`hashCode`/`toString` are the
    // component-wise implementations the JVM is REQUIRED to synthesise
    // (JLS 8.10.3) — there is no bytecode on `java.lang.Record` to shadow,
    // the real JDK leaves them abstract.
    (
        "java/lang/Record",
        "equals",
        "component-wise; Record's own are abstract",
    ),
    (
        "java/lang/Record",
        "hashCode",
        "component-wise; Record's own are abstract",
    ),
    (
        "java/lang/Record",
        "toString",
        "component-wise; Record's own are abstract",
    ),
    // `ByteBuffer` is abstract; these read the receiver's actual storage
    // (heap array or native window) via `s2_bb_read_window`, so they answer
    // for a `DirectByteBuffer` receiver as correctly as for a heap one.
    // `hashCode` iterates backward over `[position, limit)` to match
    // `Buffer.hashCode` exactly.
    (
        "java/nio/ByteBuffer",
        "equals",
        "content-compare over the real storage",
    ),
    (
        "java/nio/ByteBuffer",
        "hashCode",
        "backward content hash, matches JDK",
    ),
    (
        "java/nio/ByteBuffer",
        "compareTo",
        "content compare over the real storage",
    ),
    (
        "java/nio/ByteBuffer",
        "toString",
        "renders the RECEIVER's class name",
    ),
    // `CharBuffer` is abstract; `toString` renders the remaining chars from the
    // receiver's own backing store.
    (
        "java/nio/CharBuffer",
        "toString",
        "remaining chars from real storage",
    ),
    // `AbstractMap.toString` renders `{size=N}` where N comes from a VIRTUAL
    // `size()` call on the receiver. It is not the JDK's `{k=v, …}` rendering
    // and is recorded as a residual in the audit doc — but it reads real state
    // and `vm/src/vm.rs` has a test pinned to it, so it is not changed here.
    (
        "java/util/AbstractMap",
        "toString",
        "residual: `{size=N}`, see audit doc",
    ),
    // `AbstractPreferences.toString` is now the JDK's own definition —
    // `(isUserNode() ? "User" : "System") + " Preference Node: " +
    // absolutePath()` — composed through VIRTUAL calls, so a subclass that
    // overrides either accessor is rendered with ITS answer. It reads no slot
    // index of its own, which is what makes inheriting it correct rather than
    // merely tolerated. `abstract_preferences_to_string_composes_from_virtual_accessors`
    // below pins that property.
    (
        "java/util/prefs/AbstractPreferences",
        "toString",
        "JDK's own formula via virtual isUserNode()/absolutePath()",
    ),
];

fn production_registry() -> NativeMethodRegistry {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    registry
}

fn allowlisted(class: &str, method: &str) -> bool {
    ALLOWLIST
        .iter()
        .any(|(c, m, _)| *c == class && *m == method)
}

fn assert_no_unlisted_identity_shims(registry: &NativeMethodRegistry, which: &str) {
    let mut offenders: Vec<String> = Vec::new();
    for (class, method, descriptor, kind) in registry.dump_registrations() {
        if !INHERITANCE_INTERCEPTING_BASES.contains(&class) {
            continue;
        }
        if !IDENTITY_METHODS.contains(&method) {
            continue;
        }
        if allowlisted(class, method) {
            continue;
        }
        offenders.push(format!("  {class}.{method}{descriptor}  [{kind:?}]"));
    }
    offenders.sort();
    offenders.dedup();
    assert!(
        offenders.is_empty(),
        "{which}: {} native(s) with identity/equality semantics are registered on a base \n\
         class whose subclasses INHERIT the interception, and are not allowlisted:\n{}\n\n\
         The interpreter's native-override hierarchy walk asks for a native on each \n\
         ancestor BEFORE it checks whether that ancestor has bytecode, so each of these \n\
         answers for every subclass that does not override the method — and beats the \n\
         base class's real implementation when there is one.\n\n\
         Fix the shim so it reads the receiver's own state, or DELETE the registration so \n\
         real bytecode (or `java/lang/Object`'s stable identity natives) runs. Only add an \n\
         ALLOWLIST row in `native-builtins/tests/shim_inheritance_guard.rs` once you have \n\
         written down why the shim is right for every subclass. See \n\
         native-builtins-shim-audit.md.",
        offenders.len(),
        offenders.join("\n"),
    );
}

/// THE GATE (default / real-JDK registry).
#[test]
fn default_registry_has_no_unlisted_identity_shim_on_an_intercepting_base() {
    assert_no_unlisted_identity_shims(&production_registry(), "default registry");
}

/// THE GATE (synthetic-JDK registry). `register_builtins` is
/// `#[cfg(feature = "synthetic-jdk")]`, so this arm only exists in that build —
/// which is exactly the build where the base-class shims are densest.
#[cfg(feature = "synthetic-jdk")]
#[test]
fn synthetic_registry_has_no_unlisted_identity_shim_on_an_intercepting_base() {
    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_builtins(&mut registry);
    assert_no_unlisted_identity_shims(&registry, "synthetic-jdk registry");
}

/// The strictest form of the rule, for the `java.util` abstract collection
/// bases: this crate registers **nothing at all** on them, so a user
/// `class X extends AbstractList` inherits only real bytecode.
///
/// `java/util/AbstractMap` is excluded because it legitimately carries
/// `isEmpty`/`containsKey`/`containsValue`/`toString` (see the next test).
#[test]
fn the_java_util_abstract_collection_bases_carry_no_natives_at_all() {
    let registry = production_registry();
    let bases = [
        "java/util/AbstractList",
        "java/util/AbstractSet",
        "java/util/AbstractCollection",
        "java/util/AbstractSequentialList",
        "java/util/AbstractQueue",
    ];
    let rows: Vec<String> = registry
        .dump_registrations()
        .into_iter()
        .filter(|(class, _, _, _)| bases.contains(class))
        .map(|(c, m, d, _)| format!("  {c}.{m}{d}"))
        .collect();
    assert!(
        rows.is_empty(),
        "no native may be attached to a java.util abstract collection base — every \n\
         subclass that does not override the method inherits it, including user \n\
         subclasses. Found:\n{}",
        rows.join("\n"),
    );
}

/// REGRESSION (fails before the 2026-08-01 fix).
///
/// `java/util/AbstractMap.equals`/`hashCode` used to be registered as pure
/// identity shims — `hashCode` returned `this.as_ptr() as i32`, a RAW HEAP
/// ADDRESS that changes under a moving young collection. `AbstractMap` is an
/// abstract class and neither `HashMap`, `TreeMap`, `LinkedHashMap`,
/// `Collections$UnmodifiableMap` nor any user `extends AbstractMap` declares
/// `equals`/`hashCode`, so every map in the VM inherited them.
///
/// They are now absent, which means: real `AbstractMap` bytecode runs when it
/// is loaded, and a bare synthetic stub falls through to `java/lang/Object`'s
/// natives — the same identity answer, minus the moving-GC instability.
///
/// The four registrations that DO belong on `AbstractMap` must survive, so a
/// future "just delete the whole registrar" does not pass this by accident.
#[cfg(feature = "synthetic-jdk")]
#[test]
fn abstract_map_refuses_equals_and_hash_code_but_keeps_its_state_readers() {
    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_builtins(&mut registry);
    let am = "java/util/AbstractMap";

    assert!(
        registry
            .find(am, "equals", "(Ljava/lang/Object;)Z")
            .is_none(),
        "AbstractMap.equals must NOT be shimmed: the real implementation is entry-wise, \
         and an identity answer is inherited by every Map that does not override equals"
    );
    assert!(
        registry.find(am, "hashCode", "()I").is_none(),
        "AbstractMap.hashCode must NOT be shimmed: the real implementation is the sum of \
         entry hashes, and the shim it replaced returned a raw heap address that changes \
         under a moving collection"
    );

    for (method, descriptor) in [
        ("isEmpty", "()Z"),
        ("containsKey", "(Ljava/lang/Object;)Z"),
        ("containsValue", "(Ljava/lang/Object;)Z"),
        ("toString", "()Ljava/lang/String;"),
    ] {
        assert!(
            registry.find(am, method, descriptor).is_some(),
            "AbstractMap.{method}{descriptor} is still needed for synthetic maps"
        );
    }
}

/// The allowlist row for `AbstractPreferences.toString` claims it composes
/// from the receiver's own accessors rather than from a slot index. That claim
/// is the entire justification for letting an identity-semantics native sit on
/// an inheritance-intercepting base, so pin the two accessors it composes from:
/// if a future edit deletes `isUserNode` or `absolutePath`, the shim silently
/// goes back to rendering something it invented, and the allowlist row would
/// still say otherwise.
#[cfg(feature = "synthetic-jdk")]
#[test]
fn abstract_preferences_to_string_composes_from_virtual_accessors() {
    let mut registry = NativeMethodRegistry::new();
    cratonvm_native_builtins::register_builtins(&mut registry);
    for cls in [
        "java/util/prefs/Preferences",
        "java/util/prefs/AbstractPreferences",
    ] {
        assert!(
            registry.find(cls, "isUserNode", "()Z").is_some(),
            "{cls}.toString renders the User/System half from a virtual \
             isUserNode() call — it must resolve"
        );
        assert!(
            registry
                .find(cls, "absolutePath", "()Ljava/lang/String;")
                .is_some(),
            "{cls}.toString renders the path half from a virtual absolutePath() \
             call — it must resolve"
        );
    }
}

/// The refusal above is only safe because `java/lang/Object` supplies a
/// *stable* identity fallback: `native_object_hash_code` goes through
/// `ctx.identity_hash_code`, which survives relocation, unlike the raw
/// `as_ptr()` cast it replaces. Pin that the fallback exists.
#[test]
fn object_supplies_the_identity_fallback_the_refusals_depend_on() {
    let registry = production_registry();
    assert!(
        registry
            .find("java/lang/Object", "hashCode", "()I")
            .is_some(),
        "the AbstractMap refusal falls through to Object.hashCode — it must be registered"
    );
    assert!(
        registry
            .find("java/lang/Object", "equals", "(Ljava/lang/Object;)Z")
            .is_some(),
        "the AbstractMap refusal falls through to Object.equals — it must be registered"
    );
}
