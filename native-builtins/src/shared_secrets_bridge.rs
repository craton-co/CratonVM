// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.4 — `SharedSecrets` bridge natives.
//!
//! Implements the `jdk/internal/access/SharedSecrets.getJavaXxxAccess()`
//! family plus the individual interface method natives that each
//! singleton exposes.  See `vm/src/runtime/shared_secrets.rs` for the
//! architectural overview and the canonical interface ↔ owner-class
//! mapping.
//!
//! Every factory call returns a process-wide singleton of the
//! owner class.  Because interface dispatch resolves through the
//! receiver's concrete class, an invokeinterface against the
//! returned object lands on the native we registered on the owner
//! class, never on a missing Java field / bytecode path.
//!
//! # Thickness budget
//!
//! Most bridges are thin (forward to an existing VM capability or
//! return a spec-compatible default):
//!
//! * `JavaLangRefAccess.*`, `JavaNetHttpCookieAccess.parseCookie`,
//!   `JavaIOAccess.console` / `charset`, `JavaNetUriAccess.create` —
//!   ~10 lines of code each.
//!
//!   `parseCookie` is **not a JDK method name**; the real interface declares
//!   `parse(String)` and `header(HttpCookie)`. See `register_factories` for the
//!   measured registered-vs-declared table across the four fabricated carriers,
//!   and for why `--jdk-only` now refuses their factories rather than handing
//!   back an object no `invokeinterface` can hit.
//!
//! Medium-thickness:
//!
//! * `JavaLangAccess.currentCarrierThread`, `blockedOn`,
//!   `newStringUtf8NoRepl`, `getBytesUtf8NoRepl`,
//!   `newStackTraceElement` — route to existing natives the VM
//!   already ships (Thread.currentThread, String constructor, etc.).
//! * `JavaNioAccess.newDirectByteBuffer` — allocates a DirectBuffer
//!   via the existing Panama path.
//! * `JavaUtilZipFileAccess.getEntry` — uses the existing
//!   `ZipFile` native ops surface.
//!
//! Thickest (reflective invoke or new functionality):
//!
//! * `JavaLangAccess.getDeclaredPublicMethods` —
//!   reflects `Class` declared methods and filters for ACC_PUBLIC.
//! * `JavaLangReflectAccess.copyMethod` / `copyField` /
//!   `copyConstructor` — delegate to existing lang_class natives.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError};
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::{obj_arg, try_alloc_concurrent_synthetic};

/// WP1.4 — Re-export of the canonical concrete-class mapping.
/// Mirrors `vm/src/runtime/shared_secrets.rs::SharedSecretsInterface`
/// (we can't `use` it here because native-builtins does not depend
/// on vm). The mirror is NOT exact and must not be made exact: this
/// table has `java/io/FileDescriptor$1`, that one additionally has
/// `java/io/ObjectInputStream$1`, whose getter is deliberately not
/// intercepted. F24-1 (2026-08-13) added the check that keeps the
/// difference to exactly that — `owner_classes_mirror_the_native_builtins_bridge`,
/// a vm-crate unit test reading `owner_classes()` — after the claim
/// that used to sit here ("a compile-time `#[test]` in the vm crate
/// asserts the two lists stay in sync") turned out to describe a
/// test nobody had written, while the two lists disagreed in both
/// directions at equal length. Applied by F33-1, which owns this
/// file; F24-1 could only nominate it.
const FACTORIES: &[(&str, &str, &str)] = &[
    // (factory_method, return_type_descriptor, owner_class)
    (
        "getJavaLangAccess",
        "Ljdk/internal/access/JavaLangAccess;",
        "java/lang/System$1",
    ),
    (
        "getJavaLangInvokeAccess",
        "Ljdk/internal/access/JavaLangInvokeAccess;",
        "java/lang/invoke/MethodHandleImpl$1",
    ),
    (
        "getJavaLangRefAccess",
        "Ljdk/internal/access/JavaLangRefAccess;",
        "java/lang/ref/Reference$1",
    ),
    (
        "getJavaLangReflectAccess",
        "Ljdk/internal/access/JavaLangReflectAccess;",
        "java/lang/reflect/ReflectAccess",
    ),
    (
        "getJavaIOAccess",
        "Ljdk/internal/access/JavaIOAccess;",
        "java/io/Console$1",
    ),
    (
        "getJavaIORandomAccessFileAccess",
        "Ljdk/internal/access/JavaIORandomAccessFileAccess;",
        "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
    ),
    (
        "getJavaIOFileDescriptorAccess",
        "Ljdk/internal/access/JavaIOFileDescriptorAccess;",
        "java/io/FileDescriptor$1",
    ),
    (
        "getJavaNetInetAddressAccess",
        "Ljdk/internal/access/JavaNetInetAddressAccess;",
        "java/net/InetAddress$1",
    ),
    (
        "getJavaNetUriAccess",
        "Ljdk/internal/access/JavaNetUriAccess;",
        "cratonvm/internal/ss/JavaNetUriAccess$1",
    ),
    (
        "getJavaNioAccess",
        "Ljdk/internal/access/JavaNioAccess;",
        "java/nio/Buffer$2",
    ),
    // F17-1 (2026-08-13): `getJavaSecurityAccess` USED TO BE HERE AND IS GONE.
    // Do not re-add it. JEP 486 removed the Security Manager and took
    // `jdk.internal.access.JavaSecurityAccess` with it, so there is no
    // differently-spelled equivalent to correct this to — the whole interface is
    // absent, not just the getter. Measured on Microsoft 25.0.3+9-LTS:
    //
    //     $ javap jdk.internal.access.JavaSecurityAccess
    //     Error: class not found: jdk.internal.access.JavaSecurityAccess
    //     $ javap jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess
    //     0
    //
    // `getJavaxSecurityAccess()Ljdk/internal/access/JavaxSecurityAccess;` IS on
    // the JDK 25 surface and looks like a near-miss for it in a name-keyed
    // search. It is a different interface (`javax.security.auth.Subject`
    // plumbing), not a rename of this one — see
    // scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv, which lists
    // both `getJavaxSecurityAccess` and `setJavaxSecurityAccess` and neither
    // spelling of the `Java`-prefixed pair.
    //
    // Nothing dispatched to the deleted triple: no `.java` in this tree names
    // `SharedSecrets.getJavaSecurityAccess`, and no JDK 25 bytecode can call a
    // method its own `SharedSecrets` does not declare, so `call_native` had no
    // reachable path to it in any mode. F17-1 nonetheless KEPT
    // `register_java_security_access`, which registered two natives on the
    // fabricated `java/security/AccessController$1`; F33-1 (2026-08-13) deleted
    // those too, after re-verifying that nothing mints or names that receiver.
    // See the `// JavaSecurityAccess — DELETED` note further down for the three
    // closed doors and for what had to move in the same commit.
    //
    // `javaUtilJarAccess` — NOT `getJavaUtilJarAccess`. This one is a spelling
    // correction, not a deletion: the interface exists and the descriptor was
    // already right, but the real method has never carried the `get` prefix.
    //
    //     $ javap jdk.internal.access.SharedSecrets | grep JarAccess
    //       public static ...JavaUtilJarAccess javaUtilJarAccess();
    //       (and setJavaUtilJarAccess(JavaUtilJarAccess) — the setter DOES have
    //        the prefix, which is how the getter's spelling got invented)
    //
    // `getJavaUtilJarAccess` was measured dead everywhere before this change
    // (scripts/baselines/jdk-only-dead-everywhere.tsv:192, bucket
    // `method-nowhere`) precisely because no caller could ever name it.
    (
        "javaUtilJarAccess",
        "Ljdk/internal/access/JavaUtilJarAccess;",
        "cratonvm/internal/ss/JavaUtilJarAccess$1",
    ),
    (
        "getJavaUtilZipFileAccess",
        "Ljdk/internal/access/JavaUtilZipFileAccess;",
        "java/util/zip/ZipFile$1",
    ),
    (
        "getJavaNetHttpCookieAccess",
        "Ljdk/internal/access/JavaNetHttpCookieAccess;",
        "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
    ),
    // NOTE: getJavaObjectInputStreamAccess is intentionally NOT intercepted.
    // In JDK 25 ObjectInputStream.<clinit> creates the access object via an
    // `invokedynamic` (a `ObjectInputStream::checkArray` method-ref lambda) and
    // stores it through the real SharedSecrets.setJavaObjectInputStreamAccess —
    // there is no `ObjectInputStream$1` anonymous class. A synthetic singleton
    // of a non-existent owner class fails invokeinterface resolution (the
    // dispatch lands on the abstract `JavaObjectInputStreamAccess.checkArray`,
    // raising AbstractMethodError "has no Code attribute"). Letting the real
    // getter return the real lambda runs ObjectInputStream.checkArray correctly
    // (needed by HashSet/HashMap/etc. readObject, e.g. the Gradle test worker).
    (
        "getJavaUtilResourceBundleAccess",
        "Ljdk/internal/access/JavaUtilResourceBundleAccess;",
        "java/util/ResourceBundle$1",
    ),
];

/// Registry of interface owner-class names.  Used to iterate all
/// singletons from tests and from the VM-boot hook that
/// pre-allocates the synthetic stubs so `SharedSecrets.getXxx()`
/// can return them without a bytecode `<clinit>` pass.
pub fn owner_classes() -> impl Iterator<Item = &'static str> {
    FACTORIES.iter().map(|(_, _, owner)| *owner)
}

/// `(factory_method, owner_class)` for every row of `FACTORIES`.
///
/// F33-1 (2026-08-13). [`owner_classes`] projects away the half that the
/// `--jdk-only` pairing invariant is about: a factory and the owner it returns
/// must be admitted or refused TOGETHER, and with only the owner column an
/// out-of-crate check can see the owner's fate but not the factory's. The
/// integration ratchet
/// (`no_shared_secrets_factory_outlives_the_owner_strict_mode_drops` in
/// `native-builtins/tests/stub_ratchet.rs`) reads this instead of restating
/// fourteen name/owner pairs it would then have to keep in sync — a second copy
/// of this table is precisely how the two `SharedSecrets` lists drifted while
/// every length check on both sides passed (F24-1).
///
/// The return descriptor is deliberately not exposed: it is derivable
/// (`format!("(){ret}")`) and no consumer outside this crate needs it yet.
pub fn factory_methods_and_owners() -> impl Iterator<Item = (&'static str, &'static str)> {
    FACTORIES.iter().map(|(method, _, owner)| (*method, *owner))
}

/// Allocate a fresh singleton of `owner_class`.
///
/// We intentionally allocate fresh every call — every registered
/// native on an owner class is object-identity-agnostic (reads
/// nothing from instance fields), so skipping the cache keeps the
/// implementation simple and avoids heap-GC interactions.  If a
/// future bridge decides to stash state on the singleton, it
/// should move to a per-(VM, interface) cache.
///
/// # The `Err` arm returns a WRONG-CLASS receiver, not an error
///
/// F33-1 (2026-08-13) corrects the claim that used to sit on that arm — *"the
/// invokeinterface resolution still routes through the stored class name in the
/// native registry"*. That is a premise, and `--jdk-only` invalidates it: the
/// registry is the thing strict mode edits. `NativeMethodRegistry::register`
/// re-tags by RECEIVER CLASS
/// (`native-api/src/no_image_receiver.rs`), so under `--jdk-only` a stand-in
/// owner has NO registered methods left to route to, and the `ClassId(0)` object
/// this arm hands back is typed as a `Java*Access` while being a class nobody
/// asked for. The failure is silent — a wrong-class receiver, not a throw —
/// which is why it survived unnoticed.
///
/// The premise is restored for the `SharedSecrets` factories by
/// `register_factories` below, which now refuses a factory in exactly the modes
/// that refuse its owner, so this arm is unreachable for those owners in strict
/// mode. It is NOT restored for the other two callers
/// (`jdk/internal/reflect/ReflectionFactory` and
/// `java/lang/management/BufferPoolMXBean`), which still take it deliberately;
/// both are real JDK class names, so `ensure_class_initialized` is expected to
/// succeed and the `Err` arm is the not-loadable fallback. **Do not read this
/// function as infallible-and-fine.** See
/// docs/known-issues/jdk-only/F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md
/// # …and the `Err` arm is REACHED, in compatible mode, today
///
/// MEASURED 2026-08-21, `JarAccessProbe` on the real-JDK path, no flags:
///
/// ```text
///   HotSpot   javaUtilJarAccess() -> java.util.jar.JavaUtilJarAccessImpl
///   CratonVM  javaUtilJarAccess() -> cratonvm.synthetic.AnonymousObject$1
/// ```
///
/// `cratonvm/internal/ss/JavaUtilJarAccess$1` is a name CratonVM invents; no
/// image declares it, so `ensure_class_initialized` cannot resolve it and every
/// call took the `ClassId(0)` arm — which the allocator renders as
/// `cratonvm/synthetic/AnonymousObject$N`, a class with an EMPTY method table.
/// All five natives `register_java_util_jar_access` puts on the stand-in were
/// therefore INERT: they were registered on a class the factory never handed
/// out. Same for the other two invented owners (`JavaIORandomAccessFileAccess$1`,
/// `JavaNetHttpCookieAccess$1`).
///
/// What it cost: `URLClassLoader.definePackage` calls
/// `javaUtilJarAccess().getTrustedAttributes(man, name)` for every package it
/// defines from a JAR, so every Tomcat webapp class load out of a JAR threw
/// `NoSuchMethodError: java.util.jar.Attributes
/// cratonvm.synthetic.AnonymousObject$1.getTrustedAttributes(...)` — an HTTP
/// 500 out of `JspServlet.service`, and the whole residual left in
/// `known-issues/tomcat/ecj-operandstack-*` once the JIT half was fixed.
///
/// The fix is to mint the stand-in under ITS OWN NAME, which is what
/// `alloc_named_synthetic_singleton` already does for `java/nio/Buffer$2` two
/// functions below — the registry is keyed by receiver class name, so a
/// correctly-named receiver is exactly what makes those five registrations
/// reachable. `try_ensure_synthetic_class` refuses under `--jdk-only`, so the
/// strict-mode disposition the paragraph above describes is unchanged: a
/// refusal, not a wrong-class object.
fn alloc_singleton(
    ctx: &mut dyn NativeContext,
    owner_class: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    // Resolve the real class when the image has it. The synthetic fallback
    // reserves 1 field slot so downstream callers that happen to poke at
    // field 0 don't go out of bounds.
    //
    // `ClassId(0)` is checked explicitly, not just the `Err`: it is the
    // untyped-allocation id, so an `Ok(ClassId(0))` would land on exactly the
    // `AnonymousObject$N` receiver this function exists to stop handing out.
    if let Ok(cid) = ctx.ensure_class_initialized(owner_class) {
        if cid != cratonvm_types::ClassId::new(0) {
            let real_fields = ctx.class_num_total_fields(cid);
            return Ok(ctx.alloc_object(cid, real_fields.max(1)));
        }
    }
    alloc_named_synthetic_singleton(ctx, owner_class)
}

fn alloc_owner_instance(ctx: &mut dyn NativeContext, owner_class: &str) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized(owner_class).ok()?;
    let real_fields = ctx.class_num_total_fields(cid);
    Some(ctx.alloc_object(cid, real_fields.max(1)))
}

/// Fallible since 2026-08-10 (JDK-only wave 2, step 3). This is only reached
/// when NEITHER real `java.nio.Buffer$2` nor `Buffer$1` is loadable, i.e. on an
/// image with no `java.base`; fabricating a `java/nio/Buffer$2` stand-in there
/// is the compatibility substitution contract §5 refuses, so a strict run gets
/// the `NoClassDefFoundError` instead.
fn alloc_named_synthetic_singleton(
    ctx: &mut dyn NativeContext,
    owner_class: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    let cid = crate::util_concurrent_ext::refused_class(ctx, owner_class, 1)?;
    let fields = ctx.class_num_total_fields(cid).max(1);
    Ok(ctx.alloc_object(cid, fields))
}

fn alloc_java_nio_access_singleton(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    // JDK 25 implements JavaNioAccess as Buffer$2; JDK 17 implements it as
    // Buffer$1. If neither class is loadable, keep a named owner so
    // invokeinterface resolves against registered JavaNioAccess bridge methods
    // instead of the generic AnonymousObject$1 fallback.
    match alloc_owner_instance(ctx, "java/nio/Buffer$2")
        .or_else(|| alloc_owner_instance(ctx, "java/nio/Buffer$1"))
    {
        Some(obj) => Ok(obj),
        None => alloc_named_synthetic_singleton(ctx, "java/nio/Buffer$2"),
    }
}

