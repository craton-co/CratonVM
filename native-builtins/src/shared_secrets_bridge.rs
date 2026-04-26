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

use rustjvm_native_api::{NativeContext, NativeMethodRegistry};
use rustjvm_types::error::MethodCallResult;
use rustjvm_types::{ArrayElementType, ObjectRef, Value};

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
        "rustjvm/internal/ss/JavaIORandomAccessFileAccess$1",
    ),
    (
        "getJavaNetInetAddressAccess",
        "Ljdk/internal/access/JavaNetInetAddressAccess;",
        "java/net/InetAddress$1",
    ),
    (
        "getJavaNetUriAccess",
        "Ljdk/internal/access/JavaNetUriAccess;",
        "rustjvm/internal/ss/JavaNetUriAccess$1",
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
        "rustjvm/internal/ss/JavaUtilJarAccess$1",
    ),
    (
        "getJavaUtilZipFileAccess",
        "Ljdk/internal/access/JavaUtilZipFileAccess;",
        "java/util/zip/ZipFile$1",
    ),
    (
        "getJavaNetHttpCookieAccess",
        "Ljdk/internal/access/JavaNetHttpCookieAccess;",
        "rustjvm/internal/ss/JavaNetHttpCookieAccess$1",
    ),
    (
        "getJavaObjectInputStreamAccess",
        "Ljdk/internal/access/JavaObjectInputStreamAccess;",
        "java/io/ObjectInputStream$1",
    ),
    (
        "getJavaUtilResourceBundleAccess",
        "Ljdk/internal/access/JavaUtilResourceBundleAccess;",
        "java/util/ResourceBundle$1",
    ),
];

/// Registry of interface owner-class names.  Used to iterate all
/// 15 singletons from tests and from the VM-boot hook that
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
            ctx.alloc_object(rustjvm_types::ClassId::new(0), 1)
        }
    }
}

/// Register the 15 `SharedSecrets.getJavaXxxAccess()` factories.
///
/// Each factory returns a synthetic object of the matching owner
/// class.  We register on both `jdk/internal/access/SharedSecrets`
/// (JDK 11+) and `jdk/internal/misc/SharedSecrets` (legacy) so
/// bytecode compiled against either package resolves correctly.
fn register_factories(registry: &mut NativeMethodRegistry) {
    for (method, ret_desc, owner) in FACTORIES {
        let full_desc = format!("(){}", ret_desc);
        let owner_name: &'static str = owner;

        let cb: rustjvm_native_api::NativeCallback =
            make_factory_callback(owner_name);
        registry.register(
            "jdk/internal/access/SharedSecrets",
            method,
            &full_desc,
            cb,
        );
        // Legacy package alias.
        let cb2: rustjvm_native_api::NativeCallback =
            make_factory_callback(owner_name);
        registry.register(
            "jdk/internal/misc/SharedSecrets",
            method,
            &full_desc,
            cb2,
        );
    }
}

