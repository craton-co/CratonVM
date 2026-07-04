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
//! * `JavaSecurityAccess.doIntersectionPrivilege` — invokes the
//!   `PrivilegedAction.run()` interface method via `ctx.invoke`.
//! * `JavaLangReflectAccess.copyMethod` / `copyField` /
//!   `copyConstructor` — delegate to existing lang_class natives.

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::MethodCallResult;
use cratonvm_types::{ArrayElementType, ObjectRef, Value};

use crate::alloc_concurrent_synthetic;

/// WP1.4 — Re-export of the canonical concrete-class mapping.
/// Mirrors `vm/src/runtime/shared_secrets.rs::SharedSecretsInterface`
/// (we can't `use` it here because native-builtins does not depend
/// on vm; a compile-time `#[test]` in the vm crate asserts the two
/// lists stay in sync).
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
        "java/nio/Buffer$1",
    ),
    (
        "getJavaSecurityAccess",
        "Ljdk/internal/access/JavaSecurityAccess;",
        "java/security/AccessController$1",
    ),
    (
        "getJavaUtilJarAccess",
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

/// Allocate a fresh singleton of `owner_class`.
///
/// We intentionally allocate fresh every call — every registered
/// native on an owner class is object-identity-agnostic (reads
/// nothing from instance fields), so skipping the cache keeps the
/// implementation simple and avoids heap-GC interactions.  If a
/// future bridge decides to stash state on the singleton, it
/// should move to a per-(VM, interface) cache.
fn alloc_singleton(ctx: &mut dyn NativeContext, owner_class: &str) -> ObjectRef {
    // Resolve (or synthesise) the class id, then allocate.  The
    // synthetic class has 1 field slot reserved so downstream
    // callers that happen to poke at field 0 don't go out of
    // bounds.
    match ctx.ensure_class_initialized(owner_class) {
        Ok(cid) => {
            let real_fields = ctx.class_num_total_fields(cid);
            ctx.alloc_object(cid, real_fields.max(1))
        }
        Err(_) => {
            // Class did not exist; use ClassId(0) as a placeholder —
            // the invokeinterface resolution still routes through
            // the stored class name in the native registry.
            ctx.alloc_object(cratonvm_types::ClassId::new(0), 1)
        }
    }
}

/// Register the `SharedSecrets.getJavaXxxAccess()` factories.
///
/// Each factory returns a synthetic object of the matching owner
/// class.  We register on both `jdk/internal/access/SharedSecrets`
/// (JDK 11+) and `jdk/internal/misc/SharedSecrets` (legacy) so
/// bytecode compiled against either package resolves correctly.
fn register_factories(registry: &mut NativeMethodRegistry) {
    for (method, ret_desc, owner) in FACTORIES {
        let full_desc = format!("(){}", ret_desc);
        let owner_name: &'static str = owner;

        let cb: cratonvm_native_api::NativeCallback = make_factory_callback(owner_name);
        registry.register("jdk/internal/access/SharedSecrets", method, &full_desc, cb);
        // Legacy package alias.
        let cb2: cratonvm_native_api::NativeCallback = make_factory_callback(owner_name);
        registry.register("jdk/internal/misc/SharedSecrets", method, &full_desc, cb2);
    }
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
                let obj = alloc_singleton(ctx, $owner);
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
    gen_factory!(f_jnio, "java/nio/Buffer$1");
    gen_factory!(f_jsec, "java/security/AccessController$1");
    gen_factory!(f_jujar, "cratonvm/internal/ss/JavaUtilJarAccess$1");
    gen_factory!(f_juzf, "java/util/zip/ZipFile$1");
    gen_factory!(f_jnhc, "cratonvm/internal/ss/JavaNetHttpCookieAccess$1");
    gen_factory!(f_jois, "java/io/ObjectInputStream$1");
    gen_factory!(f_jurb, "java/util/ResourceBundle$1");

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
        "java/nio/Buffer$1" => f_jnio,
        "java/security/AccessController$1" => f_jsec,
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
    let obj = alloc_singleton(ctx, "jdk/internal/reflect/ReflectionFactory");
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
    let m_obj = alloc_concurrent_synthetic(ctx, "java/lang/Module", 2);
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

/// `JavaLangAccess.defineClass(ClassLoader, String, byte[], ProtectionDomain,
/// String)` -> `Class<?>`.
///
/// `jdk.internal.reflect.ClassDefiner.defineClass` (used to define
/// reflection-generated method/constructor accessor classes, and by other
/// internal callers) invokes this via `invokeinterface JavaLangAccess`.
/// Without it, real-JDK mode threw `NoSuchMethodError` out of
/// `ClassDefiner.defineClass`, which aborted the JUnit launcher during
/// session setup on JDK 21+ (every class in a run collapsed to LOADERR).
///
/// Real JDK's `System$1.defineClass` body is just
/// `loader.defineClass(name, b, 0, b.length, pd)` — delegate to the loader's
/// own protected 5-arg `defineClass` so this goes through the exact same
/// path as any other `ClassLoader.defineClass` call.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = ClassLoader,
/// args[2] = String name, args[3] = byte[] b, args[4] = ProtectionDomain pd,
/// args[5] = String source (unused — the real body ignores it too).
fn jla_define_class(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let loader = args.get(1).copied().unwrap_or(Value::Object(None));
    let name = args.get(2).copied().unwrap_or(Value::Object(None));
    let bytes = args.get(3).copied().unwrap_or(Value::Object(None));
    let pd = args.get(4).copied().unwrap_or(Value::Object(None));
    let len = match bytes {
        Value::Object(Some(arr)) => ctx.array_length(arr) as i32,
        _ => 0,
    };
    // `ctx.invoke_virtual` runs the REAL bytecode for `ClassLoader.defineClass`
    // (that is its whole purpose — bridges use it to deliberately reach real
    // JDK behavior instead of re-entering the native registry). That bytecode
    // opens with `preDefineClass(name, pd)`, whose `checkName` rejects any
    // name containing `/` with `NoClassDefFoundError: IllegalName: ...` — but
    // ordinary bytecode `invokevirtual` call sites for this exact method
    // never reach that check at all, because `cl_define_class_pd` (below) is
    // registered on `java/lang/ClassLoader.defineClass` and wins native
    // dispatch first. Call it directly so this bridge gets the SAME behavior
    // any other `loader.defineClass(name, b, off, len, pd)` caller gets,
    // instead of a divergent bytecode path. Args shape matches a normal
    // instance call: [receiver=loader, name, bytes, off, len, pd].
    crate::classloader::cl_define_class_basic(
        ctx,
        &[loader, name, bytes, Value::Int(0), Value::Int(len), pd],
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

/// `JavaLangAccess.start(Thread, ThreadContainer)` -> `void`.
///
/// `jdk.internal.vm.SharedThreadContainer.start(Thread)` (structured-
/// concurrency / virtual-thread executor plumbing — e.g. Jetty's thread
/// pool) calls this via `invokeinterface JavaLangAccess` to start a thread
/// while registering it with a container that tracks its children. Without
/// it, `NoSuchMethodError` here aborted every thread-pool-backed HTTP
/// client (Jetty) at startup. CratonVM does not model thread containers
/// (no structured-concurrency introspection), so — like the
/// `defineUnnamedModule`/`addEnableNativeAccess` bridges above — this is a
/// behavioral passthrough: just start the thread for real.
///
/// INSTANCE method: args[0] = receiver (System$1), args[1] = Thread,
/// args[2] = ThreadContainer (ignored).
fn jla_start_in_container(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let thread_obj = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    ctx.invoke_virtual(thread_obj, "start", "()V", &[])?;
    Ok(None)
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
        jla_get_declared_public_methods,
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
    // `jdk.internal.reflect.ClassDefiner.defineClass` -> JavaLangAccess.defineClass.
    registry.register(
        owner,
        "defineClass",
        "(Ljava/lang/ClassLoader;Ljava/lang/String;[BLjava/security/ProtectionDomain;Ljava/lang/String;)Ljava/lang/Class;",
        jla_define_class,
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

fn jnio_get_buffer_pool(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return a synthetic BufferPool whose `getCount` / `getMemoryUsed`
    // natives are already registered in phases_late.
    let pool = alloc_singleton(ctx, "java/lang/management/BufferPoolMXBean");
    Ok(Some(Value::Object(Some(pool))))
}

fn jnio_new_direct_byte_buffer(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // JavaNioAccess.newDirectByteBuffer(long addr, int cap[, Object att,
    // MemorySegment seg]) -> a DirectByteBuffer wrapping the native address.
    // args = [this(Buffer$1), addr, cap, ...]. JDK-25 has no `DirectByteBuffer(long,int)`
    // ctor; the real `Buffer$1` does `new DirectByteBuffer(addr, cap, att, seg)`
    // via the 4-arg `(JILjava/lang/Object;Ljava/lang/foreign/MemorySegment;)V`
    // constructor. (The old handler invoked a non-existent `(JI)V` ctor on the
    // wrong receiver and returned an uninitialized buffer.)
    let addr = args.get(1).cloned().unwrap_or(Value::Long(0));
    let cap = args.get(2).cloned().unwrap_or(Value::Int(0));
    let att = args.get(3).cloned().unwrap_or(Value::Object(None));
    if std::env::var_os("CRATONVM_DBG_NET").is_some() {
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

/// RKC16N.11 — return a synthetic `jdk.internal.misc.VM$BufferPool`
/// instance (rather than the previous null) so that the caller — typically
/// `ManagementFactoryHelper.getBufferPoolMXBeans` and friends, which
/// immediately invoke `getName()` / `getCount()` / `getMemoryUsed()` /
/// `getTotalCapacity()` on the returned reference — does not NPE.
///
/// The natives bound to `jdk/internal/misc/VM$BufferPool` (registered
/// alongside this in `register_java_nio_access`) provide safe defaults:
/// `"direct"` for the name, zero counters for the rest. That's
/// spec-compatible: the legacy interface only documents the value
/// shape, not strict per-call accuracy of the counters.
fn jnio_get_direct_buffer_pool(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let pool = alloc_concurrent_synthetic(ctx, "jdk/internal/misc/VM$BufferPool", 4);
    Ok(Some(Value::Object(Some(pool))))
}

fn vm_buffer_pool_get_name(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Legacy `jdk.internal.misc.VM$BufferPool.getName()` — the only
    // direct-buffer pool surface this object covers, so always
    // "direct".  Real JDK uses the same constant.
    let s = ctx.create_string("direct");
    Ok(Some(Value::Object(Some(s))))
}

fn vm_buffer_pool_get_count(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // No live direct-buffer accounting — return zero so callers that
    // chart counts see "no pool activity" rather than NPEing.
    Ok(Some(Value::Long(0)))
}

fn vm_buffer_pool_get_total_capacity(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(0)))
}

fn vm_buffer_pool_get_memory_used(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(Some(Value::Long(0)))
}

fn register_java_nio_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/nio/Buffer$1";
    registry.register(
        owner,
        "getBufferPool",
        "()Ljava/lang/management/BufferPoolMXBean;",
        jnio_get_buffer_pool,
    );
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
    // RKC16N.10 follow-on / RKC16N.11: legacy JDK 8/9 SharedSecrets
    // accessor still referenced by ManagementFactory.<clinit> in some
    // JDK 25 builds. Originally returned null, which downstream NPE'd
    // when the caller dereferenced the result. Now returns a synthetic
    // `jdk.internal.misc.VM$BufferPool` whose four natives — registered
    // just below — yield safe defaults.
    registry.register(
        owner,
        "getDirectBufferPool",
        "()Ljdk/internal/misc/VM$BufferPool;",
        jnio_get_direct_buffer_pool,
    );

    // RKC16N.11: the four `VM$BufferPool` interface methods. The
    // returned synthetic from `jnio_get_direct_buffer_pool` carries
    // no instance state, so all four bindings are stateless lambdas
    // returning safe defaults. Registered on the synthetic class
    // name (matches the shape used elsewhere in the bridge).
    let pool_owner = "jdk/internal/misc/VM$BufferPool";
    registry.register(
        pool_owner,
        "getName",
        "()Ljava/lang/String;",
        vm_buffer_pool_get_name,
    );
    registry.register(pool_owner, "getCount", "()J", vm_buffer_pool_get_count);
    registry.register(
        pool_owner,
        "getTotalCapacity",
        "()J",
        vm_buffer_pool_get_total_capacity,
    );
    registry.register(
        pool_owner,
        "getMemoryUsed",
        "()J",
        vm_buffer_pool_get_memory_used,
    );
}

// JavaSecurityAccess ----------------------------------------------------------

fn jsec_do_intersection_privilege(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // (PrivilegedAction action, AccessControlContext stack,
    //  AccessControlContext context) -> Object
    // Invokes action.run() ignoring the contexts (cratonvm has no
    // real SecurityManager — AC natives already degrade to
    // "always allow").
    // INSTANCE method: args[0] = receiver (AccessController$1),
    // args[1] = action, args[2..4] = the two AccessControlContexts.
    if let Some(Value::Object(Some(action))) = args.get(1) {
        ctx.invoke(
            "java/security/PrivilegedAction",
            "run",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(*action))],
        )
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn jsec_get_protect_domains(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Returns ProtectionDomain[] for a given AccessControlContext.
    // We synthesise a single-element array with the PD from
    // `Class.getProtectionDomain0` on the caller's class — which
    // Session 92's N1 agent already landed.
    // INSTANCE method: args[0] = receiver (AccessController$1), args[1] = acc.
    if let Some(Value::Object(Some(acc))) = args.get(1) {
        let _ = acc; // acc currently unused — session92 AC accepts any ACC.
        let pd = ctx.new_object("java/security/ProtectionDomain")?;
        let arr = ctx.new_ref_array(
            ctx.class_id_by_name("java/security/ProtectionDomain")
                .unwrap_or(cratonvm_types::ClassId::new(0)),
            1,
        );
        if let Some(Value::Object(Some(pd_ref))) = pd {
            ctx.set_array_element(arr, 0, Value::Object(Some(pd_ref)));
        }
        Ok(Some(Value::Object(Some(arr))))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn register_java_security_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/security/AccessController$1";
    registry.register(
        owner,
        "doIntersectionPrivilege",
        "(Ljava/security/PrivilegedAction;Ljava/security/AccessControlContext;Ljava/security/AccessControlContext;)Ljava/lang/Object;",
        jsec_do_intersection_privilege,
    );
    registry.register(
        owner,
        "getProtectDomains",
        "(Ljava/security/AccessControlContext;)[Ljava/security/ProtectionDomain;",
        jsec_get_protect_domains,
    );
}

// JavaUtilJarAccess -----------------------------------------------------------

fn jujar_jar_file_has_classpath_attribute(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Returning false is spec-compatible — callers short-circuit
    // the full attribute scan.
    Ok(Some(Value::Int(0)))
}

fn jujar_ensure_initialization(_ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    Ok(None)
}

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
    register_java_security_access(registry);
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

    #[test]
    fn all_factories_listed() {
        // getJavaObjectInputStreamAccess removed: JDK 25 uses an invokedynamic
        // lambda (no ObjectInputStream$1), so the real getter must run.
        assert_eq!(FACTORIES.len(), 15);
    }

    #[test]
    fn every_factory_returns_access_interface() {
        for (method, ret, _) in FACTORIES {
            assert!(method.starts_with("getJava"));
            assert!(method.ends_with("Access"));
            assert!(ret.contains("Access;"));
        }
    }

    #[test]
    fn owner_classes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for owner in owner_classes() {
            assert!(seen.insert(owner), "duplicate owner class: {owner}");
        }
        // Every factory has a distinct owner class, so the unique-owner count
        // tracks FACTORIES.len() (15 after `getJavaObjectInputStreamAccess` was
        // removed — see `all_factories_listed`). Derive it so the two stay in
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
        // For each of the 15 owner classes, probe for one
        // representative method we know is registered. This is
        // a sanity check that every `register_*` call landed.
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
            (
                "java/nio/Buffer$1",
                "getBufferPool",
                "()Ljava/lang/management/BufferPoolMXBean;",
            ),
            (
                "java/security/AccessController$1",
                "getProtectDomains",
                "(Ljava/security/AccessControlContext;)[Ljava/security/ProtectionDomain;",
            ),
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
        assert_eq!(expected.len(), 15);
        for (owner, method, desc) in expected {
            assert!(
                r.find(owner, method, desc).is_some(),
                "expected method {owner}.{method}{desc} not registered"
            );
        }
    }
}
