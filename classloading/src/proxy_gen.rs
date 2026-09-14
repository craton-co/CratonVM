// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Proxy bytecode emitter — generates a `ClassFile` per
//! (loader, sorted-iface-list) key for `java.lang.reflect.Proxy.newProxyInstance`.
//!
//! # v1 design
//!
//! Each generated class:
//! 1. Extends whatever `native_builtins::reflect_annotations::
//!    proxy_super_class_name()` returns, which the caller writes into
//!    [`ProxyClassSpec::super_class`]. **By default that is the real
//!    `java/lang/reflect/Proxy`, not `Proxy$Instance`** —
//!    `CRATONVM_REAL_PROXY_SUPER` is `truthy_word_default_true`. The
//!    synthetic `java/lang/reflect/Proxy$Instance` (which owns the
//!    `handler`, `interfaces` and `hashSeed` fields plus the
//!    `<init>(InvocationHandler, Class[])` constructor that populates them)
//!    is the *opt-out* super, and is also what the
//!    `ProxyClassOutcome::Degrade`/`Failed` fallbacks allocate.
//!
//!    Corrected 2026-08-13 (lane F32). This item used to say the super was
//!    always `Proxy$Instance` and that "the interpreter's existing
//!    cast/dispatch hooks generalise via *is the receiver an instance of (a
//!    subclass of) Proxy$Instance?*". That sentence is the premise the
//!    interpreter's vtable fast path was written against, and it stopped
//!    being true when the real-super gate landed default-on: the inlined
//!    chain walk at `vm/src/runtime/interpreter/dispatch_virtual.rs:478`
//!    still matches `Proxy$Instance` alone, so it recognises no shipped
//!    proxy. `typecheck.rs`'s `class_name_is_proxy_super` is the predicate
//!    that asks the question correctly; see
//!    `docs/known-issues/jdk-only/F32-1-the-proxy-route-and-the-drifted-twin-20260813.md`.
//! 2. Declares the iface set in `interfaces[]`.
//! 3. Emits one method body per declared interface method (abstract or
//!    default). The body boxes args and routes through
//!    `Proxy$Dispatch.invokeProxy`, an `INVOKESTATIC` helper registered as
//!    a builtin native.
//! 4. Emits a constructor that just delegates to `super.<init>`.
//!
//! # No StackMapTable
//!
//! These class files are emitted **without** a `StackMapTable` attribute.
//! Per JVMS §4.10.1 a stack map is only required when a method has at
//! least one branch target or exception handler reachable from entry.
//! Every method body produced by this emitter (constructor, per-method
//! dispatch shim, and the `<clinit>` initialiser) is straight-line and
//! has an empty `exception_table`, so Pass 3 verification accepts the
//! class without any frames. The bridge in
//! `native_builtins::define_or_get_proxy_class` consequently passes
//! `skip_verification: false` (WP2.5-v3 item 4 — verified by the
//! `emitted_class_is_straight_line_no_handlers` regression test).
//!
//! # Nest host, access flags and module of a generated class
//!
//! Recorded here because JVMS §5.4.4 member access checks are being wired
//! into the interpreter for the first time (see the STATUS block in
//! `access_control.rs`), and everything this emitter mints is about to start
//! being checked. Audited 2026-07-26.
//!
//! **These are ordinary classes, not hidden classes.** They go through
//! `NativeContext::define_class_full` →
//! `ClassManager::define_class_with_options` with `skip_verification: false`,
//! so full Pass 2/3 verification runs and the class gets normal type maps —
//! the trusted-hidden-class escape hatch is *not* taken. `hidden` is false.
//!
//! **Name / package.** `build_proxy_spec_for` picks
//! `jdk/proxy<M>/$Proxy<N>` when every proxied interface is public, and
//! `<iface-package>/$Proxy<N>` when any is not (matching the JDK, and
//! deliberately avoiding the `java/`/`sun/` "Prohibited package name"
//! rejection).
//!
//! **Nest.** The class `attributes[]` table emitted below is *empty*: no
//! `NestHost`, no `NestMembers`, no `InnerClasses`. `confirmed_nest_host`
//! maps `nest_host: None` to the class's own name, so a `$ProxyN` is its own
//! nest host and is a nestmate of nothing else. That is the correct and safe
//! answer, because no emitted body touches another class's `private` member:
//! the only private members involved are its own `m_<i>` static fields
//! (same-class access), and every off-class target is public — `Class
//! .getMethod`, `<Wrapper>.valueOf` / `.TYPE` / `.<prim>Value`. Proxies do
//! **not** have the lambda-body problem where a runtime-generated class can
//! never appear in a host's `NestMembers`.
//!
//! **The one thing to check before wiring member access.** Every emitted
//! method body ends in `INVOKESTATIC java/lang/reflect/Proxy$Dispatch
//! .invokeProxy`, and `java/lang/reflect/Proxy$Dispatch` **is never defined
//! as a class**. It exists only as an owner-name key in the native registry
//! (`register_reflect_proxy_natives`); nothing calls
//! `ensure_synthetic_class` for it, so `get_loaded_class_id` returns `None`.
//! A member-access check that resolves the methodref's owner `Class` before
//! checking will not find one. Today `check_module_access_by_id` fails open
//! on exactly that (`interpreter.rs:41800` guards on `get_loaded_class_id`);
//! a stricter wiring that instead *loads* the owner turns every dynamic-proxy
//! call in the VM into `NoClassDefFoundError`. The super
//! `java/lang/reflect/Proxy$Instance` is by contrast a real (synthetic-stub)
//! class: `ACC_PUBLIC | ACC_SUPER`, with its `<init>` declared
//! `PUBLIC | NATIVE`, so the `INVOKESPECIAL` in the generated constructor is
//! accessible cross-package. Under the `proxy_super_class_name()` gate that
//! super becomes the *real* `java.lang.reflect.Proxy`, whose constructor is
//! `protected` — that call then depends on the subclass clause of §5.4.4 and
//! on `receiver_ok_for_protected` not being satisfied vacuously by
//! `receiver: None`.
//!
//! **Module.** `module_name` is assigned by `define_class_shared_with_options`
//! from `ModuleRegistry::module_for_package`. No module owns `jdk/proxyN`, so
//! the common case is the unnamed module and every JPMS check short-circuits
//! to allow. In the non-public-interface case the proxy lands in the
//! interface's package and inherits whatever module owns it.
//!
//! **Known deviation — class access flags.** `access_flags` below is
//! unconditionally `ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_SYNTHETIC`. The
//! JDK's `ProxyBuilder` emits `ACC_FINAL | ACC_SUPER` (i.e. *package-private*)
//! whenever any proxied interface is non-public. CratonVM therefore publishes
//! a proxy of a package-private interface as a public class in that
//! interface's package, and `Modifier.isPublic(proxyClass.getModifiers())`
//! answers `true` where the JDK answers `false` (Spring's `ClassUtils`
//! visibility helpers and Mockito's mock-visibility logic both read that).
//! Fixing it needs the publicness of the interface set, which this emitter is
//! not given: `ProxyClassSpec` carries interface *names* only. The fix is a
//! new `ProxyClassSpec` field set by `native_builtins::build_proxy_spec_for`
//! — which already computes exactly this predicate for the package choice
//! (`non_public_pkg`) — and a two-line change here. It is a cross-file change
//! and is left to the owner of `native-builtins`.
//!
//! # v3 closure (items 3, 4, 6 landed)
//!
//! - **Static `Method[]` slots (item 3 — DONE).** Each generated class
//!   declares a `private static final Method m_<i>` per `ProxyMethod`.
//!   The emitted `<clinit>` populates each via
//!   `Class.forName(iface).getMethod(name, paramTypes)`. Method bodies
//!   `GETSTATIC m_<i>` and push the cached `Method` to the dispatch
//!   helper. The `Proxy$Dispatch.invokeProxy` contract is now
//!   `(Object, Method, Object[]) Object` (was `(Object, String, String,
//!   Object[]) Object`).
//! - **`StackMapTable` (item 4 — DONE).** Verified above; emitter is
//!   straight-line, no frames required, `skip_verification: false`.
//! - **`UndeclaredThrowableException` wrapping (item 6 — DONE).** The
//!   helper `native_builtins::wrap_undeclared_throwable` inspects the
//!   thrown exception against the cached `Method.exceptionTypes` and
//!   wraps non-`RuntimeException` / non-`Error` mismatches in
//!   `java.lang.reflect.UndeclaredThrowableException`. The synthetic
//!   `proxy_invoke_handler_shared` path keeps verbatim propagation for
//!   backward compatibility with the WP2.5 v1+v2 test fixtures.
//!
//! # Method body shape (per `ProxyMethod`)
//!
//! ```text
//! ALOAD_0                                    ; receiver
//! LDC "<method.name>"                        ; method name
//! LDC "<method.descriptor>"                  ; method descriptor
//! ICONST_<argc> ; ANEWARRAY java/lang/Object ; Object[] for boxed args
//! <for each parameter slot N starting at 1:
//!     DUP ; ICONST_<i> ;
//!     <load slot N (ILOAD/LLOAD/FLOAT/DLOAD/ALOAD)> ;
//!     <if primitive: INVOKESTATIC <Wrapper>.valueOf(<prim>)<Wrapper>> ;
//!     AASTORE>
//! INVOKESTATIC java/lang/reflect/Proxy$Dispatch.invokeProxy
//!     (Ljava/lang/Object;Ljava/lang/String;Ljava/lang/String;[Ljava/lang/Object;)
//!     Ljava/lang/Object;
//! <return:
//!     V       → POP ; RETURN
//!     L...;   → CHECKCAST <ret>; ARETURN
//!     [...    → CHECKCAST <retArr>; ARETURN
//!     I/S/B/C/Z → CHECKCAST <Wrapper>; INVOKEVIRTUAL <prim>Value(); IRETURN
//!     J       → CHECKCAST Long;    INVOKEVIRTUAL longValue();    LRETURN
//!     F       → CHECKCAST Float;   INVOKEVIRTUAL floatValue();   FRETURN
//!     D       → CHECKCAST Double;  INVOKEVIRTUAL doubleValue();  DRETURN
//! >
//! ```

use std::collections::HashMap;

use cratonvm_types::error::ClassFileError;

// ─────────────────────────────────────────────────────────────────────────────
// Public API
// ─────────────────────────────────────────────────────────────────────────────

/// Spec for a single proxy class to emit.
#[derive(Debug, Clone)]
pub struct ProxyClassSpec {
    /// Internal name, e.g. `"java/lang/reflect/$Proxy0"`.
    pub gen_class_name: String,
    /// Internal name of super class. **By default this is the real
    /// `"java/lang/reflect/Proxy"`**, written by
    /// `native_builtins::build_proxy_spec_for` from `proxy_super_class_name()`;
    /// `"java/lang/reflect/Proxy$Instance"` is the `CRATONVM_REAL_PROXY_SUPER=0`
    /// opt-out. This string is what every "is the receiver a proxy?" name test
    /// in the interpreter has to agree with — see
    /// `vm/src/runtime/interpreter/typecheck.rs::class_name_is_proxy_super`.
    pub super_class: String,
    /// Internal names of interfaces this proxy implements.
    pub interfaces: Vec<String>,
    /// Methods to emit (collected by the caller from all ifaces' abstract+default methods).
    pub methods: Vec<ProxyMethod>,
}

/// One method to emit on the proxy.
#[derive(Debug, Clone)]
pub struct ProxyMethod {
    /// Method name, e.g. `"get"`.
    pub name: String,
    /// JVMS descriptor, e.g. `"()Ljava/lang/Object;"` or `"(I)I"`.
    pub descriptor: String,
    /// Whether this is an interface default method. We still emit a body so
    /// the dispatch hook intercepts the call; without an emitted body the
    /// JVM would inherit the iface's default impl directly and bypass the
    /// InvocationHandler.
    pub is_default: bool,

    // ── v3 additions ─────────────────────────────────────────────────────
    /// Internal name of the iface that declares this method, e.g.
    /// `"java/util/function/Supplier"`. Used by `<clinit>` to resolve the
    /// `Class` mirror and call `getMethod`. For `Object` overrides
    /// (`equals` / `hashCode` / `toString`) this is `"java/lang/Object"`.
    pub iface_owner: String,
    /// Internal names of each parameter type. For primitives use the
    /// wrapper-class internal names (`"java/lang/Integer"`, etc.); the
    /// `<clinit>` emitter rewrites those to `<Wrapper>.TYPE` GETSTATICs so
    /// `Class.getMethod` matches by primitive `Class<?>`. Length matches
    /// the descriptor's parameter slot count (long/double = 1 here, NOT
    /// raw JVM slot count).
    pub param_class_names: Vec<String>,
    /// Internal name of the DECLARED interface (an entry of
    /// [`ProxyClassSpec::interfaces`]) through which this method is reachable.
    /// `iface_owner` may be a *super*-interface of it, or `java/lang/Object`.
    ///
    /// This exists so `<clinit>` can obtain the owner `Class` mirror from the
    /// generated class's OWN `getInterfaces()` instead of from a constant-pool
    /// `LDC class <iface_owner>`. Constant-pool class resolution is by NAME
    /// through the flat global store, so when two class loaders each hold a
    /// copy of the interface, `<clinit>` could bind the other world's copy —
    /// and `Method.equals` compares declaring classes by IDENTITY, so every
    /// `Map<Method, …>` lookup the caller makes against its own
    /// `getMethods()` then misses. Byte Buddy's `JavaDispatcher` is exactly
    /// such a map; the miss surfaced as
    /// `IllegalStateException: No proxy target found for …Executable.isInstance(Object)`
    /// and took Mockito's inline mock maker down with it whenever a second
    /// loader world (Spring Boot's `ModifiedClassPathClassLoader`) had already
    /// loaded Byte Buddy. `getInterfaces()` is loader-faithful by construction:
    /// the generated class is defined with `force_loader_faithful_linking`.
    ///
    /// `None` (or a name absent from the emitted `interfaces[]`) falls back to
    /// the constant-pool form — correct whenever only one copy exists, and the
    /// only option for `java/lang/Object` methods.
    pub iface_root: Option<String>,
    /// Internal names of declared exception types (for UndeclaredThrowable
    /// checking in the dispatch helper). Empty if none. Populated by
    /// `native_builtins::build_proxy_spec_for` from
    /// `MethodMetadata.exceptions`.
    pub exception_types: Vec<String>,
}