/// Register the `SharedSecrets.getJavaXxxAccess()` factories.
///
/// Each factory returns a synthetic object of the matching owner
/// class.  We register on both `jdk/internal/access/SharedSecrets`
/// (JDK 11+) and `jdk/internal/misc/SharedSecrets` (legacy) so
/// bytecode compiled against either package resolves correctly.
///
/// # A factory is exactly as admissible as the receiver it hands out
///
/// F33-1 (2026-08-13). This registrar's ambient kind is `Bridge`
/// (`register_wp1_4_shared_secrets`), but `NativeMethodRegistry::register`
/// re-tags by RECEIVER CLASS, and the receiver of a *factory* registration is
/// `SharedSecrets` — not the object it returns. Those two facts used to split
/// this one registrar down the middle:
///
/// | registered on | kind | `--jdk-only` |
/// |---|---|---|
/// | `jdk/internal/access/SharedSecrets` — every factory | `Bridge` | survived |
/// | `cratonvm/internal/ss/…$1` — four owners' methods | `SyntheticStub` | dropped |
///
/// So strict mode kept four natives that shadow real JDK bytecode getters and
/// hand back a carrier on which it had just dropped every method — a
/// wrong-class receiver rather than an error, because `alloc_singleton`'s `Err`
/// arm is infallible (see its doc comment).
///
/// The fix is the rule this loop now applies: **a factory carries the kind of
/// the owner it returns.** `receiver_declared_by_no_supported_image` is exactly
/// the predicate that adjudicated the owner, so asking it again about the owner
/// — not about `SharedSecrets` — makes the two halves refuse together or
/// survive together, atomically, with no second list to drift.
///
/// It is a DERIVED rule, deliberately: add a `cratonvm/…` stand-in owner to
/// `VM_MINTED_STAND_IN_RECEIVERS`, or add `java/io/ObjectInputStream$1` to
/// `NO_IMAGE_JDK_RECEIVERS` (nominated in that module), and its factory follows
/// automatically. `factory_kind_follows_the_owner_it_hands_out` pins both
/// directions.
///
/// **Why refusing is the honest disposition and "make it work" is not**, for
/// the four owners this fires on today. Measured with `javap -p` on Microsoft
/// 25.0.3+9-LTS, the methods registered on those carriers against the interface
/// the factory's return descriptor promises:
///
/// ```text
///   JavaIORandomAccessFileAccess  registered: open(String,String), openAsChannel(RandomAccessFile)
///                                 JDK 25:     openAndDelete(File,String)          -> 0 of 1
///   JavaNetHttpCookieAccess       registered: parseCookie(String)
///                                 JDK 25:     parse(String), header(HttpCookie)   -> 0 of 2
///   JavaUtilJarAccess             registered: jarFileHasClassPathAttribute, ensureInitialization
///                                 JDK 25:     + getTrustedAttributes, isInitializing, entryFor
///                                                                                  -> 2 of 5
///   JavaNetUriAccess              registered: create(String,String)                -> 1 of 1
/// ```
///
/// Two of the four carry NO method the interface declares, so an
/// `invokeinterface JavaNetHttpCookieAccess.parse` on the object this factory
/// returns misses in **every** mode, not just strict. `parseCookie` and `open`
/// are names CratonVM invented; nothing else in the tree names them
/// (`grep -rn 'parseCookie\|openAsChannel'` finds only this file and the
/// tables that list its owners). Retargeting these onto the JDK's own
/// implementation classes — `java/util/jar/JavaUtilJarAccessImpl`,
/// `java/net/URI$1`, and the two anonymous classes in `HttpCookie.<clinit>` /
/// `RandomAccessFile.<clinit>` — is a real option and a bigger change; it would
/// make the natives shadow live bytecode on real classes in BOTH modes, which
/// is a §1.4 question needing a strict-corpus measurement. Refusing in strict
/// needs none: the alternative being refused is already broken.
fn register_factories(registry: &mut NativeMethodRegistry) {
    for (method, ret_desc, owner) in FACTORIES {
        let full_desc = format!("(){}", ret_desc);
        let owner_name: &'static str = owner;
        // Only the demotion is STATED. The other ten factories keep inheriting
        // the registrar's ambient `Bridge` through plain `register`, so this
        // change cannot move a row it is not about — and if a future author
        // re-scopes `register_wp1_4_shared_secrets`, the ten follow the new
        // ambient instead of a `Bridge` frozen into this function.
        let demote = factory_is_orphaned_by_strict_mode(owner_name);

        let cb: cratonvm_native_api::NativeCallback = make_factory_callback(owner_name);
        let cb2: cratonvm_native_api::NativeCallback = make_factory_callback(owner_name);
        // Legacy package alias. `jdk/internal/misc/SharedSecrets` is itself in
        // `NO_IMAGE_JDK_RECEIVERS`, so `register` re-tags every row on it
        // `SyntheticStub` regardless of which arm we take below; it is written
        // the same way only so the two spellings do not read as two rules.
        if demote {
            registry.register_with_kind(
                "jdk/internal/access/SharedSecrets",
                method,
                &full_desc,
                cb,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
            registry.register_with_kind(
                "jdk/internal/misc/SharedSecrets",
                method,
                &full_desc,
                cb2,
                cratonvm_native_api::NativeKind::SyntheticStub,
            );
        } else {
            registry.register("jdk/internal/access/SharedSecrets", method, &full_desc, cb);
            registry.register("jdk/internal/misc/SharedSecrets", method, &full_desc, cb2);
        }
    }
}

/// Whether `--jdk-only` would drop every method on `owner_class`, so that a
/// factory handing it out must be dropped with them.
///
/// This is [`receiver_declared_by_no_supported_image`] asked about the object a
/// factory RETURNS instead of about the class the factory is registered on.
/// Split out as a named function so the guard below calls the code the
/// registrar runs rather than restating the rule — a second copy of a
/// predicate is how the two `SharedSecrets` lists drifted in the first place
/// (F24-1).
///
/// [`receiver_declared_by_no_supported_image`]:
///     cratonvm_native_api::no_image_receiver::receiver_declared_by_no_supported_image
fn factory_is_orphaned_by_strict_mode(owner_class: &str) -> bool {
    cratonvm_native_api::no_image_receiver::receiver_declared_by_no_supported_image(owner_class)
}

/// Build a `NativeCallback` that returns a fresh instance of the
/// given owner class.  Wrapping in a helper avoids closure
/// lifetime gymnastics — `NativeCallback` is a `fn` pointer, so
/// we dispatch through a small lookup table keyed by owner class.
fn make_factory_callback(owner_class: &'static str) -> cratonvm_native_api::NativeCallback {
    // Because NativeCallback is a fn pointer (not a closure) we
    // can't capture `owner_class`.  Instead, encode the owner
    // via a match in a generated fn, one per entry in
    // FACTORIES.  We achieve this with a lookup against an
    // index-bounded match.
    macro_rules! gen_factory {
        ($name:ident, $owner:literal) => {
            fn $name(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
                let obj = alloc_singleton(ctx, $owner)?;
                Ok(Some(Value::Object(Some(obj))))
            }
        };
    }
    gen_factory!(f_jla, "java/lang/System$1");
    gen_factory!(f_jlia, "java/lang/invoke/MethodHandleImpl$1");
    gen_factory!(f_jlra, "java/lang/ref/Reference$1");
    gen_factory!(f_jlrefa, "java/lang/reflect/ReflectAccess");
    gen_factory!(f_jioa, "java/io/Console$1");
    gen_factory!(
        f_jiorafa,
        "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1"
    );
    gen_factory!(f_jiofd, "java/io/FileDescriptor$1");
    gen_factory!(f_jniaa, "java/net/InetAddress$1");
    gen_factory!(f_jnuri, "cratonvm/internal/ss/JavaNetUriAccess$1");
    gen_factory!(f_jujar, "cratonvm/internal/ss/JavaUtilJarAccess$1");
    gen_factory!(f_juzf, "java/util/zip/ZipFile$1");
    gen_factory!(f_jnhc, "cratonvm/internal/ss/JavaNetHttpCookieAccess$1");
    gen_factory!(f_jois, "java/io/ObjectInputStream$1");
    gen_factory!(f_jurb, "java/util/ResourceBundle$1");

    fn f_jnio(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
        let obj = alloc_java_nio_access_singleton(ctx)?;
        Ok(Some(Value::Object(Some(obj))))
    }

    match owner_class {
        "java/lang/System$1" => f_jla,
        "java/lang/invoke/MethodHandleImpl$1" => f_jlia,
        "java/lang/ref/Reference$1" => f_jlra,
        "java/lang/reflect/ReflectAccess" => f_jlrefa,
        "java/io/Console$1" => f_jioa,
        "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1" => f_jiorafa,
        "java/io/FileDescriptor$1" => f_jiofd,
        "java/net/InetAddress$1" => f_jniaa,
        "cratonvm/internal/ss/JavaNetUriAccess$1" => f_jnuri,
        "java/nio/Buffer$2" => f_jnio,
        // `"java/security/AccessController$1" => f_jsec` was here until
        // 2026-08-13 (F33-1). It had been unreachable since F17-1 deleted the
        // `getJavaSecurityAccess` row from `FACTORIES` — `make_factory_callback`
        // is only ever called with an owner from that table — so this arm was
        // the last thing keeping the fabricated receiver mintable at all.
        "cratonvm/internal/ss/JavaUtilJarAccess$1" => f_jujar,
        "java/util/zip/ZipFile$1" => f_juzf,
        "cratonvm/internal/ss/JavaNetHttpCookieAccess$1" => f_jnhc,
        "java/io/ObjectInputStream$1" => f_jois,
        "java/util/ResourceBundle$1" => f_jurb,
        _ => panic!("SharedSecrets bridge: unknown owner class {owner_class}"),
    }
}

// ---------------------------------------------------------------------------
// Per-interface method registrations
// ---------------------------------------------------------------------------

// JavaLangAccess (Thinnest + a few mediums) -----------------------------------

fn jla_current_carrier_thread(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // In platform-thread-only mode carrier == current thread.
    let t = ctx.current_thread_object();
    Ok(Some(Value::Object(Some(t))))
}

fn jla_get_reflection_factory(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // The real JDK returns a `jdk.internal.reflect.ReflectionFactory`
    // singleton.  Most callers immediately invoke methods on it
    // (e.g. `newConstructorAccessor`) which our reflection layer
    // implements via the existing lang_class native surface.
    // Returning a synthetic 1-field object gives the caller a
    // non-null that routes through invokevirtual dispatch into
    // lang_class.
    let obj = alloc_singleton(ctx, "jdk/internal/reflect/ReflectionFactory")?;
    Ok(Some(Value::Object(Some(obj))))
}

fn jla_blocked_on(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `JavaLangAccess.blockedOn(Thread, Interruptible)` records the
    // `Interruptible` for the given thread so `Thread.interrupt()`
    // can notify an NIO I/O operation.  Rustjvm's threading layer
    // handles blocking interrupts directly through `park_state`
    // and the interrupted flag — no bookkeeping to do here.
    Ok(None)
}

fn jla_get_carrier_thread_local(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(tl))) = args.get(1) else {
        return Ok(Some(Value::Object(None)));
    };
    let key = ctx.identity_hash_code(*tl);
    if let Some(value) = {
        let table = carrier_thread_locals()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        table.get(&key).copied()
    } {
        return Ok(Some(match value {
            CarrierThreadLocalValue::Root(handle) => ctx
                .resolve_global_root(handle)
                .map(|obj| Value::Object(Some(obj)))
                .unwrap_or(Value::Object(None)),
            CarrierThreadLocalValue::Plain(value) => value,
        }));
    }

    let pin = ctx.pin_native_root(*tl);
    let init = ctx.invoke_virtual(
        ctx.read_native_pin(pin, *tl),
        "initialValue",
        "()Ljava/lang/Object;",
        &[],
    )?;
    ctx.unpin_native_roots(pin);
    let value = init.unwrap_or(Value::Object(None));
    let stored = carrier_value_from_java(ctx, value);
    let old = carrier_thread_locals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, stored);
    if let Some(old) = old {
        drop_carrier_value_root(ctx, old);
    }
    Ok(Some(match stored {
        CarrierThreadLocalValue::Root(handle) => ctx
            .resolve_global_root(handle)
            .map(|obj| Value::Object(Some(obj)))
            .unwrap_or(Value::Object(None)),
        CarrierThreadLocalValue::Plain(value) => value,
    }))
}

#[derive(Clone, Copy)]
enum CarrierThreadLocalValue {
    Root(usize),
    Plain(Value),
}

static CARRIER_THREAD_LOCALS: OnceLock<Mutex<HashMap<i32, CarrierThreadLocalValue>>> =
    OnceLock::new();

fn carrier_thread_locals() -> &'static Mutex<HashMap<i32, CarrierThreadLocalValue>> {
    CARRIER_THREAD_LOCALS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn drop_carrier_value_root(ctx: &mut dyn NativeContext, value: CarrierThreadLocalValue) {
    if let CarrierThreadLocalValue::Root(handle) = value {
        let _ = ctx.remove_global_root(handle);
    }
}

fn carrier_value_from_java(ctx: &mut dyn NativeContext, value: Value) -> CarrierThreadLocalValue {
    match value {
        Value::Object(Some(obj)) => CarrierThreadLocalValue::Root(ctx.add_global_root(obj)),
        other => CarrierThreadLocalValue::Plain(other),
    }
}

fn jla_set_carrier_thread_local(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(Value::Object(Some(tl))) = args.get(1) else {
        return Ok(None);
    };
    let value = args.get(2).copied().unwrap_or(Value::Object(None));
    let key = ctx.identity_hash_code(*tl);
    let new_value = carrier_value_from_java(ctx, value);
    let old = carrier_thread_locals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, new_value);
    if let Some(old) = old {
        drop_carrier_value_root(ctx, old);
    }
    Ok(None)
}

fn jla_remove_carrier_thread_local(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(tl))) = args.get(1) else {
        return Ok(None);
    };
    let key = ctx.identity_hash_code(*tl);
    let old = carrier_thread_locals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&key);
    if let Some(old) = old {
        drop_carrier_value_root(ctx, old);
    }
    Ok(None)
}

fn jla_is_carrier_thread_local_present(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(tl))) = args.get(1) else {
        return Ok(Some(Value::Int(0)));
    };
    let key = ctx.identity_hash_code(*tl);
    let present = carrier_thread_locals()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key);
    Ok(Some(Value::Int(present as i32)))
}

fn jla_set_cause(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `setCause(Throwable t, Throwable cause)` sets the `cause`
    // field on `t` directly, bypassing the normal
    // `Throwable.initCause` guards.  Our Throwable layout has
    // `cause` at a reflection-discoverable slot; use
    // `set_field_by_name` for robustness across synthetic vs
    // real-JDK layouts.
    // INSTANCE method: args[0] = receiver (System$1), args[1] = t,
    // args[2] = cause.
    if let (Some(Value::Object(Some(t))), Some(c)) = (args.get(1), args.get(2)) {
        ctx.set_field_by_name(*t, "cause", *c);
    }
    Ok(None)
}

fn jla_get_enum_constants_shared(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = receiver (System$1 / JavaLangAccess object)
    // args[1] = the Class<E> argument
    if let Some(Value::Object(Some(cls))) = args.get(1) {
        ctx.invoke(
            "java/lang/Class",
            "getEnumConstantsShared",
            "()[Ljava/lang/Object;",
            &[Value::Object(Some(*cls))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// `JavaLangAccess.defineUnnamedModule(ClassLoader)` -> `Module`.
///
/// `jdk.internal.loader.BootLoader.<clinit>` calls this to build the boot
/// loader's unnamed module:
/// ```text
/// UNNAMED_MODULE = SharedSecrets.getJavaLangAccess().defineUnnamedModule(null);
/// ```
/// Without this native the `invokeinterface` raised `NoSuchMethodError`, which
/// failed `BootLoader.<clinit>`, parked `BootLoader` in `InitializationError`,
/// and made every downstream reference (JMX `PlatformMBeanProvider`, the
/// `URLClassPath` boot scan, ...) throw `NoClassDefFoundError: BootLoader`.
///
/// CratonVM does not model JPMS module layers, so we return a synthetic
/// unnamed `java.lang.Module` (name field = null, matching the real unnamed
/// module). The caller only stores it and passes it to the no-op
/// `setBootLoaderUnnamedModule0` and to `addEnableNativeAccess`.
fn jla_define_unnamed_module(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Mirror `Class.getModule()` shape: 2-field synthetic Module, field 0 =
    // name (None = unnamed).
    let m_obj = try_alloc_concurrent_synthetic(ctx, "java/lang/Module", 2)?;
    ctx.set_field(m_obj, 0, Value::Object(None));
    Ok(Some(Value::Object(Some(m_obj))))
}

/// `JavaLangAccess.addEnableNativeAccess(Module)` -> `Module`.
///
/// Real JDK marks the module as allowed to call restricted native methods and
/// returns the same module for chaining. `BootLoader.<clinit>` calls it on the
/// unnamed module and discards the result. CratonVM does not enforce native
/// access, so this is an identity passthrough returning the argument module.
fn jla_add_enable_native_access(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args[0] = receiver (System$1), args[1] = the Module argument.
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

/// `JavaLangAccess.defineClass(ClassLoader loader, String name, byte[] b,
/// ProtectionDomain pd, String source)` -> `Class<?>`.
///
/// `jdk.internal.reflect.ClassDefiner.defineClass(String, byte[], int, int,
/// ClassLoader)` — used by `ReflectionFactory` to materialize the
/// serialization-constructor-accessor / `MethodAccessor` classes that JUnit5's
/// `SessionPerRequestLauncher` (and every Arquillian
/// `testsuite/integration-arquillian/tests/base` test class) generates during
/// test-plan construction — slices its `(off, len)` window down to an
/// exact-size array before calling
/// `SharedSecrets.getJavaLangAccess().defineClass(parent, name, bytes, null,
/// "__ClassDefiner__")`. `System$1` (the `JavaLangAccess` singleton, see
/// `register_java_lang_access`) never had this method registered, so the
/// `invokeinterface` raised `NoSuchMethodError`, aborting `ClassDefiner`
/// before any Arquillian base test class could run.
///
/// No offset/length here (unlike `ClassLoader.defineClass1`/`defineClass2`) —
/// the real `JavaLangAccess` interface method takes the whole array. Shares
/// the same `define_class_via_full` backend as `defineClass1` so magic-byte
/// validation, panic-guarding, and PD/code-source attribution stay unified
/// across every defineClass entry point.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = loader,
/// args[2] = name, args[3] = byte[], args[4] = pd, args[5] = source (unused —
/// `define_class_via_full` derives its own SourceFile handling).
fn jla_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    let loader = args.get(1).copied().unwrap_or(Value::Object(None));
    let name = crate::classloader::read_optional_internal_name(ctx, args, 2);
    let byte_array = match args.get(3) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "JavaLangAccess.defineClass: bytes must not be null".into(),
            }
            .into());
        }
    };
    let len = ctx.array_length(byte_array);
    let bytes =
        crate::classloader::read_byte_array_slice(ctx, byte_array, 0, len).map_err(|_msg| {
            cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(0))
        })?;

    let mut opts = cratonvm_native_api::DefineClassFull::default();
    // `System$1` is the JDK's trusted JavaLangAccess implementation. HotSpot
    // routes this path around ClassLoader.preDefineClass' protected-package
    // guard for generated core-library helpers, including
    // java/lang/invoke/BoundMethodHandle$Species_* classes used by FFM
    // VarHandle setup. Treat it like the other JVM-internal define paths.
    opts.privileged_define = true;
    if let Some(Value::Object(Some(pd))) = args.get(4) {
        opts.code_source_url = crate::classloader::extract_pd_code_source_url(ctx, *pd);
    }

    let loader_id = crate::classloader::loader_id_for(ctx, loader);
    crate::classloader::define_class_via_full(
        ctx, &name, bytes, loader_id, opts, false, None, loader,
    )
}