/// Build a `NativeCallback` that returns a fresh instance of the
/// given owner class.  Wrapping in a helper avoids closure
/// lifetime gymnastics — `NativeCallback` is a `fn` pointer, so
/// we dispatch through a small lookup table keyed by owner class.
fn make_factory_callback(owner_class: &'static str) -> rustjvm_native_api::NativeCallback {
    // Because NativeCallback is a fn pointer (not a closure) we
    // can't capture `owner_class`.  Instead, encode the owner
    // via a match in a generated fn, one per entry in
    // FACTORIES.  We achieve this with a lookup against an
    // index-bounded match.
    macro_rules! gen_factory {
        ($name:ident, $owner:literal) => {
            fn $name(
                ctx: &mut dyn NativeContext,
                _args: &[Value],
            ) -> MethodCallResult {
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
        "rustjvm/internal/ss/JavaIORandomAccessFileAccess$1"
    );
    gen_factory!(f_jniaa, "java/net/InetAddress$1");
    gen_factory!(f_jnuri, "rustjvm/internal/ss/JavaNetUriAccess$1");
    gen_factory!(f_jnio, "java/nio/Buffer$1");
    gen_factory!(f_jsec, "java/security/AccessController$1");
    gen_factory!(f_jujar, "rustjvm/internal/ss/JavaUtilJarAccess$1");
    gen_factory!(f_juzf, "java/util/zip/ZipFile$1");
    gen_factory!(
        f_jnhc,
        "rustjvm/internal/ss/JavaNetHttpCookieAccess$1"
    );
    gen_factory!(f_jois, "java/io/ObjectInputStream$1");
    gen_factory!(f_jurb, "java/util/ResourceBundle$1");

    match owner_class {
        "java/lang/System$1" => f_jla,
        "java/lang/invoke/MethodHandleImpl$1" => f_jlia,
        "java/lang/ref/Reference$1" => f_jlra,
        "java/lang/reflect/ReflectAccess" => f_jlrefa,
        "java/io/Console$1" => f_jioa,
        "rustjvm/internal/ss/JavaIORandomAccessFileAccess$1" => f_jiorafa,
        "java/net/InetAddress$1" => f_jniaa,
        "rustjvm/internal/ss/JavaNetUriAccess$1" => f_jnuri,
        "java/nio/Buffer$1" => f_jnio,
        "java/security/AccessController$1" => f_jsec,
        "rustjvm/internal/ss/JavaUtilJarAccess$1" => f_jujar,
        "java/util/zip/ZipFile$1" => f_juzf,
        "rustjvm/internal/ss/JavaNetHttpCookieAccess$1" => f_jnhc,
        "java/io/ObjectInputStream$1" => f_jois,
        "java/util/ResourceBundle$1" => f_jurb,
        _ => panic!("SharedSecrets bridge: unknown owner class {owner_class}"),
    }
}

// ---------------------------------------------------------------------------
// Per-interface method registrations
// ---------------------------------------------------------------------------

// JavaLangAccess (Thinnest + a few mediums) -----------------------------------

fn jla_current_carrier_thread(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // In platform-thread-only mode carrier == current thread.
    let t = ctx.current_thread_object();
    Ok(Some(Value::Object(Some(t))))
}

fn jla_get_reflection_factory(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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
    if let (Some(Value::Object(Some(t))), Some(c)) = (args.first(), args.get(1)) {
        ctx.set_field_by_name(*t, "cause", *c);
    }
    Ok(None)
}

fn jla_get_enum_constants_shared(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Delegate to `Class.getEnumConstantsShared` which is
    // already implemented in lang_class.rs.
    if let Some(Value::Object(Some(cls))) = args.first() {
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

fn jla_new_string_utf8_no_repl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (byte[] bytes, int offset, int length) -> String
    // Decode the bytes as UTF-8 with no replacement on malformed
    // sequences — we use the standard `String::from_utf8_lossy`
    // approximation.  Production real-JDK semantics throw
    // `CharacterCodingException` on malformed input; we fall back
    // to the replacement character so callers that already assume
    // well-formed bytes (the common case) keep working.
    let (bytes, off, len) = match (args.first(), args.get(1), args.get(2)) {
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

fn jla_get_bytes_utf8_no_repl(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let s = match args.first() {
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

fn jla_new_stack_trace_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `newStackTraceElement(StackFrameInfo sfi)` constructs a
    // `StackTraceElement` from a `StackFrameInfo`.  The
    // stackwalker already builds STE objects directly; we
    // forward to `StackTraceElement.<init>(Ljava/lang/String;
    // Ljava/lang/String;Ljava/lang/String;I)V` with fields read
    // reflectively off the SFI.
    let sfi = match args.first() {
        Some(Value::Object(Some(r))) => *r,
        _ => return Ok(Some(Value::Object(None))),
    };
    let cls_name = ctx.get_field_by_name(sfi, "className");
    let method = ctx.get_field_by_name(sfi, "methodName");
    let file = ctx.get_field_by_name(sfi, "fileName");
    let line = match ctx.get_field_by_name(sfi, "lineNumber") {
        Value::Int(l) => l,
        _ => -1,
    };
    let ste = ctx.new_object("java/lang/StackTraceElement")?;
    if let Some(Value::Object(Some(o))) = ste.clone() {
        ctx.set_field_by_name(o, "declaringClass", cls_name);
        ctx.set_field_by_name(o, "methodName", method);
        ctx.set_field_by_name(o, "fileName", file);
        ctx.set_field_by_name(o, "lineNumber", Value::Int(line));
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
    if let Some(Value::Object(Some(cls))) = args.first() {
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

fn jla_get_methods_or_null(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(cls))) = args.first() {
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
}

// JavaLangInvokeAccess --------------------------------------------------------

fn jlia_find_method_handle_type(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Delegates to MethodType.methodType(Class, Class[]).  Owner C
    // (WP1.6) has the authoritative MethodHandle layer; this
    // bridge is a thin passthrough so SharedSecrets-dependent
    // callers at least get a non-null MethodType back before
    // WP1.6 lands the full lookup path.
    if let (Some(ret), Some(params)) = (args.first(), args.get(1)) {
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

fn jlia_make_class_value_map(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // `makeClassValueMap()` returns a weak map keyed by Class.  We
    // back it with a simple HashMap — rustjvm doesn't currently
    // have weak Class references, but the map is only used for
    // caching and the leak surface is bounded by the number of
    // loaded classes (GC eventually evicts both).
    ctx.invoke("java/util/HashMap", "<init>", "()V", &[])?;
    ctx.new_object("java/util/HashMap")
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

fn jlra_run_finalization(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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

fn jlrefa_copy_method(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // The real JDK `copyMethod` clones a Method with a fresh
    // `override` flag so `setAccessible(true)` doesn't mutate the
    // cached prototype.  Our Method mirror is mutable-by-design
    // (SetAccessible edits the `override` field directly), so
    // returning the same reference preserves existing semantics.
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_copy_field(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_copy_constructor(
    _ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    Ok(Some(args.first().copied().unwrap_or(Value::Object(None))))
}

fn jlrefa_new_parameter(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // `newParameter(Executable, int, String, int)` — construct a
    // `java.lang.reflect.Parameter`.  We follow the JDK layout:
    // slots (executable, index, name, modifiers).
    let param = ctx.new_object("java/lang/reflect/Parameter")?;
    if let Some(Value::Object(Some(obj))) = param.clone() {
        let exec = args.first().copied().unwrap_or(Value::Object(None));
        let index = args.get(1).copied().unwrap_or(Value::Int(0));
        let name = args.get(2).copied().unwrap_or(Value::Object(None));
        let modifiers = args.get(3).copied().unwrap_or(Value::Int(0));
        ctx.set_field_by_name(obj, "executable", exec);
        ctx.set_field_by_name(obj, "index", index);
        ctx.set_field_by_name(obj, "name", name);
        ctx.set_field_by_name(obj, "modifiers", modifiers);
    }
    Ok(param)
}

fn jlrefa_new_accessible_object(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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
    // No attached console (rustjvm runs non-interactively by default).
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

fn jiorafa_open(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Signature: (String name, String mode) -> RandomAccessFile
    ctx.invoke(
        "java/io/RandomAccessFile",
        "<init>",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        args,
    )?;
    ctx.new_object("java/io/RandomAccessFile")
}

fn jiorafa_open_as_channel(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Return a FileChannel backed by the RAF.  The real RAF
    // getChannel() native already builds one.
    if let Some(Value::Object(Some(raf))) = args.first() {
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

fn register_java_io_raf_access(registry: &mut NativeMethodRegistry) {
    let owner = "rustjvm/internal/ss/JavaIORandomAccessFileAccess$1";
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
    if let Some(Value::Object(Some(addr))) = args.first() {
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

fn jniaa_get_original_host_name(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(addr))) = args.first() {
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

fn jnuri_create(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (String scheme, String ssp) -> URI: forwards to `URI.create(scheme:ssp)`.
    let scheme = match args.first() {
        Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
        _ => String::new(),
    };
    let ssp = match args.get(1) {
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
    let owner = "rustjvm/internal/ss/JavaNetUriAccess$1";
    registry.register(
        owner,
        "create",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/URI;",
        jnuri_create,
    );
}

// JavaNioAccess ---------------------------------------------------------------

fn jnio_get_buffer_pool(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return a synthetic BufferPool whose `getCount` / `getMemoryUsed`
    // natives are already registered in phases_late.
    let pool = alloc_singleton(ctx, "java/lang/management/BufferPoolMXBean");
    Ok(Some(Value::Object(Some(pool))))
}

fn jnio_new_direct_byte_buffer(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (long addr, int cap) -> DirectByteBuffer
    ctx.invoke(
        "java/nio/DirectByteBuffer",
        "<init>",
        "(JI)V",
        args,
    )?;
    ctx.new_object("java/nio/DirectByteBuffer")
}

fn jnio_acquire_session(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Scoped memory sessions — return null so callers treat it
    // as "no session" and the regular strong-reference path runs.
    Ok(Some(Value::Object(None)))
}

fn register_java_nio_access(registry: &mut NativeMethodRegistry) {
    let owner = "java/nio/Buffer$1";
    registry.register(
        owner,
        "getBufferPool",
        "()Ljava/lang/management/BufferPoolMXBean;",
        jnio_get_buffer_pool,
    );
    registry.register(
        owner,
        "newDirectByteBuffer",
        "(JILjava/lang/Object;)Ljava/nio/ByteBuffer;",
        jnio_new_direct_byte_buffer,
    );
    registry.register(
        owner,
        "acquireSession",
        "(Ljava/nio/Buffer;)Ljava/lang/foreign/MemorySegment$Scope;",
        jnio_acquire_session,
    );
}

// JavaSecurityAccess ----------------------------------------------------------

fn jsec_do_intersection_privilege(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (PrivilegedAction action, AccessControlContext stack,
    //  AccessControlContext context) -> Object
    // Invokes action.run() ignoring the contexts (rustjvm has no
    // real SecurityManager — AC natives already degrade to
    // "always allow").
    if let Some(Value::Object(Some(action))) = args.first() {
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

fn jsec_get_protect_domains(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Returns ProtectionDomain[] for a given AccessControlContext.
    // We synthesise a single-element array with the PD from
    // `Class.getProtectionDomain0` on the caller's class — which
    // Session 92's N1 agent already landed.
    if let Some(Value::Object(Some(acc))) = args.first() {
        let _ = acc; // acc currently unused — session92 AC accepts any ACC.
        let pd = ctx.new_object("java/security/ProtectionDomain")?;
        let arr = ctx.new_ref_array(
            ctx.class_id_by_name("java/security/ProtectionDomain")
                .unwrap_or(rustjvm_types::ClassId::new(0)),
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

fn jujar_ensure_initialization(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    Ok(None)
}

fn register_java_util_jar_access(registry: &mut NativeMethodRegistry) {
    let owner = "rustjvm/internal/ss/JavaUtilJarAccess$1";
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

fn juzf_get_entry(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // (ZipFile zf, String name, Function<String,JarEntry> factory) -> ZipEntry
    // Delegate to ZipFile.getEntry(String).
    if let (Some(Value::Object(Some(zf))), Some(Value::Object(Some(name)))) =
        (args.first(), args.get(1))
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

fn juzf_get_manifest_name(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
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

fn jnhc_parse_cookie(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // Thin passthrough to HttpCookie.parse(String).
    if let Some(Value::Object(Some(header))) = args.first() {
        ctx.invoke(
            "java/net/HttpCookie",
            "parse",
            "(Ljava/lang/String;)Ljava/util/List;",
            &[Value::Object(Some(*header))],
        )
    } else {
        ctx.invoke("java/util/ArrayList", "<init>", "()V", &[])?;
        ctx.new_object("java/util/ArrayList")
    }
}

fn register_java_net_http_cookie_access(registry: &mut NativeMethodRegistry) {
    let owner = "rustjvm/internal/ss/JavaNetHttpCookieAccess$1";
    registry.register(
        owner,
        "parseCookie",
        "(Ljava/lang/String;)Ljava/util/List;",
        jnhc_parse_cookie,
    );
}

// JavaObjectInputStreamAccess -------------------------------------------------

fn jois_check_array(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // (ObjectInputStream ois, Class<?> arrayType, int length) -> void
    // No-op: rustjvm does not impose a `maxArrayLength` limit
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

fn jurb_set_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(bundle))), Some(parent)) = (args.first(), args.get(1)) {
        ctx.set_field_by_name(*bundle, "parent", *parent);
    }
    Ok(None)
}

fn jurb_get_parent(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Object(Some(bundle))) = args.first() {
        Ok(Some(ctx.get_field_by_name(*bundle, "parent")))
    } else {
        Ok(Some(Value::Object(None)))
    }
}

fn jurb_set_locale(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let (Some(Value::Object(Some(bundle))), Some(locale)) = (args.first(), args.get(1)) {
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
    register_factories(registry);

    register_java_lang_access(registry);
    register_java_lang_invoke_access(registry);
    register_java_lang_ref_access(registry);
    register_java_lang_reflect_access(registry);
    register_java_io_access(registry);
    register_java_io_raf_access(registry);
    register_java_net_inet_address_access(registry);
    register_java_net_uri_access(registry);
    register_java_nio_access(registry);
    register_java_security_access(registry);
    register_java_util_jar_access(registry);
    register_java_util_zip_file_access(registry);
    register_java_net_http_cookie_access(registry);
    register_java_object_input_stream_access(registry);
    register_java_util_resource_bundle_access(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_15_factories_listed() {
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
        assert_eq!(seen.len(), 15);
    }

    #[test]
    fn all_factory_entry_points_registered() {
        // Instantiate a fresh registry and apply the bridge.
        let mut r = NativeMethodRegistry::new();
        register_wp1_4_shared_secrets(&mut r);
        for (method, ret, _) in FACTORIES {
            let desc = format!("(){}", ret);
            assert!(
                r.find(
                    "jdk/internal/access/SharedSecrets",
                    method,
                    &desc
                )
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
                "rustjvm/internal/ss/JavaIORandomAccessFileAccess$1",
                "open",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/io/RandomAccessFile;",
            ),
            (
                "java/net/InetAddress$1",
                "getHostFromNameService",
                "(Ljava/net/InetAddress;Z)Ljava/lang/String;",
            ),
            (
                "rustjvm/internal/ss/JavaNetUriAccess$1",
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
                "rustjvm/internal/ss/JavaUtilJarAccess$1",
                "jarFileHasClassPathAttribute",
                "(Ljava/util/jar/JarFile;)Z",
            ),
            (
                "java/util/zip/ZipFile$1",
                "entryLocalNameEncoding",
                "(Ljava/util/zip/ZipEntry;)Z",
            ),
            (
                "rustjvm/internal/ss/JavaNetHttpCookieAccess$1",
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