/// Emit a JVMS §4 ClassFile for the given proxy spec. Returns the raw bytes.
///
/// The result is intended to be consumed by `cratonvm_reader::read_class`
/// (round-trip) and `ClassManager::define_class_with_options`.
///
/// v3 layout: emits a `static Method m_<i>` field per `ProxyMethod`,
/// populated by a generated `<clinit>` that resolves each method via
/// `Class.forName(iface).getMethod(name, paramTypes)`. Each method body
/// then uses `GETSTATIC m_<i>` instead of recreating a `Method` per call.
fn invalid_proxy_classfile(class_name: &str, message: impl Into<String>) -> ClassFileError {
    ClassFileError::InvalidClassFile {
        class_name: class_name.to_string(),
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// Emission census — so a silent degrade stops being silent
// ---------------------------------------------------------------------------

/// Generated classfiles emitted successfully, split by the super the spec
/// asked for. The split is the point: `real_proxy_super()` decides which
/// name a generated `$ProxyN` carries on its chain, and every "is this a
/// proxy?" predicate in the VM is a name test against one or both of those
/// names. An instrument that sees `real=N, synthetic=0` and a proxy-guard
/// counter of 0 has the whole diagnosis in two numbers.
static PROXY_EMIT_OK_REAL_SUPER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
static PROXY_EMIT_OK_SYNTHETIC_SUPER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// Emission failures — the classifier's `Failed("emit")` arm, which in the
/// default (non-strict) configuration degrades to a different boxing
/// implementation with no exception and no log.
static PROXY_EMIT_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(ok_real_super, ok_synthetic_super, failed)` since process start.
///
/// A reader exists deliberately. A `pub static` counter with a `fetch_add`
/// and no reader is a write-only counter — the shape that hid
/// `PAR_SWEEP_ACCEPTS` — so this pair ships together and the accessor is
/// what a `--jdk-only-report` row or a test would call.
pub fn proxy_emit_counts() -> (u64, u64, u64) {
    use std::sync::atomic::Ordering::Acquire;
    (
        PROXY_EMIT_OK_REAL_SUPER.load(Acquire),
        PROXY_EMIT_OK_SYNTHETIC_SUPER.load(Acquire),
        PROXY_EMIT_FAILED.load(Acquire),
    )
}

/// Which census bucket an emitted class belongs to.
///
/// Classified from the super the spec actually names, **not** by re-reading
/// `real_proxy_super()`: this crate cannot see `native-builtins`' flags, and
/// the spec is in any case the authority on what was emitted — the
/// `Degrade`/`Failed` fallbacks allocate the synthetic shim regardless of
/// what the gate says, so a flag read would mislabel exactly the rows that
/// matter. Split out as a pure fn so it can be tested without touching the
/// process-global counters (which several other tests in this module bump).
fn emitted_super_is_the_synthetic_shim(super_class: &str) -> bool {
    super_class == "java/lang/reflect/Proxy$Instance"
}

/// Emit a JVM classfile for the given proxy spec.
///
/// # Why this wrapper exists
///
/// `native_builtins::define_or_get_proxy_class` classifies a failure here as
/// `ProxyClassOutcome::Failed("emit")`, and its caller
/// (`native_proxy_new_instance`) then checks `real_proxy_strict()` — which is
/// `affirmative_word(src, "CRATONVM_REAL_PROXY_STRICT")`, i.e. **OFF by
/// default**. In the shipping configuration an emission failure therefore
/// allocates the synthetic `Proxy$Instance` shim instead and returns a
/// perfectly ordinary-looking proxy. Nothing throws, nothing is logged
/// (`define_or_get_proxy_class`'s `FALLBACK(emit)` line is behind
/// `CRATONVM_DBG_PROXY`, which is off), and the two implementations do not
/// agree: the emitted bytecode boxes arguments with real `X.valueOf`
/// invokestatic (canonical, measured `true` on HotSpot for all of
/// `proxy.int/char/bool/boolTRUE/long/byte/short`), while the shim's
/// `vm_exec::proxy_box_value_for_desc` allocates fresh wrappers for five of
/// its seven arms. A capability that quietly degrades to a *different answer*
/// is the shape that hides defects.
///
/// So the failure is made loud **unconditionally** — it is not a debug event,
/// it is a capability loss — and both outcomes are counted so an instrument
/// can see the degrade without reading stderr. The log costs nothing on a
/// healthy run because the branch is only taken on a genuine `Err`, which
/// `emit_proxy_classfile_inner` produces only for a malformed descriptor or a
/// constant-pool/local-slot overflow.
///
/// **Not done here, and deliberately:** the default is NOT flipped to strict.
/// See `docs/known-issues/jdk-only/F32-1-the-proxy-route-and-the-drifted-twin-20260813.md`
/// §5 for what flipping it would break — briefly, `Failed("spec")` and
/// `Failed("define")` are reachable for reasons that are not the caller's
/// fault (an interface ClassId that will not resolve to a name; a duplicate
/// define; a non-public interface forcing a prohibited package), and under
/// strict mode each becomes an `IllegalArgumentException` out of
/// `Proxy.newProxyInstance` where the app previously got a working proxy.
/// That is a behaviour change, and it needs the proxy soak, not this lane.
pub fn emit_proxy_classfile(spec: &ProxyClassSpec) -> Result<Vec<u8>, ClassFileError> {
    use std::sync::atomic::Ordering::AcqRel;
    match emit_proxy_classfile_inner(spec) {
        Ok(bytes) => {
            if emitted_super_is_the_synthetic_shim(&spec.super_class) {
                PROXY_EMIT_OK_SYNTHETIC_SUPER.fetch_add(1, AcqRel);
            } else {
                PROXY_EMIT_OK_REAL_SUPER.fetch_add(1, AcqRel);
            }
            Ok(bytes)
        }
        Err(e) => {
            let n = PROXY_EMIT_FAILED.fetch_add(1, AcqRel) + 1;
            eprintln!(
                "[cratonvm] PROXY GENERATION FAILED (emit) for {} extends {} \
                 with {} interface(s), {} method(s): {:?} — this is failure #{} \
                 this process. CRATONVM_REAL_PROXY_STRICT is off by default, so \
                 the caller will SILENTLY fall back to the synthetic \
                 java/lang/reflect/Proxy$Instance shim, whose argument boxing is \
                 a different implementation (fresh wrappers where the generated \
                 bytecode uses the canonical X.valueOf). Re-run with \
                 CRATONVM_REAL_PROXY_STRICT=1 to make this throw \
                 IllegalArgumentException as the JDK does, or CRATONVM_DBG_PROXY=1 \
                 for the other two failure stages.",
                spec.gen_class_name,
                spec.super_class,
                spec.interfaces.len(),
                spec.methods.len(),
                e,
                n
            );
            Err(e)
        }
    }
}

fn emit_proxy_classfile_inner(spec: &ProxyClassSpec) -> Result<Vec<u8>, ClassFileError> {
    // ── Structural de-duplication (JVMS §4.1 / §4.6) ─────────────────────
    //
    // Two classfile invariants the emitter is responsible for, because a
    // violation of either produces a `ClassFormatError`-shaped class that
    // no amount of correct method-body emission can rescue:
    //
    //   §4.1  "No two entries in the `interfaces` array may be the same
    //          class or interface."
    //   §4.6  "No two methods in one `class` file may have the same name
    //          and descriptor."
    //
    // The interfaces one is *reachable from user code*. `ProxyClassSpec`
    // carries interface **names**, but its producer
    // (`native_builtins::build_proxy_spec_for`) derives them from a list of
    // `ClassId`s that is deduplicated **by ClassId**. Two distinct ClassIds
    // can share a name — that is the whole point of loader-based class
    // identity, and this VM hits it routinely (see
    // `force_loader_faithful_linking` in `define_or_get_proxy_class`). So
    //
    //     Proxy.newProxyInstance(l, new Class[]{ ifaceFromLoaderA,
    //                                            ifaceFromLoaderB }, h)
    //
    // where both classes are named `com/foo/Bar` survives the ClassId dedup
    // and arrives here as `interfaces: ["com/foo/Bar", "com/foo/Bar"]`.
    // `CpBuilder::add_class` dedups, so the emitted `interfaces[]` was two
    // *byte-identical* u2 entries — exactly the §4.1 violation. HotSpot
    // rejects such a classfile outright.
    //
    // Both are collapsed here rather than reported as errors: at the
    // classfile level a repeated interface name is indistinguishable from a
    // single one, and a repeated (name, descriptor) would produce two
    // identical dispatch shims. The *policy* question — the JDK's
    // `Proxy.newProxyInstance` throws `IllegalArgumentException("repeated
    // interface")` — belongs to the caller, which alone can tell "same name,
    // two loaders" from "same class twice".
    //
    // The dedup must happen BEFORE `m_field_refs` / `m_field_meta` are
    // allocated: `emit_proxy_method` indexes them positionally, so the
    // fields[], the `<clinit>` PUTSTATIC targets and the method bodies all
    // have to agree on one method list.
    let mut seen_iface: Vec<&str> = Vec::with_capacity(spec.interfaces.len());
    for name in &spec.interfaces {
        if !seen_iface.iter().any(|s| *s == name.as_str()) {
            seen_iface.push(name.as_str());
        }
    }
    let interfaces = seen_iface;

    let mut methods: Vec<&ProxyMethod> = Vec::with_capacity(spec.methods.len());
    for m in &spec.methods {
        if !methods
            .iter()
            .any(|p| p.name == m.name && p.descriptor == m.descriptor)
        {
            methods.push(m);
        }
    }

    if interfaces.len() > u16::MAX as usize {
        return Err(invalid_proxy_classfile(
            &spec.gen_class_name,
            format!(
                "proxy implements {} interfaces, exceeding the classfile u2 limit",
                interfaces.len()
            ),
        ));
    }
    if methods.len() > (u16::MAX as usize).saturating_sub(3) {
        return Err(invalid_proxy_classfile(
            &spec.gen_class_name,
            format!(
                "proxy declares {} methods, exceeding the classfile methods_count limit",
                methods.len().saturating_add(3)
            ),
        ));
    }

    let mut cp = CpBuilder::new();

    // Resolve all class/method/string CP indices we'll need up-front.
    let this_class_idx = cp.add_class(&spec.gen_class_name);
    let super_class_idx = cp.add_class(&spec.super_class);
    let iface_idxs: Vec<u16> = interfaces.iter().map(|i| cp.add_class(i)).collect();
    let code_attr_name_idx = cp.add_utf8("Code");

    // Constructor descriptor and CP refs for super.<init>.
    //
    // proxy-real-classfile real-super migration: when the proxy extends the
    // *real* `java.lang.reflect.Proxy`, its super constructor is the 1-arg
    // `Proxy(InvocationHandler)` — NOT the synthetic `Proxy$Instance`'s 2-arg
    // `(InvocationHandler, Class[])`. The generated `$ProxyN.<init>` mirrors the
    // super arity (it just forwards its args), and the real `Proxy` stores the
    // handler in its sole instance field `h` (slot 0). For the synthetic super
    // the 2-arg shape is unchanged. (The `<clinit>` / per-method bodies are
    // independent of the super, so only the constructor differs.)
    let real_super = spec.super_class == "java/lang/reflect/Proxy";
    let ctor_desc = if real_super {
        "(Ljava/lang/reflect/InvocationHandler;)V"
    } else {
        "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"
    };
    let init_name_idx = cp.add_utf8("<init>");
    let ctor_desc_idx = cp.add_utf8(ctor_desc);
    let super_ctor_ref = cp.add_methodref(&spec.super_class, "<init>", ctor_desc);

    // v3 dispatch helper INVOKESTATIC ref — new contract:
    //   (Object receiver, Method m, Object[] args) -> Object
    let dispatch_owner = "java/lang/reflect/Proxy$Dispatch";
    let dispatch_name = "invokeProxy";
    let dispatch_desc = "(Ljava/lang/Object;Ljava/lang/reflect/Method;\
                         [Ljava/lang/Object;)Ljava/lang/Object;";
    let dispatch_ref = cp.add_methodref(dispatch_owner, dispatch_name, dispatch_desc);
    let object_class_idx = cp.add_class("java/lang/Object");

    // Pre-allocate the FieldRef CP entries for each `m_<i>: Method` slot;
    // method bodies reference them via GETSTATIC and `<clinit>` writes
    // them via PUTSTATIC. Resolving them up-front keeps the slot indices
    // stable across both emission passes.
    let method_field_desc = "Ljava/lang/reflect/Method;";
    let m_field_refs: Vec<u16> = (0..methods.len())
        .map(|i| cp.add_fieldref(&spec.gen_class_name, &format!("m_{i}"), method_field_desc))
        .collect();
    // Capture the (name_utf8, desc_utf8) pairs explicitly for the
    // fields[] section below — fieldref allocation guarantees the
    // matching Utf8 already exists in the CP, but we want the indices
    // for the field_info entries.
    let m_field_meta: Vec<(u16, u16)> = (0..methods.len())
        .map(|i| {
            (
                cp.add_utf8(&format!("m_{i}")),
                cp.add_utf8(method_field_desc),
            )
        })
        .collect();

    // Emit method blobs.
    let mut method_blobs: Vec<Vec<u8>> = Vec::with_capacity(methods.len() + 3);

    // 1) Constructor `<init>` — pure super-delegate. `real_super` selects the
    //    1-arg (real `Proxy`) vs 2-arg (synthetic `Proxy$Instance`) shape.
    method_blobs.push(emit_constructor(
        init_name_idx,
        ctor_desc_idx,
        code_attr_name_idx,
        super_ctor_ref,
        real_super,
    ));

    // 1b) proxy-real-classfile increment 7 — JDK-shape 1-arg
    //     `<init>(InvocationHandler)` for the synthetic-super case, so the
    //     generated class is reflectively constructable exactly like a real JDK
    //     proxy: `pc.getConstructor(InvocationHandler.class).newInstance(h)`
    //     (the `Proxy.getProxyClass` path). The real-super case already emits the
    //     1-arg ctor above; for the synthetic super the canonical ctor is 2-arg
    //     (`(InvocationHandler, Class[])`), so we ADD a 1-arg ctor that delegates
    //     to the 2-arg synthetic super with a null interfaces array
    //     (`super(handler, null)`). `newProxyInstance` itself bypasses the ctor
    //     entirely (alloc + field writes), so this only affects reflective /
    //     `new`-based construction. Additive — the 2-arg ctor is retained.
    if !real_super {
        let one_arg_desc_idx = cp.add_utf8("(Ljava/lang/reflect/InvocationHandler;)V");
        method_blobs.push(emit_synthetic_one_arg_ctor(
            init_name_idx,
            one_arg_desc_idx,
            code_attr_name_idx,
            super_ctor_ref,
        ));
    }

    // 2) `<clinit>` — populate every `m_<i>` static slot.
    method_blobs.push(emit_clinit(
        &methods,
        &mut cp,
        code_attr_name_idx,
        &m_field_refs,
        &spec.gen_class_name,
        &interfaces,
    )?);

    // 3) One body per declared method.
    for (i, m) in methods.iter().enumerate() {
        method_blobs.push(emit_proxy_method(
            &mut cp,
            m,
            code_attr_name_idx,
            dispatch_ref,
            object_class_idx,
            m_field_refs[i],
        )?);
    }

    // ── Assemble ClassFile (JVMS §4.1) ───────────────────────────────────
    cp.check(&spec.gen_class_name)?;

    let mut out = Vec::with_capacity(
        256 + cp.entries.len() + method_blobs.iter().map(|b| b.len()).sum::<usize>(),
    );

    // magic + version (Java 21 = 65)
    out.extend_from_slice(&0xCAFEBABE_u32.to_be_bytes());
    out.extend_from_slice(&0_u16.to_be_bytes()); // minor
    out.extend_from_slice(&65_u16.to_be_bytes()); // major (Java 21)

    // constant_pool_count = 1 + total_entries (1-based, slot 0 reserved)
    out.extend_from_slice(&(cp.count + 1).to_be_bytes());
    out.extend_from_slice(&cp.entries);

    // access_flags = ACC_PUBLIC | ACC_FINAL | ACC_SUPER | ACC_SYNTHETIC
    let access_flags: u16 = 0x0001 | 0x0010 | 0x0020 | 0x1000;
    out.extend_from_slice(&access_flags.to_be_bytes());

    // this_class, super_class
    out.extend_from_slice(&this_class_idx.to_be_bytes());
    out.extend_from_slice(&super_class_idx.to_be_bytes());

    // interfaces[]
    out.extend_from_slice(&(iface_idxs.len() as u16).to_be_bytes());
    for idx in &iface_idxs {
        out.extend_from_slice(&idx.to_be_bytes());
    }

    // fields[] — one `static Method m_<i>` per declared method.
    // ACC_PRIVATE | ACC_STATIC | ACC_FINAL | ACC_SYNTHETIC = 0x101A.
    // (`final` matches JDK ProxyGenerator; `<clinit>` may still write
    // it because PUTSTATIC of a final field is permitted in <clinit>
    // per JVMS §4.9.2 / §5.4.3.2.)
    out.extend_from_slice(&(methods.len() as u16).to_be_bytes());
    for (name_idx, desc_idx) in &m_field_meta {
        let field_flags: u16 = 0x0002 | 0x0008 | 0x0010 | 0x1000;
        out.extend_from_slice(&field_flags.to_be_bytes());
        out.extend_from_slice(&name_idx.to_be_bytes());
        out.extend_from_slice(&desc_idx.to_be_bytes());
        // attributes_count = 0 (no ConstantValue — written from <clinit>)
        out.extend_from_slice(&0_u16.to_be_bytes());
    }

    // methods[]
    out.extend_from_slice(&(method_blobs.len() as u16).to_be_bytes());
    for blob in &method_blobs {
        out.extend_from_slice(blob);
    }

    // class attributes[] — empty.
    out.extend_from_slice(&0_u16.to_be_bytes());

    Ok(out)
}

// ─────────────────────────────────────────────────────────────────────────────
// Descriptor parsing
// ─────────────────────────────────────────────────────────────────────────────

/// Kind of a single descriptor parameter / return type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DescKind {
    /// Boolean/byte/char/short/int — single JVM stack slot.
    Int,
    /// long — double JVM stack slot.
    Long,
    /// float — single JVM stack slot.
    Float,
    /// double — double JVM stack slot.
    Double,
    /// Reference (object or array). `descriptor` is the full descriptor
    /// string (e.g. `"Ljava/lang/String;"` or `"[I"`) so we can produce
    /// correct CHECKCAST class names.
    Reference(String),
}

impl DescKind {
    fn slots(&self) -> u16 {
        match self {
            DescKind::Long | DescKind::Double => 2,
            _ => 1,
        }
    }
}

/// Return-type kind for a method descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnKind {
    Void,
    Prim(DescKind),
    Reference(String),
}