fn jla_define_class_hidden(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use cratonvm_types::error::RuntimeError;

    const NESTMATE: i32 = 0x01;
    const HIDDEN: i32 = 0x02;

    let loader = args.get(1).copied().unwrap_or(Value::Object(None));
    let lookup_mirror = match args.get(2) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    };
    let name = crate::classloader::read_optional_internal_name(ctx, args, 3);
    let byte_array = match args.get(4) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "JavaLangAccess.defineClass: bytes must not be null".into(),
            }
            .into());
        }
    };
    let len = ctx.array_length(byte_array);
    let bytes =
        crate::classloader::read_byte_array_slice(ctx, byte_array, 0, len).map_err(|_msg| {
            cratonvm_types::error::MethodCallFailed::from(RuntimeError::aioobe_index_only(0))
        })?;

    let mut opts = cratonvm_native_api::DefineClassFull::default();
    if let Some(Value::Object(Some(pd))) = args.get(5) {
        opts.code_source_url = crate::classloader::extract_pd_code_source_url(ctx, *pd);
    }
    let initialize = matches!(args.get(6), Some(Value::Int(v)) if *v != 0);
    let flags = match args.get(7) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let is_jdk_internal_define = name.starts_with("java/")
        || name.starts_with("jdk/")
        || name.starts_with("sun/")
        || name.starts_with("com/sun/");
    if is_jdk_internal_define {
        // JDK MethodHandles spin implementation classes in protected platform
        // packages through JavaLangAccess. Some of those classes are hidden;
        // others, such as BoundMethodHandle species classes, are ordinary
        // generated classes. CratonVM's dynamic define backend maps loader id 0
        // to the application namespace, so use the explicit privileged flag for
        // this JDK-internal bridge while keeping ordinary application
        // ClassLoader.defineClass package checks intact.
        opts.privileged_define = true;
    }
    if (flags & HIDDEN) != 0 {
        opts.hidden = true;
        opts.skip_verification = true;
    }
    if (flags & NESTMATE) != 0 {
        if let Some(lk) = lookup_mirror {
            if let Some(cid) = crate::lang_class::mirror_class_id(ctx, lk) {
                opts.nest_host_class_name = ctx
                    .nest_host_name(cid)
                    .or_else(|| ctx.class_name_of_id(cid));
            }
        }
    }
    let class_data = match args.get(8) {
        Some(Value::Object(Some(_))) => Some(args[8]),
        _ => None,
    };

    let is_jdk_internal_name =
        name.starts_with("java/") || name.starts_with("jdk/") || name.starts_with("sun/");
    let loader_id = if is_jdk_internal_name {
        0
    } else {
        crate::classloader::loader_id_for(ctx, loader)
    };
    // Don't record a defining loader for the forced-namespace-0 JDK-internal
    // case above — the recorded loader must match the namespace the class
    // actually landed in, or `defining_loader_for` would report a loader
    // whose namespace disagrees with `loader_id_of_class(cid)`.
    let loader_for_registration = if is_jdk_internal_name {
        Value::Object(None)
    } else {
        loader
    };
    crate::classloader::define_class_via_full(
        ctx,
        &name,
        bytes,
        loader_id,
        opts,
        initialize,
        class_data,
        loader_for_registration,
    )
}

/// `JavaLangAccess.getConstantPool(Class<?>)` -> `jdk.internal.reflect.ConstantPool`.
///
/// ByteBuddy's class-file reader consults this via `invokeinterface
/// JavaLangAccess` when instrumenting a class for Mockito's inline mock
/// maker (`Instrumentation.redefineClasses`/`retransformClasses`). Without
/// it, `NoSuchMethodError` on this method aborted the redefine with
/// `MockitoException: Could not modify all classes [...]`, and every mock()
/// of a JDK class in this suite failed. Thin passthrough to the existing
/// `Class.getConstantPool()` native — same synthetic ConstantPool mirror.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = Class<?>.
fn jla_get_constant_pool(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let class_obj = args.get(1).copied().unwrap_or(Value::Object(None));
    crate::lang_class::native_class_get_constant_pool(ctx, &[class_obj])
}

/// Whether this VM takes a terminating thread back out of its `ThreadContainer`.
///
/// **This is now `true`, because the de-registration half landed.** The
/// constant is not a policy dial; it is a factual statement about the VM, and
/// the fact changed. `vm/src/vm/vm_exec.rs::run_thread_exit_shared` invokes
/// `java/lang/Thread.exit()V` on the terminating thread from both worker-death
/// paths (normal return and after the uncaught-exception dispatch), which is
/// what reaches `container.remove(this)` —
/// docs/known-issues/jdk-only/W7-27-thread-exit-java-cleanup.md.
///
/// Why the pairing is not optional. `jdk.internal.misc.ThreadFlock.awaitAll()`
/// is
///
/// ```java
///     if (threadCount == 0) return true;
///     while (threadCount > 0 && !permit) { ... LockSupport.park(); }
/// ```
///
/// so a container that is only ever added to does not leak quietly — it hangs
/// every `join()`/`close()` built on it. HotSpot gets the decrement from
/// `Thread.exit()` (JDK 25 `Thread.java`, `private void exit()`):
///
/// ```java
///     ThreadContainer container = threadContainer();
///     if (container != null) {
///         container.remove(this);
///     }
/// ```
///
/// MEASURED against `target/release/cratonvm.exe` (2026-08-11), i.e. the binary
/// that had NEITHER half, under `--real-jdk`, by invoking the JDK's own
/// package-private `Thread.start(ThreadContainer)` reflectively — the exact body
/// the enabled branch below reaches — on a live `StructuredTaskScope`'s flock:
///
/// ```text
///                                       HotSpot 25    cratonvm --real-jdk
///   flock threads after start                1              1
///   flock threads after the worker died      0              1   <- not removed
///   join() called after the worker died   0-1 ms        never returned (3/3)
/// ```
///
/// That table is why this constant existed: the add half alone turns today's
/// "join() waits for nothing" into "join() waits forever", which is strictly
/// worse. With `Thread.exit()` wired up, the right-hand column is the thing
/// this flip is expected to move to `0` / a number.
///
/// **WHAT A WRONG FLIP LOOKS LIKE, so a suite run can be read.** The failure
/// mode is a HANG, not a wrong answer, and it has one signature per consumer:
///
/// * A vector that opens a `StructuredTaskScope` (or anything reaching
///   `ThreadFlock`) never finishes: no `FAIL` line, no `PASS` line, the run
///   stops mid-transcript and the harness times out. `join()`/`close()` are
///   parked in `LockSupport.park()` on a `threadCount` that never fell. Note
///   that a suite timeout in this tree is more often a crash with no result
///   line than a real wait, so check the process is still alive before reading
///   a timeout as this: here it IS a live park, not a fault.
/// * With `-ea`, `ThreadFlock.onExit`'s `assert removed` fires instead: that is
///   the OPPOSITE regression — a thread removed twice, or removed from a
///   container it was never added to.
/// * `WARN … Thread.exit() failed on terminating thread …` on every thread
///   death means `exit()` is being reached and throwing inside real JDK
///   bytecode; the container is then still held and the first bullet follows.
/// * Jetty/Tomcat thread pools use `SharedThreadContainer`, which never waits
///   on a count, so they cannot show this. Do not read a green Jetty arm as
///   evidence the flip is sound.
///
/// The escape hatch is `CRATONVM_THREAD_CONTAINERS=0`, which restores the
/// pre-flip behaviour in the same binary; `=1` forces it on. Bisect with that
/// rather than by rebuilding.
const VM_REMOVES_THREADS_FROM_CONTAINERS: bool = true;

/// Read once per process: the constant above, overridable by
/// `CRATONVM_THREAD_CONTAINERS` (`1` on, `0` off).
///
/// Read through `flags::runtime_var`, not `std::env::var`. A raw `getenv` is
/// served live and is therefore invisible to `flags::with_thread_overrides`,
/// so a test selecting an arm through the supported hook would silently have
/// measured the developer's ambient environment instead — which is exactly the
/// A/B this pair prescribes. The token is declared `THREADS/thread-containers`
/// in `types/src/flag_groups.rs`; `flag_declaration_guard.rs` fails the build
/// for any `CRATONVM_*` name that is read but declared nowhere.
fn thread_container_registration_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_THREAD_CONTAINERS").as_deref() {
            Ok("1") => true,
            Ok("0") => false,
            _ => VM_REMOVES_THREADS_FROM_CONTAINERS,
        }
    })
}

/// `JavaLangAccess.start(Thread, ThreadContainer)` -> `void`.
///
/// `jdk.internal.vm.SharedThreadContainer.start(Thread)` (Jetty's thread pool)
/// and `jdk.internal.misc.ThreadFlock.start(Thread)` (every structured-
/// concurrency `fork`) call this via `invokeinterface JavaLangAccess` to start
/// a thread *and register it with the container that tracks it*. Without the
/// registration at all, `NoSuchMethodError` here aborted every thread-pool-
/// backed HTTP client (Jetty) at startup, which is why the bridge exists.
///
/// The container is not introspection. `ThreadFlock.awaitAll()` returns
/// immediately while `threadCount == 0`, and `StructuredTaskScopeImpl.join()`
/// is `flock.awaitAll()`, so dropping `args[2]` is what makes JEP 505's
/// `join()` wait for nothing —
/// docs/known-issues/jdk-only/W7-18-structured-task-scope-jep505.md measured 15
/// divergent lines, and three CratonVM runs re-taken today disagreed with each
/// other on eight — all downstream of this one dropped argument, because a
/// `join()` that does not wait turns the whole API into a race. The registry has
/// several other consumers (`ThreadContainers.root()` enumeration, thread
/// dumps, JFR), and they are enumerated in
/// docs/known-issues/jdk-only/W7-23-thread-container-registration.md.
///
/// It is no longer dropped by default: `VM_REMOVES_THREADS_FROM_CONTAINERS` is
/// `true` now that `Thread.exit()` runs on both worker-death paths. Read that
/// constant's doc comment before changing anything here — it carries the
/// measured hang the interlock existed for, and the runtime signature of a
/// wrong flip. `CRATONVM_THREAD_CONTAINERS=0` restores the drop in the same
/// binary, and the drop is still announced once per process so the wrong answer
/// is not silent when it is selected.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = Thread,
/// args[2] = ThreadContainer.
fn jla_start_in_container(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let thread_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let container = args.get(2).copied().unwrap_or(Value::Object(None));

    if matches!(container, Value::Object(Some(_))) {
        if thread_container_registration_enabled() {
            // `Thread.start(Ljdk/internal/vm/ThreadContainer;)V` is a DIFFERENT
            // triple from `Thread.start()V` (which `lib.rs` shadows with
            // `native_thread_start0`), and nothing registers a native on it, so
            // this reaches the JDK's own bytecode: `setThreadContainer(container)`
            // + `container.add(this)` + `start0()`. `start0()V` *is* registered
            // on the real-JDK path, so the spawn is still CratonVM's own — this
            // adds the bookkeeping the passthrough skipped, it does not move the
            // thread onto a different spawn mechanism. Measured to land the
            // registration: the flock's thread set goes 0 -> 1 across this call.
            //
            // Real-JDK bytecode only. In synthetic mode `java/lang/Thread` has a
            // fabricated layout with no such method, and the synthetic
            // `StructuredTaskScope` runs `fork` on the forking thread anyway
            // (W7-18 §6), so there is no flock to keep a count in.
            ctx.invoke_virtual(
                thread_obj,
                "start",
                "(Ljdk/internal/vm/ThreadContainer;)V",
                &[container],
            )?;
            return Ok(None);
        }
        warn_thread_container_dropped_once();
    }

    ctx.invoke_virtual(thread_obj, "start", "()V", &[])?;
    Ok(None)
}

/// One line per process, not per thread: Jetty starts hundreds of pool threads
/// through this bridge and a per-call warning would bury the run it is trying
/// to explain.
fn warn_thread_container_dropped_once() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        tracing::warn!(
            "JavaLangAccess.start: thread started WITHOUT registering it with its \
             jdk.internal.vm.ThreadContainer. StructuredTaskScope.join(), \
             ThreadFlock.awaitAll() and ThreadContainers.root() enumeration will \
             not see it (see W7-23-thread-container-registration)."
        );
    });
}

/// `JavaLangAccess.join(String prefix, String suffix, String delimiter,
/// String[] elements, int size)` -> `String`.
///
/// A fast-path helper `String.join(...)`/`StringJoiner`-adjacent code calls
/// to concatenate `elements[0..size]` with `delimiter` between them and
/// `prefix`/`suffix` on the ends, bypassing the general `StringJoiner`
/// machinery. Missing here broke Apache HttpClient5's
/// `HttpComponentsClientHttpRequestFactoryTests` (`NoSuchMethodError` on
/// this exact signature).
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = prefix,
/// args[2] = suffix, args[3] = delimiter, args[4] = String[] elements,
/// args[5] = int size.
fn jla_join(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let read = |ctx: &dyn NativeContext, v: Option<&Value>| -> String {
        match v {
            Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
            _ => String::new(),
        }
    };
    let prefix = read(ctx, args.get(1));
    let suffix = read(ctx, args.get(2));
    let delimiter = read(ctx, args.get(3));
    let elements = match args.get(4) {
        Some(Value::Object(Some(arr))) => *arr,
        _ => {
            let s = ctx.create_string(&format!("{prefix}{suffix}"));
            return Ok(Some(Value::Object(Some(s))));
        }
    };
    let size = args.get(5).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let mut out = prefix;
    for i in 0..size {
        if i > 0 {
            out.push_str(&delimiter);
        }
        if let Value::Object(Some(s)) = ctx.get_array_element(elements, i) {
            out.push_str(&ctx.read_string(s).unwrap_or_default());
        }
    }
    out.push_str(&suffix);
    let s = ctx.create_string(&out);
    Ok(Some(Value::Object(Some(s))))
}

fn jla_new_string_utf8_no_repl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (byte[] bytes, int offset, int length) -> String
    // Decode the bytes as UTF-8 with no replacement on malformed
    // sequences — we use the standard `String::from_utf8_lossy`
    // approximation.  Production real-JDK semantics throw
    // `CharacterCodingException` on malformed input; we fall back
    // to the replacement character so callers that already assume
    // well-formed bytes (the common case) keep working.
    //
    // INSTANCE method: args[0] = receiver (System$1 / JavaLangAccess),
    // args[1] = byte[], args[2] = int offset, args[3] = int length. Reading
    // args[0] as the byte[] (the previous bug) made the pattern fail and the
    // handler return null — surfacing as `ZipEntry.<init>` NPE "name" when
    // `ZipCoder.UTF8.toString` decoded a jar entry name via this method.
    let (bytes, off, len) = match (args.get(1), args.get(2), args.get(3)) {
        (Some(Value::Object(Some(b))), Some(Value::Int(o)), Some(Value::Int(l))) => {
            (*b, *o as usize, *l as usize)
        }
        _ => return Ok(Some(Value::Object(None))),
    };
    let array_len = ctx.array_length(bytes);
    let end = off.saturating_add(len).min(array_len);
    let mut buf = Vec::with_capacity(end.saturating_sub(off));
    for i in off..end {
        if let Value::Int(b) = ctx.get_array_element(bytes, i) {
            buf.push(b as u8);
        }
    }
    let s = String::from_utf8_lossy(&buf);
    let obj = ctx.create_string(&s);
    Ok(Some(Value::Object(Some(obj))))
}

fn jla_get_bytes_utf8_no_repl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (System$1), args[1] = the String.
    let s = match args.get(1) {
        Some(Value::Object(Some(r))) => ctx.read_string(*r).unwrap_or_default(),
        _ => String::new(),
    };
    let bytes = s.as_bytes();
    let arr = ctx.new_array(ArrayElementType::Byte, bytes.len());
    for (i, b) in bytes.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*b as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