/// Parse the parameter list of a method descriptor (the part between `(`
/// and `)`).
///
/// Audit fix (MED): malformed descriptors from loaded classes surface as
/// [`ClassFileError::InvalidClassFile`] instead of panicking.
pub fn descriptor_param_slots(desc: &str) -> Result<Vec<DescKind>, ClassFileError> {
    let bytes = desc.as_bytes();
    let mut out = Vec::new();
    let start =
        bytes
            .iter()
            .position(|&b| b == b'(')
            .ok_or_else(|| ClassFileError::InvalidClassFile {
                class_name: String::new(),
                message: format!("method descriptor '{desc}' missing '('"),
            })?
            + 1;
    let end =
        bytes
            .iter()
            .position(|&b| b == b')')
            .ok_or_else(|| ClassFileError::InvalidClassFile {
                class_name: String::new(),
                message: format!("method descriptor '{desc}' missing ')'"),
            })?;
    let mut i = start;
    while i < end {
        let (kind, advanced) = parse_one_field(desc, i)?;
        out.push(kind);
        if advanced <= i {
            // Defensive: a non-advancing parse would loop forever.
            break;
        }
        i = advanced;
    }
    Ok(out)
}

/// Parse the return type of a method descriptor.
///
/// Audit fix (MED): malformed descriptors from loaded classes surface as
/// [`ClassFileError::InvalidClassFile`] instead of panicking.
pub fn descriptor_return(desc: &str) -> Result<ReturnKind, ClassFileError> {
    let bytes = desc.as_bytes();
    let close =
        bytes
            .iter()
            .position(|&b| b == b')')
            .ok_or_else(|| ClassFileError::InvalidClassFile {
                class_name: String::new(),
                message: format!("method descriptor '{desc}' missing ')'"),
            })?;
    let after = close + 1;
    if after >= bytes.len() {
        return Ok(ReturnKind::Void);
    }
    if bytes[after] == b'V' {
        return Ok(ReturnKind::Void);
    }
    let (kind, _) = parse_one_field(desc, after)?;
    Ok(match kind {
        DescKind::Reference(s) => ReturnKind::Reference(s),
        other => ReturnKind::Prim(other),
    })
}

/// Parse a single field-type (JVMS §4.3.2) at `desc[i..]`. Returns
/// `(kind, next_index)`.
///
/// Audit fix (MED): descriptors can originate from loaded — and therefore
/// potentially malformed — classes. A bad descriptor byte or an `L`-type
/// missing its `;` terminator must surface as a typed
/// [`ClassFileError::InvalidClassFile`] rather than panicking the VM.
fn parse_one_field(desc: &str, i: usize) -> Result<(DescKind, usize), ClassFileError> {
    let bytes = desc.as_bytes();
    let byte = *bytes
        .get(i)
        .ok_or_else(|| ClassFileError::InvalidClassFile {
            class_name: String::new(),
            message: format!("descriptor '{desc}' truncated at index {i}"),
        })?;
    match byte {
        b'B' | b'C' | b'I' | b'S' | b'Z' => Ok((DescKind::Int, i + 1)),
        b'J' => Ok((DescKind::Long, i + 1)),
        b'F' => Ok((DescKind::Float, i + 1)),
        b'D' => Ok((DescKind::Double, i + 1)),
        b'L' => {
            let rel = desc[i + 1..]
                .find(';')
                .ok_or_else(|| ClassFileError::InvalidClassFile {
                    class_name: String::new(),
                    message: format!("L-type missing terminator ';' in descriptor '{desc}'"),
                })?;
            let semi = i + 1 + rel;
            let raw = &desc[i..=semi];
            Ok((DescKind::Reference(raw.to_string()), semi + 1))
        }
        b'[' => {
            let (inner, next) = parse_one_field(desc, i + 1)?;
            let _ = inner;
            let raw = &desc[i..next];
            Ok((DescKind::Reference(raw.to_string()), next))
        }
        other => Err(ClassFileError::InvalidClassFile {
            class_name: String::new(),
            message: format!("unsupported descriptor byte: 0x{other:02X} in '{desc}'"),
        }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Constant-pool builder
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
enum CpKey {
    // Round 5 audit fix (MED): `Utf8` was removed from this enum. UTF-8
    // dedup is now performed by `CpBuilder::utf8_dedup` (a dedicated
    // `HashMap<String, u16>`) so probe-time lookup uses a `&str` key
    // — no `s.to_string()` per `add_utf8` call. Owned strings are
    // allocated only on cache miss (the actual insert path).
    Class(String),
    String(u16),
    NameAndType(u16, u16),
    MethodRef(String, String, String),
    FieldRef(String, String, String),
    InterfaceMethodRef(String, String, String),
    Integer(i32),
}

/// Linear-append constant pool with `(tag, key) -> u16` dedup.
///
/// `entries` is the raw byte stream of CP info structures (no tombstone —
/// the tombstone at index 0 is implicit; the final classfile encodes
/// `constant_pool_count = count + 1`).
#[derive(Debug)]
pub struct CpBuilder {
    entries: Vec<u8>,
    dedup: HashMap<CpKey, u16>,
    /// Round 5 audit fix (MED): dedicated UTF-8 dedup map keyed on
    /// owned `String`. `add_utf8` probes with the borrowed `&str`
    /// (HashMap's `K: Borrow<Q>` impl makes `get(&str)` work against
    /// `HashMap<String, _>`) so the owned `String` is only built on
    /// cache miss. Previously every probe allocated an owned `String`
    /// for `CpKey::Utf8(s.to_string())` regardless of hit/miss — a
    /// 12+ allocations-per-method overhead on `proxy_gen` emission.
    utf8_dedup: HashMap<String, u16>,
    count: u16,
    error: Option<String>,
}

impl CpBuilder {
    fn new() -> Self {
        Self {
            entries: Vec::with_capacity(256),
            dedup: HashMap::new(),
            utf8_dedup: HashMap::new(),
            count: 0,
            error: None,
        }
    }

    /// Allocate the next 1-based CP slot. Long/Double take 2 slots — proxy
    /// emission never produces them (no LDC2_W of a long/double constant),
    /// so the simple +1 is sufficient.
    fn next_index(&mut self) -> u16 {
        if self.error.is_some() {
            return 0;
        }
        if self.count >= u16::MAX - 1 {
            return self.fail("constant pool overflow (>65534 entries)");
        }
        self.count += 1;
        self.count
    }

    fn fail(&mut self, message: impl Into<String>) -> u16 {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
        0
    }

    fn check(&self, class_name: &str) -> Result<(), ClassFileError> {
        match &self.error {
            Some(message) => Err(invalid_proxy_classfile(class_name, message.clone())),
            None => Ok(()),
        }
    }

    /// CONSTANT_Utf8 (tag 1, JVMS §4.4.7).
    ///
    /// Round 5 audit fix (MED): probe with the borrowed `&str` first
    /// (zero allocation) and only build the owned `String` key on
    /// cache miss. `HashMap<String, _>::get(&str)` works because
    /// `String: Borrow<str>` — the lookup hashes the `&str` directly.
    pub fn add_utf8(&mut self, s: &str) -> u16 {
        if self.error.is_some() {
            return 0;
        }
        if let Some(&i) = self.utf8_dedup.get(s) {
            return i;
        }
        let bytes = s.as_bytes();
        if bytes.len() > u16::MAX as usize {
            return self.fail(format!("UTF-8 entry too long: {} bytes", bytes.len()));
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(1);
        self.entries
            .extend_from_slice(&(bytes.len() as u16).to_be_bytes());
        self.entries.extend_from_slice(bytes);
        self.utf8_dedup.insert(s.to_string(), idx);
        idx
    }

    /// CONSTANT_Class (tag 7).
    pub fn add_class(&mut self, internal_name: &str) -> u16 {
        let key = CpKey::Class(internal_name.to_string());
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let name_idx = self.add_utf8(internal_name);
        if name_idx == 0 {
            return 0;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(7);
        self.entries.extend_from_slice(&name_idx.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_String (tag 8).
    pub fn add_string(&mut self, s: &str) -> u16 {
        let utf8_idx = self.add_utf8(s);
        if utf8_idx == 0 {
            return 0;
        }
        let key = CpKey::String(utf8_idx);
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(8);
        self.entries.extend_from_slice(&utf8_idx.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_NameAndType (tag 12).
    pub fn add_name_and_type(&mut self, name: &str, desc: &str) -> u16 {
        let n = self.add_utf8(name);
        let d = self.add_utf8(desc);
        if n == 0 || d == 0 {
            return 0;
        }
        let key = CpKey::NameAndType(n, d);
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(12);
        self.entries.extend_from_slice(&n.to_be_bytes());
        self.entries.extend_from_slice(&d.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_Methodref (tag 10).
    pub fn add_methodref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let key = CpKey::MethodRef(owner.into(), name.into(), desc.into());
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let class_idx = self.add_class(owner);
        let nat_idx = self.add_name_and_type(name, desc);
        if class_idx == 0 || nat_idx == 0 {
            return 0;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(10);
        self.entries.extend_from_slice(&class_idx.to_be_bytes());
        self.entries.extend_from_slice(&nat_idx.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_Fieldref (tag 9).
    pub fn add_fieldref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let key = CpKey::FieldRef(owner.into(), name.into(), desc.into());
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let class_idx = self.add_class(owner);
        let nat_idx = self.add_name_and_type(name, desc);
        if class_idx == 0 || nat_idx == 0 {
            return 0;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(9);
        self.entries.extend_from_slice(&class_idx.to_be_bytes());
        self.entries.extend_from_slice(&nat_idx.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_InterfaceMethodref (tag 11).
    pub fn add_interface_methodref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let key = CpKey::InterfaceMethodRef(owner.into(), name.into(), desc.into());
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let class_idx = self.add_class(owner);
        let nat_idx = self.add_name_and_type(name, desc);
        if class_idx == 0 || nat_idx == 0 {
            return 0;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(11);
        self.entries.extend_from_slice(&class_idx.to_be_bytes());
        self.entries.extend_from_slice(&nat_idx.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }

    /// CONSTANT_Integer (tag 3).
    pub fn add_integer(&mut self, value: i32) -> u16 {
        let key = CpKey::Integer(value);
        if let Some(&i) = self.dedup.get(&key) {
            return i;
        }
        let idx = self.next_index();
        if idx == 0 {
            return 0;
        }
        self.entries.push(3);
        self.entries.extend_from_slice(&value.to_be_bytes());
        self.dedup.insert(key, idx);
        idx
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Code-stream builder
// ─────────────────────────────────────────────────────────────────────────────

/// Opcodes from JVMS §6.5 (only the ones we emit).
mod op {
    pub const ALOAD_0: u8 = 0x2A;
    pub const ALOAD: u8 = 0x19;
    pub const ILOAD: u8 = 0x15;
    pub const LLOAD: u8 = 0x16;
    pub const FLOAD: u8 = 0x17;
    pub const DLOAD: u8 = 0x18;
    pub const ACONST_NULL: u8 = 0x01;
    pub const ICONST_M1: u8 = 0x02;
    pub const ICONST_0: u8 = 0x03;
    pub const ICONST_5: u8 = 0x08;
    pub const BIPUSH: u8 = 0x10;
    pub const SIPUSH: u8 = 0x11;
    pub const LDC: u8 = 0x12;
    pub const LDC_W: u8 = 0x13;
    pub const DUP: u8 = 0x59;
    pub const POP: u8 = 0x57;
    pub const ANEWARRAY: u8 = 0xBD;
    pub const AASTORE: u8 = 0x53;
    pub const AALOAD: u8 = 0x32;
    pub const CHECKCAST: u8 = 0xC0;
    pub const INVOKESTATIC: u8 = 0xB8;
    pub const INVOKESPECIAL: u8 = 0xB7;
    pub const INVOKEVIRTUAL: u8 = 0xB6;
    pub const RETURN: u8 = 0xB1;
    pub const ARETURN: u8 = 0xB0;
    pub const IRETURN: u8 = 0xAC;
    pub const LRETURN: u8 = 0xAD;
    pub const FRETURN: u8 = 0xAE;
    pub const DRETURN: u8 = 0xAF;
}

#[derive(Debug)]
pub struct CodeBuilder {
    pub bytes: Vec<u8>,
}

impl CodeBuilder {
    fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(64),
        }
    }

    pub fn emit_aload(&mut self, n: u8) {
        if n == 0 {
            self.bytes.push(op::ALOAD_0);
        } else if n <= 3 {
            self.bytes.push(0x2A + n);
        } else {
            self.bytes.push(op::ALOAD);
            self.bytes.push(n);
        }
    }

    pub fn emit_iload(&mut self, n: u8) {
        if n <= 3 {
            self.bytes.push(0x1A + n);
        } else {
            self.bytes.push(op::ILOAD);
            self.bytes.push(n);
        }
    }

    pub fn emit_lload(&mut self, n: u8) {
        if n <= 3 {
            self.bytes.push(0x1E + n);
        } else {
            self.bytes.push(op::LLOAD);
            self.bytes.push(n);
        }
    }

    pub fn emit_fload(&mut self, n: u8) {
        if n <= 3 {
            self.bytes.push(0x22 + n);
        } else {
            self.bytes.push(op::FLOAD);
            self.bytes.push(n);
        }
    }

    pub fn emit_dload(&mut self, n: u8) {
        if n <= 3 {
            self.bytes.push(0x26 + n);
        } else {
            self.bytes.push(op::DLOAD);
            self.bytes.push(n);
        }
    }

    /// LDC if the index fits in u8, else LDC_W. Used for String CP entries.
    pub fn emit_ldc_w(&mut self, idx: u16) {
        if idx <= u8::MAX as u16 {
            self.bytes.push(op::LDC);
            self.bytes.push(idx as u8);
        } else {
            self.bytes.push(op::LDC_W);
            self.bytes.extend_from_slice(&idx.to_be_bytes());
        }
    }

    pub fn emit_iconst(&mut self, n: i32) {
        if (-1..=5).contains(&n) {
            self.bytes.push(
                (op::ICONST_0 as i32 + n).clamp(op::ICONST_M1 as i32, op::ICONST_5 as i32) as u8,
            );
        } else if (i8::MIN as i32..=i8::MAX as i32).contains(&n) {
            self.bytes.push(op::BIPUSH);
            self.bytes.push(n as i8 as u8);
        } else if (i16::MIN as i32..=i16::MAX as i32).contains(&n) {
            self.bytes.push(op::SIPUSH);
            self.bytes.extend_from_slice(&(n as i16).to_be_bytes());
        } else {
            unreachable!("ICONST out of range: {n}");
        }
    }

    pub fn emit_invokestatic(&mut self, idx: u16) {
        self.bytes.push(op::INVOKESTATIC);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub fn emit_invokespecial(&mut self, idx: u16) {
        self.bytes.push(op::INVOKESPECIAL);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub fn emit_invokevirtual(&mut self, idx: u16) {
        self.bytes.push(op::INVOKEVIRTUAL);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub fn emit_anewarray(&mut self, idx: u16) {
        self.bytes.push(op::ANEWARRAY);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub fn emit_checkcast(&mut self, idx: u16) {
        self.bytes.push(op::CHECKCAST);
        self.bytes.extend_from_slice(&idx.to_be_bytes());
    }

    pub fn emit_aastore(&mut self) {
        self.bytes.push(op::AASTORE);
    }

    pub fn emit_aaload(&mut self) {
        self.bytes.push(op::AALOAD);
    }

    pub fn emit_aconst_null(&mut self) {
        self.bytes.push(op::ACONST_NULL);
    }

    pub fn emit_dup(&mut self) {
        self.bytes.push(op::DUP);
    }

    pub fn emit_pop(&mut self) {
        self.bytes.push(op::POP);
    }

    pub fn emit_areturn(&mut self) {
        self.bytes.push(op::ARETURN);
    }

    pub fn emit_ireturn(&mut self) {
        self.bytes.push(op::IRETURN);
    }

    pub fn emit_lreturn(&mut self) {
        self.bytes.push(op::LRETURN);
    }

    pub fn emit_freturn(&mut self) {
        self.bytes.push(op::FRETURN);
    }

    pub fn emit_dreturn(&mut self) {
        self.bytes.push(op::DRETURN);
    }

    pub fn emit_return(&mut self) {
        self.bytes.push(op::RETURN);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Method emission
// ─────────────────────────────────────────────────────────────────────────────

/// Emit the method_info bytes (JVMS §4.6) for the constructor:
/// `<init>(InvocationHandler, Class[]) { super(handler, ifaces); return; }`
fn emit_constructor(
    init_name_idx: u16,
    ctor_desc_idx: u16,
    code_attr_name_idx: u16,
    super_ctor_ref: u16,
    real_super: bool,
) -> Vec<u8> {
    let mut code = CodeBuilder::new();
    code.emit_aload(0); // this
    code.emit_aload(1); // handler
    if !real_super {
        // Synthetic `Proxy$Instance.<init>(InvocationHandler, Class[])` takes
        // the interfaces array too; the real `Proxy(InvocationHandler)` does not.
        code.emit_aload(2); // interfaces
    }
    code.emit_invokespecial(super_ctor_ref);
    code.emit_return();

    // Real super: 1 arg (this + handler). Synthetic super: 2 args (+ ifaces).
    let (max_stack, max_locals): (u16, u16) = if real_super { (2, 2) } else { (3, 3) };
    let access_flags: u16 = 0x0001; // ACC_PUBLIC

    build_method_info(
        access_flags,
        init_name_idx,
        ctor_desc_idx,
        code_attr_name_idx,
        max_stack,
        max_locals,
        code.bytes,
    )
}

/// proxy-real-classfile increment 7 — emit the JDK-shape 1-arg
/// `<init>(InvocationHandler) { super(handler, null); return; }` for the
/// synthetic-super case. `super_ctor_ref` is the 2-arg
/// `Proxy$Instance.<init>(InvocationHandler, Class[])V` methodref; the null
/// `Class[]` leaves the interfaces slot unset (the synthetic super's native
/// `<init>` writes slot 0 = handler regardless, which is all the dispatch path
/// reads). This makes `pc.getConstructor(InvocationHandler.class).newInstance(h)`
/// work on a generated proxy obtained via `Proxy.getProxyClass`. Straight-line,
/// no `StackMapTable`.
fn emit_synthetic_one_arg_ctor(
    init_name_idx: u16,
    one_arg_desc_idx: u16,
    code_attr_name_idx: u16,
    super_ctor_ref: u16,
) -> Vec<u8> {
    let mut code = CodeBuilder::new();
    code.emit_aload(0); // this
    code.emit_aload(1); // handler
    code.emit_aconst_null(); // interfaces = null
    code.emit_invokespecial(super_ctor_ref); // super(handler, null)
    code.emit_return();

    // Stack: this + handler + null = 3 at the invokespecial. Locals: this + handler.
    let (max_stack, max_locals): (u16, u16) = (3, 2);
    let access_flags: u16 = 0x0001; // ACC_PUBLIC

    build_method_info(
        access_flags,
        init_name_idx,
        one_arg_desc_idx,
        code_attr_name_idx,
        max_stack,
        max_locals,
        code.bytes,
    )
}

fn checked_iconst_usize(value: usize, context: &str) -> Result<i32, ClassFileError> {
    if value > i16::MAX as usize {
        return Err(invalid_proxy_classfile(
            "",
            format!("{context} value {value} exceeds current proxy iconst/sipush encoding"),
        ));
    }
    Ok(value as i32)
}

fn checked_load_slot(slot: u16, method_name: &str) -> Result<u8, ClassFileError> {
    u8::try_from(slot).map_err(|_| {
        invalid_proxy_classfile(
            "",
            format!(
                "proxy method {method_name} requires local slot {slot}, exceeding non-wide load encoding"
            ),
        )
    })
}

fn checked_param_slot_count(params: &[DescKind], method_name: &str) -> Result<u16, ClassFileError> {
    params.iter().try_fold(0u16, |acc, kind| {
        acc.checked_add(kind.slots()).ok_or_else(|| {
            invalid_proxy_classfile(
                "",
                format!("proxy method {method_name} parameter slots exceed u2 max_locals"),
            )
        })
    })
}

fn checked_proxy_max_stack(units: u16, context: &str) -> Result<u16, ClassFileError> {
    4u16.checked_add(units.checked_mul(2).ok_or_else(|| {
        invalid_proxy_classfile("", format!("{context} max_stack calculation overflowed"))
    })?)
    .ok_or_else(|| invalid_proxy_classfile("", format!("{context} max_stack exceeds u2 limit")))
}

/// Emit method_info for one proxy method (delegating into
/// `Proxy$Dispatch.invokeProxy`).
///
/// v3 shape:
/// ```text
///   ALOAD_0                               ; receiver
///   GETSTATIC m_<i>:Ljava/lang/reflect/Method;
///   ICONST_<argc> ; ANEWARRAY java/lang/Object
///   <box args>
///   INVOKESTATIC Proxy$Dispatch.invokeProxy(Object, Method, Object[]) Object
///   <unbox / return>
/// ```
fn emit_proxy_method(
    cp: &mut CpBuilder,
    m: &ProxyMethod,
    code_attr_name_idx: u16,
    dispatch_ref: u16,
    object_class_idx: u16,
    m_field_ref: u16,
) -> Result<Vec<u8>, ClassFileError> {
    let _ = m.is_default; // currently informational only; same body either way

    let params = descriptor_param_slots(&m.descriptor)?;
    let ret = descriptor_return(&m.descriptor)?;
    let param_count_i32 = checked_iconst_usize(params.len(), "proxy method parameter count")?;

    let mut code = CodeBuilder::new();

    // 1) ALOAD_0 (this / receiver)
    code.emit_aload(0);
    // 2) GETSTATIC m_<i> — push the cached java.lang.reflect.Method
    code.bytes.push(0xB2 /* GETSTATIC */);
    code.bytes.extend_from_slice(&m_field_ref.to_be_bytes());
    // 3) Build Object[] of length params.len() and box each arg.
    code.emit_iconst(param_count_i32);
    code.emit_anewarray(object_class_idx);

    let mut local_slot: u16 = 1; // 0 = this
    for (i, kind) in params.iter().enumerate() {
        let load_slot = checked_load_slot(local_slot, &m.name)?;
        code.emit_dup();
        code.emit_iconst(checked_iconst_usize(i, "proxy method argument index")?);
        match kind {
            DescKind::Int => {
                // For Z/B/C/S/I we must box via the matching wrapper so the
                // InvocationHandler.invoke contract delivers the correct
                // boxed type (`equals(Object)Z` → handler receives Boolean
                // arg from caller, not Integer). Parse the actual descriptor
                // byte at this parameter position.
                let wrapper = int_family_param_wrapper(&m.descriptor, i);
                let prim_letter = int_family_param_letter(&m.descriptor, i);
                code.emit_iload(load_slot);
                let valueof_desc = format!("({prim_letter})L{wrapper};");
                let r = cp.add_methodref(wrapper, "valueOf", &valueof_desc);
                code.emit_invokestatic(r);
            }
            DescKind::Long => {
                code.emit_lload(load_slot);
                let r = cp.add_methodref("java/lang/Long", "valueOf", "(J)Ljava/lang/Long;");
                code.emit_invokestatic(r);
            }
            DescKind::Float => {
                code.emit_fload(load_slot);
                let r = cp.add_methodref("java/lang/Float", "valueOf", "(F)Ljava/lang/Float;");
                code.emit_invokestatic(r);
            }
            DescKind::Double => {
                code.emit_dload(load_slot);
                let r = cp.add_methodref("java/lang/Double", "valueOf", "(D)Ljava/lang/Double;");
                code.emit_invokestatic(r);
            }
            DescKind::Reference(_) => {
                code.emit_aload(load_slot);
            }
        }
        code.emit_aastore();
        local_slot = local_slot.checked_add(kind.slots()).ok_or_else(|| {
            invalid_proxy_classfile(
                "",
                format!("proxy method {} local slot calculation overflowed", m.name),
            )
        })?;
    }

    // 5) INVOKESTATIC Proxy$Dispatch.invokeProxy(...)
    code.emit_invokestatic(dispatch_ref);

    // 6) Return path — coerce Object back to declared return type.
    match ret {
        ReturnKind::Void => {
            code.emit_pop();
            code.emit_return();
        }
        ReturnKind::Reference(raw) => {
            // CHECKCAST takes a Class CP entry. For "L<Name>;" we strip the
            // L/; per JVMS §4.4.1; for arrays "[..." we keep the descriptor.
            let cast_name = if raw.starts_with('L') && raw.ends_with(';') {
                raw[1..raw.len() - 1].to_string()
            } else {
                raw.clone()
            };
            let cast_idx = cp.add_class(&cast_name);
            code.emit_checkcast(cast_idx);
            code.emit_areturn();
        }
        ReturnKind::Prim(DescKind::Int) => {
            // The InvocationHandler returns a wrapper of the *exact*
            // declared return type. For `equals(Object)Z` the spec
            // requires `Boolean` — casting to `Integer` here triggers
            // ClassCastException at $Proxy<n>.equals (the failure mode
            // observed when Spring's Binder calls
            // ObjectUtils.nullSafeEquals on a proxied source). Pick the
            // wrapper / unboxing helper from the actual return-byte.
            let bytes = m.descriptor.as_bytes();
            let close = bytes.iter().position(|&b| b == b')').unwrap_or(0);
            let ret_byte = *bytes.get(close + 1).unwrap_or(&b'I');
            let (wrapper, value_method, value_desc) = match ret_byte {
                b'Z' => ("java/lang/Boolean", "booleanValue", "()Z"),
                b'B' => ("java/lang/Byte", "byteValue", "()B"),
                b'C' => ("java/lang/Character", "charValue", "()C"),
                b'S' => ("java/lang/Short", "shortValue", "()S"),
                _ => ("java/lang/Integer", "intValue", "()I"),
            };
            let cast_idx = cp.add_class(wrapper);
            code.emit_checkcast(cast_idx);
            let r = cp.add_methodref(wrapper, value_method, value_desc);
            code.emit_invokevirtual(r);
            code.emit_ireturn();
        }
        ReturnKind::Prim(DescKind::Long) => {
            let cast_idx = cp.add_class("java/lang/Long");
            code.emit_checkcast(cast_idx);
            let r = cp.add_methodref("java/lang/Long", "longValue", "()J");
            code.emit_invokevirtual(r);
            code.emit_lreturn();
        }
        ReturnKind::Prim(DescKind::Float) => {
            let cast_idx = cp.add_class("java/lang/Float");
            code.emit_checkcast(cast_idx);
            let r = cp.add_methodref("java/lang/Float", "floatValue", "()F");
            code.emit_invokevirtual(r);
            code.emit_freturn();
        }
        ReturnKind::Prim(DescKind::Double) => {
            let cast_idx = cp.add_class("java/lang/Double");
            code.emit_checkcast(cast_idx);
            let r = cp.add_methodref("java/lang/Double", "doubleValue", "()D");
            code.emit_invokevirtual(r);
            code.emit_dreturn();
        }
        ReturnKind::Prim(DescKind::Reference(_)) => {
            unreachable!("Prim never holds Reference")
        }
    }

    // ── Method header ────────────────────────────────────────────────────
    let name_idx = cp.add_utf8(&m.name);
    let desc_idx = cp.add_utf8(&m.descriptor);

    // Conservative max_stack: 4 baseline (this/name/desc/array) + 2*params
    // for safety (room for primitive-load + DUP + box-call temporaries).
    let param_slot_count = checked_param_slot_count(&params, &m.name)?;
    let max_stack = checked_proxy_max_stack(param_slot_count, "proxy method")?;
    // max_locals: 1 (this) + param_slot_count.
    let max_locals = 1u16.checked_add(param_slot_count).ok_or_else(|| {
        invalid_proxy_classfile(
            "",
            format!("proxy method {} max_locals exceeds u2 limit", m.name),
        )
    })?;
    // ACC_PUBLIC | ACC_FINAL (proxy methods are non-overridable).
    let access_flags: u16 = 0x0001 | 0x0010;

    Ok(build_method_info(
        access_flags,
        name_idx,
        desc_idx,
        code_attr_name_idx,
        max_stack,
        max_locals,
        code.bytes,
    ))
}

/// Emit `<clinit>()V` populating each `m_<i>` static slot via
/// `<owner Class>.getMethod(name, paramTypes)`.
///
/// The owner `Class` comes from the generated class's own `getInterfaces()`
/// whenever [`ProxyMethod::iface_root`] names one of the emitted interfaces:
///
/// ```text
/// LDC class "<gen_class_name>"                      ; this proxy class
/// INVOKEVIRTUAL Class.getInterfaces()               ; [Ljava/lang/Class;
/// ICONST_<k> ; AALOAD                               ; the exact interface
/// ```
///
/// `getMethod` searches super-interfaces, so a method declared on a
/// super-interface is still found through its declared root and comes back
/// with that world's declaring class. A constant-pool `LDC class
/// <iface_owner>` cannot do this: CP class resolution is by NAME through the
/// flat global store, so with two loader worlds holding the same interface it
/// can bind the wrong copy. `Method.equals` compares declaring classes by
/// IDENTITY, so the caller's own `Map<Method, …>` — Byte Buddy's
/// `JavaDispatcher` is one — then misses on every lookup.
///
/// Falls back to the constant-pool form for `java/lang/Object` methods and for
/// any root not present in `interfaces`.
///
/// Per-method shape (JVMS §5.5 class initialization):
/// ```text
/// <owner Class push, see above>                     ; Ljava/lang/Class;
/// LDC "<method.name>"                               ; Ljava/lang/String;
/// ICONST_<paramCount> ; ANEWARRAY java/lang/Class   ; param Class[]
/// (per param j with class_name P_j:
///     DUP ; ICONST_<j> ;
///     primitive  → GETSTATIC <Wrapper>.TYPE Ljava/lang/Class;
///     reference  → LDC class "<P_j>"
///     AASTORE)
/// INVOKEVIRTUAL Class.getMethod(String, Class[])    ; Method
/// PUTSTATIC m_<i>:Ljava/lang/reflect/Method;
/// ```
/// Followed by a single `RETURN`.
///
/// Straight-line — no branches, no exception handlers, so no
/// `StackMapTable` is required by JVMS §4.10.1. (Pass 3 type-checking
/// accepts straight-line bodies without frames.)
///
/// `methods` is the **de-duplicated** method list built by
/// `emit_proxy_classfile`; `m_field_refs[i]` must correspond to
/// `methods[i]`, so both have to be derived from the same list.
fn emit_clinit(
    methods: &[&ProxyMethod],
    cp: &mut CpBuilder,
    code_attr_name_idx: u16,
    m_field_refs: &[u16],
    gen_class_name: &str,
    interfaces: &[&str],
) -> Result<Vec<u8>, ClassFileError> {
    // Up-front constants used across every method.
    let class_class_idx = cp.add_class("java/lang/Class");
    let get_method_ref = cp.add_methodref(
        "java/lang/Class",
        "getMethod",
        "(Ljava/lang/String;[Ljava/lang/Class;)Ljava/lang/reflect/Method;",
    );
    let self_class_idx = cp.add_class(gen_class_name);
    let get_interfaces_ref =
        cp.add_methodref("java/lang/Class", "getInterfaces", "()[Ljava/lang/Class;");

    let mut code = CodeBuilder::new();
    let mut max_stack: u16 = 0;

    for (i, m) in methods.iter().enumerate() {
        // 1) Push the owner Class mirror. Prefer this class's own
        //    `getInterfaces()` entry (loader-faithful) over an `LDC class`
        //    of the owner's NAME (loader-blind) — see this function's doc.
        let root_index = m.iface_root.as_deref().and_then(|root| {
            interfaces
                .iter()
                .position(|declared| *declared == root)
                .filter(|_| m.iface_owner != "java/lang/Object")
        });
        match root_index {
            Some(k) => {
                code.emit_ldc_w(self_class_idx);
                code.emit_invokevirtual(get_interfaces_ref);
                code.emit_iconst(checked_iconst_usize(k, "proxy <clinit> interface index")?);
                code.emit_aaload();
            }
            None => {
                let iface_class_idx = cp.add_class(&m.iface_owner);
                code.emit_ldc_w(iface_class_idx);
            }
        }
        // 2) Push method name as String.
        let name_str_idx = cp.add_string(&m.name);
        code.emit_ldc_w(name_str_idx);

        // 3) Build parameter Class[] (length = descriptor param count,
        //    NOT JVM slot count — long/double count once each).
        let params = descriptor_param_slots(&m.descriptor)?;
        let param_count = params.len();
        code.emit_iconst(checked_iconst_usize(
            param_count,
            "proxy <clinit> parameter count",
        )?);
        code.emit_anewarray(class_class_idx);

        // Defensive: param_class_names should match params.len(). If a
        // caller hands us a mismatched count for a reference param, fall
        // back to the descriptor's L-name (strip L...;).
        let names = &m.param_class_names;
        for (j, kind) in params.iter().enumerate() {
            code.emit_dup();
            code.emit_iconst(checked_iconst_usize(j, "proxy <clinit> parameter index")?);
            match kind {
                // Primitive param → GETSTATIC <Wrapper>.TYPE
                DescKind::Int => {
                    let wrapper = wrapper_for_int_letter(&m.descriptor, j);
                    let fr = cp.add_fieldref(wrapper, "TYPE", "Ljava/lang/Class;");
                    code.bytes.push(0xB2 /* GETSTATIC */);
                    code.bytes.extend_from_slice(&fr.to_be_bytes());
                }
                DescKind::Long => {
                    let fr = cp.add_fieldref("java/lang/Long", "TYPE", "Ljava/lang/Class;");
                    code.bytes.push(0xB2);
                    code.bytes.extend_from_slice(&fr.to_be_bytes());
                }
                DescKind::Float => {
                    let fr = cp.add_fieldref("java/lang/Float", "TYPE", "Ljava/lang/Class;");
                    code.bytes.push(0xB2);
                    code.bytes.extend_from_slice(&fr.to_be_bytes());
                }
                DescKind::Double => {
                    let fr = cp.add_fieldref("java/lang/Double", "TYPE", "Ljava/lang/Class;");
                    code.bytes.push(0xB2);
                    code.bytes.extend_from_slice(&fr.to_be_bytes());
                }
                DescKind::Reference(raw) => {
                    // LDC class "<P_j>". Prefer the explicit
                    // param_class_names entry when supplied; otherwise
                    // fall back to descriptor (strip L...; or keep [...).
                    let class_name: String = if j < names.len() && !names[j].is_empty() {
                        names[j].clone()
                    } else if raw.starts_with('L') && raw.ends_with(';') {
                        raw[1..raw.len() - 1].to_string()
                    } else {
                        raw.clone()
                    };
                    let cidx = cp.add_class(&class_name);
                    code.emit_ldc_w(cidx);
                }
            }
            code.emit_aastore();
        }

        // 4) Class.getMethod(String, Class[]) → Method
        code.emit_invokevirtual(get_method_ref);

        // 5) PUTSTATIC m_<i>
        code.bytes.push(0xB3 /* PUTSTATIC */);
        code.bytes.extend_from_slice(&m_field_refs[i].to_be_bytes());

        // Per-method peak: 4 baseline (Class + String + Class[] + working
        // slot) + 2*param_count for DUP + per-element push.
        let peak_units = u16::try_from(param_count).map_err(|_| {
            invalid_proxy_classfile("", "proxy <clinit> parameter count exceeds u2 limit")
        })?;
        let peak = checked_proxy_max_stack(peak_units, "proxy <clinit>")?;
        if peak > max_stack {
            max_stack = peak;
        }
    }

    code.emit_return();

    // <clinit> is static and takes no args.
    let max_locals: u16 = 0;
    if max_stack < 4 {
        max_stack = 4;
    }

    let name_idx = cp.add_utf8("<clinit>");
    let desc_idx = cp.add_utf8("()V");
    let access_flags: u16 = 0x0008; // ACC_STATIC

    Ok(build_method_info(
        access_flags,
        name_idx,
        desc_idx,
        code_attr_name_idx,
        max_stack,
        max_locals,
        code.bytes,
    ))
}

/// Find the actual descriptor byte for the parameter at logical index
/// `j` (long/double counted as 1, NOT JVM slots — matches
/// `descriptor_param_slots` indexing). For reference parameters this
/// returns `b'L'` or `b'['`; for primitives it returns the canonical
/// JVMS letter (`B`, `C`, `D`, `F`, `I`, `J`, `S`, `Z`).
fn descriptor_param_byte_at(desc: &str, j: usize) -> u8 {
    let bytes = desc.as_bytes();
    let start = bytes.iter().position(|&b| b == b'(').unwrap_or(0) + 1;
    let end = bytes.iter().position(|&b| b == b')').unwrap_or(bytes.len());
    let mut i = start;
    let mut k = 0usize;
    while i < end {
        let head = bytes[i];
        // A malformed descriptor stops the walk; callers fall back to the
        // `b'I'` default below. `parse_one_field` no longer panics.
        let next = match parse_one_field(desc, i) {
            Ok((_, next)) => next,
            Err(_) => break,
        };
        if k == j {
            // Walk past array dimensions for the reporting byte. For
            // primitives `head` already is the canonical letter; for
            // reference types we return `L`.
            return head;
        }
        if next <= i {
            break;
        }
        i = next;
        k += 1;
    }
    b'I'
}

/// Wrapper class for an `Int`-family parameter at logical index `j` in
/// `desc`. Used by `emit_proxy_method` so a `Z` param boxes via
/// `Boolean.valueOf(Z)Boolean` rather than the wrong
/// `Integer.valueOf(I)Integer`.
fn int_family_param_wrapper(desc: &str, j: usize) -> &'static str {
    match descriptor_param_byte_at(desc, j) {
        b'B' => "java/lang/Byte",
        b'C' => "java/lang/Character",
        b'I' => "java/lang/Integer",
        b'S' => "java/lang/Short",
        b'Z' => "java/lang/Boolean",
        _ => "java/lang/Integer",
    }
}

/// JVMS letter for an `Int`-family parameter at logical index `j`. Used
/// to assemble the matching `valueOf` descriptor (e.g. `(Z)` for a
/// `boolean` param).
fn int_family_param_letter(desc: &str, j: usize) -> &'static str {
    match descriptor_param_byte_at(desc, j) {
        b'B' => "B",
        b'C' => "C",
        b'I' => "I",
        b'S' => "S",
        b'Z' => "Z",
        _ => "I",
    }
}

/// Map descriptor letter at parameter position `j` (in descriptor `desc`)
/// to the wrapper class's internal name for `<Wrapper>.TYPE` lookup.
/// Only meaningful when the descriptor slot is in the `Int` family
/// (B/C/I/S/Z); long/double/float are handled directly by `emit_clinit`.
fn wrapper_for_int_letter(desc: &str, j: usize) -> &'static str {
    let bytes = desc.as_bytes();
    let start = bytes.iter().position(|&b| b == b'(').unwrap_or(0) + 1;
    let end = bytes.iter().position(|&b| b == b')').unwrap_or(bytes.len());
    let mut i = start;
    let mut k = 0usize;
    while i < end {
        // A malformed descriptor stops the walk; the caller falls back to
        // the `java/lang/Integer` default. `parse_one_field` no longer
        // panics.
        let (kind, next) = match parse_one_field(desc, i) {
            Ok(parsed) => parsed,
            Err(_) => break,
        };
        if k == j {
            return match bytes[i] {
                b'B' => "java/lang/Byte",
                b'C' => "java/lang/Character",
                b'I' => "java/lang/Integer",
                b'S' => "java/lang/Short",
                b'Z' => "java/lang/Boolean",
                _ => "java/lang/Integer",
            };
        }
        let _ = kind;
        if next <= i {
            break;
        }
        i = next;
        k += 1;
    }
    "java/lang/Integer"
}

/// Build a `method_info` blob (JVMS §4.6) wrapping a single Code attribute
/// (JVMS §4.7.3). No exception_table, no inner attributes (no
/// LineNumberTable, no LocalVariableTable, **no StackMapTable** — see
/// module doc).
fn build_method_info(
    access_flags: u16,
    name_idx: u16,
    desc_idx: u16,
    code_attr_name_idx: u16,
    max_stack: u16,
    max_locals: u16,
    code_bytes: Vec<u8>,
) -> Vec<u8> {
    // Code attribute body:
    //   u2 max_stack; u2 max_locals;
    //   u4 code_length; u1 code[code_length];
    //   u2 exception_table_length; ExceptionTableEntry[];   -- 0 entries
    //   u2 attributes_count; attribute_info[];              -- 0 entries
    let mut code_attr_body = Vec::with_capacity(12 + code_bytes.len());
    code_attr_body.extend_from_slice(&max_stack.to_be_bytes());
    code_attr_body.extend_from_slice(&max_locals.to_be_bytes());
    code_attr_body.extend_from_slice(&(code_bytes.len() as u32).to_be_bytes());
    code_attr_body.extend_from_slice(&code_bytes);
    code_attr_body.extend_from_slice(&0_u16.to_be_bytes()); // exception_table_length
    code_attr_body.extend_from_slice(&0_u16.to_be_bytes()); // attributes_count

    let mut out = Vec::with_capacity(8 + 6 + code_attr_body.len());
    out.extend_from_slice(&access_flags.to_be_bytes());
    out.extend_from_slice(&name_idx.to_be_bytes());
    out.extend_from_slice(&desc_idx.to_be_bytes());
    out.extend_from_slice(&1_u16.to_be_bytes()); // 1 attribute (Code)
    out.extend_from_slice(&code_attr_name_idx.to_be_bytes());
    out.extend_from_slice(&(code_attr_body.len() as u32).to_be_bytes());
    out.extend_from_slice(&code_attr_body);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_reader::read_class;

    fn supplier_spec() -> ProxyClassSpec {
        ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy0".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["java/util/function/Supplier".to_string()],
            methods: vec![ProxyMethod {
                name: "get".to_string(),
                descriptor: "()Ljava/lang/Object;".to_string(),
                is_default: false,
                iface_owner: "java/util/function/Supplier".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            }],
        }
    }

    #[test]
    fn emits_supplier_proxy_with_get_method() {
        let bytes = emit_proxy_classfile(&supplier_spec()).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("emitted class file must round-trip through reader");
        assert_eq!(&*cf.this_class, "java/lang/reflect/$Proxy0");
        assert_eq!(
            cf.super_class.as_ref().map(|s| &**s),
            Some("java/lang/reflect/Proxy$Instance")
        );
        assert!(cf
            .interfaces
            .iter()
            .any(|s| &**s == "java/util/function/Supplier"));
        assert!(cf.methods.iter().any(|m| &*m.name == "<init>"
            && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"));
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
    }

    /// proxy-real-classfile real-super migration: a spec whose `super_class` is
    /// the real `java.lang.reflect.Proxy` emits a `$ProxyN` that extends it with
    /// a **1-arg** `<init>(InvocationHandler)` delegating to
    /// `Proxy.<init>(InvocationHandler)` — matching the real JDK proxy shape —
    /// while the synthetic-super path keeps the 2-arg ctor (covered above).
    #[test]
    fn emits_real_proxy_super_with_one_arg_ctor() {
        let mut spec = supplier_spec();
        spec.gen_class_name = "com/sun/proxy/$Proxy9".to_string();
        spec.super_class = "java/lang/reflect/Proxy".to_string();
        let bytes = emit_proxy_classfile(&spec).expect("real-super emitter must succeed");
        let cf = read_class(&bytes).expect("real-super class file must round-trip");

        // Extends the REAL Proxy, not the synthetic shim.
        assert_eq!(
            cf.super_class.as_ref().map(|s| &**s),
            Some("java/lang/reflect/Proxy")
        );

        // The constructor is the 1-arg InvocationHandler form (no Class[]).
        assert!(
            cf.methods.iter().any(|m| &*m.name == "<init>"
                && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;)V"),
            "real-super proxy must have a 1-arg <init>(InvocationHandler)"
        );
        // …and NOT the synthetic 2-arg form.
        assert!(
            !cf.methods.iter().any(|m| &*m.name == "<init>"
                && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"),
            "real-super proxy must not carry the synthetic 2-arg <init>"
        );
        // The interface method is still emitted.
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
    }

    /// proxy-real-classfile increment 7: the synthetic-super (`Proxy$Instance`)
    /// path now emits BOTH the canonical 2-arg
    /// `<init>(InvocationHandler, Class[])` AND a JDK-shape 1-arg
    /// `<init>(InvocationHandler)` (delegating to the 2-arg super with a null
    /// interfaces array), so the generated class is reflectively constructable
    /// via `getConstructor(InvocationHandler.class).newInstance(h)` — the
    /// `Proxy.getProxyClass` reflective-construction path.
    #[test]
    fn emits_synthetic_super_with_both_ctors() {
        let mut spec = supplier_spec();
        spec.gen_class_name = "jdk/proxy1/$Proxy0".to_string();
        spec.super_class = "java/lang/reflect/Proxy$Instance".to_string();
        let bytes = emit_proxy_classfile(&spec).expect("synthetic-super emitter must succeed");
        let cf = read_class(&bytes).expect("synthetic-super class file must round-trip");

        assert_eq!(
            cf.super_class.as_ref().map(|s| &**s),
            Some("java/lang/reflect/Proxy$Instance")
        );
        // Canonical 2-arg ctor retained (existing execution paths depend on it).
        assert!(
            cf.methods.iter().any(|m| &*m.name == "<init>"
                && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"),
            "synthetic-super proxy must retain the 2-arg <init>"
        );
        // JDK-shape 1-arg ctor also emitted (reflective construction).
        assert!(
            cf.methods.iter().any(|m| &*m.name == "<init>"
                && &*m.descriptor == "(Ljava/lang/reflect/InvocationHandler;)V"),
            "synthetic-super proxy must also emit the 1-arg <init>(InvocationHandler)"
        );
        // The interface method is still emitted.
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "get" && &*m.descriptor == "()Ljava/lang/Object;"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // JVMS §4.1 / §4.6 structural invariants
    // ─────────────────────────────────────────────────────────────────────

    /// JVMS §4.1: "No two entries in the `interfaces` array may be the same
    /// class or interface."
    ///
    /// Reachable repro: `build_proxy_spec_for` deduplicates the requested
    /// interfaces **by `ClassId`**, and two distinct `ClassId`s can share a
    /// name when the same interface is loaded by two different class loaders
    /// — routine in this VM (`force_loader_faithful_linking` exists precisely
    /// because of it). So
    ///
    /// ```java
    /// Proxy.newProxyInstance(l, new Class[]{ barFromLoaderA, barFromLoaderB }, h)
    /// ```
    ///
    /// arrived here as `interfaces: ["com/foo/Bar", "com/foo/Bar"]`. Because
    /// `CpBuilder::add_class` dedups, the emitted `interfaces[]` held two
    /// byte-identical u2 entries — a `ClassFormatError`-shaped classfile that
    /// HotSpot rejects outright.
    #[test]
    fn duplicate_interface_names_are_collapsed_to_one_entry() {
        let spec = ProxyClassSpec {
            gen_class_name: "jdk/proxy1/$Proxy7".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec![
                "java/lang/Runnable".to_string(),
                "java/lang/Runnable".to_string(), // same name, other loader
            ],
            methods: vec![ProxyMethod {
                name: "run".to_string(),
                descriptor: "()V".to_string(),
                is_default: false,
                iface_owner: "java/lang/Runnable".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            }],
        };
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("class file must round-trip");

        assert_eq!(
            cf.interfaces.len(),
            1,
            "JVMS §4.1 forbids repeated entries in interfaces[]; got {:?}",
            cf.interfaces.iter().map(|s| &**s).collect::<Vec<_>>()
        );
        assert_eq!(&*cf.interfaces[0], "java/lang/Runnable");
    }

    /// Interface **order** is load-bearing (Spring's
    /// `AopProxyUtils.proxiedUserInterfaces` trims trailing infrastructure
    /// interfaces off `getInterfaces()`), so the dedup must keep first
    /// occurrences in place rather than sorting or keeping the last.
    #[test]
    fn interface_dedup_preserves_first_occurrence_order() {
        let mut spec = supplier_spec();
        spec.interfaces = vec![
            "com/example/ITestBean".to_string(),
            "org/springframework/aop/SpringProxy".to_string(),
            "com/example/ITestBean".to_string(), // dup of [0]
            "org/springframework/aop/framework/Advised".to_string(),
        ];
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("class file must round-trip");
        let names: Vec<&str> = cf.interfaces.iter().map(|s| &**s).collect();
        assert_eq!(
            names,
            vec![
                "com/example/ITestBean",
                "org/springframework/aop/SpringProxy",
                "org/springframework/aop/framework/Advised",
            ]
        );
    }

    /// JVMS §4.6: "No two methods in one `class` file may have the same name
    /// and descriptor." A repeated `(name, descriptor)` also used to desync
    /// the positional `m_<i>` bookkeeping's *intent* — two shims resolving
    /// the same `Method` into two different static slots.
    #[test]
    fn duplicate_method_signatures_are_collapsed_to_one_method() {
        let mut spec = supplier_spec();
        let m = spec.methods[0].clone();
        spec.methods = vec![m.clone(), m];
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("class file must round-trip");

        let gets = cf
            .methods
            .iter()
            .filter(|mm| &*mm.name == "get" && &*mm.descriptor == "()Ljava/lang/Object;")
            .count();
        assert_eq!(gets, 1, "JVMS §4.6 forbids duplicate name+descriptor pairs");
    }

    /// The `m_<i>` static `Method` slots, the `<clinit>` PUTSTATIC targets and
    /// the per-method GETSTATIC references are all indexed positionally, so
    /// they must be derived from the *same* de-duplicated method list. A
    /// leftover `spec.methods.len()` anywhere would emit one more field than
    /// there are methods (or leave `m_<n-1>` never written).
    #[test]
    fn field_count_matches_deduped_method_count() {
        let mut spec = supplier_spec();
        let get = spec.methods[0].clone();
        let mut other = get.clone();
        other.name = "getOther".to_string();
        // 3 entries, 2 distinct signatures.
        spec.methods = vec![get.clone(), other, get];

        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("class file must round-trip");

        let method_fields = cf
            .fields
            .iter()
            .filter(|f| f.name.starts_with("m_"))
            .count();
        assert_eq!(method_fields, 2, "one Method slot per distinct signature");
        assert!(
            cf.fields.iter().any(|f| &*f.name == "m_0"),
            "slots must be contiguous from 0"
        );
        assert!(cf.fields.iter().any(|f| &*f.name == "m_1"));
        assert!(
            !cf.fields.iter().any(|f| &*f.name == "m_2"),
            "no slot may be allocated for the collapsed duplicate"
        );
    }

    #[test]
    fn emits_two_iface_proxy_with_both() {
        let spec = ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy1".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec![
                "java/lang/Runnable".to_string(),
                "java/util/concurrent/Callable".to_string(),
            ],
            methods: vec![
                ProxyMethod {
                    name: "run".to_string(),
                    descriptor: "()V".to_string(),
                    is_default: false,
                    iface_owner: "java/lang/Runnable".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "call".to_string(),
                    descriptor: "()Ljava/lang/Object;".to_string(),
                    is_default: false,
                    iface_owner: "java/util/concurrent/Callable".to_string(),
                    param_class_names: vec![],
                    exception_types: vec!["java/lang/Exception".to_string()],
                    iface_root: None,
                },
            ],
        };
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("two-iface emitted class file must parse");
        assert!(cf.interfaces.iter().any(|s| &**s == "java/lang/Runnable"));
        assert!(cf
            .interfaces
            .iter()
            .any(|s| &**s == "java/util/concurrent/Callable"));
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "run" && &*m.descriptor == "()V"));
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "call" && &*m.descriptor == "()Ljava/lang/Object;"));
        assert!(cf.methods.iter().any(|m| &*m.name == "<init>"));
    }

    #[test]
    fn cp_dedup_returns_same_index_for_identical_utf8() {
        let mut cp = CpBuilder::new();
        let a = cp.add_utf8("java/lang/Object");
        let b = cp.add_utf8("java/lang/Object");
        let c = cp.add_utf8("java/lang/Object");
        assert_eq!(a, b);
        assert_eq!(b, c);
        let d = cp.add_utf8("java/lang/String");
        assert_ne!(a, d);
        let cls1 = cp.add_class("java/lang/Object");
        let cls2 = cp.add_class("java/lang/Object");
        assert_eq!(cls1, cls2);
    }

    #[test]
    fn descriptor_param_parsing_handles_mixed() {
        let p = descriptor_param_slots("(IJLjava/lang/String;[BD)V")
            .expect("well-formed descriptor must parse");
        assert_eq!(p.len(), 5);
        assert_eq!(p[0], DescKind::Int);
        assert_eq!(p[1], DescKind::Long);
        assert!(matches!(p[2], DescKind::Reference(ref s) if s == "Ljava/lang/String;"));
        assert!(matches!(p[3], DescKind::Reference(ref s) if s == "[B"));
        assert_eq!(p[4], DescKind::Double);
    }

    #[test]
    fn descriptor_return_parsing_handles_void_and_ref() {
        assert_eq!(descriptor_return("()V").unwrap(), ReturnKind::Void);
        assert_eq!(
            descriptor_return("()I").unwrap(),
            ReturnKind::Prim(DescKind::Int)
        );
        assert!(matches!(
            descriptor_return("()Ljava/lang/Object;").unwrap(),
            ReturnKind::Reference(ref s) if s == "Ljava/lang/Object;"
        ));
    }

    #[test]
    fn malformed_descriptor_yields_error_not_panic() {
        // Audit fix (MED): unknown descriptor byte and an unterminated
        // L-type must surface as a typed error, never panic.
        assert!(descriptor_param_slots("(Q)V").is_err());
        assert!(descriptor_param_slots("(Ljava/lang/String)V").is_err());
        assert!(descriptor_return("()Q").is_err());
        assert!(descriptor_return("()Ljava/lang/String").is_err());
    }

    /// The emission census must be able to see a degrade.
    ///
    /// Two properties, split so neither depends on the other and neither is
    /// flaky under `cargo test`'s parallel threads: the counters are
    /// monotonic and reachable (a `fetch_add` with no reader is a write-only
    /// counter, which is the shape this instrument exists to avoid), and the
    /// real/synthetic classification is a pure function of the spec.
    ///
    /// `>=` rather than `==` is deliberate and is what makes this sound:
    /// several sibling tests in this module also call `emit_proxy_classfile`,
    /// the counters are process-global, and they never decrease — so "our own
    /// call added at least one" is true regardless of what else ran.
    #[test]
    fn the_emission_census_counts_both_outcomes_and_can_be_read() {
        let (ok_real_0, ok_syn_0, failed_0) = super::proxy_emit_counts();

        // A successful emission is counted.
        let good = supplier_spec();
        emit_proxy_classfile(&good).expect("the canonical spec must emit");
        let (ok_real_1, ok_syn_1, _) = super::proxy_emit_counts();
        assert!(
            (ok_real_1 + ok_syn_1) >= (ok_real_0 + ok_syn_0) + 1,
            "a successful emit must bump an ok bucket: before=({ok_real_0},{ok_syn_0}) \
             after=({ok_real_1},{ok_syn_1})"
        );

        // A failure is counted TOO — the whole point. Without this the
        // instrument reports a healthy `ok` count while every proxy in the
        // process is silently the shim.
        let mut bad = supplier_spec();
        // A bad RETURN byte rather than a bad parameter: it fails in
        // `descriptor_return` with the spec's `param_class_names` still
        // consistent (both empty), so the test exercises the error PATH and
        // not some downstream length mismatch.
        bad.methods[0].descriptor = "()Q".to_string();
        emit_proxy_classfile(&bad).expect_err("a malformed descriptor must fail to emit");
        let (_, _, failed_1) = super::proxy_emit_counts();
        assert!(
            failed_1 >= failed_0 + 1,
            "a failed emit must bump the failure counter: before={failed_0} after={failed_1}"
        );

        // Classification, tested purely so no global count is involved.
        assert!(super::emitted_super_is_the_synthetic_shim(
            "java/lang/reflect/Proxy$Instance"
        ));
        assert!(
            !super::emitted_super_is_the_synthetic_shim("java/lang/reflect/Proxy"),
            "the REAL base class is the default super — bucketing it as the \
             synthetic shim would make the census report a degrade on every \
             healthy proxy, which is worse than not counting at all"
        );
        assert!(!super::emitted_super_is_the_synthetic_shim(
            "jdk/proxy1/$Proxy0"
        ));
    }

    #[test]
    fn proxy_emit_rejects_oversized_utf8_name_without_panic() {
        let mut spec = supplier_spec();
        spec.gen_class_name = format!("pkg/{}", "A".repeat(u16::MAX as usize + 1));
        let err = emit_proxy_classfile(&spec).expect_err("oversized Utf8 must be rejected");
        match err {
            ClassFileError::InvalidClassFile { message, .. } => {
                assert!(message.contains("UTF-8 entry too long"), "got: {message}");
            }
            other => panic!("expected InvalidClassFile, got {other:?}"),
        }
    }

    #[test]
    fn cp_builder_overflow_records_error_without_panic() {
        let mut cp = CpBuilder::new();
        cp.count = u16::MAX - 1;
        assert_eq!(cp.add_utf8("overflow"), 0);
        let err = cp.check("Overflow").expect_err("overflow must be recorded");
        match err {
            ClassFileError::InvalidClassFile { message, .. } => {
                assert!(message.contains("constant pool overflow"), "got: {message}");
            }
            other => panic!("expected InvalidClassFile, got {other:?}"),
        }
    }

    #[test]
    fn proxy_emit_rejects_local_slot_overflow_without_panic() {
        let param_count = 300usize;
        let descriptor = format!("({})V", "I".repeat(param_count));
        let spec = ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy_slots".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["x/ManyArgs".to_string()],
            methods: vec![ProxyMethod {
                name: "many".to_string(),
                descriptor,
                is_default: false,
                iface_owner: "x/ManyArgs".to_string(),
                param_class_names: vec!["java/lang/Integer".to_string(); param_count],
                exception_types: vec![],
                iface_root: None,
            }],
        };
        let err = emit_proxy_classfile(&spec).expect_err("local slot overflow must be rejected");
        match err {
            ClassFileError::InvalidClassFile { message, .. } => {
                assert!(message.contains("local slot"), "got: {message}");
            }
            other => panic!("expected InvalidClassFile, got {other:?}"),
        }
    }

    #[test]
    fn emits_int_return_method_uses_iret_in_tail() {
        let spec = ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy2".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["x/Y".to_string()],
            methods: vec![ProxyMethod {
                name: "calc".to_string(),
                descriptor: "(I)I".to_string(),
                is_default: false,
                iface_owner: "x/Y".to_string(),
                param_class_names: vec!["java/lang/Integer".to_string()],
                exception_types: vec![],
                iface_root: None,
            }],
        };
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("int-return emitted class file must parse");
        let m = cf
            .methods
            .iter()
            .find(|m| &*m.name == "calc" && &*m.descriptor == "(I)I")
            .expect("calc method present");
        // Round-trip succeeded — the verifier-skip path will accept it. Spot-
        // check the access flags include ACC_PUBLIC | ACC_FINAL.
        assert_eq!(m.access_flags.bits() & 0x0011, 0x0011);
    }

    #[test]
    fn emits_void_return_method() {
        let spec = ProxyClassSpec {
            gen_class_name: "java/lang/reflect/$Proxy3".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["java/lang/Runnable".to_string()],
            methods: vec![ProxyMethod {
                name: "run".to_string(),
                descriptor: "()V".to_string(),
                is_default: false,
                iface_owner: "java/lang/Runnable".to_string(),
                param_class_names: vec![],
                exception_types: vec![],
                iface_root: None,
            }],
        };
        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("void-return emitted class file must parse");
        assert!(cf
            .methods
            .iter()
            .any(|m| &*m.name == "run" && &*m.descriptor == "()V"));
    }

    // ── v3 additions ─────────────────────────────────────────────────────

    /// The generated class must declare `<clinit>` and a `m_0` static field
    /// of type `Ljava/lang/reflect/Method;` per the v3 layout (item 3).
    #[test]
    fn emits_clinit_with_static_method_fields() {
        let bytes = emit_proxy_classfile(&supplier_spec()).expect("emitter must succeed");
        let cf = read_class(&bytes).expect("v3 supplier proxy must round-trip");

        // `<clinit>` exists and is static + void.
        let clinit = cf
            .methods
            .iter()
            .find(|m| &*m.name == "<clinit>")
            .expect("v3 emitter must produce <clinit>");
        assert_eq!(&*clinit.descriptor, "()V");
        assert!(
            clinit.access_flags.bits() & 0x0008 != 0,
            "<clinit> must be ACC_STATIC, got 0x{:04X}",
            clinit.access_flags.bits()
        );

        // `m_0` field present, descriptor is `Ljava/lang/reflect/Method;`,
        // and ACC_STATIC.
        let f = cf
            .fields
            .iter()
            .find(|f| &*f.name == "m_0")
            .expect("m_0 field must exist");
        assert_eq!(&*f.descriptor, "Ljava/lang/reflect/Method;");
        assert!(
            f.access_flags.bits() & 0x0008 != 0,
            "m_0 must be ACC_STATIC"
        );
    }

    /// `<clinit>` must take the owner `Class` off the generated class's own
    /// `getInterfaces()` — NOT off a constant-pool `LDC class <owner name>`.
    ///
    /// CP class resolution is by NAME through the flat global store, so with
    /// two loader worlds holding the same interface it can bind the other
    /// world's copy. `Method.equals` compares declaring classes by IDENTITY,
    /// so the caller's own `Map<Method, …>` built from `getMethods()` then
    /// misses every lookup — Byte Buddy's `JavaDispatcher` throwing
    /// `No proxy target found for …Executable.isInstance(Object)`, which took
    /// Mockito's inline mock maker down in
    /// `HikariDataSourceConfigurationTests`.
    ///
    /// Also pins the fallback: a method whose owner is `java/lang/Object` has
    /// no declared-interface root and keeps the `LDC class` form.
    #[test]
    fn clinit_resolves_owner_through_get_interfaces_not_by_name() {
        use cratonvm_reader::attribute::Attribute;

        let spec = ProxyClassSpec {
            gen_class_name: "pkg/$ProxyRoot".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["pkg/A".to_string(), "pkg/B".to_string()],
            methods: vec![
                // Declared directly on the second interface.
                ProxyMethod {
                    name: "b".to_string(),
                    descriptor: "()V".to_string(),
                    is_default: false,
                    iface_owner: "pkg/B".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: Some("pkg/B".to_string()),
                },
                // Inherited from a SUPER-interface of the first — the root is
                // still a declared interface, and `getMethod` searches
                // super-interfaces, so index 0 is the right lookup receiver.
                ProxyMethod {
                    name: "inherited".to_string(),
                    descriptor: "()V".to_string(),
                    is_default: false,
                    iface_owner: "pkg/SuperOfA".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: Some("pkg/A".to_string()),
                },
                // Object method — no interface root, keeps `LDC class`.
                ProxyMethod {
                    name: "hashCode".to_string(),
                    descriptor: "()I".to_string(),
                    is_default: false,
                    iface_owner: "java/lang/Object".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
            ],
        };

        let bytes = emit_proxy_classfile(&spec).expect("emitter must succeed");
        let mut cf = read_class(&bytes).expect("emitted class file must round-trip");
        let cp = &cf.constant_pool;
        for m in &mut cf.methods {
            for attr in &mut m.attributes {
                attr.decode(cp).expect("method attribute must decode");
            }
        }
        let clinit = cf
            .methods
            .iter()
            .find(|m| &*m.name == "<clinit>")
            .expect("emitter must produce <clinit>");
        let code = clinit
            .attributes
            .iter()
            .find_map(|a| match a.as_decoded() {
                Some(Attribute::Code(c)) => Some(c),
                _ => None,
            })
            .expect("<clinit> must have a Code attribute");

        // Two interface-rooted methods => two `getInterfaces()` call sites,
        // and neither `pkg/B` nor `pkg/SuperOfA` may be reached by name.
        let get_interfaces_calls = code
            .code
            .iter()
            .filter(|b| **b == op::INVOKEVIRTUAL)
            .count();
        assert!(
            get_interfaces_calls >= 2,
            "expected a getInterfaces() call per interface-rooted method, \
             found {get_interfaces_calls} INVOKEVIRTUAL bytes"
        );
        assert!(
            code.code.contains(&op::AALOAD),
            "<clinit> must index getInterfaces() with AALOAD"
        );

        // The owner names of interface-rooted methods must NOT appear as
        // constant-pool Class entries — that is the loader-blind form.
        let class_names = constant_pool_class_names(&cf);
        assert!(
            !class_names.iter().any(|n| n == "pkg/SuperOfA"),
            "super-interface owner must not be resolved by name, cp classes: {class_names:?}"
        );
        assert!(
            class_names.iter().any(|n| n == "pkg/$ProxyRoot"),
            "the generated class itself must be an LDC-able Class constant"
        );
        // The Object fallback still resolves by name, which is safe: there is
        // exactly one `java/lang/Object`.
        assert!(
            class_names.iter().any(|n| n == "java/lang/Object"),
            "Object-owner methods keep the LDC class form"
        );
    }

    /// Every `CONSTANT_Class` name in the emitted class file.
    fn constant_pool_class_names(cf: &cratonvm_reader::ClassFile) -> Vec<String> {
        let mut out = Vec::new();
        for i in 1..cf.constant_pool.len() {
            if let Some(name) = cf.constant_pool.get_class_name(i as u16) {
                out.push(name.to_string());
            }
        }
        out
    }

    /// WP2.5-v3 item 4 audit: every emitted method body is straight-line
    /// (no branches, no exception handlers) — the precondition that lets
    /// us drop `skip_verification: true` in the bridge. Scans every Code
    /// attribute for branch opcodes and asserts the exception_table is
    /// empty.
    #[test]
    fn emitted_class_is_straight_line_no_handlers() {
        use cratonvm_reader::attribute::Attribute;

        let specs = vec![
            // Reference return, no params.
            supplier_spec(),
            // Void return + multiple primitive + reference params.
            ProxyClassSpec {
                gen_class_name: "java/lang/reflect/$Proxy_mix".to_string(),
                super_class: "java/lang/reflect/Proxy$Instance".to_string(),
                interfaces: vec!["x/Y".to_string()],
                methods: vec![ProxyMethod {
                    name: "mix".to_string(),
                    descriptor: "(IJFDLjava/lang/String;)V".to_string(),
                    is_default: false,
                    iface_owner: "x/Y".to_string(),
                    param_class_names: vec![
                        "java/lang/Integer".to_string(),
                        "java/lang/Long".to_string(),
                        "java/lang/Float".to_string(),
                        "java/lang/Double".to_string(),
                        "java/lang/String".to_string(),
                    ],
                    exception_types: vec![],
                    iface_root: None,
                }],
            },
            // Each primitive return — exercises CHECKCAST + unbox tail.
            ProxyClassSpec {
                gen_class_name: "java/lang/reflect/$Proxy_prims".to_string(),
                super_class: "java/lang/reflect/Proxy$Instance".to_string(),
                interfaces: vec!["x/Z".to_string()],
                methods: vec![
                    ProxyMethod {
                        name: "i".into(),
                        descriptor: "()I".into(),
                        is_default: false,
                        iface_owner: "x/Z".into(),
                        param_class_names: vec![],
                        exception_types: vec![],
                        iface_root: None,
                    },
                    ProxyMethod {
                        name: "j".into(),
                        descriptor: "()J".into(),
                        is_default: false,
                        iface_owner: "x/Z".into(),
                        param_class_names: vec![],
                        exception_types: vec![],
                        iface_root: None,
                    },
                    ProxyMethod {
                        name: "f".into(),
                        descriptor: "()F".into(),
                        is_default: false,
                        iface_owner: "x/Z".into(),
                        param_class_names: vec![],
                        exception_types: vec![],
                        iface_root: None,
                    },
                    ProxyMethod {
                        name: "d".into(),
                        descriptor: "()D".into(),
                        is_default: false,
                        iface_owner: "x/Z".into(),
                        param_class_names: vec![],
                        exception_types: vec![],
                        iface_root: None,
                    },
                ],
            },
        ];

        for spec in &specs {
            let bytes = emit_proxy_classfile(spec).expect("emitter must succeed");
            let mut cf = read_class(&bytes).expect("emitted class file must round-trip");
            // `read_class` returns every attribute in its lazy `Raw` form,
            // so `as_decoded()` would yield `None` until we force a decode.
            // Decode each method's attributes against the class's constant
            // pool before inspecting the Code attribute. We split the borrow
            // (`constant_pool` immutable, `methods` mutable — disjoint fields)
            // so the decode can mutate each `LazyAttribute` in place.
            let cp = &cf.constant_pool;
            for m in &mut cf.methods {
                for attr in &mut m.attributes {
                    attr.decode(cp).expect("method attribute must decode");
                }
            }
            for m in &cf.methods {
                let code = m
                    .attributes
                    .iter()
                    .find_map(|a| match a.as_decoded() {
                        Some(Attribute::Code(c)) => Some(c),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("method {} missing Code attr", m.name));
                assert!(
                    code.exception_table.is_empty(),
                    "WP2.5-v3 item 4: method {} of {} must have empty \
                     exception_table to skip StackMapTable",
                    m.name,
                    spec.gen_class_name,
                );
                if let Some((op, pc)) = scan_for_branch(&code.code) {
                    panic!(
                        "WP2.5-v3 item 4: method {} of {} contains branch \
                         opcode 0x{:02X} at pc={} — would require StackMapTable",
                        m.name, spec.gen_class_name, op, pc,
                    );
                }
            }
        }
    }

    /// Walk a Code byte stream and return `Some((opcode, pc))` for the
    /// first branch / switch / athrow / wide-ret encountered, or `None`.
    /// Lengths drawn from JVMS Chapter 6.
    fn scan_for_branch(code: &[u8]) -> Option<(u8, usize)> {
        let mut pc = 0usize;
        while pc < code.len() {
            let op = code[pc];
            if matches!(
                op,
                0x99..=0xa6   // if<cond>, if_icmp<cond>, if_acmp<eq/ne>
                | 0xa7        // goto
                | 0xa8        // jsr
                | 0xa9        // ret
                | 0xaa        // tableswitch
                | 0xab        // lookupswitch
                | 0xbf        // athrow
                | 0xc6        // ifnull
                | 0xc7        // ifnonnull
                | 0xc8        // goto_w
                | 0xc9 // jsr_w
            ) {
                return Some((op, pc));
            }
            pc += match op {
                0xaa => return Some((op, pc)),
                0xab => return Some((op, pc)),
                0xc4 => {
                    if pc + 1 < code.len() && code[pc + 1] == 0x84 {
                        6
                    } else {
                        4
                    }
                }
                0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
                0x11 | 0x13 | 0x14 | 0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0x84 => 3,
                0xc5 => 4,
                0xb9 | 0xba => 5,
                _ => 1,
            };
        }
        None
    }

    // ── proxy-real-classfile increment 1 ─────────────────────────────────
    //
    // The canonical-path acceptance test: a `Proxy.newProxyInstance`-style
    // single-interface probe must yield the REAL generated `$ProxyN` class
    // defined through the normal class loader with full Pass-2/3
    // verification (`skip_verification: false`) — NOT the synthetic
    // `Proxy$Instance` shim. This exercises the exact wiring
    // `native_builtins::define_or_get_proxy_class` uses: emit the canonical
    // spec, register the synthetic super + interface, then
    // `define_class_with_options`.
    //
    // Before increment 1 the real path was a fallible fast-path in front of
    // the synthetic shim; this test pins that the verified define of the
    // generated bytecode SUCCEEDS for the standard single-interface case, so
    // the real `$ProxyN` is what reflection observes.

    /// The canonical single-interface spec, matching what
    /// `build_proxy_spec_for` emits: the interface method plus the three
    /// Object methods (equals/hashCode/toString) it always appends.
    fn canonical_single_iface_spec() -> ProxyClassSpec {
        ProxyClassSpec {
            gen_class_name: "com/sun/proxy/$Proxy0".to_string(),
            super_class: "java/lang/reflect/Proxy$Instance".to_string(),
            interfaces: vec!["pkg/Greeter".to_string()],
            methods: vec![
                ProxyMethod {
                    name: "equals".to_string(),
                    descriptor: "(Ljava/lang/Object;)Z".to_string(),
                    is_default: false,
                    iface_owner: "java/lang/Object".to_string(),
                    param_class_names: vec!["java/lang/Object".to_string()],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "hashCode".to_string(),
                    descriptor: "()I".to_string(),
                    is_default: false,
                    iface_owner: "java/lang/Object".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "hello".to_string(),
                    descriptor: "(Ljava/lang/String;)Ljava/lang/String;".to_string(),
                    is_default: false,
                    iface_owner: "pkg/Greeter".to_string(),
                    param_class_names: vec!["java/lang/String".to_string()],
                    exception_types: vec![],
                    iface_root: None,
                },
                ProxyMethod {
                    name: "toString".to_string(),
                    descriptor: "()Ljava/lang/String;".to_string(),
                    is_default: false,
                    iface_owner: "java/lang/Object".to_string(),
                    param_class_names: vec![],
                    exception_types: vec![],
                    iface_root: None,
                },
            ],
        }
    }

    #[test]
    fn real_proxy_classfile_defines_verified_not_synthetic_shim() {
        use crate::{ClassLoaderId, ClassManager, DefineClassOptions};

        let mut cm = ClassManager::new(&[], &[], &[]);

        // Register the synthetic super (`Proxy$Instance`, 3 slots:
        // handler / interfaces / identity-hash) and the proxied interface
        // so `define_class_with_options` can resolve both at link time —
        // exactly what `define_or_get_proxy_class` does via
        // `ctx.try_ensure_synthetic_class(...).expect("Compatible mode fabricates; this fixture never runs under --jdk-only")` before defining the proxy.
        cm.try_ensure_synthetic_class("java/lang/reflect/Proxy$Instance", 3)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.try_ensure_synthetic_class("pkg/Greeter", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");

        let spec = canonical_single_iface_spec();
        let bytes = emit_proxy_classfile(&spec).expect("canonical proxy emit must succeed");

        // Define through the normal loader WITH verification enabled — the
        // canonical path. `skip_verification: false` mirrors the bridge in
        // `define_or_get_proxy_class`; if the generated bytecode failed
        // Pass-3 this would be an `Err` and the production code would
        // silently degrade to the synthetic shim.
        let opts = DefineClassOptions {
            skip_verification: false,
            ..Default::default()
        };
        let cid = cm
            .define_class_with_options(
                "com/sun/proxy/$Proxy0",
                &bytes,
                ClassLoaderId::Application,
                opts,
            )
            .expect(
                "real generated $ProxyN must define+verify through the normal \
                 loader (canonical path), not fall back to the synthetic shim",
            );

        let defined = cm
            .get_class(cid)
            .expect("defined proxy class must be retrievable");

        // It is the REAL generated `$ProxyN`, not the synthetic shim:
        //   * its name is the generated `com/sun/proxy/$Proxy0`,
        //   * NOT `java/lang/reflect/Proxy$Instance`,
        //   * and it is a fully-parsed class, not an `is_synthetic_stub`.
        assert_eq!(&*defined.name, "com/sun/proxy/$Proxy0");
        assert_ne!(&*defined.name, "java/lang/reflect/Proxy$Instance");
        assert!(
            !defined.origin.is_compatibility_stub(),
            "the canonical proxy must be a real generated classfile, \
             not a synthetic stub"
        );

        // Its superclass is the synthetic `Proxy$Instance` (so the existing
        // `proxy_chain_reaches_instance` / `isProxyClass` walk still
        // classifies it as a proxy).
        let super_id = defined.superclass.expect("proxy must have a superclass");
        let super_name = cm
            .get_class(super_id)
            .map(|c| c.name.to_string())
            .expect("superclass must be retrievable");
        assert_eq!(super_name, "java/lang/reflect/Proxy$Instance");

        // The generated class carries the proxied interface in its own
        // metadata (not borrowed from a shared shim side-table).
        assert!(
            defined.interfaces.iter().any(|&iid| cm
                .get_class(iid)
                .map(|c| &*c.name == "pkg/Greeter")
                .unwrap_or(false)),
            "generated proxy must declare the proxied interface",
        );

        // The interface method body is present on the generated class —
        // this is the dispatch shim that routes through the handler.
        assert!(
            defined.methods.iter().any(|m| &*m.name == "hello"
                && &*m.descriptor == "(Ljava/lang/String;)Ljava/lang/String;"),
            "generated proxy must emit the interface method body",
        );
    }

    // ── proxy-real-classfile increment 2 ─────────────────────────────────

    /// Object-method routing parity (design §2/§4): a proxy constructed via
    /// the real path must (a) carry the synthetic `Proxy$Instance` super
    /// constructor the generated `<init>` delegates to — so executing it does
    /// NOT raise `NoSuchMethodError` — and (b) route `hashCode()` /
    /// `equals(Object)` / `toString()` through the `InvocationHandler`, i.e.
    /// each is emitted as a dispatch shim that forwards to
    /// `Proxy$Dispatch.invokeProxy` (which calls `handler.invoke(...)`),
    /// matching the JDK generated proxy body rather than inheriting Object's
    /// identity implementations.
    #[test]
    fn real_proxy_object_methods_route_through_handler_and_ctor_resolves() {
        use crate::{ClassLoaderId, ClassManager, DefineClassOptions};
        use cratonvm_reader::attribute::Attribute;

        let mut cm = ClassManager::new(&[], &[], &[]);

        // (a) Register the synthetic super exactly as
        // `define_or_get_proxy_class` does. Increment 2 added the
        // `Proxy$Instance.<init>(InvocationHandler, Class[])V` method entry to
        // `synthetic_stub_ctor_methods`, so the stub's method table now
        // carries the ctor the generated `$ProxyN.<init>` resolves against.
        let super_id = cm
            .try_ensure_synthetic_class("java/lang/reflect/Proxy$Instance", 3)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        cm.try_ensure_synthetic_class("pkg/Greeter", 0)
            .expect("Compatible mode fabricates; this fixture never runs under --jdk-only");
        let super_cls = cm
            .get_class(super_id)
            .expect("synthetic Proxy$Instance must be registered");
        assert!(
            super_cls.methods.iter().any(|m| {
                &*m.name == "<init>"
                    && &*m.descriptor
                        == "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V"
            }),
            "the super ctor the generated <init> delegates to must resolve \
             (no NoSuchMethodError when the generated <init> executes)",
        );

        // Define the real generated proxy through the normal loader, verified.
        let spec = canonical_single_iface_spec();
        let bytes = emit_proxy_classfile(&spec).expect("canonical proxy emit must succeed");
        let opts = DefineClassOptions {
            skip_verification: false,
            ..Default::default()
        };
        let cid = cm
            .define_class_with_options(
                "com/sun/proxy/$Proxy0",
                &bytes,
                ClassLoaderId::Application,
                opts,
            )
            .expect("real generated $ProxyN must define+verify");
        let defined = cm.get_class(cid).expect("defined proxy retrievable");

        // (b) hashCode / equals / toString are each emitted as concrete
        // dispatch shims on the generated class (not inherited from Object).
        for (name, desc) in [
            ("hashCode", "()I"),
            ("equals", "(Ljava/lang/Object;)Z"),
            ("toString", "()Ljava/lang/String;"),
        ] {
            assert!(
                defined
                    .methods
                    .iter()
                    .any(|m| &*m.name == name && &*m.descriptor == desc),
                "generated proxy must emit an Object-method dispatch shim for \
                 {name}{desc} (JDK routes these through the handler)",
            );
        }

        // Each Object-method shim routes through the handler-forwarding
        // helper: its Code references `Proxy$Dispatch.invokeProxy`. We assert
        // the methodref is present in the generated class's constant pool by
        // re-reading the raw bytes (UTF-8 entries for owner + name), then
        // confirm the shims actually carry a Code body (they would inherit
        // Object's impl otherwise).
        let cf = read_class(&bytes).expect("generated proxy must round-trip");
        let cp = &cf.constant_pool;
        let mut has_dispatch_owner = false;
        let mut has_dispatch_name = false;
        for idx in 1..(cp.len() as u16) {
            if let Some(cratonvm_reader::constant_pool::ConstantPoolEntry::Utf8(s)) = cp.get(idx) {
                match &**s {
                    "java/lang/reflect/Proxy$Dispatch" => has_dispatch_owner = true,
                    "invokeProxy" => has_dispatch_name = true,
                    _ => {}
                }
            }
        }
        assert!(
            has_dispatch_owner && has_dispatch_name,
            "generated proxy must reference Proxy$Dispatch.invokeProxy — the \
             handler-forwarding helper the Object-method shims route through",
        );

        // The hashCode/equals/toString shims each have a non-empty Code body
        // (a real dispatch shim), confirming they don't fall through to
        // Object's inherited identity implementations.
        let mut cf2 = read_class(&bytes).expect("generated proxy must round-trip");
        let cp = &cf2.constant_pool;
        for m in &mut cf2.methods {
            for attr in &mut m.attributes {
                let _ = attr.decode(cp);
            }
        }
        for name in ["hashCode", "equals", "toString"] {
            let m = cf2
                .methods
                .iter()
                .find(|m| &*m.name == name)
                .unwrap_or_else(|| panic!("{name} shim must be present"));
            let code = m
                .attributes
                .iter()
                .find_map(|a| match a.as_decoded() {
                    Some(Attribute::Code(c)) => Some(c),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} shim must carry a Code body"));
            assert!(
                !code.code.is_empty(),
                "{name} dispatch shim must have a non-empty body that forwards \
                 to the handler, not inherit Object's identity impl",
            );
        }
    }
}