fn jla_new_stack_trace_element(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `newStackTraceElement(StackFrameInfo sfi)` constructs a
    // `StackTraceElement` from a `StackFrameInfo`.  The
    // stackwalker already builds STE objects directly; we
    // forward to `StackTraceElement.<init>(Ljava/lang/String;
    // Ljava/lang/String;Ljava/lang/String;I)V` with fields read
    // reflectively off the SFI.
    // INSTANCE method: args[0] = receiver (System$1), args[1] = the
    // StackFrameInfo.
    let sfi = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let class_dotted = match ctx.get_field_by_name(sfi, "className") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let method = match ctx.get_field_by_name(sfi, "methodName") {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let file = match ctx.get_field_by_name(sfi, "fileName") {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => None,
    };
    let line = match ctx.get_field_by_name(sfi, "lineNumber") {
        Value::Int(l) => l,
        _ => -1,
    };
    let class_slashed = class_dotted.replace('.', "/");
    let ste = ctx.new_object("java/lang/StackTraceElement")?;
    if let Some(Value::Object(Some(o))) = ste.clone() {
        // Use the shared filler so `declaringClassObject` is also
        // populated with the real Class mirror — `computeFormat()`
        // needs it (otherwise `getClassLoader0()` NPEs / mis-resolves).
        crate::lang_misc::fill_stack_trace_element(
            ctx,
            o,
            &class_slashed,
            &class_dotted,
            &method,
            file.as_deref(),
            line,
        );
    }
    Ok(ste)
}

fn jla_get_declared_public_methods(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Delegate to Class.getDeclaredMethods() and let the caller
    // filter.  The real JDK path filters to public methods here;
    // we accept the slight over-return since callers (e.g.
    // annotation scanners) already filter by modifier.
    // INSTANCE method: args[0] = receiver (System$1), args[1] = the Class
    // (the String/Class[] filter args of the 3-arg overload are ignored).
    if let Some(Value::Object(Some(cls))) = args.get(1) {
        ctx.invoke(
            "java/lang/Class",
            "getDeclaredMethods",
            "()[Ljava/lang/reflect/Method;",
            &[Value::Object(Some(*cls))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// `JavaLangAccess.getDeclaredPublicMethods(Class<?> klass, String name,
/// Class<?>... parameterTypes)` -> `List<Method>`.
///
/// A DIFFERENT method from the array-returning overload above, and it was
/// pointed at the same handler, so it answered a `Method[]` of EVERY declared
/// method where the JDK declares a `List<Method>` filtered to the public
/// declarations named `name` with exactly `parameterTypes`.
///
/// What that broke, found 2026-08-07 once `--jdk-only` could finally reach it:
/// `java.util.ServiceLoader.findStaticProviderMethod` is
///
/// ```java
/// List<Method> methods = LANG_ACCESS.getDeclaredPublicMethods(clazz, "provider");
/// ```
///
/// so the module-path `provider()` STATIC FACTORY form could never be found.
/// `loadProvider` then fell through to its constructor branch and reported
/// `ServiceConfigurationError: ... FactoryGreeter not a subtype`, which is true
/// and beside the point: a factory provider deliberately does not implement the
/// service. Measured on `regression-suite/src/RJdkModule.java:223`.
///
/// The filters are all three load-bearing:
///
/// * **public** - `getDeclaredPublicMethods` is public-only, and a private
///   `provider()` must NOT be honoured;
/// * **name** - the caller passes `"provider"` and expects nothing else back;
/// * **parameter types** - the varargs are an EXACT match, and
///   `findStaticProviderMethod` passes none, meaning the no-arg method. A
///   `provider(String)` overload must not satisfy it.
///
/// `parameterTypes` compares by `Class.getName()` rather than by identity: the
/// comparison spans `invoke` calls that can move objects, and a name is stable
/// across a GC where an `ObjectRef` is not.
fn jla_get_declared_public_methods_list(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (System$1), args[1] = the Class,
    // args[2] = the name, args[3] = the Class[] of parameter types.
    let cls = match args.get(1) {
        Some(Value::Object(Some(c))) => *c,
        _ => return new_empty_list(ctx),
    };
    let want_name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let want_params: Option<Vec<String>> = match args.get(3) {
        Some(Value::Object(Some(arr))) => {
            let arr = *arr;
            let n = ctx.array_length(arr);
            let mut v = Vec::with_capacity(n);
            for i in 0..n {
                match ctx.get_array_element(arr, i) {
                    Value::Object(Some(c)) => {
                        let name = class_name_via_get_name(ctx, c);
                        v.push(name);
                    }
                    _ => v.push(String::new()),
                }
            }
            Some(v)
        }
        _ => None,
    };

    let list = match new_array_list(ctx) {
        Some(l) => l,
        None => return Ok(Some(Value::Object(None))),
    };
    let list_pin = ctx.pin_native_root(list);

    let methods = match ctx.invoke(
        "java/lang/Class",
        "getDeclaredMethods",
        "()[Ljava/lang/reflect/Method;",
        &[Value::Object(Some(cls))],
    ) {
        Ok(Some(Value::Object(Some(arr)))) => arr,
        _ => {
            let list = ctx.read_native_pin(list_pin, list);
            ctx.unpin_native_roots(list_pin);
            return Ok(Some(Value::Object(Some(list))));
        }
    };
    let arr_pin = ctx.pin_native_root(methods);
    let len = ctx.array_length(methods);

    const ACC_PUBLIC: i32 = 0x0001;
    for i in 0..len {
        let methods = ctx.read_native_pin(arr_pin, methods);
        let m = match ctx.get_array_element(methods, i) {
            Value::Object(Some(m)) => m,
            _ => continue,
        };
        let m_pin = ctx.pin_native_root(m);

        let m_ref = ctx.read_native_pin(m_pin, m);
        let name_ok = match ctx.invoke_virtual(m_ref, "getName", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => match (&want_name, ctx.read_string(s)) {
                (Some(want), Some(got)) => *want == got,
                (None, _) => true,
                _ => false,
            },
            _ => false,
        };
        if !name_ok {
            ctx.unpin_native_roots(m_pin);
            continue;
        }

        let m_ref = ctx.read_native_pin(m_pin, m);
        let is_public = matches!(
            ctx.invoke_virtual(m_ref, "getModifiers", "()I", &[]),
            Ok(Some(Value::Int(v))) if (v & ACC_PUBLIC) != 0
        );
        if !is_public {
            ctx.unpin_native_roots(m_pin);
            continue;
        }

        if let Some(want) = &want_params {
            let m_ref = ctx.read_native_pin(m_pin, m);
            let got: Vec<String> =
                match ctx.invoke_virtual(m_ref, "getParameterTypes", "()[Ljava/lang/Class;", &[]) {
                    Ok(Some(Value::Object(Some(pt)))) => {
                        let n = ctx.array_length(pt);
                        let mut v = Vec::with_capacity(n);
                        for j in 0..n {
                            match ctx.get_array_element(pt, j) {
                                Value::Object(Some(c)) => {
                                    let name = class_name_via_get_name(ctx, c);
                                    v.push(name);
                                }
                                _ => v.push(String::new()),
                            }
                        }
                        v
                    }
                    _ => Vec::new(),
                };
            if got != *want {
                ctx.unpin_native_roots(m_pin);
                continue;
            }
        }

        let list_ref = ctx.read_native_pin(list_pin, list);
        let m_ref = ctx.read_native_pin(m_pin, m);
        let _ = ctx.invoke_virtual(
            list_ref,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(m_ref))],
        );
        ctx.unpin_native_roots(m_pin);
    }

    let list = ctx.read_native_pin(list_pin, list);
    ctx.unpin_native_roots(arr_pin);
    ctx.unpin_native_roots(list_pin);
    Ok(Some(Value::Object(Some(list))))
}

/// `cls.getName()` as a Rust `String`, empty when it cannot be read.
fn class_name_via_get_name(ctx: &mut dyn NativeContext, cls: ObjectRef) -> String {
    match ctx.invoke_virtual(cls, "getName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// An empty `java.util.List`, for the early-out paths above.
fn new_empty_list(ctx: &mut dyn NativeContext) -> MethodCallResult {
    Ok(Some(Value::Object(new_array_list(ctx))))
}

/// A constructed, empty `java.util.ArrayList`.
///
/// Allocate-then-`<init>` rather than a bare allocation: the list is handed to
/// real JDK bytecode, which reads `elementData`/`size`, and an object whose
/// constructor never ran has neither.
fn new_array_list(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let cid = ctx.ensure_class_initialized("java/util/ArrayList").ok()?;
    let list = ctx.alloc_object(cid, ctx.class_num_total_fields(cid).max(4));
    ctx.invoke(
        "java/util/ArrayList",
        "<init>",
        "()V",
        &[Value::Object(Some(list))],
    )
    .ok()?;
    Some(list)
}

fn jla_get_methods_or_null(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (System$1), args[1] = the Class.
    if let Some(Value::Object(Some(cls))) = args.get(1) {
        ctx.invoke(
            "java/lang/Class",
            "getMethods",
            "()[Ljava/lang/reflect/Method;",
            &[Value::Object(Some(*cls))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

/// `JavaLangAccess.findBootstrapClassOrNull(String name)` -> `Class<?>`.
///
/// `jdk.internal.loader.BootLoader.loadClassOrNull(String name)` calls this to
/// check whether the bootstrap loader has already loaded `name` WITHOUT
/// triggering a fresh load — a pure "is it already loaded" probe, not a
/// resolve-or-define path. Missing this registration raised
/// `NoSuchMethodError` on every SSSD Arquillian test row (see
/// docs/known-issues/keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md).
///
/// `ctx.class_id_by_name` is a pure lookup against the already-loaded class
/// table — it does not itself trigger classloading (the same guarantee
/// `classloader::find_loaded_class_for_loader` relies on for
/// `ClassLoader.findLoadedClass`/`findLoadedClass0`), so this preserves the
/// real JDK's "does not trigger loading" contract.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = name (dotted or
/// slashed binary name).
fn jla_find_bootstrap_class_or_null(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let name = match args.get(1) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default().replace('.', "/"),
        _ => return Ok(Some(Value::Object(None))),
    };
    match ctx.class_id_by_name(&name) {
        Some(cid) => Ok(Some(Value::Object(Some(ctx.get_class_mirror(cid))))),
        None => Ok(Some(Value::Object(None))),
    }
}

/// `JavaLangAccess.findNative(ClassLoader, String)` -> native address.
///
/// The real JDK's `SymbolLookup.loaderLookup()` lambda reaches this method via
/// `SharedSecrets.getJavaLangAccess()`. CratonVM cannot currently map the Java
/// `ClassLoader` object back to a per-loader native-library list, so use the
/// VM's default native-symbol lookup: all loaded libraries first, then the
/// process lookup. A missing symbol returns 0, matching the native lookup
/// convention used by `NativeLibrary.findEntry0`.
fn jla_find_native(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let name = match args.get(2) {
        Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
        _ => String::new(),
    };
    // -1 searches every loaded library, so this is the same arbitrary-symbol
    // address oracle as `SymbolLookup.find` and `NativeLibrary.findEntry0`,
    // reached through `SharedSecrets`. Gate it identically: default-permissive,
    // denying only under CRATONVM_UNTRUSTED_CODE or a SecurityManager policy
    // that withholds `loadLibrary.*`.
    crate::security_manager::check_host_native_access_or_throw(ctx, "findNative")?;
    let addr = ctx.find_native_symbol(-1, &name).unwrap_or(0);
    Ok(Some(Value::Long(addr as i64)))
}

/// `JavaLangAccess.addReads(from, to)` is the trusted bridge used by the
/// module bootstrap code. Calling the real `System$1` bytecode reaches
/// `Module.implAddReads`; delegate to it so both its heap-side bookkeeping
/// and the `Module.addReads0` registry mirror run.
fn jla_call_module_impl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
    descriptor: &str,
) -> MethodCallResult {
    let from = match args.get(1) {
        Some(Value::Object(Some(module))) => *module,
        _ => {
            return Err(RuntimeError::NullPointerException {
                message: Some("JavaLangAccess: from module is null".to_owned()),
            }
            .into())
        }
    };
    ctx.invoke_virtual(from, method, descriptor, &args[2..])?;
    Ok(None)
}

fn jla_add_reads(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(ctx, args, "implAddReads", "(Ljava/lang/Module;)V")
}

fn jla_add_reads_all_unnamed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(ctx, args, "implAddReadsAllUnnamed", "()V")
}

fn jla_add_exports(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let descriptor = if args.len() == 4 {
        "(Ljava/lang/String;Ljava/lang/Module;)V"
    } else {
        "(Ljava/lang/String;)V"
    };
    jla_call_module_impl(ctx, args, "implAddExports", descriptor)
}

fn jla_add_exports_all_unnamed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(
        ctx,
        args,
        "implAddExportsToAllUnnamed",
        "(Ljava/lang/String;)V",
    )
}

fn jla_add_opens(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(
        ctx,
        args,
        "implAddOpens",
        "(Ljava/lang/String;Ljava/lang/Module;)V",
    )
}

fn jla_add_opens_all_unnamed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(
        ctx,
        args,
        "implAddOpensToAllUnnamed",
        "(Ljava/lang/String;)V",
    )
}

fn jla_add_uses(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    jla_call_module_impl(ctx, args, "implAddUses", "(Ljava/lang/Class;)V")
}

fn register_java_lang_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/lang/System$1";
    registry.register(
        owner,
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        jla_current_carrier_thread,
    );
    registry.register(
        owner,
        "currentThread0",
        "()Ljava/lang/Thread;",
        jla_current_carrier_thread,
    );
    registry.register(
        owner,
        "addReads",
        "(Ljava/lang/Module;Ljava/lang/Module;)V",
        jla_add_reads,
    );
    registry.register(
        owner,
        "addReadsAllUnnamed",
        "(Ljava/lang/Module;)V",
        jla_add_reads_all_unnamed,
    );
    registry.register(
        owner,
        "addExports",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        jla_add_exports,
    );
    registry.register(
        owner,
        "addExports",
        "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        jla_add_exports,
    );
    registry.register(
        owner,
        "addExportsToAllUnnamed",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        jla_add_exports_all_unnamed,
    );
    registry.register(
        owner,
        "addOpens",
        "(Ljava/lang/Module;Ljava/lang/String;Ljava/lang/Module;)V",
        jla_add_opens,
    );
    registry.register(
        owner,
        "addOpensToAllUnnamed",
        "(Ljava/lang/Module;Ljava/lang/String;)V",
        jla_add_opens_all_unnamed,
    );
    registry.register(
        owner,
        "addUses",
        "(Ljava/lang/Module;Ljava/lang/Class;)V",
        jla_add_uses,
    );
    registry.register(
        owner,
        "getReflectionFactory",
        "()Ljdk/internal/reflect/ReflectionFactory;",
        jla_get_reflection_factory,
    );
    registry.register(
        owner,
        "blockedOn",
        "(Ljava/lang/Thread;Lsun/nio/ch/Interruptible;)V",
        jla_blocked_on,
    );
    registry.register(
        owner,
        "blockedOn",
        "(Lsun/nio/ch/Interruptible;)V",
        jla_blocked_on,
    );
    registry.register(
        owner,
        "getCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
        jla_get_carrier_thread_local,
    );
    registry.register(
        owner,
        "setCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
        jla_set_carrier_thread_local,
    );
    registry.register(
        owner,
        "removeCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)V",
        jla_remove_carrier_thread_local,
    );
    registry.register(
        owner,
        "isCarrierThreadLocalPresent",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
        jla_is_carrier_thread_local_present,
    );
    registry.register(
        owner,
        "setCause",
        "(Ljava/lang/Throwable;Ljava/lang/Throwable;)V",
        jla_set_cause,
    );
    registry.register(
        owner,
        "getEnumConstantsShared",
        "(Ljava/lang/Class;)[Ljava/lang/Object;",
        jla_get_enum_constants_shared,
    );
    // Erased-type variant: callers with `Class<? extends Enum<E>>` see
    // the return type as `[Ljava/lang/Enum;` rather than `[Ljava/lang/Object;`.
    registry.register(
        owner,
        "getEnumConstantsShared",
        "(Ljava/lang/Class;)[Ljava/lang/Enum;",
        jla_get_enum_constants_shared,
    );
    registry.register(
        owner,
        "newStringUtf8NoRepl",
        "([BII)Ljava/lang/String;",
        jla_new_string_utf8_no_repl,
    );
    registry.register(
        owner,
        "getBytesUtf8NoRepl",
        "(Ljava/lang/String;)[B",
        jla_get_bytes_utf8_no_repl,
    );
    // JDK 22 spells these `UTF8` (all-caps), not `Utf8`. The lowercase
    // variants above were registered for an older method name; the real
    // `jdk.internal.access.JavaLangAccess` declares `newStringUTF8NoRepl`
    // / `getBytesUTF8NoRepl`. Elasticsearch's `Build.current()` version
    // read path calls `newStringUTF8NoRepl` via `invokeinterface`, which
    // raised `NoSuchMethodError` against `System$1`. Register the
    // canonical casing too (keep both so any caller spelling resolves).
    registry.register(
        owner,
        "newStringUTF8NoRepl",
        "([BII)Ljava/lang/String;",
        jla_new_string_utf8_no_repl,
    );
    registry.register(
        owner,
        "getBytesUTF8NoRepl",
        "(Ljava/lang/String;)[B",
        jla_get_bytes_utf8_no_repl,
    );
    // JDK 25 UnixPath encodes filesystem paths through JavaLangAccess with an
    // explicit Charset argument. We currently support the UTF-8/ASCII paths
    // WildFly uses and ignore the Charset object.
    for name in ["getBytesNoRepl", "uncheckedGetBytesNoRepl"] {
        registry.register(
            owner,
            name,
            "(Ljava/lang/String;Ljava/nio/charset/Charset;)[B",
            jla_get_bytes_utf8_no_repl,
        );
    }
    registry.register(
        owner,
        "newStackTraceElement",
        "(Ljava/lang/StackWalker$StackFrame;)Ljava/lang/StackTraceElement;",
        jla_new_stack_trace_element,
    );
    registry.register(
        owner,
        "getDeclaredPublicMethods",
        "(Ljava/lang/Class;Ljava/lang/String;[Ljava/lang/Class;)Ljava/util/List;",
        jla_get_declared_public_methods_list,
    );
    registry.register(
        owner,
        "getDeclaredPublicMethods",
        "(Ljava/lang/Class;)[Ljava/lang/reflect/Method;",
        jla_get_declared_public_methods,
    );
    registry.register(
        owner,
        "getMethodsOrNull",
        "(Ljava/lang/Class;)[Ljava/lang/reflect/Method;",
        jla_get_methods_or_null,
    );
    // BootLoader.<clinit> module bootstrap (see handler docs).
    registry.register(
        owner,
        "defineUnnamedModule",
        "(Ljava/lang/ClassLoader;)Ljava/lang/Module;",
        jla_define_unnamed_module,
    );
    registry.register(
        owner,
        "addEnableNativeAccess",
        "(Ljava/lang/Module;)Ljava/lang/Module;",
        jla_add_enable_native_access,
    );
    registry.register(
        owner,
        "getConstantPool",
        "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;",
        jla_get_constant_pool,
    );
    registry.register(
        owner,
        "start",
        "(Ljava/lang/Thread;Ljdk/internal/vm/ThreadContainer;)V",
        jla_start_in_container,
    );
    // `jdk.internal.reflect.ClassDefiner` (JUnit5/Arquillian serialization-
    // constructor-accessor generation) — see `jla_define_class` doc comment.
    registry.register(
        owner,
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        jla_define_class,
    );
    registry.register(
        owner,
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        jla_define_class_hidden,
    );
    // `jdk.internal.loader.BootLoader.loadClassOrNull` — see
    // `jla_find_bootstrap_class_or_null` doc comment.
    registry.register(
        owner,
        "findBootstrapClassOrNull",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        jla_find_bootstrap_class_or_null,
    );
    registry.register(
        owner,
        "findNative",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;)J",
        jla_find_native,
    );
    registry.register(
        owner,
        "join",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;I)Ljava/lang/String;",
        jla_join,
    );
    // Also register on the interface so direct invokeinterface
    // dispatch (when the receiver's concrete class lookup falls
    // back to the interface class) still hits these natives.
    let iface = "jdk/internal/access/JavaLangAccess";
    registry.register(
        iface,
        "currentCarrierThread",
        "()Ljava/lang/Thread;",
        jla_current_carrier_thread,
    );
    registry.register(
        iface,
        "currentThread0",
        "()Ljava/lang/Thread;",
        jla_current_carrier_thread,
    );
    registry.register(
        iface,
        "blockedOn",
        "(Lsun/nio/ch/Interruptible;)V",
        jla_blocked_on,
    );
    registry.register(
        iface,
        "getCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
        jla_get_carrier_thread_local,
    );
    registry.register(
        iface,
        "setCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
        jla_set_carrier_thread_local,
    );
    registry.register(
        iface,
        "removeCarrierThreadLocal",
        "(Ljdk/internal/misc/CarrierThreadLocal;)V",
        jla_remove_carrier_thread_local,
    );
    registry.register(
        iface,
        "isCarrierThreadLocalPresent",
        "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
        jla_is_carrier_thread_local_present,
    );
    // BootLoader.<clinit> invokes these via `invokeinterface JavaLangAccess`.
    registry.register(
        iface,
        "defineUnnamedModule",
        "(Ljava/lang/ClassLoader;)Ljava/lang/Module;",
        jla_define_unnamed_module,
    );
    registry.register(
        iface,
        "addEnableNativeAccess",
        "(Ljava/lang/Module;)Ljava/lang/Module;",
        jla_add_enable_native_access,
    );
    registry.register(
        iface,
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        jla_define_class,
    );
    registry.register(
        iface,
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/Class;Ljava/lang/String;[BLjava/security/ProtectionDomain;ZILjava/lang/Object;)Ljava/lang/Class;",
        jla_define_class_hidden,
    );
    registry.register(
        iface,
        "getConstantPool",
        "(Ljava/lang/Class;)Ljdk/internal/reflect/ConstantPool;",
        jla_get_constant_pool,
    );
    registry.register(
        iface,
        "start",
        "(Ljava/lang/Thread;Ljdk/internal/vm/ThreadContainer;)V",
        jla_start_in_container,
    );
    registry.register(
        iface,
        "join",
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;I)Ljava/lang/String;",
        jla_join,
    );
    registry.register(
        iface,
        "findNative",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;)J",
        jla_find_native,
    );
    for name in ["getBytesNoRepl", "uncheckedGetBytesNoRepl"] {
        registry.register(
            iface,
            name,
            "(Ljava/lang/String;Ljava/nio/charset/Charset;)[B",
            jla_get_bytes_utf8_no_repl,
        );
    }
}

// JavaLangInvokeAccess --------------------------------------------------------

fn jlia_find_method_handle_type(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Delegates to MethodType.methodType(Class, Class[]).  Owner C
    // (WP1.6) has the authoritative MethodHandle layer; this
    // bridge is a thin passthrough so SharedSecrets-dependent
    // callers at least get a non-null MethodType back before
    // WP1.6 lands the full lookup path.
    // INSTANCE method: args[0] = receiver (MethodHandleImpl$1),
    // args[1] = rtype Class, args[2] = ptypes Class[].
    if let (Some(ret), Some(params)) = (args.get(1), args.get(2)) {
        ctx.invoke(
            "java/lang/invoke/MethodType",
            "methodType",
            "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
            &[*ret, *params],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn jlia_link_method_handle_constant(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // `linkMethodHandleConstant` is the condy / ldc path for
    // CONSTANT_MethodHandle.  Deferred to Owner C's WP1.6.  For
    // now return null so that optional code paths fall back to
    // the bytecode implementation.
    Ok(Some(Value::Object(None)))
}

fn jlia_make_class_value_map(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // `makeClassValueMap()` returns a weak map keyed by Class.  We
    // back it with a simple HashMap — cratonvm doesn't currently
    // have weak Class references, but the map is only used for
    // caching and the leak surface is bounded by the number of
    // loaded classes (GC eventually evicts both).
    ctx.new_object_initialized("java/util/HashMap", "()V", &[])
}

fn register_java_lang_invoke_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/lang/invoke/MethodHandleImpl$1";
    registry.register(
        owner,
        "findMethodHandleType",
        "(Ljava/lang/Class;[Ljava/lang/Class;)Ljava/lang/invoke/MethodType;",
        jlia_find_method_handle_type,
    );
    registry.register(
        owner,
        "linkMethodHandleConstant",
        "(Ljava/lang/Class;ILjava/lang/Class;Ljava/lang/String;Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        jlia_link_method_handle_constant,
    );
    registry.register(
        owner,
        "makeClassValueMap",
        "()Ljava/util/Map;",
        jlia_make_class_value_map,
    );
}

// JavaLangRefAccess -----------------------------------------------------------

fn jlra_wait_for_reference_processing(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Rustjvm's reference processor runs inline with GC — no
    // pending work by the time this call returns.
    Ok(Some(Value::Int(0)))
}

fn jlra_run_finalization(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Finalization is driven by the FinalizerThread, already
    // polled on each GC cycle.  Calling here is a nudge — still
    // a no-op until the polled channel is empty.
    Ok(None)
}

fn register_java_lang_ref_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/lang/ref/Reference$1";
    registry.register(
        owner,
        "waitForReferenceProcessing",
        "()Z",
        jlra_wait_for_reference_processing,
    );
    registry.register(owner, "runFinalization", "()V", jlra_run_finalization);
}

// JavaLangReflectAccess -------------------------------------------------------

fn jlrefa_copy_method(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // The real JDK `copyMethod` clones a Method with a fresh
    // `override` flag so `setAccessible(true)` doesn't mutate the
    // cached prototype.  Our Method mirror is mutable-by-design
    // (SetAccessible edits the `override` field directly), so
    // returning the same reference preserves existing semantics.
    // INSTANCE method: args[0] = receiver (ReflectAccess), args[1] = the
    // Method to copy.
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_copy_field(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (ReflectAccess), args[1] = the
    // Field to copy.
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_copy_constructor(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (ReflectAccess), args[1] = the
    // Constructor to copy.
    Ok(Some(args.get(1).copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_new_parameter(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // `newParameter(Executable, int, String, int)` — construct a
    // `java.lang.reflect.Parameter`.  We follow the JDK layout:
    // slots (executable, index, name, modifiers).
    let param = ctx.new_object("java/lang/reflect/Parameter")?;
    if let Some(Value::Object(Some(obj))) = param.clone() {
        // INSTANCE method: args[0] = receiver (ReflectAccess), then the
        // (executable, index, name, modifiers) params at args[1..5].
        let exec = args.get(1).copied().unwrap_or(Value::Object(None));
        let index = args.get(2).copied().unwrap_or(Value::Int(0));
        let name = args.get(3).copied().unwrap_or(Value::Object(None));
        let modifiers = args.get(4).copied().unwrap_or(Value::Int(0));
        ctx.set_field_by_name(obj, "executable", exec);
        ctx.set_field_by_name(obj, "index", index);
        ctx.set_field_by_name(obj, "name", name);
        ctx.set_field_by_name(obj, "modifiers", modifiers);
    }
    Ok(param)
}

fn jlrefa_new_accessible_object(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    ctx.new_object("java/lang/reflect/AccessibleObject")
}

fn jlrefa_get_executable_type_annotation_bytes(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Type annotations are not stored on our reflective Method /
    // Constructor mirrors.  Returning an empty byte[] is
    // spec-compatible — callers treat it as "no type annotations".
    let arr = ctx.new_array(ArrayElementType::Byte, 0);
    Ok(Some(Value::Object(Some(arr))))
}

fn register_java_lang_reflect_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/lang/reflect/ReflectAccess";
    registry.register(
        owner,
        "copyMethod",
        "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
        jlrefa_copy_method,
    );
    registry.register(
        owner,
        "copyField",
        "(Ljava/lang/reflect/Field;)Ljava/lang/reflect/Field;",
        jlrefa_copy_field,
    );
    registry.register(
        owner,
        "copyConstructor",
        "(Ljava/lang/reflect/Constructor;)Ljava/lang/reflect/Constructor;",
        jlrefa_copy_constructor,
    );
    registry.register(
        owner,
        "newParameter",
        "(Ljava/lang/reflect/Executable;ILjava/lang/String;I)Ljava/lang/reflect/Parameter;",
        jlrefa_new_parameter,
    );
    registry.register(
        owner,
        "newAccessibleObject",
        "()Ljava/lang/reflect/AccessibleObject;",
        jlrefa_new_accessible_object,
    );
    registry.register(
        owner,
        "getExecutableTypeAnnotationBytes",
        "(Ljava/lang/reflect/Executable;)[B",
        jlrefa_get_executable_type_annotation_bytes,
    );
}

// JavaIOAccess ----------------------------------------------------------------

fn jioa_console(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // No attached console (cratonvm runs non-interactively by default).
    // Returning null matches HotSpot behaviour when stdin is not a tty.
    Ok(Some(Value::Object(None)))
}

fn jioa_charset(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return the platform charset (UTF-8).
    let utf8 = ctx.create_string("UTF-8");
    ctx.invoke(
        "java/nio/charset/Charset",
        "forName",
        "(Ljava/lang/String;)Ljava/nio/charset/Charset;",
        &[Value::Object(Some(utf8))],
    )
}

fn register_java_io_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/io/Console$1";
    registry.register(owner, "console", "()Ljava/io/Console;", jioa_console);
    registry.register(
        owner,
        "charset",
        "()Ljava/nio/charset/Charset;",
        jioa_charset,
    );
}

// JavaIORandomAccessFileAccess ------------------------------------------------

fn jiorafa_open(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Signature: (String name, String mode) -> RandomAccessFile.
    // INSTANCE method: args[0] = receiver (JavaIORandomAccessFileAccess$1),
    // args[1] = name, args[2] = mode. Allocate a fresh RandomAccessFile and
    // run its (name, mode) constructor on THAT object. The old code passed
    // the whole receiver-first `args` slice to `<init>` (so it initialised the
    // access-bridge singleton as a RAF) and then returned a *different*,
    // uninitialised RandomAccessFile from `new_object`.
    let name = args.get(1).copied().unwrap_or(Value::Object(None));
    let mode = args.get(2).copied().unwrap_or(Value::Object(None));
    ctx.new_object_initialized(
        "java/io/RandomAccessFile",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        &[name, mode],
    )
}

fn jiorafa_open_as_channel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Return a FileChannel backed by the RAF.  The real RAF
    // getChannel() native already builds one.
    // INSTANCE method: args[0] = receiver, args[1] = the RandomAccessFile.
    if let Some(Value::Object(Some(raf))) = args.get(1) {
        ctx.invoke(
            "java/io/RandomAccessFile",
            "getChannel",
            "()Ljava/nio/channels/FileChannel;",
            &[Value::Object(Some(*raf))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

// JavaIOFileDescriptorAccess --------------------------------------------------
//
// `SharedSecrets.getJavaIOFileDescriptorAccess()` backs the real
// `sun.nio.ch.FileChannelImpl` constructor (which reads `getAppend(fd)`),
// `FileOutputStream`/`RandomAccessFile`, and the NIO file path. The accessor
// just reads/writes the FileDescriptor's `fd`/`handle`/`append` fields, so the
// methods are thin field accessors; cleanup hooks are no-ops (CratonVM closes
// via the fd_table, not a PhantomCleanable). args[0] is the accessor receiver,
// args[1] the FileDescriptor, args[2] the value (setters).
fn jiofd_get(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.get(1) {
        Some(Value::Object(Some(fd))) => match ctx.get_field_by_name(*fd, "fd") {
            Value::Int(i) => i,
            _ => -1,
        },
        _ => -1,
    };
    Ok(Some(Value::Int(v)))
}

fn jiofd_set(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(fd))) = args.get(1) {
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(-1);
        ctx.set_field_by_name(*fd, "fd", Value::Int(v));
    }
    Ok(None)
}

fn jiofd_get_append(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.get(1) {
        Some(Value::Object(Some(fd))) => match ctx.get_field_by_name(*fd, "append") {
            Value::Int(i) => i,
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Value::Int(v)))
}

fn jiofd_set_append(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(fd))) = args.get(1) {
        let v = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field_by_name(*fd, "append", Value::Int(v));
    }
    Ok(None)
}

fn jiofd_get_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let v = match args.get(1) {
        Some(Value::Object(Some(fd))) => match ctx.get_field_by_name(*fd, "handle") {
            Value::Long(l) => l,
            Value::Int(i) => i as i64,
            _ => -1,
        },
        _ => -1,
    };
    Ok(Some(Value::Long(v)))
}

fn jiofd_set_handle(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    if let Some(Value::Object(Some(fd))) = args.get(1) {
        let v = match args.get(2) {
            Some(Value::Long(l)) => *l,
            Some(Value::Int(i)) => *i as i64,
            _ => -1,
        };
        ctx.set_field_by_name(*fd, "handle", Value::Long(v));
    }
    Ok(None)
}

fn jiofd_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // close / registerCleanup / unregisterCleanup — CratonVM owns fd lifetime
    // through the fd_table + explicit channel close natives, so these
    // SharedSecrets hooks are no-ops.
    Ok(None)
}

fn register_java_io_fd_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/io/FileDescriptor$1";
    registry.register(owner, "get", "(Ljava/io/FileDescriptor;)I", jiofd_get);
    registry.register(owner, "set", "(Ljava/io/FileDescriptor;I)V", jiofd_set);
    registry.register(
        owner,
        "getAppend",
        "(Ljava/io/FileDescriptor;)Z",
        jiofd_get_append,
    );
    registry.register(
        owner,
        "setAppend",
        "(Ljava/io/FileDescriptor;Z)V",
        jiofd_set_append,
    );
    registry.register(
        owner,
        "getHandle",
        "(Ljava/io/FileDescriptor;)J",
        jiofd_get_handle,
    );
    registry.register(
        owner,
        "setHandle",
        "(Ljava/io/FileDescriptor;J)V",
        jiofd_set_handle,
    );
    registry.register(owner, "close", "(Ljava/io/FileDescriptor;)V", jiofd_noop);
    registry.register(
        owner,
        "registerCleanup",
        "(Ljava/io/FileDescriptor;)V",
        jiofd_noop,
    );
    registry.register(
        owner,
        "registerCleanup",
        "(Ljava/io/FileDescriptor;Ljdk/internal/ref/PhantomCleanable;)V",
        jiofd_noop,
    );
    registry.register(
        owner,
        "unregisterCleanup",
        "(Ljava/io/FileDescriptor;)V",
        jiofd_noop,
    );
}

fn register_java_io_raf_access(registry: &mut NativeMethodRegistry) {
    let owner = "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1";
    registry.register(
        owner,
        "open",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/RandomAccessFile;",
        jiorafa_open,
    );
    registry.register(
        owner,
        "openAsChannel",
        "(Ljava/io/RandomAccessFile;)Ljava/nio/channels/FileChannel;",
        jiorafa_open_as_channel,
    );
}

// JavaNetInetAddressAccess ----------------------------------------------------

fn jniaa_get_host_from_name_service(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Fall back to `getHostName` on the InetAddress.
    // INSTANCE method: args[0] = receiver (InetAddress$1), args[1] = addr.
    if let Some(Value::Object(Some(addr))) = args.get(1) {
        ctx.invoke(
            "java/net/InetAddress",
            "getHostName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(*addr))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn jniaa_get_original_host_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (InetAddress$1), args[1] = addr.
    if let Some(Value::Object(Some(addr))) = args.get(1) {
        // `holder.originalHostName` in real JDK.  We store the
        // name in slot `originalHostName` if present, otherwise
        // fall back to the host name.
        let orig = ctx.get_field_by_name(*addr, "originalHostName");
        if matches!(orig, Value::Object(Some(_))) {
            return Ok(Some(orig));
        }
        ctx.invoke(
            "java/net/InetAddress",
            "getHostName",
            "()Ljava/lang/String;",
            &[Value::Object(Some(*addr))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn register_java_net_inet_address_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/net/InetAddress$1";
    registry.register(
        owner,
        "getHostFromNameService",
        "(Ljava/net/InetAddress;Z)Ljava/lang/String;",
        jniaa_get_host_from_name_service,
    );
    registry.register(
        owner,
        "getOriginalHostName",
        "(Ljava/net/InetAddress;)Ljava/lang/String;",
        jniaa_get_original_host_name,
    );
}

// JavaNetUriAccess ------------------------------------------------------------

fn jnuri_create(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (String scheme, String ssp) -> URI: forwards to `URI.create(scheme:ssp)`.
    // INSTANCE method: args[0] = receiver (JavaNetUriAccess$1),
    // args[1] = scheme, args[2] = ssp.
    let scheme = match args.get(1) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let ssp = match args.get(2) {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let combined = format!("{scheme}:{ssp}");
    let s = ctx.create_string(&combined);
    ctx.invoke(
        "java/net/URI",
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        &[Value::Object(Some(s))],
    )
}

fn register_java_net_uri_access(registry: &mut NativeMethodRegistry) {
    let owner = "cratonvm/internal/ss/JavaNetUriAccess$1";
    registry.register(
        owner,
        "create",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URI;",
        jnuri_create,
    );
}

// JavaNioAccess ---------------------------------------------------------------

fn jnio_new_direct_byte_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // JavaNioAccess.newDirectByteBuffer(long addr, int cap[, Object att,
    // MemorySegment seg]) -> a DirectByteBuffer wrapping the native address.
    // args = [this(Buffer$2), addr, cap, ...]. JDK-25 has no `DirectByteBuffer(long,int)`
    // ctor; the real `Buffer$2` does `new DirectByteBuffer(addr, cap, att, seg)`
    // via the 4-arg `(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V`
    // constructor. (The old handler invoked a non-existent `(JI)V` ctor on the
    // wrong receiver and returned an uninitialized buffer.)
    let addr = args.get(1).cloned().unwrap_or(Value::Long(0));
    let cap = args.get(2).cloned().unwrap_or(Value::Int(0));
    let att = args.get(3).cloned().unwrap_or(Value::Object(None));
    if crate::vmflags().io.dbg_net {
        eprintln!("[NET] newDirectByteBuffer addr={:?} cap={:?}", addr, cap);
    }
    ctx.new_object_initialized(
        "java/nio/DirectByteBuffer",
        "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V",
        &[addr, cap, att, Value::Object(None)],
    )
}

fn jnio_acquire_session(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Scoped memory sessions — return null so callers treat it
    // as "no session" and the regular strong-reference path runs.
    Ok(Some(Value::Object(None)))
}

/// JDK-25 `JavaNioAccess.acquireSession(Buffer)` / `releaseSession(Buffer)`
/// are `void` (the older overload above returned a `MemorySegment$Scope`).
/// They pin/unpin the buffer's `MemorySessionImpl` for the duration of a
/// native I/O so the backing memory can't be freed concurrently. Every
/// `FileChannel`/`SocketChannel` op on a direct buffer (including the temp
/// direct buffer the heap-buffer path substitutes) calls them via
/// `IOUtil.acquireScope`. CratonVM's direct/arena memory is never freed
/// underneath an in-flight native, so pinning is unnecessary: no-op.
fn jnio_session_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// JDK-25 `JavaNioAccess.hasSession(Buffer)` — whether the buffer is backed
/// by a non-global memory session. CratonVM tracks none, so always false.
fn jnio_has_session(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// JDK-25 `JavaNioAccess.getBufferAddress(Buffer)J` — the native `address`
/// field of the buffer (0 for a pure heap buffer; the direct/arena handle for
/// a direct buffer). Used by the heap→direct copy and the channel I/O path
/// to locate the off-heap bytes. Read the field by name so it works under the
/// real `java.nio.Buffer` layout.
fn jnio_get_buffer_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let addr = match args.get(1) {
        Some(Value::Object(Some(buf))) => match ctx.get_field_by_name(*buf, "address") {
            Value::Long(a) => a,
            Value::Int(a) => a as i64,
            _ => 0,
        },
        _ => 0,
    };
    Ok(Some(Value::Long(addr)))
}

/// JDK-25 `JavaNioAccess.getBufferBase(Buffer)Ljava/lang/Object;` — the heap
/// backing array (`hb`) of the buffer, or null for a direct buffer. Together
/// with `getBufferAddress` this gives `ScopedMemoryAccess` the (base,offset)
/// pair for a bulk copy.
fn jnio_get_buffer_base(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let base = match args.get(1) {
        Some(Value::Object(Some(buf))) => ctx.get_field_by_name(*buf, "hb"),
        _ => Value::Object(None),
    };
    // Normalize any non-reference (e.g. zero-default `Int(0)`) to null.
    Ok(Some(match base {
        Value::Object(o) => Value::Object(o),
        _ => Value::Object(None),
    }))
}

/// JDK-25 `JavaNioAccess.scaleShifts(Buffer)I` — `log2(element width)` of the
/// buffer's element type, i.e. what the JDK's own `Buffer.scaleShifts()`
/// overrides return (0 byte, 1 char/short, 2 int/float, 3 long/double).
///
/// Decided from the receiver's runtime class name, which is how the JDK's own
/// pre-25 implementation (`AbstractMemorySegmentImpl.getScaleFactor`) did it
/// via an `instanceof` chain. View classes are named `ByteBufferAsIntBufferB`,
/// where the element type is the SECOND token — so take the token with the
/// greatest index, not the first match.
fn jnio_scale_shifts(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let buf = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        // Real `scaleShifts(null)` NPEs on the virtual call; nothing on our
        // paths passes null, and 0 (byte) is the conservative answer.
        _ => return Ok(Some(Value::Int(0))),
    };
    let class_id = ctx.class_id_of_object(buf);
    let name = ctx.class_name_of_id(class_id).unwrap_or_default();
    const TOKENS: [(&str, i32); 7] = [
        ("ByteBuffer", 0),
        ("CharBuffer", 1),
        ("ShortBuffer", 1),
        ("IntBuffer", 2),
        ("FloatBuffer", 2),
        ("LongBuffer", 3),
        ("DoubleBuffer", 3),
    ];
    let mut best: Option<(usize, i32)> = None;
    for (token, shift) in TOKENS {
        if let Some(pos) = name.rfind(token) {
            if best.is_none_or(|(p, _)| pos > p) {
                best = Some((pos, shift));
            }
        }
    }
    Ok(Some(Value::Int(best.map_or(0, |(_, shift)| shift))))
}

/// JDK-25 `JavaNioAccess.isThreadConfined(Buffer)Z` — whether the buffer's
/// session is confined to the current thread. CratonVM has no sessions, so
/// the buffer is never confined.
fn jnio_is_thread_confined(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// JDK-25 `JavaNioAccess.reserveMemory(long size, long cap)V` /
/// `unreserveMemory(...)V` — `java.nio.Bits` direct-memory accounting. The
/// real DirectByteBuffer accounting already runs in `direct_buffer.rs`
/// (`try_reserve`/`release`); these SharedSecrets hooks are a no-op so the
/// JDK's parallel `Bits` counters don't double-count or block.
fn jnio_reserve_memory_noop(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// JDK-25 `JavaNioAccess.pageSize()I` — OS page size. 4 KiB on every target.
fn jnio_page_size(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(4096)))
}

fn register_java_nio_access(registry: &mut NativeMethodRegistry) {
    // The `BufferPoolMXBean` surface. Registered from HERE rather than from
    // `jmx.rs`, which is gated on the `management` feature: `JavaNioAccess
    // .getBufferPool` and `VM.getDirectBufferPool` below hand out these beans
    // on every build, so their methods have to be registered on every build too.
    register_buffer_pool_mxbean(registry);
    let owner = "java/nio/Buffer$2";
    // JDK-25 JavaNioAccess overloads: 2-arg `(JI)` (used by the NIO socket
    // read/write path via SocketDispatcher) and 4-arg
    // `(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)`.
    registry.register(
        owner,
        "newDirectByteBuffer",
        "(JI)Ljava/nio/ByteBuffer;",
        jnio_new_direct_byte_buffer,
    );
    registry.register(
        owner,
        "newDirectByteBuffer",
        "(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)Ljava/nio/ByteBuffer;",
        jnio_new_direct_byte_buffer,
    );
    registry.register(
        owner,
        "acquireSession",
        "(Ljava/nio/Buffer;)Ljava/lang/foreign/MemorySegment$Scope;",
        jnio_acquire_session,
    );
    // JDK-25 signatures: `void acquireSession(Buffer)` /
    // `void releaseSession(Buffer)` / `boolean hasSession(Buffer)`. These are
    // what `IOUtil.acquireScope`/`releaseScope` call on every channel I/O of a
    // (temp) direct buffer; without them every FileChannel/SocketChannel op
    // hits a linkage error after the interruptible-channel begin/end runs.
    registry.register(
        owner,
        "acquireSession",
        "(Ljava/nio/Buffer;)V",
        jnio_session_noop,
    );
    registry.register(
        owner,
        "releaseSession",
        "(Ljava/nio/Buffer;)V",
        jnio_session_noop,
    );
    registry.register(
        owner,
        "hasSession",
        "(Ljava/nio/Buffer;)Z",
        jnio_has_session,
    );
    // `JavaNioAccess.scaleShifts(Buffer)` delegates to the package-private
    // `Buffer.scaleShifts()`, which every concrete buffer implements as
    // `Integer.numberOfTrailingZeros(<element width in bytes>)` — verified with
    // `javap -c` on jdk-25 java.nio.{Char,Short,Int,Long,Float,Double}Buffer.
    // It is log2(element size), not a constant.
    //
    // STUB-REMOVAL (wave 4): the flat `0` was justified as "CratonVM addresses
    // every Buffer in bytes, so a non-zero shift would MIS-SCALE the reads".
    // That is backwards for the caller that exists —
    // `AbstractMemorySegmentImpl.ofBuffer` computes
    // `offset = address + (position << shift)` and `len = remaining << shift`
    // precisely BECAUSE `position`/`remaining` are in ELEMENTS, not bytes; a
    // flat 0 gave an int/long view a segment a quarter/eighth of its real size.
    // `0` is still the answer for every ByteBuffer, so the Spring STOMP/Netty
    // paths the old comment worried about are unchanged.
    registry.register(
        owner,
        "scaleShifts",
        "(Ljava/nio/Buffer;)I",
        jnio_scale_shifts,
    );
    // Some real-JDK builds expose the JavaNioAccess anonymous implementation as
    // Buffer$1 rather than Buffer$2; register both owners to avoid linkage drift.
    registry.register(
        "java/nio/Buffer$1",
        "scaleShifts",
        "(Ljava/nio/Buffer;)I",
        jnio_scale_shifts,
    );
    registry.register(
        owner,
        "isThreadConfined",
        "(Ljava/nio/Buffer;)Z",
        jnio_is_thread_confined,
    );
    registry.register(
        owner,
        "getBufferAddress",
        "(Ljava/nio/Buffer;)J",
        jnio_get_buffer_address,
    );
    registry.register(
        owner,
        "getBufferBase",
        "(Ljava/nio/Buffer;)Ljava/lang/Object;",
        jnio_get_buffer_base,
    );
    registry.register(owner, "reserveMemory", "(JJ)V", jnio_reserve_memory_noop);
    registry.register(owner, "unreserveMemory", "(JJ)V", jnio_reserve_memory_noop);
    registry.register(owner, "pageSize", "()I", jnio_page_size);
    // RKC16N.10 follow-on / RKC16N.11 / TOMCAT0807: legacy SharedSecrets
    // accessor used by JDK 17's `VM$BufferPoolsHolder.<clinit>`.
    // JDK 25 exposes JavaNioAccess as Buffer$2, JDK 17 as Buffer$1, and
    // `getDirectBufferPool` is NOT registered, deliberately. MEASURED with
    // `javap -p -c` on JDK 25, the real method is two instructions:
    //
    //     java.nio.Buffer$2.getDirectBufferPool()
    //       0: getstatic  java/nio/Bits.BUFFER_POOL
    //       3: areturn
    //
    // — a real object real bytecode already builds, needing no carrier from us.
    // The native that used to stand here handed back a FABRICATED one, and
    // under `--jdk-only` that fabrication is refused. Its caller is
    // `jdk/internal/misc/VM$BufferPoolsHolder.<clinit>`, so the refusal did not
    // surface as a catchable throwable: it poisoned `jdk.internal.misc.VM` for
    // the life of the process and took the ENTIRE platform MBeanServer with it,
    // which is what reddened `RJdkJmx`. See `WORKER-3-NOTE-9` and `-NOTE-10`.

    // RKC16N.11's four `VM$BufferPool` methods USED TO BE FOUR STATELESS
    // LAMBDAS RETURNING ZERO, registered here. They now live in
    // [`register_buffer_pool_mxbean`], on all three names a pool receiver
    // can carry (`jdk/internal/misc/VM$BufferPool` included), backed by the
    // direct-memory allocator's live counters. Registering them here as well
    // would be a second answer racing the first on registration order.
}

// ---------------------------------------------------------------------------
// java.lang.management.BufferPoolMXBean
// ---------------------------------------------------------------------------

/// The concrete class CratonVM stamps onto its `BufferPoolMXBean` instances.
///
/// Concrete, not the `java/lang/management/BufferPoolMXBean` interface it used
/// to be: an instance whose class is an interface finds only abstract methods,
/// and native lookup drops interface-declared instance natives. Same rule, and
/// the same remedy, as `panama::CRATON_SEGMENT_CLASS` and
/// `CRATON_SYSTEM_LOGGER_CLASS`; the type relationships are declared in
/// `vm/src/runtime/interpreter/typecheck.rs`'s `synthetic_implements`.
pub(crate) const CRATON_BUFFER_POOL_CLASS: &str = "cratonvm/internal/BufferPool";
/// Slot 0 — the pool's name, as a Java `String`.
const BUFFER_POOL_SLOT_NAME: usize = 0;
/// Slot 1 — [`BufferPoolKind`], as an int.
const BUFFER_POOL_SLOT_KIND: usize = 1;
const BUFFER_POOL_SLOTS: usize = 2;

/// The pools HotSpot publishes, in the order it publishes them.
///
/// MEASURED on Temurin 25.0.3+9 (`scratchpad/PoolNames.java`):
///
/// ```text
/// POOL name=mapped   count=0 used=0 cap=0 objName=java.nio:type=BufferPool,name=mapped
/// POOL name=direct   count=0 used=0 cap=0 objName=java.nio:type=BufferPool,name=direct
/// POOL name=mapped - 'non-volatile memory' ...
/// ```
///
/// The order is part of the observable answer — `getPlatformMXBeans` returns a
/// `List` and callers index it — so it is reproduced rather than sorted.
const BUFFER_POOL_NAMES: [(&str, i32); 3] = [
    ("mapped", BUFFER_POOL_KIND_MAPPED),
    ("direct", BUFFER_POOL_KIND_DIRECT),
    (
        "mapped - 'non-volatile memory'",
        BUFFER_POOL_KIND_NON_VOLATILE,
    ),
];

/// Backed by the direct-memory allocator's own live counters.
const BUFFER_POOL_KIND_DIRECT: i32 = 0;
/// `FileChannel.map` buffers. CratonVM does not account for them separately, so
/// this pool reports zeros — which is also what a HotSpot process that has
/// mapped nothing reports, and is a measured zero rather than a stub: nothing
/// in this VM increments a mapped-buffer counter, so any other number would be
/// invented.
const BUFFER_POOL_KIND_MAPPED: i32 = 1;
/// The `mapped - 'non-volatile memory'` pool. Non-volatile mapping is an
/// `ExtendedMapMode` feature CratonVM does not implement at all, so this one is
/// zero by construction.
const BUFFER_POOL_KIND_NON_VOLATILE: i32 = 2;

/// Allocate one `BufferPoolMXBean` carrier.
fn alloc_buffer_pool(
    ctx: &mut dyn NativeContext,
    name: &str,
    kind: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let class_id = match ctx.class_id_by_name(CRATON_BUFFER_POOL_CLASS) {
        Some(id) => id,
        None => match ctx.try_ensure_synthetic_class(CRATON_BUFFER_POOL_CLASS, BUFFER_POOL_SLOTS) {
            Ok(id) => id,
            // Strict mode refused the fabrication. There is no real class to
            // stand in — `ManagementFactoryHelper.getBufferPoolMXBeans` reaches
            // its pools through `SharedSecrets`, i.e. back through this file —
            // so the refusal stands as a catchable throwable rather than a
            // silently empty list.
            Err(err) => return Err(cratonvm_native_api::refusal_to_java_failure(ctx, err)),
        },
    };
    let slots = BUFFER_POOL_SLOTS.max(ctx.class_num_total_fields(class_id));
    let pool = ctx
        .try_alloc_object_gc_safe(class_id, slots)
        .unwrap_or_else(|| ctx.alloc_object(class_id, slots));
    // The name is allocated AFTER the carrier and written straight in, so there
    // is no window in which a moving young generation can relocate one of the
    // two behind the other's back.
    let pool_pin = ctx.pin_native_root(pool);
    let name_obj = ctx.create_string(name);
    let pool = ctx.read_native_pin(pool_pin, pool);
    ctx.unpin_native_roots(pool_pin);
    ctx.set_field(pool, BUFFER_POOL_SLOT_NAME, Value::Object(Some(name_obj)));
    ctx.set_field(pool, BUFFER_POOL_SLOT_KIND, Value::Int(kind));
    Ok(pool)
}

/// The `"direct"` pool — the one every caller in the JDK's own code asks for by
/// name (`VM.getDirectBufferPool`, `JavaNioAccess.getBufferPool`).

/// All three pools, in HotSpot's order — the answer to
/// `ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)`.
pub(crate) fn alloc_all_buffer_pools(
    ctx: &mut dyn NativeContext,
) -> Result<Vec<ObjectRef>, MethodCallFailed> {
    // A refused fabrication must NOT propagate from here. `alloc_buffer_pool`
    // deliberately turns the refusal into a throwable, and for
    // `JavaNioAccess.getBufferPool()` that is right. This is the LIST, and it
    // answers `ManagementFactory.getPlatformMXBeans(BufferPoolMXBean.class)`
    // during platform-server init -- so a throw here takes out every platform
    // MXBean, not just the pools. MEASURED: that is what reddened `RJdkJmx`,
    // whose checks are almost entirely about other beans.
    //
    // Empty is what this list answered before the pools became real, and it is
    // what HotSpot's own contract permits (the list is not specified to be
    // non-empty). The pools are still real everywhere the carrier CAN be built.
    if ctx.class_id_by_name(CRATON_BUFFER_POOL_CLASS).is_none()
        && ctx
            .try_ensure_synthetic_class(CRATON_BUFFER_POOL_CLASS, BUFFER_POOL_SLOTS)
            .is_err()
    {
        return Ok(Vec::new());
    }
    let mut pools = Vec::with_capacity(BUFFER_POOL_NAMES.len());
    for (name, kind) in BUFFER_POOL_NAMES {
        // Each pool is pinned while the NEXT one is allocated: `alloc_buffer_pool`
        // allocates twice (the carrier and its name), and a young-generation move
        // in between would leave every earlier element of this vector stale. This
        // is the `build_rooted_ref_array` discipline, open-coded because the
        // caller wants a `Vec`, not a Java array.
        let pins: Vec<usize> = pools.iter().map(|p| ctx.pin_native_root(*p)).collect();
        let pool = alloc_buffer_pool(ctx, name, kind);
        for (slot, pin) in pools.iter_mut().zip(pins.iter()) {
            *slot = ctx.read_native_pin(*pin, *slot);
        }
        if let Some(first) = pins.first() {
            ctx.unpin_native_roots(*first);
        }
        pools.push(pool?);
    }
    Ok(pools)
}

fn buffer_pool_kind(ctx: &dyn NativeContext, this: ObjectRef) -> i32 {
    match ctx.get_field(this, BUFFER_POOL_SLOT_KIND) {
        Value::Int(k) => k,
        // A receiver from the legacy `jdk/internal/misc/VM$BufferPool` stamp
        // carries no kind slot. That stamp only ever named the direct pool, so
        // that is the answer, not a zeroed "mapped".
        _ => BUFFER_POOL_KIND_DIRECT,
    }
}

fn buffer_pool_get_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if let Value::Object(Some(name)) = ctx.get_field(this, BUFFER_POOL_SLOT_NAME) {
        return Ok(Some(Value::Object(Some(name))));
    }
    // Legacy stamp, no name slot — see `buffer_pool_kind`.
    let s = ctx.create_string("direct");
    Ok(Some(Value::Object(Some(s))))
}

fn buffer_pool_get_count(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let count = if buffer_pool_kind(ctx, this) == BUFFER_POOL_KIND_DIRECT {
        cratonvm_native_io::direct_buffer::direct_buffer_pool_stats().0
    } else {
        0
    };
    Ok(Some(Value::Long(count)))
}

fn buffer_pool_get_memory_used(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let used = if buffer_pool_kind(ctx, this) == BUFFER_POOL_KIND_DIRECT {
        cratonvm_native_io::direct_buffer::direct_buffer_pool_stats().1
    } else {
        0
    };
    Ok(Some(Value::Long(used)))
}

/// `getTotalCapacity()`. See `direct_buffer_pool_stats` for why this is the
/// same number as `getMemoryUsed()` rather than a second counter.
fn buffer_pool_get_total_capacity(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    buffer_pool_get_memory_used(ctx, args)
}

/// `getObjectName()` — `java.nio:type=BufferPool,name=<pool name>`, the string
/// HotSpot builds (measured; see [`BUFFER_POOL_NAMES`]).
///
/// Answers `null` if `ObjectName` cannot be constructed rather than
/// propagating: this is the one method on the bean that nothing needs in order
/// to read a counter, and a JMX failure here would take out
/// `getPlatformMXBeans` for callers that only wanted `getCount()`.
fn buffer_pool_get_object_name(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let name = match buffer_pool_get_name(ctx, args)? {
        Some(Value::Object(Some(obj))) => ctx.read_string(obj).unwrap_or_default(),
        _ => String::new(),
    };
    let _ = this;
    let text = format!("java.nio:type=BufferPool,name={name}");
    let arg = ctx.create_string(&text);
    match ctx.invoke(
        "javax/management/ObjectName",
        "getInstance",
        "(Ljava/lang/String;)Ljavax/management/ObjectName;",
        &[Value::Object(Some(arg))],
    ) {
        Ok(Some(value)) => Ok(Some(value)),
        _ => Ok(Some(Value::Object(None))),
    }
}

/// The five `BufferPoolMXBean` methods, on every name a receiver can carry.
pub(crate) fn register_buffer_pool_mxbean(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    for owner in [
        CRATON_BUFFER_POOL_CLASS,
        // The interface, for a call site that resolved against its abstract
        // declaration, and the two JDK internal spellings a receiver could
        // still arrive with.
        "java/lang/management/BufferPoolMXBean",
        "jdk/internal/misc/VM$BufferPool",
    ] {
        r.register(
            owner,
            "getName",
            "()Ljava/lang/String;",
            buffer_pool_get_name,
        );
        r.register(owner, "getCount", "()J", buffer_pool_get_count);
        r.register(owner, "getMemoryUsed", "()J", buffer_pool_get_memory_used);
        r.register(
            owner,
            "getTotalCapacity",
            "()J",
            buffer_pool_get_total_capacity,
        );
        r.register(
            owner,
            "getObjectName",
            "()Ljavax/management/ObjectName;",
            buffer_pool_get_object_name,
        );
    }
    r.set_category(__prev_cat);
}

// JavaSecurityAccess — DELETED 2026-08-13 (F33-1) --------------------------
//
// `register_java_security_access` and its two bodies
// (`jsec_do_intersection_privilege`, `jsec_get_protect_domains`) stood here,
// registering `doIntersectionPrivilege` and `getProtectDomains` on
// `java/security/AccessController$1`. Do not restore them; there is nothing to
// restore them FOR. Three independent facts, each measured on Microsoft
// 25.0.3+9-LTS:
//
//   $ javap -p jdk.internal.access.JavaSecurityAccess
//   Error: class not found: jdk.internal.access.JavaSecurityAccess
//   $ javap -p 'java.security.AccessController$1'
//   Error: class not found: java.security.AccessController$1
//   $ javap -p jdk.internal.access.SharedSecrets | grep -c getJavaSecurityAccess
//   0
//
// JEP 486 removed the Security Manager and took the whole interface with it, so
// the interface, its implementation class and its accessor are all absent —
// this was never a bridge to anything, on any JDK 25 image.
//
// REACHABILITY, re-verified before deleting, because `call_native` panics on an
// unregistered triple and a Rust panic is not a Java throwable — it kills the
// VM. Three doors, all closed:
//
//   * no bytecode can name the triples: a JDK 25 class file cannot hold an
//     invokeinterface whose receiver type its own image does not declare;
//   * no native mints the receiver: `grep -rn 'AccessController[$]1'` over
//     *.rs / *.java finds no `ensure_class_initialized`, `new_object` or
//     `alloc_object` on that name — the only mint site was the `f_jsec` factory
//     callback, deleted with `getJavaSecurityAccess` by F17-1;
//   * no factory hands the owner out: `owner_classes()` iterates `FACTORIES`,
//     which has had no `java/security/` row since F17-1.
//
// WHAT ELSE MOVED WITH IT. `native-api/src/no_image_receiver.rs` keeps its
// `java/security/AccessController$1` row on purpose — the image fact it records
// is TRUE and the gate script that re-derives the table should keep checking
// it. That entry was the ONLY thing tagging these two natives `SyntheticStub`;
// deleting it instead of the registrations would have promoted two fabricated
// natives to `Bridge` and admitted them to `--jdk-only`. The entry is now
// inert, which is the correct end state, and its doc says so.
//
// Two generated baselines still name the deleted triples —
// `scripts/baselines/jdk-only-gated-never-delete.tsv:86-87` and
// `jdk-only-kind-map-25-linux.tsv:5790-5791`. They must be REGENERATED from a
// Linux census, not hand-edited: a frozen census row edited by hand is a claim
// about a tree nobody censused. Both gate scripts exit 2 ("REFUSING") off
// Linux, so neither is red on a Windows or macOS checkout meanwhile.

// JavaUtilJarAccess -----------------------------------------------------------

/// `JavaUtilJarAccess.jarFileHasClassPathAttribute(JarFile)` — does this jar's
/// main manifest carry a `Class-Path` attribute?
///
/// **This used to hardcode `false`**, under the comment "Returning false is
/// spec-compatible — callers short-circuit the full attribute scan". It is not
/// spec-compatible and the short-circuit is the defect: this predicate is the
/// gate on `jdk.internal.loader.URLClassPath$JarLoader.getClassPath()`, so a
/// constant `false` tells the JDK's own loader that no jar in the process has a
/// `Class-Path` manifest entry. A `false` here is not a cheaper `true`; it is a
/// different answer, and the caller cannot tell.
///
/// It answers from the manifest now. `Attributes.getValue` is
/// case-insensitive on the attribute NAME, which is why the literal spelling
/// here does not have to match the file's.
///
/// A jar with no manifest answers `false` — that is the JDK's answer too, not a
/// fallback: `getManifest()` returns `null` and there is no attribute to find.
///
/// `throws IOException` is on the interface (`javap -p
/// jdk.internal.access.JavaUtilJarAccess`), so a failure to read the manifest
/// PROPAGATES rather than being swallowed into `false`. Swallowing it would
/// reintroduce the same defect in a narrower window.
///
/// GC NOTE: `manifest` and `attrs` outlive `create_string`, which allocates, so
/// both are rooted and re-read — the shape that put six holes in TLS alias
/// arrays on 2026-08-18.
fn jujar_jar_file_has_classpath_attribute(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let Some(Value::Object(Some(jar))) = args.get(1).copied() else {
        return Ok(Some(Value::Int(0)));
    };
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let jar_h = scope.root(jar);
    let jar = scope.get(&jar_h);
    let manifest =
        match scope.invoke_virtual(jar, "getManifest", "()Ljava/util/jar/Manifest;", &[])? {
            Some(Value::Object(Some(m))) => m,
            // No manifest: no attribute. Same answer the real body gives.
            _ => return Ok(Some(Value::Int(0))),
        };
    let manifest_h = scope.root(manifest);
    let manifest = scope.get(&manifest_h);
    let attrs = match scope.invoke_virtual(
        manifest,
        "getMainAttributes",
        "()Ljava/util/jar/Attributes;",
        &[],
    )? {
        Some(Value::Object(Some(a))) => a,
        _ => return Ok(Some(Value::Int(0))),
    };
    let attrs_h = scope.root(attrs);
    let key = scope.create_string("Class-Path");
    let attrs = scope.get(&attrs_h);
    let value = scope.invoke_virtual(
        attrs,
        "getValue",
        "(Ljava/lang/String;)Ljava/lang/String;",
        &[Value::Object(Some(key))],
    )?;
    let present = match value {
        Some(Value::Object(Some(v))) => {
            // A present-but-empty `Class-Path:` names no jars. The JDK's
            // `getClassPath` would parse it to an empty array and add nothing,
            // so answering `true` for one costs a scan and changes nothing;
            // answering `false` is the same observable behaviour, cheaper, and
            // is what the attribute means.
            scope.read_string(v).is_some_and(|t| !t.trim().is_empty())
        }
        _ => false,
    };
    Ok(Some(Value::Int(i32::from(present))))
}

/// `JavaUtilJarAccess.ensureInitialization(JarFile)` — force the jar past its
/// lazy manifest/verification setup.
///
/// A genuine no-op HERE, and the reason is not that it is hard. The JDK's body
/// exists so that `JarFile`'s `checkForSpecialAttributes` has run before a
/// caller reads `isMultiRelease`/signer state off a jar it did not open. This
/// VM's `JarFile` has no such deferred phase to force: the manifest is parsed
/// on demand by `getManifest()` and every reader of it goes through that.
///
/// Kept registered rather than deleted so the carrier cannot answer this
/// interface method with an `AbstractMethodError` — which is what it did for
/// the three methods added below.
fn jujar_ensure_initialization(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

/// `JavaUtilJarAccess.isInitializing()` — is the CURRENT THREAD inside
/// `JarFile`'s initialization?
///
/// Always `false`, and truthfully so: [`jujar_ensure_initialization`] never
/// enters such a phase, so no thread can be inside one. This is the honest
/// answer for this VM rather than a placeholder — if the no-op above ever
/// becomes a real initializer, this has to become a real thread-local flag with
/// it, which is why the two doc comments name each other.
fn jujar_is_initializing(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

/// `JavaUtilJarAccess.getTrustedAttributes(Manifest, String)` — the per-entry
/// attributes of `name`, read from the manifest the caller supplies.
///
/// "Trusted" in the JDK names the SOURCE, not a filter: it reads the signed
/// manifest's own copy rather than one an application may have replaced, so an
/// attacker cannot forge per-entry attributes by handing over a `Manifest`
/// object. This VM has no second, application-supplied copy to be confused
/// with, so `Manifest.getAttributes(name)` IS that source.
fn jujar_get_trusted_attributes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (Some(Value::Object(Some(manifest))), Some(name)) =
        (args.get(1).copied(), args.get(2).copied())
    else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.invoke_virtual(
        manifest,
        "getAttributes",
        "(Ljava/lang/String;)Ljava/util/jar/Attributes;",
        &[name],
    )
}

/// `JavaUtilJarAccess.entryFor(JarFile, String)` — the `JarEntry` for `name`.
///
/// `getJarEntry`, not `getEntry`: the interface's return type is `JarEntry` and
/// `ZipFile.getEntry` answers a `ZipEntry`, so routing through the wider method
/// would hand the caller an object that fails its own checkcast.
fn jujar_entry_for(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (Some(Value::Object(Some(jar))), Some(name)) = (args.get(1).copied(), args.get(2).copied())
    else {
        return Ok(Some(Value::Object(None)));
    };
    ctx.invoke_virtual(
        jar,
        "getJarEntry",
        "(Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        &[name],
    )
}

/// Register the carrier `SharedSecrets.javaUtilJarAccess()` hands out.
///
/// ALL FIVE interface methods, which two of them were not until 2026-08-19.
/// `javap -p jdk.internal.access.JavaUtilJarAccess` declares
/// `jarFileHasClassPathAttribute`, `getTrustedAttributes`,
/// `ensureInitialization`, `isInitializing` and `entryFor`; this carrier had
/// the first and the third. The other three are abstract on a synthetic class,
/// so a caller reaching one got an `AbstractMethodError` — the same shape as
/// the `X509KeyManager` interface-id defect in
/// `openssl-key-material-and-engine-residuals`, and invisible until something
/// calls it.
fn register_java_util_jar_access(registry: &mut NativeMethodRegistry) {
    let owner = "cratonvm/internal/ss/JavaUtilJarAccess$1";
    registry.register(
        owner,
        "jarFileHasClassPathAttribute",
        "(Ljava/util/jar/JarFile;)Z",
        jujar_jar_file_has_classpath_attribute,
    );
    registry.register(
        owner,
        "ensureInitialization",
        "(Ljava/util/jar/JarFile;)V",
        jujar_ensure_initialization,
    );
    registry.register(owner, "isInitializing", "()Z", jujar_is_initializing);
    registry.register(
        owner,
        "getTrustedAttributes",
        "(Ljava/util/jar/Manifest;Ljava/lang/String;)Ljava/util/jar/Attributes;",
        jujar_get_trusted_attributes,
    );
    registry.register(
        owner,
        "entryFor",
        "(Ljava/util/jar/JarFile;Ljava/lang/String;)Ljava/util/jar/JarEntry;",
        jujar_entry_for,
    );
}

// JavaUtilZipFileAccess -------------------------------------------------------

fn juzf_get_entry(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (ZipFile zf, String name, Function<String,JarEntry> factory) -> ZipEntry
    // Delegate to ZipFile.getEntry(String).
    // INSTANCE method: args[0] = receiver (ZipFile$1), args[1] = zf,
    // args[2] = name (args[3] = the JarEntry factory Function, unused).
    if let (Some(Value::Object(Some(zf))), Some(Value::Object(Some(name)))) =
        (args.get(1), args.get(2))
    {
        ctx.invoke(
            "java/util/zip/ZipFile",
            "getEntry",
            "(Ljava/lang/String;)Ljava/util/zip/ZipEntry;",
            &[Value::Object(Some(*zf)), Value::Object(Some(*name))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn juzf_entry_local_name_encoding(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // UTF-8 flag.
    Ok(Some(Value::Int(1)))
}

fn juzf_get_manifest_name(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let s = ctx.create_string("META-INF/MANIFEST.MF");
    Ok(Some(Value::Object(Some(s))))
}

fn register_java_util_zip_file_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/util/zip/ZipFile$1";
    registry.register(
        owner,
        "getEntry",
        "(Ljava/util/zip/ZipFile;Ljava/lang/String;Ljava/util/function/Function;)Ljava/util/zip/ZipEntry;",
        juzf_get_entry,
    );
    registry.register(
        owner,
        "entryLocalNameEncoding",
        "(Ljava/util/zip/ZipEntry;)Z",
        juzf_entry_local_name_encoding,
    );
    registry.register(
        owner,
        "getManifestName",
        "(Ljava/util/jar/JarFile;Z)Ljava/lang/String;",
        juzf_get_manifest_name,
    );
}

// JavaNetHttpCookieAccess -----------------------------------------------------

fn jnhc_parse_cookie(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Thin passthrough to HttpCookie.parse(String).
    // INSTANCE method: args[0] = receiver (JavaNetHttpCookieAccess$1),
    // args[1] = the header String.
    if let Some(Value::Object(Some(header))) = args.get(1) {
        ctx.invoke(
            "java/net/HttpCookie",
            "parse",
            "(Ljava/lang/String;)Ljava/util/List;",
            &[Value::Object(Some(*header))],
        )
    } else {
        ctx.new_object_initialized("java/util/ArrayList", "()V", &[])
    }
}

fn register_java_net_http_cookie_access(registry: &mut NativeMethodRegistry) {
    let owner = "cratonvm/internal/ss/JavaNetHttpCookieAccess$1";
    registry.register(
        owner,
        "parseCookie",
        "(Ljava/lang/String;)Ljava/util/List;",
        jnhc_parse_cookie,
    );
}

// JavaObjectInputStreamAccess -------------------------------------------------

fn jois_check_array(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // (ObjectInputStream ois, Class<?> arrayType, int length) -> void
    // No-op: cratonvm does not impose a `maxArrayLength` limit
    // separate from the normal heap allocator; the allocation
    // itself will surface any OOM condition.
    Ok(None)
}

fn register_java_object_input_stream_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/io/ObjectInputStream$1";
    registry.register(
        owner,
        "checkArray",
        "(Ljava/io/ObjectInputStream;Ljava/lang/Class;I)V",
        jois_check_array,
    );
}

// JavaUtilResourceBundleAccess ------------------------------------------------

fn jurb_set_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (ResourceBundle$1),
    // args[1] = bundle, args[2] = parent.
    if let (Some(Value::Object(Some(bundle))), Some(parent)) = (args.get(1), args.get(2)) {
        ctx.set_field_by_name(*bundle, "parent", *parent);
    }
    Ok(None)
}

fn jurb_get_parent(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (ResourceBundle$1), args[1] = bundle.
    if let Some(Value::Object(Some(bundle))) = args.get(1) {
        Ok(Some(ctx.get_field_by_name(*bundle, "parent")))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn jurb_set_locale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // INSTANCE method: args[0] = receiver (ResourceBundle$1),
    // args[1] = bundle, args[2] = locale.
    if let (Some(Value::Object(Some(bundle))), Some(locale)) = (args.get(1), args.get(2)) {
        ctx.set_field_by_name(*bundle, "locale", *locale);
    }
    Ok(None)
}

fn register_java_util_resource_bundle_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/util/ResourceBundle$1";
    registry.register(
        owner,
        "setParent",
        "(Ljava/util/ResourceBundle;Ljava/util/ResourceBundle;)V",
        jurb_set_parent,
    );
    registry.register(
        owner,
        "getParent",
        "(Ljava/util/ResourceBundle;)Ljava/util/ResourceBundle;",
        jurb_get_parent,
    );
    registry.register(
        owner,
        "setLocale",
        "(Ljava/util/ResourceBundle;Ljava/util/Locale;)V",
        jurb_set_locale,
    );
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// WP1.4 — Register all `SharedSecrets.getXxxAccess()` factories and
/// every concrete-class interface method.  Called from the
/// native-builtins initialisation path (both synthetic-jdk and
/// real-JDK modes).
///
/// # Why an image census cannot adjudicate anything in this file
///
/// Every owner here is one of the JDK's own anonymous `Java*Access`
/// implementations — `java/lang/System$1`, `java/net/InetAddress$1`,
/// `java/lang/invoke/MethodHandleImpl$1` — and their methods are **ordinary
/// bytecode** on every image, never `ACC_NATIVE`. The registrations here are
/// CratonVM's stand-ins for that bytecode, so `image_declaring_method` scores
/// every one of them `method-nowhere`, which is exactly what a dead
/// registration also looks like.
///
/// `dc55e8057` deleted thirteen of them on that evidence and
/// `representative_method_registered_per_owner` went red — one owner per run,
/// because it reports the first gap it finds, so the restore took three rounds.
/// Restored 2026-08-10.
///
/// **If a sweep proposes deleting a row in this file, the sweep is wrong.** The
/// per-owner test below is the statement of intent the census cannot see, and
/// `scripts/jdk-only-dead-sweep.py --pinned` is how that intent is fed back to
/// it.
pub fn register_wp1_4_shared_secrets(registry: &mut NativeMethodRegistry) {
    let __prev_cat = registry.current_category();
    registry.set_category(cratonvm_native_api::NativeKind::Bridge);
    register_factories(registry);

    register_java_lang_access(registry);
    register_java_lang_invoke_access(registry);
    register_java_lang_ref_access(registry);
    register_java_lang_reflect_access(registry);
    register_java_io_access(registry);
    register_java_io_raf_access(registry);
    register_java_io_fd_access(registry);
    register_java_net_inet_address_access(registry);
    register_java_net_uri_access(registry);
    register_java_nio_access(registry);
    // `register_java_security_access(registry)` was called here until 2026-08-13
    // (F33-1). See the deletion note above `// JavaUtilJarAccess`.
    register_java_util_jar_access(registry);
    register_java_util_zip_file_access(registry);
    register_java_net_http_cookie_access(registry);
    register_java_object_input_stream_access(registry);
    register_java_util_resource_bundle_access(registry);
    registry.set_category(__prev_cat);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };

    #[test]
    fn all_factories_listed() {
        // getJavaObjectInputStreamAccess removed: JDK 25 uses an invokedynamic
        // lambda (no ObjectInputStream$1), so the real getter must run.
        //
        // F17-1 (2026-08-13): 15 -> 14. `getJavaSecurityAccess` was deleted —
        // JEP 486 removed `jdk.internal.access.JavaSecurityAccess` and the
        // getter with it. See the comment at the deletion site in `FACTORIES`.
        assert_eq!(FACTORIES.len(), 14);
    }

    /// F17-1 (2026-08-13) — REPLACES a guard that could not fail.
    ///
    /// The previous body of this test was, in full:
    ///
    /// ```ignore
    /// for (method, ret, _) in FACTORIES {
    ///     assert!(method.starts_with("getJava"));
    ///     assert!(method.ends_with("Access"));
    ///     assert!(ret.contains("Access;"));
    /// }
    /// ```
    ///
    /// Every name in `FACTORIES` was written to that shape, so the test checked
    /// the list against itself: it had no way to distinguish a JDK-true getter
    /// from an invented one, and it passed on both `getJavaSecurityAccess` (a
    /// method JDK 25 does not declare at all) and `getJavaUtilJarAccess` (a
    /// misspelling of `javaUtilJarAccess`) for as long as both were listed.
    /// Worse, its `starts_with("getJava")` clause actively *punished* the
    /// correct spelling: `javaUtilJarAccess` has no `get` prefix, so repairing
    /// the table would have reddened the guard that was supposed to protect it.
    ///
    /// The oracle is now external: the JDK 25 class surface as walked out of the
    /// runtime image by `scripts/jdk-baseline/generate.py` and frozen in
    /// `scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv`.
    ///
    /// SOUNDNESS OF THIS PARTICULAR ORACLE — **the paragraph that used to be
    /// here was true when written and is now false; F33-1 (2026-08-13) replaces
    /// it rather than leaving two readings of the same file.**
    ///
    /// It read: *"the generator keeps only rows whose flags contain `public`
    /// (`generate.py:175`), so a baseline is blind to package-private, private
    /// and `private static native` members … every member of `SharedSecrets` is
    /// `public static`, so its 65 baseline rows are its whole surface."* Both
    /// halves have moved. The public-only filter was FIXED during this session
    /// (F23-1), and the baseline was regenerated: it now carries **101 data rows
    /// = 1 CLASS + 1 EXTENDS + 1 SUPERTYPE + 32 FIELD + 66 METHOD**, i.e. the 98
    /// members `javap -p` reports, 33 of them `private,static`. The old "65" was
    /// the public-method count, and reasoning from it would now under-count the
    /// surface by a third.
    ///
    /// The conclusion survives its premise, which is why this is a correction
    /// and not a retraction: every *accessor* on `SharedSecrets` really is
    /// `public static`, so the factory-name question this test asks is answered
    /// the same way by either baseline. Re-measured for this note:
    ///
    /// ```text
    /// $ javap -p jdk.internal.access.SharedSecrets | wc -l      # 98 members + wrapper
    /// $ javap    jdk.internal.access.SharedSecrets | wc -l      # 65 — the public view
    /// ```
    ///
    /// The one *method* plain `javap` hides is `ensureClassInitialized`; the
    /// other 32 hidden members are the private static backing fields. **Any
    /// claim about a member of this class must be checked at `-p`** — the
    /// public view is missing exactly the machinery the four self-initialising
    /// getters (`javaUtilJarAccess`, `getJavaNetUriAccess`,
    /// `getJavaNetHttpCookieAccess`, `getJavaIORandomAccessFileAccess`) run
    /// through, which is what `register_factories`' disposition turns on.
    #[test]
    fn every_factory_is_declared_by_jdk25_shared_secrets() {
        let baseline = crate::jdk_baseline::parse(crate::jdk_baseline::SHARED_SECRETS);
        for (method, ret, _) in FACTORIES {
            let desc = format!("(){ret}");
            assert!(
                baseline.declares(method, &desc),
                "SharedSecrets bridge registers `{method}{desc}`, which JDK 25's \
                 jdk.internal.access.SharedSecrets does not declare. Candidate \
                 descriptors the JDK does declare under that name: {:?}. If the \
                 list is empty the member is absent outright (delete the entry); \
                 if it is non-empty you have the descriptor wrong. Oracle: \
                 scripts/baselines/jdk25-jdk.internal.access.SharedSecrets.tsv.",
                baseline.descriptors_named(method),
            );
        }
    }

    /// Mutation check for the guard above: it must actually reject the two
    /// spellings F17-1 removed, not merely accept the ones that survived.
    ///
    /// Without this, `every_factory_is_declared_by_jdk25_shared_secrets` would
    /// be one silent `parse()` change away from being as vacuous as the test it
    /// replaced (a `Baseline` that parsed to zero rows would make `declares`
    /// return `false` for everything — but an empty `FACTORIES` loop would still
    /// pass). Asserting a KNOWN-BAD name is rejected and a KNOWN-GOOD one is
    /// accepted pins both directions.
    #[test]
    fn jdk25_baseline_rejects_the_two_spellings_f17_1_removed() {
        let baseline = crate::jdk_baseline::parse(crate::jdk_baseline::SHARED_SECRETS);
        assert!(
            !baseline.declares(
                "getJavaSecurityAccess",
                "()Ljdk/internal/access/JavaSecurityAccess;"
            ),
            "JEP 486 removed JavaSecurityAccess; nothing declares this getter"
        );
        assert!(
            !baseline.declares(
                "getJavaUtilJarAccess",
                "()Ljdk/internal/access/JavaUtilJarAccess;"
            ),
            "the real spelling has never carried the `get` prefix"
        );
        assert!(
            baseline.declares(
                "javaUtilJarAccess",
                "()Ljdk/internal/access/JavaUtilJarAccess;"
            ),
            "…and the corrected spelling must be the one the JDK declares"
        );
    }

    #[test]
    fn owner_classes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for owner in owner_classes() {
            assert!(seen.insert(owner), "duplicate owner class: {owner}");
        }
        // Every factory has a distinct owner class, so the unique-owner count
        // tracks FACTORIES.len() (14 since F17-1 removed `getJavaSecurityAccess`
        // — see `all_factories_listed`). Derive it so the two stay in
        // lock-step instead of drifting on the next factory add/remove.
        assert_eq!(seen.len(), FACTORIES.len());
    }

    #[test]
    fn all_factory_entry_points_registered() {
        // Instantiate a fresh registry and apply the bridge.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        for (method, ret, _) in FACTORIES {
            let desc = format!("(){}", ret);
            assert!(
                r.find("jdk/internal/access/SharedSecrets", method, &desc)
                    .is_some(),
                "factory {method} not registered (descriptor {desc})"
            );
            assert!(
                r.find("jdk/internal/misc/SharedSecrets", method, &desc)
                    .is_some(),
                "legacy-package factory {method} not registered"
            );
        }
    }

    #[test]
    fn representative_method_registered_per_owner() {
        // For each of the 14 owner classes, probe for one
        // representative method we know is registered. This is
        // a sanity check that every `register_*` call landed.
        //
        // F17-1 (2026-08-13): 14 here is NOT `FACTORIES.len()` (also 14, by
        // coincidence since F33-1) and must not be re-derived from it — the two
        // lists have never had the same membership. Measured against the current
        // tree, the owner probed below that has NO entry in `FACTORIES` is
        // `java/io/ObjectInputStream$1` (deliberately not intercepted, so the
        // real invokedynamic getter runs — see the note in `FACTORIES`), and the
        // factory owner NOT probed below is `java/io/FileDescriptor$1`. Reading
        // either count as the other is how the two drift with nothing noticing.
        //
        // F33-1 (2026-08-13): 15 -> 14. The `java/security/AccessController$1`
        // probe went with `register_java_security_access`, whose whole family —
        // interface, implementation class and `SharedSecrets` accessor — JEP 486
        // removed. This assertion is the reason that deletion could not be made
        // by the two earlier lanes that tried: it names the triple, so the
        // registrar and the probe have to move in one commit.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        let expected: &[(&str, &str, &str)] = &[
            (
                "java/lang/System$1",
                "currentCarrierThread",
                "()Ljava/lang/Thread;",
            ),
            (
                "java/lang/invoke/MethodHandleImpl$1",
                "makeClassValueMap",
                "()Ljava/util/Map;",
            ),
            (
                "java/lang/ref/Reference$1",
                "waitForReferenceProcessing",
                "()Z",
            ),
            (
                "java/lang/reflect/ReflectAccess",
                "copyMethod",
                "(Ljava/lang/reflect/Method;)Ljava/lang/reflect/Method;",
            ),
            ("java/io/Console$1", "console", "()Ljava/io/Console;"),
            (
                "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
                "open",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/RandomAccessFile;",
            ),
            (
                "java/net/InetAddress$1",
                "getHostFromNameService",
                "(Ljava/net/InetAddress;Z)Ljava/lang/String;",
            ),
            (
                "cratonvm/internal/ss/JavaNetUriAccess$1",
                "create",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URI;",
            ),
            // `java/nio/Buffer$2.getBufferPool()` was HERE and is retired
            // (`61a1647aa`). `javap -p` says NO supported JDK 25 image
            // declares it, so the registration could never be dispatched and
            // this row asserted the presence of a dead one.
            (
                "cratonvm/internal/ss/JavaUtilJarAccess$1",
                "jarFileHasClassPathAttribute",
                "(Ljava/util/jar/JarFile;)Z",
            ),
            (
                "java/util/zip/ZipFile$1",
                "entryLocalNameEncoding",
                "(Ljava/util/zip/ZipEntry;)Z",
            ),
            (
                "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
                "parseCookie",
                "(Ljava/lang/String;)Ljava/util/List;",
            ),
            (
                "java/io/ObjectInputStream$1",
                "checkArray",
                "(Ljava/io/ObjectInputStream;Ljava/lang/Class;I)V",
            ),
            (
                "java/util/ResourceBundle$1",
                "setParent",
                "(Ljava/util/ResourceBundle;Ljava/util/ResourceBundle;)V",
            ),
        ];
        assert_eq!(expected.len(), 13);
        for (owner, method, desc) in expected {
            assert!(
                r.find(owner, method, desc).is_some(),
                "expected method {owner}.{method}{desc} not registered"
            );
        }
    }

    #[test]
    fn java_nio_access_direct_buffer_pool_registered_for_jdk17_shape() {
        // JDK 17's VM$BufferPoolsHolder invokes
        // SharedSecrets.getJavaNioAccess().getDirectBufferPool() through the
        // JavaNioAccess interface. Depending on host JDK shape and CratonVM
        // anonymous-class metadata, the receiver can be Buffer$1, Buffer$2, or
        // reached only via the interface owner.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        let descriptor = "()Ljdk/internal/misc/VM$BufferPool;";

        // RETIRED 2026-08-24 (`61a1647aa`), and the assertion is INVERTED
        // rather than deleted, because the absence is the property now.
        //
        // `javap` decided it: `java.nio.Buffer$2.getDirectBufferPool()` is
        //
        //     0: getstatic  java/nio/Bits.BUFFER_POOL
        //     3: areturn
        //
        // two instructions returning an object real bytecode already builds.
        // The native in front of it existed only to hand back a FABRICATED
        // carrier, and under `--jdk-only` that fabrication is refused from
        // inside `VM$BufferPoolsHolder.<clinit>` — where a refusal is not
        // catchable, poisons `jdk.internal.misc.VM` for the process, and takes
        // the whole platform MBeanServer with it. The interception was strictly
        // worse than nothing, so re-adding it must fail this test.
        for owner in [
            "java/nio/Buffer$1",
            "java/nio/Buffer$2",
            "jdk/internal/access/JavaNioAccess",
        ] {
            assert!(
                r.find(owner, "getDirectBufferPool", descriptor).is_none(),
                "{owner}.getDirectBufferPool is registered again — the real                  `Bits.BUFFER_POOL` bytecode must serve it, or strict mode                  loses the platform MBeanServer out of a <clinit>"
            );
        }

        for (method, desc) in [
            ("getName", "()Ljava/lang/String;"),
            ("getCount", "()J"),
            ("getTotalCapacity", "()J"),
            ("getMemoryUsed", "()J"),
        ] {
            assert!(
                r.find("jdk/internal/misc/VM$BufferPool", method, desc)
                    .is_some(),
                "VM$BufferPool.{method}{desc} must be registered"
            );
        }
    }

    #[test]
    fn java_lang_access_define_class_registered() {
        // `jdk.internal.reflect.ClassDefiner.defineClass` calls
        // `SharedSecrets.getJavaLangAccess().defineClass(...)`, which
        // dispatches on the `System$1` singleton's concrete class. Missing
        // this registration aborted every Arquillian
        // `testsuite/integration-arquillian/tests/base` test class with
        // NoSuchMethodError during JUnit5 test-plan construction (see
        // docs/known-issues/keycloak-arquillian-system1-defineclass-nosuchmethod.md).
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        assert!(
            r.find(
                "java/lang/System$1",
                "defineClass",
                "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some(),
            "JavaLangAccess.defineClass not registered on java/lang/System$1"
        );
    }

    #[test]
    fn java_lang_access_find_bootstrap_class_or_null_registered() {
        // `jdk.internal.loader.BootLoader.loadClassOrNull` calls
        // `SharedSecrets.getJavaLangAccess().findBootstrapClassOrNull(name)`,
        // which dispatches on the `System$1` singleton's concrete class.
        // Missing this registration raised NoSuchMethodError on every SSSD
        // Keycloak Arquillian test row (see
        // docs/known-issues/keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md).
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        assert!(
            r.find(
                "java/lang/System$1",
                "findBootstrapClassOrNull",
                "(Ljava/lang/String;)Ljava/lang/Class;",
            )
            .is_some(),
            "JavaLangAccess.findBootstrapClassOrNull not registered on java/lang/System$1"
        );
    }

    #[test]
    fn java_lang_access_blocked_on_jdk21_overload_registered() {
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        let desc = "(Lsun/nio/ch/Interruptible;)V";
        assert!(
            r.find("java/lang/System$1", "blockedOn", desc).is_some(),
            "JavaLangAccess.blockedOn(Interruptible) not registered on java/lang/System$1"
        );
        assert!(
            r.find("jdk/internal/access/JavaLangAccess", "blockedOn", desc)
                .is_some(),
            "JavaLangAccess.blockedOn(Interruptible) not registered on interface fallback"
        );
    }

    #[test]
    fn java_lang_access_carrier_thread_local_registered() {
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        for (name, desc) in [
            (
                "getCarrierThreadLocal",
                "(Ljdk/internal/misc/CarrierThreadLocal;)Ljava/lang/Object;",
            ),
            (
                "setCarrierThreadLocal",
                "(Ljdk/internal/misc/CarrierThreadLocal;Ljava/lang/Object;)V",
            ),
            (
                "removeCarrierThreadLocal",
                "(Ljdk/internal/misc/CarrierThreadLocal;)V",
            ),
            (
                "isCarrierThreadLocalPresent",
                "(Ljdk/internal/misc/CarrierThreadLocal;)Z",
            ),
        ] {
            assert!(
                r.find("java/lang/System$1", name, desc).is_some(),
                "JavaLangAccess.{name}{desc} not registered on java/lang/System$1"
            );
            assert!(
                r.find("jdk/internal/access/JavaLangAccess", name, desc)
                    .is_some(),
                "JavaLangAccess.{name}{desc} not registered on interface fallback"
            );
        }
    }

    #[test]
    fn java_lang_access_find_native_registered() {
        // `java.lang.foreign.SymbolLookup.loaderLookup()` calls through the
        // real JDK `JavaLangAccess.findNative(ClassLoader, String)` bridge.
        // The System$1 concrete owner is the normal dispatch target; the
        // interface registration covers fallback invokeinterface resolution.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        let desc = "(Ljava/lang/ClassLoader;Ljava/lang/String;)J";
        assert!(
            r.find("java/lang/System$1", "findNative", desc).is_some(),
            "JavaLangAccess.findNative not registered on java/lang/System$1"
        );
        assert!(
            r.find("jdk/internal/access/JavaLangAccess", "findNative", desc)
                .is_some(),
            "JavaLangAccess.findNative not registered on interface fallback"
        );
    }

    /// F33-1 (2026-08-13) — **a factory must carry the kind of the owner it
    /// hands out, not the kind its own receiver class earns.**
    ///
    /// This is the guard for the capability defect described on
    /// [`register_factories`]. It is deliberately an IFF and not a list check:
    /// the interesting failure is not "the wrong four are demoted", it is
    /// "a factory and its owner disagree", which is what a future author
    /// reintroduces by adding a stand-in owner without thinking about the
    /// getter that returns it.
    ///
    /// # It is red on the tree this replaced
    ///
    /// Before this change every factory was a plain `register` under the
    /// registrar's ambient `Bridge`, so all fourteen came back `Bridge` and the
    /// four `cratonvm/internal/ss/…$1` rows failed the `==`. That is the
    /// mutation check in the direction that matters, and it is worth being
    /// precise that the OTHER direction is also covered: making
    /// `register_factories` demote unconditionally reddens the ten real-JDK
    /// owners, so "fix it by demoting everything" cannot pass either.
    ///
    /// The frozen four at the end are a second, weaker assertion on the same
    /// run. They do not carry the invariant — the loop does — but they name the
    /// population, so a change that legitimately shrinks it (retargeting a
    /// carrier onto the JDK's own implementation class, e.g.
    /// `java/util/jar/JavaUtilJarAccessImpl`) has to say so here.
    #[test]
    fn factory_kind_follows_the_owner_it_hands_out() {
        use cratonvm_native_api::NativeKind;

        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);

        let mut demoted: Vec<&str> = Vec::new();
        for (method, ret, owner) in FACTORIES {
            let desc = format!("(){ret}");
            let kind = r
                .kind_of("jdk/internal/access/SharedSecrets", method, &desc)
                .unwrap_or_else(|| panic!("SharedSecrets.{method}{desc} is not registered at all"));
            let owner_is_dropped = factory_is_orphaned_by_strict_mode(owner);
            assert_eq!(
                kind == NativeKind::SyntheticStub,
                owner_is_dropped,
                "SharedSecrets.{method}{desc} is registered `{}` while its owner \
                 `{owner}` is {} by `--jdk-only`. A factory and the receiver it \
                 returns must be admitted or refused TOGETHER: a surviving \
                 factory over a dropped owner shadows the real JDK getter and \
                 hands back a carrier with no methods (`alloc_singleton`'s `Err` \
                 arm makes that a wrong-class receiver, not an error), and a \
                 refused factory over a live owner removes a working bridge for \
                 nothing. Fix `register_factories`, not this test.",
                kind.as_str(),
                if owner_is_dropped { "DROPPED" } else { "kept" },
            );
            if owner_is_dropped {
                demoted.push(*owner);
            }
        }

        demoted.sort_unstable();
        assert_eq!(
            demoted,
            [
                "cratonvm/internal/ss/JavaIORandomAccessFileAccess$1",
                "cratonvm/internal/ss/JavaNetHttpCookieAccess$1",
                "cratonvm/internal/ss/JavaNetUriAccess$1",
                "cratonvm/internal/ss/JavaUtilJarAccess$1",
            ],
            "the set of SharedSecrets factories `--jdk-only` refuses has changed. \
             GROWTH means a new fabricated owner (or a real owner newly added to \
             `NO_IMAGE_JDK_RECEIVERS`) — check that its getter really is better \
             served by the JDK's own bytecode. SHRINKAGE means a carrier was \
             retargeted onto a real class, which is a fix: shrink this list with \
             it and say which class in the commit message. See \
             docs/known-issues/jdk-only/F33-1-a-factory-and-its-owner-must-share-one-kind-20260813.md"
        );
    }

    /// A `cratonvm/…` owner that nobody listed keeps `Bridge` and re-opens the
    /// defect silently.
    ///
    /// [`factory_kind_follows_the_owner_it_hands_out`] cannot catch that on its
    /// own: it asks `factory_is_orphaned_by_strict_mode` about the owner, and an
    /// unlisted `cratonvm/` name answers `false`, so factory and owner would
    /// agree — both `Bridge`, both surviving strict, and the methods on a class
    /// no image declares would be dispatching in the mode that exists to refuse
    /// exactly that. The IFF is satisfied by the wrong shared answer.
    ///
    /// So the population needs its own statement. Every `cratonvm/`-namespaced
    /// owner in [`FACTORIES`] is a stand-in for a JDK shape by construction —
    /// it exists only because the bridge had no real class to name — so it
    /// belongs in `VM_MINTED_STAND_IN_RECEIVERS`. (The `VM_SERVICE_RECEIVERS`
    /// escape hatch does not apply: a reviewed VM service is not something
    /// `SharedSecrets` hands to JDK bytecode as a `Java*Access`.)
    #[test]
    fn every_fabricated_factory_owner_is_a_listed_stand_in() {
        for (method, _, owner) in FACTORIES {
            if !owner.starts_with("cratonvm/") {
                continue;
            }
            assert!(
                cratonvm_native_api::no_image_receiver::VM_MINTED_STAND_IN_RECEIVERS
                    .contains(owner),
                "`{owner}` is a fabricated receiver handed out by \
                 SharedSecrets.{method}, and it is NOT in \
                 `VM_MINTED_STAND_IN_RECEIVERS` \
                 (native-api/src/no_image_receiver.rs). Unlisted, its methods \
                 keep `NativeKind::Bridge` and dispatch under `--jdk-only` on a \
                 class no JDK image declares — and this factory keeps `Bridge` \
                 with them, so the two agree and \
                 `factory_kind_follows_the_owner_it_hands_out` stays green while \
                 the whole family is wrong. Add the name to that table IN SORTED \
                 POSITION (it is binary-searched)."
            );
        }
    }
}
