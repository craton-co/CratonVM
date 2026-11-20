#[allow(unused_imports)]
use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::vm::vm_exec) fn invoke_on_class_shared_inner(
    shared: &SharedVm,
    thread: &mut JvmThread,
    class_id: ClassId,
    method_name: &str,
    descriptor: &str,
    args: &[Value],
    kind: OnClassKind,
    // What the call site resolved, when `class_id` is the receiver's own class
    // (`invoke_on_receiver_class_selecting`); `None` everywhere else.
    receiver_selection: Option<crate::runtime::resolve::selection::ResolvedRef>,
) -> MethodCallResult {
    // The pinned dispatch copy: inline up to eight slots, so the ordinary
    // call allocates nothing here (wave 15; was `args.to_vec()`).
    let mut rooted_args: smallvec::SmallVec<[Value; 8]> = smallvec::SmallVec::from_slice(args);
    let args_roots = PinnedDispatchArgs::new(thread, &rooted_args);
    let args = &mut rooted_args;
    dbg_dispatch_tally("invoke_on_class_shared_inner", "", method_name, descriptor);
    // Asked before anything below can move `receiver_selection`.
    let receiver_selected = receiver_selection.is_some();
    // gci i8/dm (`gcd-d5f-jni-default-dispatch-costs-500ns-20260928`): the
    // per-thread JNI dispatch memo, opt-in `CRATONVM_JNI_DISPATCH_MEMO`. A
    // table-bound JNI native this thread already dispatched through the whole
    // by-name prefix below (the receiver and retarget arms, the class-name
    // doors, the method walk, the registry and `resolve_native_dispatch_wave1`
    // asks, two `class_manager` reads with a `String` each, `find_jni_native`)
    // goes straight to the same arm body, `jni_table_native_call`, under the
    // same monitor. Filled only where none of those answers can change for the
    // key (`jni_dispatch_memo_fill`); served while the JNI table's registration
    // generation and the redefinition count are the fill's. Off: one byte
    // compare per call.
    // gci i10/cr: a receiver-selected call (the interpreter's virtual
    // fallback) asks it too, and is served only by a row verified for its own
    // resolved reference (`CRATONVM_JNI_DISPATCH_MEMO_SELECTED`).
    if let Some(hit) = jni_dispatch_memo_hit(
        shared,
        thread,
        class_id,
        method_name,
        descriptor,
        &args[..],
        receiver_selection,
    ) {
        // The engagement count (`CRATONVM_DBG_DISPATCH_TALLY`).
        let label = if receiver_selected {
            JNI_DISPATCH_MEMO_SELECTED_HIT
        } else {
            JNI_DISPATCH_MEMO_HIT
        };
        dbg_dispatch_tally(label, "", method_name, descriptor);
        let _sync_guard = if hit.is_synchronized {
            Some(class_shared_sync_monitor(
                shared,
                thread,
                hit.declaring_class_id,
                hit.is_static,
                &args_roots,
                args,
            )?)
        } else {
            None
        };
        let args: &[Value] = args;
        return jni_table_native_call(
            shared,
            thread,
            hit.fn_ptr,
            hit.declaring_class_id,
            hit.is_static,
            &hit.class_name,
            method_name,
            descriptor,
            args,
        );
    }
    // The rescue retries of the slow path below keep their pre-wave-52 rule:
    // only `NoRetarget` opts out of them (a static call reaches them only
    // when its method was not found where the caller resolved it).
    let no_retarget = kind == OnClassKind::NoRetarget;
    if kind == OnClassKind::Static {
        // The positive control of `VmConfig::by_name_static_kind`
        // (`CRATONVM_DBG_DISPATCH_TALLY`): a static tail that skipped the
        // receiver arm and the retarget walk.
        dbg_dispatch_tally(ON_CLASS_STATIC_KIND, "", method_name, descriptor);
    }
    if kind != OnClassKind::Static && method_name != "<init>" && method_name != "<clinit>" {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            let recv_cid = shared.mem.heap.class_id_of(recv);
            // The id-range test first (`is_lambda_proxy_class`): an ordinary
            // receiver is answered with one compare, where this used to take
            // the `lambda_proxies` read lock and probe the map on every call.
            if shared.classes.is_lambda_proxy_class(recv_cid) {
                if let Some(result) = crate::runtime::interpreter::try_lambda_dispatch(
                    shared,
                    thread,
                    recv,
                    recv_cid,
                    method_name,
                    descriptor,
                    &args[1..],
                )? {
                    return Ok(result);
                }
            }
        }
    }
    // C25: Virtual dispatch on an interface (or abstract class) вЂ” if the
    // passed class_id names an interface/abstract class and the first arg is
    // a concrete object, re-target dispatch onto the receiver's actual class.
    // Without this, invoking e.g. `Iterator.hasNext()Z` through this entry
    // point finds the abstract interface method (no Code attribute) and
    // fails with an internal error. Callers that pass a specific class_id
    // intentionally (invokespecial, <clinit>) use the <init>/<clinit> name
    // filter below to opt out, OR pass `OnClassKind::NoRetarget` (WP2.9
    // findSpecial). A resolved static (`OnClassKind::Static`) has no receiver.
    //
    // KC patch (γ generalisation): when the receiver's `class_id_of`
    // returns `ClassId::new(0)` (synthetic alloc that lost class_id, or
    // a bare `Object` slot) we cannot use the receiver to drive
    // dispatch — but we also must NOT keep the original `class_id`
    // unchanged if it names an interface (resolution will land on a
    // method with no Code and surface as NSME).  The CP class is the
    // spec-correct resolution target so we leave it as-is here; the
    // S111r10 receiver-walk and the `class_name == "java/lang/Object"`
    // path inside `invoke_or_native` handle the substitution downstream.
    let named_class_id = class_id;
    let class_id =
        if kind == OnClassKind::Unknown && method_name != "<init>" && method_name != "<clinit>" {
            let recv_cid = args.get(0).and_then(|v| {
                if let Value::Object(Some(o)) = v {
                    // JVMS §4.4.1: an array type inherits its method table from
                    // `java.lang.Object`. An array header stores the COMPONENT
                    // class id, so retargeting dispatch onto it would select the
                    // component class's override (e.g. `Foo[].clone()` landing on
                    // `Foo.clone()`) and run that body with the array as `this`.
                    if shared.mem.heap.kind_of(*o) == cratonvm_types::ObjectKind::Array {
                        None
                    } else {
                        Some(shared.mem.heap.class_id_of(*o))
                    }
                } else {
                    None
                }
            });
            if let Some(rc) = recv_cid {
                if rc != class_id && rc != ClassId::new(0) {
                    let cm = shared.classes.class_manager.read();
                    let this_is_iface_or_abs = cm
                        .get_class(class_id)
                        .map(|c| c.is_interface() || c.is_abstract())
                        .unwrap_or(false);
                    let recv_is_concrete =
                        cm.get_class(rc).map(|c| !c.is_interface()).unwrap_or(false);
                    // Never retarget a STATIC method. A method reference to a static
                    // interface method (`Iface::staticMethod`, e.g. JUnit's
                    // `TestDescriptor::containsTests`) reaches here with the
                    // interface as `class_id` and the first SAM argument as
                    // `args[0]`. That argument is a plain parameter, not a dispatch
                    // receiver — retargeting onto its concrete class mis-resolves
                    // (static interface methods are not inherited by implementors)
                    // and recurses into the lambda SAM → StackOverflow. Resolve the
                    // method on the original `class_id` first to detect this.
                    // Interpreter round i6 wave 116, lane L4
                    // (`CRATONVM_EXECUTE_METHOD_INDEX_MEMO`): the hierarchy
                    // walk only decides a retarget, and only an interface or
                    // abstract `class_id` with a concrete receiver can be
                    // retargeted, so with the switch it is asked last. Every
                    // static `DirectMethodHandle$Holder` / `Invokers$Holder`
                    // re-entry (receiver: the handle; the class neither
                    // interface nor abstract) walked a 217-method table here.
                    let retarget_possible = !shared.config.execute_method_index_memo
                        || (this_is_iface_or_abs && recv_is_concrete);
                    let original_is_static = retarget_possible
                        && crate::classloading::find_method_recursive(
                            class_id,
                            method_name,
                            descriptor,
                            &cm.class_store,
                        )
                        .map(|(m, _)| m.is_static())
                        .unwrap_or(false);
                    if !original_is_static
                        && this_is_iface_or_abs
                        && recv_is_concrete
                        && cm.is_subclass_of(rc, class_id)
                    {
                        rc
                    } else {
                        class_id
                    }
                } else {
                    class_id
                }
            } else {
                class_id
            }
        } else {
            class_id
        };
    // `VirtualMachine.attach` is a concrete JDK method, so the normal
    // real-bytecode preference would enter AttachProvider discovery before
    // reaching the registered in-process self-attach native.  Byte Buddy
    // invokes this exact surface reflectively for Mockito's inline maker.
    // Resolve it here, after the reflective Method target is known but before
    // bytecode selection, so it cannot dispatch through the unsupported
    // socket/provider protocol.
    // ONE read guard for both of the questions the hot prefix asks about
    // `class_id`: its name, and whether it is a class whose exact-name native
    // must be preferred over an inherited bytecode body. They used to be two
    // separate `class_manager.read()` calls on every single invoke; see the
    // block comment on `prefer_exact_class_native` below for what the second
    // one decides, and `docs/known-issues/h2/` for the profile that made the
    // acquisition count worth counting.
    let (class_name, prefer_exact_class_native) = {
        let cm = shared.classes.class_manager.read();
        let class = cm.get_class(class_id);
        // A stack copy, not an `Arc<str>` clone: the name outlives this guard,
        // and `Arc::clone` on a hot class is an atomic RMW that every mutator
        // performs on the SAME cache line — the identical contention this
        // coalescing exists to reduce (the JIT's dispatch memo holds its names
        // as `Rc<str>` for the same reason). Not a `String` either (W3A item
        // 5): that allocated on every call; a JDK name fits [`NameBuf`] inline.
        let class_name = class
            .map(|class| NameBuf::new(&class.name))
            .unwrap_or_default();
        // `dispatch_lacks_class_file`, NOT `is_synthetic_stub`: this branch is
        // asking question (2) — "does this class have no bytecode of its own,
        // so a native registered under its exact name is the only thing that
        // can answer?" — not question (1), "is this a compatibility
        // substitution?". They gave the same answer for every class until
        // `java/lang/reflect/Proxy$Instance` was correctly reclassified
        // `VmInternal`; it still needs this branch, and now says so
        // structurally. See `Class::dispatch_lacks_class_file`.
        let stub_or_interface = class
            .map(|c| c.dispatch_lacks_class_file() || c.is_interface())
            .unwrap_or(false);
        let prefer = stub_or_interface || class_name.starts_with("cratonvm/internal/");
        (class_name, prefer)
    };
    // `cratonvm/internal/*` classes (`UnmodifiableList`/`Map`/`Set`/
    // `Collection`/`EntrySet`/`Itr`/`ListItr`/`MapEntry`, ...) are pure
    // Rust-native VM-internal wrapper types with NO bytecode of their own --
    // unlike `java/util/*` classes, which are only synthetic stubs until
    // real JDK bytecode loads, these never gain real bytecode. A method
    // registered as a native under the receiver's own exact class name here
    // (e.g. `toString`/`hashCode`/`equals`) is NOT separately declared as a
    // method entry, so `find_method_recursive` below walks straight past it
    // to `java/lang/Object`'s real bytecode `toString()`/etc (identity-hash
    // format instead of e.g. "[foo, bar]"). Ordinary bytecode `invokevirtual`
    // never hits this: the interpreter's own dispatch checks the native
    // registry before falling back to inherited bytecode. Only entry points
    // that bypass that check land here instead -- reflective `Method.invoke
    // ()` and any native-code-initiated `ctx.invoke_virtual` (e.g. `String
    // .valueOf`/`StringBuilder.append(Object)` on such a wrapper). Check the
    // exact-class native FIRST for this namespace so those paths see the
    // same result ordinary bytecode dispatch already does.
    // The `is_interface()` disjunct: a few JDK reflection types
    // (`TypeVariable`, and potentially sibling `sun.reflect.generics`
    // interfaces) are represented directly as instances of their own
    // PUBLIC INTERFACE's `ClassId` -- real Java can never have a concrete
    // object whose class IS an interface, so seeing one here (after the
    // C25 retarget above already tried to redirect an interface `class_id`
    // onto the receiver's concrete class and couldn't) means this receiver
    // is one of those synthetic representations. `TypeVariable.toString()`
    // has an exact-class native (below) that a real `TypeVariableImpl`
    // would provide via bytecode; without preferring it here,
    // `find_method_recursive` walks straight to `java/lang/Object
    // .toString()` and any non-reflective caller (`StringBuilder.append
    // (Object)`, string concat, `HashMap.toString()`) gets the identity-
    // hash format instead of the variable's name -- confirmed via
    // `"x" + typeVar` and `Map.of(typeVar, ...).toString()` both showing
    // the bug while a direct `typeVar.toString()` call (ordinary bytecode
    // invokevirtual, a different, already-correct dispatch path) did not.
    // Lambda-proxy receivers are already handled and returned above, so
    // they never reach this branch.
    if prefer_exact_class_native {
        // Generalises the `cratonvm/internal/*` case above: ANY synthetic-
        // stub class (no real bytecode -- either a permanently-synthetic
        // VM-internal representation, e.g. the concrete class CratonVM
        // allocates for `java/lang/reflect/TypeVariable` instances, or a
        // real class temporarily stubbed before its actual bytecode loads)
        // has the identical gap: a method registered as a native under its
        // own exact class name but not separately declared in the class's
        // method table is invisible to `find_method_recursive`'s hierarchy
        // walk below, which instead lands on an inherited real-bytecode
        // method (typically `java/lang/Object`'s) -- e.g. `TypeVariable
        // .toString()` returning the identity-hash format ("java.lang.
        // reflect.TypeVariable@25bf") instead of just the variable's name
        // ("T"), breaking `Map<TypeVariable, Type>.toString()` and hence
        // `GenericTypeResolver`-based debug output. Once a class's real
        // bytecode loads, `is_synthetic_stub` flips false and this check
        // naturally stops applying to it.
        if let Some((callback, native_kind, native_id)) = shared
            .natives
            .native_methods
            .find_with_kind_and_id(&class_name, method_name, descriptor)
        {
            // An interface can declare concrete static/default Java methods.
            // Resolve its own declaration before using a representation-native;
            // having ACC_INTERFACE does not make Arena.ofConfined synthetic.
            // Representation classes with no declaration retain their exact-name
            // callback rather than inheriting Object's unrelated implementation.
            let admitted = {
                let cm = shared.classes.class_manager.read();
                match cm.get_class(class_id).and_then(|class| {
                    // Wave 116, lane L4: the per-VM declared-index memo
                    // under `CRATONVM_EXECUTE_METHOD_INDEX_MEMO` (L7's, the
                    // same answer: verified by name and descriptor).
                    let declared = if shared.config.execute_method_index_memo {
                        crate::runtime::interpreter::find_declared_method_memoized(
                            shared,
                            class_id,
                            class,
                            method_name,
                            descriptor,
                        )
                    } else {
                        class.find_method(method_name, descriptor)
                    };
                    declared.map(|method| {
                        resolve_dispatch_in(
                            &shared.natives.native_shadow_sink,
                            dispatch_policy(shared),
                            class,
                            method,
                            Some((callback, native_kind)),
                        )
                    })
                }) {
                    Some(DispatchDecision::Reject(violation)) => {
                        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                    }
                    Some(decision) => decision.native_callback(),
                    None => Some(callback),
                }
            };
            if let Some(callback) = admitted {
                check_native_dispatch_capability(shared, &class_name, method_name, descriptor)?;
                census_general_native_dispatch(
                    shared,
                    callback,
                    &class_name,
                    method_name,
                    descriptor,
                );
                let native_id =
                    by_name_native_id(shared, native_id, &class_name, method_name, descriptor);
                return by_name_native_call(
                    shared,
                    thread,
                    callback,
                    native_id,
                    args,
                    (&class_name, method_name, descriptor),
                )
                .map(|value| coerce_native_return(value, descriptor));
            }
        }
    }

    // `java.nio.file.Path` is a genuine interface with no `toString()` body of
    // its own. The receiver-retargeting block above only substitutes the
    // receiver's actual class for `class_id`/`class_name` when the ORIGINAL
    // (CP-symbolic) class at the call site is itself an interface/abstract
    // class (`this_is_iface_or_abs`). A call site whose declared parameter
    // type is `Object` — e.g. `String.valueOf(Object obj)`'s internal
    // `obj.toString()`, which is exactly what javac compiles `"literal:" +
    // aPath` down to (a real `invokestatic String.valueOf` ahead of the
    // `StringConcatFactory` indy call, not a direct `Path.toString()` call) —
    // never retargets, so `class_name` here stays `java/lang/Object` and the
    // `java/nio/file/Path` force-native entry deep in the `check_override`
    // chain below (keyed on `class_name`) can never fire, even though the
    // ACTUAL RECEIVER is one of our synthetic Path values. Check the
    // receiver's real class directly, independent of the resolved
    // `class_name`. Same family as
    // `path-tostring-indy-stringconcat-dead-dispatch-FIXED.md`
    // (which covered this exact call shape) — this hunk went missing from
    // `invoke_on_class_shared_inner` somewhere between that fix landing
    // (a6ce01fe2, 2026-07-19) and dev tip; re-added 2026-07-21 after
    // `templateLocationEmpty` regressed with the identical symptom
    // (`file:java.nio.file.Path@<hash>` instead of the real path).
    //
    // Under `--real-jdk` only a Path of the class this VM mints its own Paths
    // as (the platform `UnixPath` / `WindowsPath`, or the bare interface) is
    // redirected: its text lives in the slots the native reads. Any other
    // `Path` -- a real `jdk.nio.zipfs.ZipPath` / `jdk.internal.jrtfs.JrtPath`
    // now that `jar:`/`jrt:` are served by the real providers, or an
    // application's own implementation -- has a `toString()` of its own, and
    // redirecting it looped: the native's answer for such a receiver is a
    // virtual `toString()`, which came straight back here, and the native's
    // re-entrancy guard answered `""` (`"x" + zipPath` printed `x`, and an
    // in-process `javac` saw every classpath-jar class under an empty name).
    //
    // gci i6/jd: the same rule in BOTH modes. `--compatible` used to redirect
    // every Path subtype, which was harmless while `--compatible` minted its
    // own carriers for `jar:`/`jrt:`; since the `jdk-archive-fs` alignment row
    // went ON (2026-10-09) `--compatible` serves them with the real providers
    // too, and a `JrtPath` sent here got the carrier native's answer: its slot
    // read lands on `JrtPath.resolved` (the field at `WindowsPath.path`'s
    // index), else the re-entrancy guard's `""`, else the interface's
    // `Object.toString()`. javac's `BaseFileManager.getKind(Path)` then saw no
    // `.class` suffix and parsed a jrt `module-info.class` as SOURCE
    // (`gci-i4orch-compatible-concurrent-javac-reads-a-module-info-class-as-source`).
    // A foreign Path's own `toString()` is HotSpot's answer in every mode.
    if method_name == "toString" && descriptor == "()Ljava/lang/String;" {
        if let Some(Value::Object(Some(recv))) = args.first().copied() {
            let recv_cid = shared.mem.heap.class_id_of(recv);
            let (is_path, is_carrier) = {
                let cm = shared.classes.class_manager.read();
                let is_path = cm
                    .find_bootstrap_class_by_name("java/nio/file/Path")
                    .map(|path_cid| cm.is_subclass_of(recv_cid, path_cid))
                    .unwrap_or(false);
                let is_carrier = is_path
                    && cm.get_class(recv_cid).is_some_and(|c| {
                        &*c.name == cratonvm_native_api::path_layout::impl_class()
                            || &*c.name == "java/nio/file/Path"
                    });
                (is_path, is_carrier)
            };
            if is_path
                && !is_carrier
                && false
            {
                let recv_name = shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(recv_cid)
                    .map(|c| c.name.to_string())
                    .unwrap_or_default();
                eprintln!("[FOREIGN-PATH] by-name toString not redirected: receiver {recv_name}");
            }
            if is_carrier {
                if let Some(callback) = shared.natives.native_methods.find(
                    "java/nio/file/Path",
                    "toString",
                    "()Ljava/lang/String;",
                ) {
                    return safe_native_call(shared, thread, callback, args)
                        .map(|value| coerce_native_return(value, descriptor));
                }
            }
        }
    }
    // SBR wave 5 (lane JSSE): under `--real-jdk` with the JSSE family retired
    // (`RETIRED_SHADOW_SBR_JSSE_TRIPLES`) the JDK's own `SSLContextImpl` /
    // `SSLSocketFactoryImpl` / `SSLSocketImpl` / `SSLServerSocketFactoryImpl`
    // run as bytecode, so the by-name arms below that routed those REAL
    // objects to the natives on the public class are closed. Only the
    // `sun/security/ssl/*Impl` disjuncts close: the public-class ones serve
    // `--compatible`'s carriers, and under `--real-jdk` their natives are
    // either retired (the `find` misses) or abstract (no real receiver has
    // the public class as its runtime class).
    let jsse_impl_doors_open = !jsse_impl_doors_closed(shared);
    // `SSLContext.getInstance` returns a SunJSSE provider object.  The TLS
    // bridge is registered on the public API class and must own this complete
    // family before provider bytecode can create an incompatible context SPI.
    // Keep this at the invocation entry point: static getInstance calls do
    // not have a provider receiver for a later hierarchy fallback to repair.
    if (class_name == "javax/net/ssl/SSLContext"
        || (jsse_impl_doors_open && class_name.starts_with("sun/security/ssl/SSLContextImpl")))
        && matches!(
            (method_name, descriptor),
            ("getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;")
                | ("init", "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V")
                | ("getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;")
                | ("createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
                | ("createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;")
        )
    {
        if let Some(callback) = shared
            .natives.native_methods
            .find("javax/net/ssl/SSLContext", method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|value| coerce_native_return(value, descriptor));
        }
    }
    // `SSLContext.getSocketFactory()` exposes SunJSSE's concrete provider
    // class. Route its factory methods to the public bridge, which owns the
    // accepted-socket TLS upgrade used by MockWebServer.
    if (class_name == "javax/net/ssl/SSLSocketFactory"
        || (jsse_impl_doors_open
            && class_name.starts_with("sun/security/ssl/SSLSocketFactoryImpl")))
        && method_name == "createSocket"
        && matches!(
            descriptor,
            "(Ljava/lang/String;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;"
                | "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;"
        )
    {
        if let Some(callback) = shared.natives.native_methods.find(
            "javax/net/ssl/SSLSocketFactory",
            method_name,
            descriptor,
        ) {
            return safe_native_call(shared, thread, callback, args)
                .map(|value| coerce_native_return(value, descriptor));
        }
    }
    // The SunJSSE factory can expose a concrete SSLSocket implementation.
    // Route its stream/lifecycle methods through the public bridge that owns
    // the rustls stream returned by createSocket(Socket, ...).
    if (class_name == "javax/net/ssl/SSLSocket"
        || (jsse_impl_doors_open && class_name.starts_with("sun/security/ssl/SSLSocketImpl")))
        && matches!(
            (method_name, descriptor),
            ("startHandshake", "()V")
                | ("getInputStream", "()Ljava/io/InputStream;")
                | ("getOutputStream", "()Ljava/io/OutputStream;")
                | ("getSession", "()Ljavax/net/ssl/SSLSession;")
                | ("close", "()V")
                | ("isClosed", "()Z")
                | ("isConnected", "()Z")
                | ("getPort", "()I")
                | ("getApplicationProtocol", "()Ljava/lang/String;")
                | ("getHandshakeApplicationProtocol", "()Ljava/lang/String;")
                | ("getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
                | ("setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
                | ("setUseClientMode", "(Z)V")
                | ("getUseClientMode", "()Z")
                | ("setNeedClientAuth", "(Z)V")
                | ("getNeedClientAuth", "()Z")
                | ("setWantClientAuth", "(Z)V")
                | ("getWantClientAuth", "()Z")
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find("javax/net/ssl/SSLSocket", method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|value| coerce_native_return(value, descriptor));
        }
    }
    if class_name == "com/sun/tools/attach/VirtualMachine"
        && matches!(
            (method_name, descriptor),
            (
                "attach",
                "(Ljava/lang/String;)Lcom/sun/tools/attach/VirtualMachine;"
            ) | ("loadAgent", "(Ljava/lang/String;Ljava/lang/String;)V")
                | ("loadAgent", "(Ljava/lang/String;)V")
                | ("detach", "()V")
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find(&class_name, method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|value| coerce_native_return(value, descriptor));
        }
    }
    // `Socket.setKeepAlive` on a real-JDK socket can read CratonVM's
    // synthetic TLS state as its private `impl` field.  The resulting
    // receiver is a String and the JDK attempts the impossible call below.
    // It is an internal socket-option write only; String has no such API, so
    // suppressing it is both narrower and safer than allowing an NSME.
    if class_name == "java/lang/String"
        && method_name == "setOption"
        && descriptor == "(ILjava/lang/Object;)V"
    {
        return Ok(None);
    }
    // `ServerSocket.accept()` has a native parent implementation, so the
    // later abstract-method rescue cannot displace it. A synthetic
    // SSLServerSocket owns a separate TLS listener registry and must always
    // prefer its concrete bridge before inherited-native lookup.
    if method_name == "accept" && descriptor == "()Ljava/net/Socket;" {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let receiver_class = shared.mem.heap.class_id_of(*receiver);
            let receiver_name = shared
                .classes
                .class_manager
                .read()
                .get_class(receiver_class)
                .map(|class| class.name.to_string())
                .unwrap_or_default();
            if receiver_name == "javax/net/ssl/SSLServerSocket" {
                if let Some(callback) =
                    shared
                        .natives
                        .native_methods
                        .find(&receiver_name, method_name, descriptor)
                {
                    return safe_native_call(shared, thread, callback, args)
                        .map(|value| coerce_native_return(value, descriptor));
                }
            }
        }
    }
    // UnboundID retains an SSLServerSocketFactory in a field whose declared
    // type is ServerSocketFactory, then invokes its concrete parent overloads
    // (`createServerSocket(II)` and `(IILjava/net/InetAddress;)`).  The
    // parent has registered native implementations, so ordinary resolution
    // never reaches the TLS factory's more-specific bridge.  Prefer that
    // bridge from the receiver's actual synthetic class before the parent
    // native can manufacture a plaintext listener.
    if method_name == "createServerSocket"
        && matches!(
            descriptor,
            "(I)Ljava/net/ServerSocket;"
                | "(II)Ljava/net/ServerSocket;"
                | "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;"
        )
    {
        if let Some(Value::Object(Some(receiver))) = args.first() {
            let receiver_class = shared.mem.heap.class_id_of(*receiver);
            let receiver_name = shared
                .classes
                .class_manager
                .read()
                .get_class(receiver_class)
                .map(|class| class.name.to_string())
                .unwrap_or_default();
            // The real `SSLServerSocketFactoryImpl` is closed with the other
            // JSSE Impl doors (`jsse_impl_doors_closed`): under `--real-jdk`
            // with the family retired its own bytecode builds a real
            // `SSLServerSocketImpl`.
            if receiver_name == "javax/net/ssl/SSLServerSocketFactory"
                || (receiver_name == "sun/security/ssl/SSLServerSocketFactoryImpl"
                    && !jsse_impl_doors_closed(shared))
            {
                if let Some(callback) = shared
                    .natives
                    .native_methods
                    // The real JDK factory carries its SSLContext in the
                    // same first instance slot consumed by the bridge.
                    // Reuse the bridge registered on its public API type
                    // rather than interpreting `SSLServerSocketImpl`,
                    // whose host socket path bypasses the TLS registry.
                    .find(
                        "javax/net/ssl/SSLServerSocketFactory",
                        method_name,
                        descriptor,
                    )
                {
                    return safe_native_call(shared, thread, callback, args)
                        .map(|value| coerce_native_return(value, descriptor));
                }
            }
        }
    }
    // The real-JDK Thread methods read the host field layout directly.  A
    // CratonVM Thread can instead carry a stale/mis-slotted value there, which
    // made Mockito plugin discovery dispatch `getResources` on a String.
    // The registered natives resolve the VM-owned context-loader state and
    // must win over those concrete bodies.
    if class_name == "java/lang/Thread"
        && matches!(
            (method_name, descriptor),
            ("getContextClassLoader", "()Ljava/lang/ClassLoader;")
                | ("setContextClassLoader", "(Ljava/lang/ClassLoader;)V")
        )
    {
        if let Some(callback) =
            shared
                .natives
                .native_methods
                .find(&class_name, method_name, descriptor)
        {
            return safe_native_call(shared, thread, callback, args)
                .map(|value| coerce_native_return(value, descriptor));
        }
    }
    // Find the method (walking the superclass chain). After the C25 retarget
    // above, the receiver's class SELECTS (JVMS §5.4.6) against the method the
    // named class resolves to, so a private, static or cross-package
    // package-private same-signature method no longer hides the override;
    // only a strict answer replaces the walk's (see
    // `selection::refine_lenient_selection`).
    let (is_native, is_synchronized, is_static, declaring_class_id, resolved_tail_override) = {
        let cm = shared.classes.class_manager.read();
        let store = &cm.class_store;
        let found =
            crate::classloading::find_method_recursive(class_id, method_name, descriptor, store);
        // Where §5.4.6 selection ends in an error for the retargeted receiver
        // — an abstract selected method, or conflicting maximally-specific
        // defaults — raise it as HotSpot does (reflection and JNI select the
        // same way) instead of running the body the lenient walk found past
        // it. Both modes since round 12 wave 8; `--compatible` keeps the
        // walk's answer only under `CRATONVM_NATIVE_INVOKE_STRICT_SELECTION=0`
        // (see `native_invoke_strict_selection`, asked only once an error is
        // found).
        if class_id != named_class_id {
            if let Some(error) = crate::runtime::resolve::selection::strict_selection_error(
                store,
                class_id,
                named_class_id,
                method_name,
                descriptor,
                found,
            )
            .filter(|_| native_invoke_strict_selection(shared))
            {
                drop(cm);
                return Err(selection_error_failure(shared, thread, error));
            }
        }
        let found = if class_id != named_class_id {
            crate::runtime::resolve::selection::refine_lenient_selection(
                store,
                class_id,
                named_class_id,
                method_name,
                descriptor,
                found,
            )
        } else if let Some(resolved) = receiver_selection {
            // Only a strict `Selected` is taken; no §5.4.6 error is raised
            // here (the caller's `try_stackless_invoke` step 4 raises those
            // when it selects on the receiver's class), so a lenient answer
            // never turns into a new failure.
            crate::runtime::resolve::selection::refine_lenient_selection_for(
                store,
                class_id,
                resolved,
                method_name,
                descriptor,
                found,
            )
        } else {
            found
        };
        match found {
            Some((method, declaring_id)) => {
                let mut native = method.is_native();
                let mut declaring_id_out = declaring_id;
                // If the method has no Code attribute (abstract/interface) but we
                // have a native override, use it. This handles Path.getFileSystem()
                // and similar abstract methods with native implementations.
                // Concrete Code reaches the canonical resolver at the tail.
                // The compatibility override chain may promote only bodyless
                // declarations here; otherwise it bypasses that resolver by
                // setting is_native before the Java-call tail is reached.
                if !native && method.code().is_none() {
                    let class_name = store.get(declaring_id).map(|c| &*c.name).unwrap_or("");
                    // For abstract methods, always check native overrides.
                    // For concrete methods, only check native overrides for classes
                    // where we create synthetic objects without running <init>
                    // (e.g. ByteArrayInputStream created by getResourceAsStream).
                    //
                    // JDK-ONLY-WAVE2: the `check_override` chain header. Of the
                    // disjuncts below, exactly ONE survives contract §7:
                    // `method.is_abstract()`, which is §7 step 3b (no `Code`,
                    // so a registered native is the only thing there is to run
                    // — the documented deviation in `resolve_dispatch`'s
                    // banner). Every other disjunct is a class-name exception
                    // saying "prefer our native over the real JDK's concrete
                    // bytecode", which is precisely what §1.4 forbids.
                    //
                    // **Strict half CLOSED 2026-08-10.** The record's own answer
                    // to "what must replace the chain" was *nothing*, and that
                    // is now enforced: under `--real-jdk` the name half is not
                    // consulted, every refusal is recorded by triple, and §7
                    // step 3 answers `Bytecode` for all of them. Measured on the
                    // 23-vector strict corpus: the chain admitted TWELVE triples
                    // by name there and no vector changed verdict when they
                    // stopped being admitted.
                    //
                    // Removing them under `Compatible` remains a separate,
                    // per-family exercise; each entry is load-bearing for a real
                    // boot today, and three static-analysis tranches
                    // (unreachability, then redundancy) already took the chain
                    // from 217 disjuncts to 179 without moving the admitted set.
                    // Under `JdkOnly` the name half of this chain is not
                    // consulted at all. Every disjunct below `is_abstract()`
                    // says "prefer our native over the real JDK's CONCRETE
                    // bytecode", which is exactly what §1.4 forbids, and the
                    // wave-2 record's own answer to "what must replace the
                    // chain" is *nothing*: §7 step 3 returns `Bytecode` for all
                    // of them. `method.is_abstract()` stays in BOTH modes — it
                    // is §7 step 3b, the documented deviation in
                    // `resolve_dispatch`'s banner (no `Code`, so a registered
                    // native is the only body there is), and removing it would
                    // stop the VM booting in either mode.
                    //
                    // Shaped as one boolean rather than a `!jdk_only_strict &&`
                    // threaded through 179 disjuncts so the census below can
                    // still see what the chain WOULD have admitted, and record
                    // the strict refusal by name. A refusal nobody can name is
                    // the failure mode this whole lane exists to remove.
                    // One resolver: only abstract (no-Code) methods dispatch to a
                    // registered native here; concrete methods with an Intrinsic
                    // are resolved via `resolve_dispatch_in` for `resolved_tail_override`.
                    let check_override = method.is_abstract();
                    if check_override_census_on() {
                        CHECK_OVERRIDE_REACHED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if check_override {
                            CHECK_OVERRIDE_TRUE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    if check_override
                        && shared
                            .natives
                            .native_methods
                            .find(class_name, method_name, descriptor)
                            .is_some()
                    {
                        record_check_override_hit(
                            class_name,
                            method_name,
                            descriptor,
                            true,
                        );
                        native = true;
                    }
                    // C25: For abstract methods (e.g. Iterator.hasNext, Enumeration.hasMoreElements),
                    // also check the receiver class's native registry вЂ” natives for synthetic
                    // wrapper classes (java/util/Enumeration$Impl) are registered on the
                    // wrapper class name, not on the interface. Without this, dispatch on
                    // an Enumeration$Impl receiver to Iterator.hasNext() resolves to the
                    // abstract method and fails with "no Code attribute".
                    //
                    // Deliberately NOT gated on `!native`: `check_override` above may
                    // already have matched a *generic* native registered directly on the
                    // abstract method's declaring interface (e.g. a fallback
                    // `java/util/Map$Entry.getKey` that reads field slot 0), which is
                    // less specific than a native registered on the receiver's own
                    // concrete wrapper class. Without re-checking here, that generic
                    // interface-level match wins unconditionally and is never displaced
                    // by a more specific wrapper-class override reachable only through
                    // this block — this was the root cause of
                    // `structured-logging-map-entry-getkey-lambda-dispatch-precedence`:
                    // `Map.Entry::getKey`/`getValue` invoked as method references over a
                    // synthetic `cratonvm/internal/UnmodifiableMapEntry` receiver
                    // resolved to the generic `Map$Entry` native (reading the wrapper's
                    // own backing-entry field directly) instead of
                    // `UnmodifiableMapEntry`'s own correctly-delegating native, returning
                    // the whole wrapped entry instead of the key/value. A match on the
                    // receiver's own class is always at least as specific as one found
                    // via the abstract declaring class, so prefer it whenever it exists.
                    if method.is_abstract() && class_id != declaring_id {
                        let recv_name = store.get(class_id).map(|c| &*c.name).unwrap_or("");
                        if !recv_name.is_empty()
                            && shared
                                .natives
                                .native_methods
                                .find(recv_name, method_name, descriptor)
                                .is_some()
                        {
                            native = true;
                            declaring_id_out = class_id;
                        }
                    }
                    // ES-SEGALLOC: the `class_id != declaring_id` gate above only
                    // fires when the earlier C25 retarget (top of
                    // `invoke_on_class_shared_inner`) already re-pointed `class_id`
                    // at the receiver's own runtime class. That retarget requires
                    // `recv_is_concrete` (`!c.is_interface()`) -- a real invariant for
                    // ordinary Java objects, whose runtime class is never literally an
                    // interface. CratonVM's own synthetic factories break that
                    // invariant: `Arena.ofAuto()/ofConfined()/ofShared()/global()`
                    // (native-builtins/src/panama.rs::register_pe_arena) allocate
                    // their return value under the literal interface name
                    // `java/lang/foreign/Arena`, so `recv_is_concrete` is false and
                    // `class_id` never leaves `java/lang/foreign/SegmentAllocator`.
                    // A default method inherited from `SegmentAllocator` (e.g.
                    // `allocate(MemoryLayout)` / `allocate(long)`, both real-JDK
                    // bytecode) then does `this.allocate(byteSize, byteAlignment)` --
                    // an `invokeinterface SegmentAllocator.allocate(JJ)` whose
                    // `class_id == declaring_id` (both resolve to the abstract
                    // `SegmentAllocator` declaration), so the block above never even
                    // looks at Arena's native registry and this throws
                    // `AbstractMethodError: SegmentAllocator.allocate(JJ)... has no
                    // Code attribute` instead of dispatching to the already-registered
                    // `Arena.allocate(JJ)` native. Recover the receiver's *actual*
                    // runtime class directly from `args[0]` (independent of whether
                    // the interface-exclusion above retargeted `class_id`) and check
                    // its native registry too -- this generalises the rescue to any
                    // interface-stamped synthetic receiver, not just concrete ones.
                    // The static method reference can already have selected a
                    // generic native on the abstract parent (for example
                    // `ServerSocketFactory.createServerSocket(II)`), which
                    // used to suppress this receiver-specific rescue.  The
                    // concrete synthetic receiver's bridge is more specific
                    // and must win even in that case: otherwise an
                    // `SSLServerSocketFactory` silently constructs a
                    // plaintext listener through its parent factory.
                    // `ServerSocket.accept()` is concrete on the parent, but a
                    // synthetic SSLServerSocket must still route to its own
                    // TLS-aware native. Without this one concrete exception,
                    // the parent accept path bypasses the listener registry
                    // entirely and an LDAPS client waits until timeout.
                    if method.is_abstract()
                        || (method_name == "accept" && descriptor == "()Ljava/net/Socket;")
                    {
                        let recv_actual_cid = args.first().and_then(|v| {
                            if let Value::Object(Some(o)) = v {
                                let rc = shared.mem.heap.class_id_of(*o);
                                if rc != ClassId::new(0) && rc != declaring_id {
                                    Some(rc)
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });
                        if let Some(rc) = recv_actual_cid {
                            let recv_name = store.get(rc).map(|c| &*c.name).unwrap_or("");
                            if !recv_name.is_empty()
                                && shared
                                    .natives
                                    .native_methods
                                    .find(recv_name, method_name, descriptor)
                                    .is_some()
                            {
                                native = true;
                                declaring_id_out = rc;
                            }
                        }
                    }
                    if !native && !no_retarget {
                        if let Some(Value::Object(Some(recv))) = args.first().copied() {
                            let recv_cid = shared.mem.heap.class_id_of(recv);
                            let recv_name = store.get(recv_cid).map(|c| &*c.name).unwrap_or("");
                            if shared.natives.native_methods.find_app_workaround_with_aux(
                                cratonvm_native_api::AppWorkaroundKind::ReceiverSubtypeNativeRetarget,
                                recv_name,
                                method_name,
                                descriptor,
                            ) == Some(&*class_name)
                                && shared
                                    .natives
                                    .native_methods
                                    .find(recv_name, method_name, descriptor)
                                    .is_some()
                            {
                                native = true;
                                declaring_id_out = recv_cid;
                            }
                        }
                    }
                }
                // Keep the resolved-method verdict through the final Java-call
                // tail. A second name-only registry lookup there used to let a
                // Bridge replace concrete Code in Compatible mode (including
                // MemorySessionImpl.createConfined/createShared), returning an
                // abstract session whose lifetime did not follow Arena.close().
                let resolved_tail_override = if native {
                    None
                } else {
                    store.get(declaring_id_out).and_then(|declaring_class| {
                        let registration = shared.natives.native_methods.find_with_kind(
                            &declaring_class.name,
                            method_name,
                            descriptor,
                        );
                        resolve_dispatch_in(
                            &shared.natives.native_shadow_sink,
                            dispatch_policy(shared),
                            declaring_class,
                            method,
                            registration,
                        )
                        .native_callback()
                    })
                };
                (
                    native,
                    method.is_synchronized(),
                    method.is_static(),
                    declaring_id_out,
                    resolved_tail_override,
                )
            }
            None => {
                // Method not found in class hierarchy вЂ” try the native registry
                // as a fallback. This handles synthetic stub classes (JDK classes
                // loaded without .class files) whose methods are all native.
                // Walk the superclass chain so inherited native methods (e.g.
                // Object.hashCode called on a subclass) are found.
                let mut lookup_id = Some(class_id);
                let mut found_callback = None;
                while let Some(cid) = lookup_id {
                    if let Some(cls) = store.get(cid) {
                        if let Some(cb) =
                            shared
                                .natives
                                .native_methods
                                .find(&cls.name, method_name, descriptor)
                        {
                            found_callback = Some(cb);
                            break;
                        }
                        lookup_id = cls.superclass;
                    } else {
                        break;
                    }
                }
                // Interpreter round i5 wave 100, lane L2
                // (`i100-L2-unresolvable-reference-runs-the-receivers-method`,
                // `L2W100UnresolvedSiteWarm` v2/vp/i2): a virtual call named a
                // class that provably resolves no such member (JVMS 5.4.3.3 /
                // 5.4.3.4), on a receiver of a real subtype of it. HotSpot's
                // `NoSuchMethodError` names that class before any selection;
                // the receiver fallbacks below ran a same-signature method the
                // receiver's class declares instead. Only on this miss (a
                // resolved call never gets here), only for a non-`Object`
                // named class with real class bytes and no native under its
                // name, and under `CRATONVM_UNRESOLVABLE_METHOD_REF_THROWS`.
                if found_callback.is_none()
                    && kind == OnClassKind::Unknown
                    && method_name != "<init>"
                    && method_name != "<clinit>"
                    && store
                        .get(named_class_id)
                        .is_some_and(|c| &*c.name != "java/lang/Object")
                    && match args.first() {
                        Some(Value::Object(Some(recv))) => {
                            let recv_cid = shared.mem.heap.class_id_of(*recv);
                            recv_cid != ClassId::new(0)
                                && recv_cid != named_class_id
                                && shared.mem.heap.kind_of(*recv)
                                    != cratonvm_types::ObjectKind::Array
                                && cm.is_subclass_of(recv_cid, named_class_id)
                        }
                        _ => false,
                    }
                    && crate::runtime::resolve::selection::member_provably_absent(
                        store,
                        named_class_id,
                        method_name,
                        descriptor,
                    )
                {
                    let named = store
                        .get(named_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default();
                    if shared
                        .natives
                        .native_methods
                        .find(&named, method_name, descriptor)
                        .is_none()
                        && crate::runtime::interpreter::invoke::unresolvable_method_ref_throws_enabled()
                    {
                        drop(cm);
                        return Err(crate::runtime::exceptions::throw_linkage_error(
                            shared,
                            thread,
                            LinkageError::NoSuchMethodError {
                                class_name: named,
                                method_name: method_name.to_string(),
                                method_descriptor: descriptor.to_string(),
                            },
                        ));
                    }
                }
                let class_name = store
                    .get(class_id)
                    .map(|c| c.name.to_string())
                    .unwrap_or_else(|| format!("<unknown class {class_id}>"));
                drop(cm);

                if let Some(callback) = found_callback {
                    return safe_native_call(shared, thread, callback, args);
                }

                // Signature-polymorphic methods (JVM spec В§5.4.3.4):
                // MethodHandle.invoke / invokeExact / invokeWithArguments and
                // VarHandle.get / set / compareAndSet etc. are called with the
                // call-site descriptor, but registered with a generic one.
                //
                // The `linkTo*` linker intrinsics too (interpreter round i2
                // wave 55, lane L4; `is_method_handle_linker_intrinsic`): a
                // static call with no handle receiver, so none of the
                // receiver-keyed steps below (the downcall routing, the poly
                // witness, the route cache) applies to it.
                let is_linker = is_method_handle_linker_intrinsic(&class_name, method_name);
                if is_linker || is_signature_polymorphic_method_name(method_name) {
                    // Check if receiver is a MethodHandle or VarHandle.
                    // DirectMethodHandle / BoundMethodHandle / DelegatingMethodHandle
                    // and their inner species (e.g. DirectMethodHandle$Constructor,
                    // BoundMethodHandle$Species_L) are concrete MethodHandle
                    // subclasses whose names do NOT start with "MethodHandle", so
                    // the prefix check missed them — invokeExact on such a receiver
                    // then fell through to normal resolution and threw
                    // NoSuchMethodError ("DirectMethodHandle$Constructor.invokeExact
                    // ()Ljava/lang/Object;"). Jackson 3 (tools.jackson) invokes a
                    // record/POJO canonical constructor directly through a
                    // DirectMethodHandle$Constructor, so its readValue silently
                    // failed. Recognise any java/lang/invoke class carrying
                    // "MethodHandle" in its name as a signature-polymorphic receiver.
                    let is_mh = is_method_handle_signature_polymorphic_receiver(&class_name);
                    // Recognise any java/lang/invoke class carrying "VarHandle" in
                    // its name, not just ones literally prefixed "VarHandle" — the
                    // JEP 454 FFM API's `SegmentVarHandle` (used by
                    // `MemorySegment.get/set`) is a top-level class that does NOT
                    // share that prefix, and previously fell through to normal
                    // method resolution and threw NoSuchMethodError. Mirrors the
                    // analogous `is_mh` broadening above.
                    let is_vh = is_var_handle_signature_polymorphic_receiver(&class_name);
                    if is_mh || is_vh {
                        let base = if is_mh {
                            "java/lang/invoke/MethodHandle"
                        } else {
                            "java/lang/invoke/VarHandle"
                        };
                        // OPEN — the wave-2 census gap (invoke.rs:3296), noted
                        // here because this is where it is produced.
                        //
                        // The three `find` loops below dispatch without calling
                        // `record_invocation`, so `--dump-native-registry`
                        // reports `invocations: 0` for EVERY
                        // signature-polymorphic MethodHandle/VarHandle native
                        // even on a run where it executed thousands of times.
                        // Anyone using that column as a "did this native run"
                        // oracle gets a false negative; W8-1 was misled by
                        // exactly this.
                        //
                        // It is NOT a one-line fix. Mechanically it is small —
                        // `find(c, m, d)` is documented to equal
                        // `resolve_id(c, m, d).and_then(callback_of)`, so each
                        // site can resolve the id, count it, and redeem the
                        // callback for the price of one array index. The
                        // blocker is what the count would mean: `record_
                        // invocation` feeds `invocations_by_kind`, which the
                        // jdk-only gate reads with `NativeKind::SyntheticStub`
                        // and asserts is zero. The kinds reachable from here
                        // are not knowable at this site — the third loop
                        // resolves an ARBITRARY receiver class name, and
                        // several reachable registrations (e.g.
                        // `java/lang/foreign/DowncallHandle`'s invoke/
                        // invokeExact/invokeBasic in `panama.rs`, and
                        // `MethodHandle.invokeWithArguments`) use plain
                        // `register`, taking whatever category was active. So
                        // closing the gap needs a build plus a gate run to
                        // confirm it does not flip that assertion off zero,
                        // which is a measurement this lane could not make.
                        //
                        // Publish the CALL-SITE descriptor for the natives
                        // that cannot decide without it. `arms_poly_call_site`
                        // owns which names those are and why; everything else
                        // signature-polymorphic is CLEARED, so nothing here can
                        // leave a stale descriptor for a later dispatch to read
                        // as its own. See `cratonvm_native_api::poly_call_site`.
                        if arms_poly_call_site(&class_name, method_name) {
                            cratonvm_native_api::poly_call_site::arm(descriptor);
                        } else {
                            cratonvm_native_api::poly_call_site::clear();
                        }
                        // The VarHandle twin: the static types of the value
                        // parameters, which decide CCE vs WMTE (and whether a
                        // reference may be unboxed at all) for a primitive
                        // variable. Armed here, next to the native call, and
                        // cleared below if no native ran. See
                        // `arms_vh_value_site`.
                        let armed_vh_value_site =
                            is_vh && arms_vh_value_site(&class_name, method_name);
                        // Round 15 wave 3 (lane vh4): the whole parameter-site
                        // word, for the reads too (see `arms_vh_value_site`).
                        if armed_vh_value_site {
                            // With the site's primitive return in the top byte
                            // (wave 57, lane L4: `asType`'s return direction).
                            cratonvm_native_api::poly_call_site::arm_vh_param_site(
                                cratonvm_native_api::poly_call_site::vh_param_site_word(descriptor),
                            );
                        }
                        // Round 15 wave 2 hand-back (lane vh2; VH2-3): the
                        // handle's variable kind, read before the native can
                        // move the handle (`unbox_poly_return_checked_vh`).
                        let vh_variable_is_reference = if is_vh {
                            let receiver = match args.first() {
                                Some(Value::Object(Some(o))) => Some(*o),
                                _ => None,
                            };
                            vh_reference_return_variable(
                                shared,
                                thread,
                                method_name,
                                descriptor,
                                receiver,
                            )
                        } else {
                            None
                        };
                        // Try all possible registered descriptors for signature-polymorphic methods.
                        // These methods are registered with generic Object[] params but varying return types.
                        let poly_descs = SIGNATURE_POLYMORPHIC_NATIVE_DESCRIPTORS;
                        let prefer_exact =
                            prefers_exact_signature_polymorphic_receiver(&class_name);
                        // The resolved owner of invokeExact is MethodHandle, not
                        // the concrete receiver. Route a synthetic Panama
                        // DowncallHandle by its runtime class before falling
                        // back to MethodHandle's generic bridge.
                        let dynamic_downcall = !is_linker
                            && args
                                .first()
                                .and_then(|value| match value {
                                    Value::Object(Some(receiver)) => shared
                                        .classes
                                        .class_manager
                                        .read()
                                        .get_class(shared.mem.heap.class_id_of(*receiver))
                                        .map(|class| {
                                            class.name.as_ref()
                                                == "java/lang/foreign/DowncallHandle"
                                        }),
                                    _ => None,
                                })
                                .unwrap_or(false);
                        let exact_class = if dynamic_downcall {
                            "java/lang/foreign/DowncallHandle"
                        } else {
                            class_name.as_str()
                        };
                        if prefer_exact || dynamic_downcall {
                            for poly_desc in &poly_descs {
                                if let Some(cb) = shared.natives.native_methods.find(
                                    exact_class,
                                    method_name,
                                    poly_desc,
                                ) {
                                    let mut r = safe_native_call(shared, thread, cb, args);
                                    // Round 15 wave 3 (lane vh4): a native that
                                    // did not take the word must not leave it.
                                    if armed_vh_value_site {
                                        cratonvm_native_api::poly_call_site::clear_vh_value_site();
                                        r = vh_astype_refusal_site_text(
                                            shared,
                                            thread,
                                            r,
                                            method_name,
                                            descriptor,
                                        );
                                    }
                                    let r = r?;
                                    return unbox_poly_return_checked_vh(
                                        shared,
                                        thread,
                                        r,
                                        descriptor,
                                        method_name,
                                        vh_variable_is_reference,
                                    );
                                }
                            }
                        }
                        // The route the interpreter's signature-polymorphic
                        // site cache replays (`sig_poly_site::note_route`;
                        // wave 48, lane L7). Not for the exact-first or
                        // downcall shapes, which it does not reproduce.
                        //
                        // Merge of JIT round 15 with i1 wave 48: nor for a
                        // `VarHandle` site whose door this block runs beyond
                        // what the replay (`SigPolyDoor::PolyBlock`) does --
                        // round 15's parameter-site word with COORDINATE
                        // classes (wave 3, lane vh3/vh4: the replay arms only
                        // the low 16 value bits, so a mistyped coordinate
                        // would go unjudged on a hit) or its reference-return
                        // checkcast (wave 2 hand-back, VH2-3:
                        // `unbox_poly_return_checked_vh`, which the replay does
                        // not call). Such a site keeps this slow path.
                        //
                        // JIT round 16 wave 7 (lane varhandle;
                        // `r16-varhandle-compare-and-set-seven-times-slower-since-round-15-FIXED-20261001.md`):
                        // that exclusion caught EVERY site with three or more
                        // parameters -- an instance `compareAndSet` /
                        // `compareAndExchange` (`(LH;II)Z`), an array `set` /
                        // `get` with an index -- because the receiver
                        // coordinate's class sits in the word's third byte. Those
                        // sites lost the cache and paid this whole slow path per
                        // call (~7x under `--nojit`). The replay now arms the
                        // whole word and runs the reference-return checkcast
                        // (`SigPolyDoor::PolyBlock`), so it reproduces this door
                        // for every `VarHandle` site.
                        // `CRATONVM_VH_SITE_CACHE_FULL_DOOR=0` restores the
                        // exclusion.
                        let vh_param_word = armed_vh_value_site.then(|| {
                            cratonvm_native_api::poly_call_site::vh_param_site_word(descriptor)
                        });
                        let vh_narrow_reference_return =
                            is_vh && vh_site_returns_narrow_reference(method_name, descriptor);
                        let replay_reproduces_vh_door = !is_vh
                            || true
                            || (vh_param_word.is_none_or(|word| {
                                (word
                                    & cratonvm_native_api::poly_call_site::VH_PARAM_SITE_PARAM_MASK)
                                    >> 16
                                    == 0
                            }) && !vh_narrow_reference_return);
                        let note_route = |thread: &mut JvmThread, owner: &str, poly_desc: &str| {
                            if prefer_exact
                                || dynamic_downcall
                                || is_linker
                                || !replay_reproduces_vh_door
                            {
                                return;
                            }
                            crate::runtime::interpreter::sig_poly_site::note_route(
                                shared,
                                thread,
                                match args.first() {
                                    Some(Value::Object(Some(receiver))) => Some(*receiver),
                                    _ => None,
                                },
                                owner,
                                method_name,
                                poly_desc,
                                descriptor,
                                crate::runtime::interpreter::sig_poly_site::SigPolyDoor::PolyBlock {
                                    // The whole word this block armed (with the
                                    // full-door switch off, the guard above
                                    // admits only a word with nothing above its
                                    // low 16 value bits, as before).
                                    vh_param_word,
                                    vh_narrow_reference_return,
                                },
                                arms_poly_call_site(&class_name, method_name),
                            );
                        };
                        for poly_desc in &poly_descs {
                            if let Some(cb) =
                                shared
                                    .natives
                                    .native_methods
                                    .find(base, method_name, poly_desc)
                            {
                                if is_mh && !prefer_exact && !dynamic_downcall && !is_linker {
                                    note_poly_mh_native(
                                        shared,
                                        thread,
                                        cb,
                                        args,
                                        method_name,
                                        descriptor,
                                    );
                                }
                                if is_linker {
                                    note_linker_from_bytecode(method_name, descriptor);
                                }
                                note_route(thread, base, poly_desc);
                                let mut r = safe_native_call(shared, thread, cb, args);
                                if armed_vh_value_site {
                                    cratonvm_native_api::poly_call_site::clear_vh_value_site();
                                    r = vh_astype_refusal_site_text(
                                        shared,
                                        thread,
                                        r,
                                        method_name,
                                        descriptor,
                                    );
                                }
                                let r = r?;
                                return unbox_poly_return_checked_vh(
                                    shared,
                                    thread,
                                    r,
                                    descriptor,
                                    method_name,
                                    vh_variable_is_reference,
                                );
                            }
                        }
                        if !prefer_exact {
                            // Also try the exact class name
                            for poly_desc in &poly_descs {
                                if let Some(cb) = shared.natives.native_methods.find(
                                    &class_name,
                                    method_name,
                                    poly_desc,
                                ) {
                                    if is_mh && !prefer_exact && !dynamic_downcall && !is_linker {
                                        note_poly_mh_native(
                                            shared,
                                            thread,
                                            cb,
                                            args,
                                            method_name,
                                            descriptor,
                                        );
                                    }
                                    note_route(thread, &class_name, poly_desc);
                                    let mut r = safe_native_call(shared, thread, cb, args);
                                    if armed_vh_value_site {
                                        cratonvm_native_api::poly_call_site::clear_vh_value_site();
                                        r = vh_astype_refusal_site_text(
                                            shared,
                                            thread,
                                            r,
                                            method_name,
                                            descriptor,
                                        );
                                    }
                                    let r = r?;
                                    return unbox_poly_return_checked_vh(
                                        shared,
                                        thread,
                                        r,
                                        descriptor,
                                        method_name,
                                        vh_variable_is_reference,
                                    );
                                }
                            }
                        }
                        // No native ran, so nothing took the value-site cell:
                        // do not leave it for a later door that never armed.
                        if armed_vh_value_site {
                            cratonvm_native_api::poly_call_site::clear_vh_value_site();
                        }
                    }
                }

                // Check interfaces for default methods (e.g. Function$AndThen
                // implements Function, so Function.andThen should be found).
                //
                // SPLITERATOR-FALLTHROUGH: When the receiver is a real-JDK
                // subclass implementing an interface for which we have BOTH
                // a registered native (e.g. `java/util/Spliterator.tryAdvance`
                // registered for our synthetic Spliterator instances) AND the
                // interface declares a default method (or any super-interface
                // does), we MUST prefer the JDK default-method bytecode over
                // our native: the native operates on synthetic field layouts
                // (field 0 = backing array, field 1 = cursor) and silently
                // returns false/0 when invoked on a real-JDK subclass like
                // `ServiceLoaderUtil$ServiceLoaderSpliterator` whose field 0
                // is an Iterator. This regressed log4j's
                // `PropertySource$Util.<clinit>` (Stream.forEach over a
                // ServiceLoader-driven Spliterator returned 0 elements,
                // surfacing as IAE during WildFly boot).
                {
                    let cm2 = shared.classes.class_manager.read();
                    if let Some(class) = cm2.class_store.get(class_id) {
                        // Collect transitive interfaces (BFS over super-ifaces)
                        // so we find default methods declared on a parent
                        // interface even if the receiver implements only a
                        // sub-interface.
                        let mut iface_queue: Vec<ClassId> =
                            class.interfaces.iter().copied().collect();
                        let mut visited_ifaces: std::collections::HashSet<ClassId> =
                            std::collections::HashSet::new();
                        let mut iface_names: Vec<String> = Vec::new();
                        let mut i = 0;
                        while i < iface_queue.len() {
                            let iid = iface_queue[i];
                            i += 1;
                            if !visited_ifaces.insert(iid) {
                                continue;
                            }
                            if let Some(iface) = cm2.class_store.get(iid) {
                                iface_names.push(iface.name.to_string());
                                iface_queue.extend_from_slice(&iface.interfaces);
                            }
                        }
                        // First pass: prefer a non-abstract default method on
                        // any (super-)interface — running the JDK bytecode is
                        // always safer than dispatching to a native shaped for
                        // synthetic receivers.
                        let mut default_iface: Option<ClassId> = None;
                        for &iid in visited_ifaces.iter() {
                            if let Some(iface) = cm2.class_store.get(iid) {
                                if let Some(m) = iface.find_method(method_name, descriptor) {
                                    // A DEFAULT method only: a static interface
                                    // method is not inherited, so `C.m` / `J.m`
                                    // reaching `I`'s static is HotSpot's
                                    // `NoSuchMethodError` (the terminal below),
                                    // not a call of it. The JIT's by-name doors
                                    // land here; one warm call ran `I.m`
                                    // (interpreter round i4 wave 98, lane L7;
                                    // `L7W98StaticShadowDispatch` s1-s4).
                                    if !m.is_abstract() && !m.is_static() {
                                        default_iface = Some(iid);
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some(iid) = default_iface {
                            drop(cm2);
                            return invoke_on_class_shared(
                                shared,
                                thread,
                                iid,
                                method_name,
                                descriptor,
                                args,
                            );
                        }
                        drop(cm2);
                        // Second pass: fall back to a native registered on
                        // any interface name (legacy behavior).
                        for iface_name in &iface_names {
                            if let Some(callback) = shared.natives.native_methods.find(
                                iface_name,
                                method_name,
                                descriptor,
                            ) {
                                return safe_native_call(shared, thread, callback, args);
                            }
                        }
                    }
                }

                // S111r7 — receiver-driven fallback for bare `Object`
                // dispatch. When real-JDK bytecode allocates an inner-class
                // view (e.g. `HashMap$KeySet`, `HashMap$KeyIterator`) we
                // don't load from the JDK module image, the heap object
                // ends up with `cid=0` and `class_id_of` propagates as
                // `java/lang/Object`. Without this rescue, the eventual
                // `invokeinterface Set.iterator()` /
                // `Iterator.hasNext()` etc. dispatches against
                // `Object.<missing>` and surfaces a swallowed
                // `NoSuchMethodError`. Spring boot's
                // `GenericConversionService.addConverter()` →
                // `getConvertibleTypes().iterator()` is the canonical
                // tripwire — fixing it unblocks 9+ Spring boot apps.
                //
                // Strategy: if the dispatch class is `Object` AND the
                // method isn't an `Object` method, route the call through
                // a name-based native lookup against the well-known
                // collection-view fallbacks. The `keySet/values/entrySet`
                // wrappers we materialise via `make_hashset_with_elements`
                // expose the same external contract, so dispatching to
                // those natives against the original receiver's
                // surrounding HashMap recovers the iterator.
                if class_name == "java/lang/Object" && !is_object_member(method_name, descriptor) {
                    // Lambda-proxy receiver rescue: some long-running mixed-mode
                    // stream/lambda paths reach this slow NSME path with the CP
                    // dispatch class collapsed to Object, while the heap receiver
                    // still has a synthetic lambda ClassId tracked only in
                    // `lambda_proxies` (not in class_store). Mirror the JIT helper
                    // route and dispatch the functional-interface SAM through the
                    // lambda metadata before trying generic Object fallbacks.
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.mem.heap.class_id_of(recv);
                        if shared.classes.lambda_proxies.read().contains_key(&recv_cid) {
                            if let Some(result) = crate::runtime::interpreter::try_lambda_dispatch(
                                shared,
                                thread,
                                recv,
                                recv_cid,
                                method_name,
                                descriptor,
                                &args[1..],
                            )? {
                                return Ok(result);
                            }
                        }
                    }

                    // Synthetic-receiver rescue: when both the dispatch class
                    // AND the heap receiver are bare `java/lang/Object`, the
                    // object was allocated by a native with `ClassId::new(0)`
                    // (Pattern / Matcher / similar synthetic placeholders that
                    // intentionally avoid running real-JDK `<clinit>`). Probe
                    // the native registry for the method+descriptor against
                    // the well-known synthetic types so subsequent
                    // `Pattern.matcher(CharSequence)` / `Matcher.find()` etc.
                    // dispatch to our registered natives instead of NSME-ing
                    // against `Object.matcher` / `Object.find` / `Object.group`.
                    //
                    // We probe a fixed set of candidate class names rather
                    // than scanning the whole registry: keeps the slow-path
                    // bounded, and the false-positive risk is near zero
                    // because the candidate list is exactly the synthetic
                    // types we allocate with ClassId 0.
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.mem.heap.class_id_of(recv);
                        if recv_cid == ClassId::new(0)
                            || shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(recv_cid)
                                .map(|c| c.name.as_ref() == "java/lang/Object")
                                .unwrap_or(false)
                        {
                            const SYNTH_RECEIVER_CANDIDATES: &[&str] =
                                &["java/util/regex/Pattern", "java/util/regex/Matcher"];
                            for cand in SYNTH_RECEIVER_CANDIDATES {
                                if let Some(cb) = shared.natives.native_methods.find(
                                    cand,
                                    method_name,
                                    descriptor,
                                ) {
                                    tracing::warn!(
                                        target: "cratonvm_vm::dispatch::synth_rescue",
                                        rescued_via = %cand,
                                        method = %format!("{method_name}{descriptor}"),
                                        "RECEIVER-IS-OBJECT synth rescue fired",
                                    );
                                    return safe_native_call(shared, thread, cb, args);
                                }
                            }
                        }
                    }

                    // Try receiver class chain — covers the case where
                    // recv_cid is a valid non-Object class but the CP
                    // dispatch resolved to Object due to a synthetic alloc.
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.mem.heap.class_id_of(recv);
                        let cm2 = shared.classes.class_manager.read();
                        let recv_name = cm2
                            .class_store
                            .get(recv_cid)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        drop(cm2);
                        // JVMS §4.4.1: an array's method table comes from
                        // `Object`, but its header carries the COMPONENT class
                        // id — walking that chain would run the component
                        // class's own body with the array as `this`.
                        let recv_is_array =
                            shared.mem.heap.kind_of(recv) == cratonvm_types::ObjectKind::Array;
                        if !recv_is_array
                            && !recv_name.is_empty()
                            && recv_name != "java/lang/Object"
                        {
                            // Native registered on the receiver's class
                            // (or any superclass on the chain).
                            let cm3 = shared.classes.class_manager.read();
                            let mut walk_cid = Some(recv_cid);
                            while let Some(cid) = walk_cid {
                                if let Some(cls) = cm3.class_store.get(cid) {
                                    if let Some(cb) = shared.natives.native_methods.find(
                                        &cls.name,
                                        method_name,
                                        descriptor,
                                    ) {
                                        drop(cm3);
                                        return safe_native_call(shared, thread, cb, args);
                                    }
                                    walk_cid = cls.superclass;
                                } else {
                                    break;
                                }
                            }
                            // Bytecode method on the receiver's class chain.
                            if let Some((_method, declaring_id)) =
                                crate::classloading::find_method_recursive(
                                    recv_cid,
                                    method_name,
                                    descriptor,
                                    &cm3.class_store,
                                )
                            {
                                drop(cm3);
                                return invoke_on_class_shared(
                                    shared,
                                    thread,
                                    declaring_id,
                                    method_name,
                                    descriptor,
                                    args,
                                );
                            }
                            drop(cm3);
                        }
                    }
                }

                if crate::runtime::env_cache::nsme_dbg() {
                    // Diagnostic: when an NSME is about to be raised, capture
                    // the receiver's actual concrete class and the caller's
                    // method name so a wrong-dispatch (receiver vs. cp class
                    // mismatch) shows up in logs without rebuilding.
                    let recv_dbg = match args.first() {
                        Some(Value::Object(Some(o))) => {
                            let cid = shared.mem.heap.class_id_of(*o);
                            let cm3 = shared.classes.class_manager.read();
                            cm3.get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| format!("<cid {cid}>"))
                        }
                        Some(Value::Object(None)) => "<null>".to_string(),
                        Some(v) => format!("<non-obj {v:?}>"),
                        None => "<no-args>".to_string(),
                    };
                    let caller_dbg = thread
                        .frames
                        .last()
                        .map(|f| {
                            format!(
                                "{}.{}{}",
                                f.class_name(),
                                f.method_name(),
                                f.method_descriptor()
                            )
                        })
                        .unwrap_or_else(|| "<no-frame>".to_string());
                    eprintln!(
                        "[NSME_DBG] dispatch_class={class_name} method={method_name}{descriptor} receiver={recv_dbg} caller={caller_dbg}"
                    );
                    // Full Java call stack (mirrors CRATONVM_DBG_NPE_STACK) --
                    // the immediate caller alone doesn't show whether this
                    // invoke originates from real bytecode several frames up
                    // (e.g. real JDK URLClassPath machinery) or from a native
                    // helper reusing the wrong object as a receiver.
                    let cm4 = shared.classes.class_manager.read();
                    eprintln!("[NSME_DBG] full stack:");
                    for (i, f) in thread.frames.iter().enumerate().rev().take(30) {
                        let cn = cm4
                            .get_class(f.class_id)
                            .map(|c| c.name.to_string())
                            .unwrap_or_default();
                        eprintln!(
                            "  [{i}] {}.{}{} pc={}",
                            cn,
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        );
                    }
                    drop(cm4);
                }
                // Compatibility fallback: a few real-world call sites have shown
                // descriptor canonicalization drift (same signature but object
                // return type with/without trailing ';'). Before surfacing
                // NSME, retry native dispatch with the alternate descriptor.
                let alt_descriptor = if descriptor.ends_with(';') {
                    descriptor.trim_end_matches(';').to_string()
                } else {
                    format!("{descriptor};")
                };
                if alt_descriptor != descriptor {
                    if let Some(cb) = shared.natives.native_methods.find(
                        &class_name,
                        method_name,
                        &alt_descriptor,
                    ) {
                        return safe_native_call(shared, thread, cb, args);
                    }
                }
                // Surefire bootstrap compatibility: older framework bytecode
                // expects `Thread.getThreadGroup()` very early, before our
                // synthetic ThreadGroup model is fully wired. Returning null
                // matches HotSpot's "no group yet" behavior for bootstrap
                // helper threads and avoids hard-failing the fork startup.
                if class_name == "java/lang/Thread"
                    && method_name == "getThreadGroup"
                    && descriptor == "()Ljava/lang/ThreadGroup;"
                {
                    return Ok(Some(Value::Object(None)));
                }
                // Loader-aware receiver retry: this recursive slow path can be
                // entered after name-based dispatch picked the global copy of a
                // class while the heap receiver is a loader-specific copy with
                // extra bytecode-enhancement interfaces/default methods. Retry
                // against the receiver's real class before falling through to
                // CP-interface or native-only rescues.
                if !no_retarget && method_name != "<init>" && method_name != "<clinit>" {
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.mem.heap.class_id_of(recv);
                        if recv_cid != class_id && recv_cid != ClassId::new(0) {
                            let cm_recv = shared.classes.class_manager.read();
                            if let Some((m, declaring_id)) =
                                crate::classloading::find_method_recursive(
                                    recv_cid,
                                    method_name,
                                    descriptor,
                                    &cm_recv.class_store,
                                )
                            {
                                if declaring_id != class_id && !m.is_abstract() && !m.is_static() {
                                    drop(cm_recv);
                                    return invoke_on_class_shared_no_retarget(
                                        shared,
                                        thread,
                                        declaring_id,
                                        method_name,
                                        descriptor,
                                        args,
                                    );
                                }
                            }
                        }
                    }
                }

                // KAFKA-DEFAULT-RESCUE: invokeinterface on a receiver whose
                // runtime class is bare `java/lang/Object` (a synthetic
                // ServiceLoader provider stub) can land here when the target
                // is a default method declared on the CP-resolved interface
                // itself. The receiver class doesn't list the interface in
                // its `interfaces` table, so neither this function's
                // hierarchy walk nor the existing receiver-based rescues
                // find it. Consult the CP-resolved interface directly
                // before raising NSME.
                if let Some(cp_iface_cid) = pending_cp_iface() {
                    if cp_iface_cid != class_id {
                        let cm_iface = shared.classes.class_manager.read();
                        if let Some((m, declaring_id)) = crate::classloading::find_method_recursive(
                            cp_iface_cid,
                            method_name,
                            descriptor,
                            &cm_iface.class_store,
                        ) {
                            // Only rescue with a real default method: a
                            // concrete instance method declared on the
                            // interface (or a super-interface) of the
                            // CP-resolved type. Abstract / static entries
                            // would not be valid dispatch targets here.
                            if !m.is_abstract() && !m.is_static() {
                                drop(cm_iface);
                                return invoke_on_class_shared(
                                    shared,
                                    thread,
                                    declaring_id,
                                    method_name,
                                    descriptor,
                                    args,
                                );
                            }
                        }
                    }
                }
                // Final native-registry fallback: a few real-JDK classes
                // (e.g. `java/io/PrintStream`) have package-private methods
                // that real-world bytecode reaches via a wider-typed call
                // site (`Writer.write(String,int,int)` against a PrintStream
                // receiver). Method resolution against the class file misses
                // these, but we provide Rust natives for them. Walk both the
                // dispatch class chain and the receiver class chain probing
                // the native registry before surfacing NSME.
                {
                    let cm_nat = shared.classes.class_manager.read();
                    let mut probe_cid = Some(class_id);
                    while let Some(cid) = probe_cid {
                        if let Some(cls) = cm_nat.class_store.get(cid) {
                            if crate::runtime::env_cache::nsme_dbg() {
                                eprintln!(
                                    "[NSME_DBG] native-probe class={} method={}{} hit={}",
                                    cls.name,
                                    method_name,
                                    descriptor,
                                    shared
                                        .natives
                                        .native_methods
                                        .find(&cls.name, method_name, descriptor)
                                        .is_some()
                                );
                            }
                            if let Some(cb) = shared.natives.native_methods.find(
                                &cls.name,
                                method_name,
                                descriptor,
                            ) {
                                drop(cm_nat);
                                return safe_native_call(shared, thread, cb, args);
                            }
                            probe_cid = cls.superclass;
                        } else {
                            break;
                        }
                    }
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        let recv_cid = shared.mem.heap.class_id_of(recv);
                        let mut probe_cid = Some(recv_cid);
                        while let Some(cid) = probe_cid {
                            if cid == class_id {
                                break;
                            }
                            if let Some(cls) = cm_nat.class_store.get(cid) {
                                if let Some(cb) = shared.natives.native_methods.find(
                                    &cls.name,
                                    method_name,
                                    descriptor,
                                ) {
                                    drop(cm_nat);
                                    return safe_native_call(shared, thread, cb, args);
                                }
                                probe_cid = cls.superclass;
                            } else {
                                break;
                            }
                        }
                    }
                    drop(cm_nat);
                }
                // Annotation-proxy rescue: reflective dispatch (e.g.
                // `Method.invoke` on `ExtendWith::value`) resolves the
                // receiver's runtime class directly — which for our
                // synthetic annotation proxies is
                // `java/lang/annotation/AnnotationProxy`, a class with no
                // bytecode methods at all. Its members are served from the
                // proxy's stored name/value element arrays, so route there
                // as the last resort before surfacing NoSuchMethodError.
                if let Some(Value::Object(Some(recv))) = args.first().copied() {
                    let recv_is_ann_proxy = shared.mem.heap.kind_of(recv)
                        == cratonvm_types::ObjectKind::Object
                        && shared
                            .classes
                            .class_manager
                            .read()
                            .get_class(shared.mem.heap.class_id_of(recv))
                            .map(|c| &*c.name == "java/lang/annotation/AnnotationProxy")
                            .unwrap_or(false);
                    if recv_is_ann_proxy {
                        return annotation_proxy_invoke_shared(
                            shared,
                            thread,
                            recv,
                            method_name,
                            &args[1..],
                        );
                    }
                }
                if let Some(result) =
                    object_serialization_hook_neutral_result(method_name, descriptor, args)
                {
                    if None::<std::ffi::OsString>
                        .is_some()
                    {
                        eprintln!(
                            "[rf-ser] neutral missing serialization hook {}.{}{} caller={}",
                            class_name,
                            method_name,
                            descriptor,
                            thread.frames.last().map(|f| f.method_name()).unwrap_or("")
                        );
                    }
                    return Ok(result);
                }

                // Diagnostic aid: a NoSuchMethodError against a
                // `is_synthetic_stub` class is almost always a masked
                // classpath gap, not a genuine method-resolution bug — the
                // class itself was never found on any classpath entry, so
                // `create_synthetic_stub` fabricated an empty stand-in (see
                // `is_enterprise_stub_prefix` in classloading/class_manager.rs)
                // and every method call against it fails here. Tag the log
                // line so this is diagnosable without a debug rebuild
                // (previously required `RUST_LOG=debug` to see the separate
                // "Falling back to synthetic stub" line and correlate it).
                let stub_hint = if shared
                    .classes
                    .class_manager
                    .read()
                    .get_class(class_id)
                    .map(|c| c.origin.is_compatibility_stub())
                    .unwrap_or(false)
                {
                    " [class not found on any classpath entry — synthetic stub, add the missing jar]"
                } else {
                    ""
                };
                // A VM-dispatch lookup can allocate/re-enter before it proves
                // its target. If that moved the receiver, retry against the
                // collector-forwarded runtime class rather than turning a
                // stale `Map` into `Object.get` at this terminal miss.
                args_roots.refresh(args);
                if !no_retarget && method_name != "<init>" && method_name != "<clinit>" {
                    if let Some(Value::Object(Some(recv))) = args.first().copied() {
                        if shared.mem.heap.kind_of(recv) != cratonvm_types::ObjectKind::Array {
                            let refreshed_cid = shared.mem.heap.class_id_of(recv);
                            if refreshed_cid != class_id && refreshed_cid != ClassId::new(0) {
                                let cm = shared.classes.class_manager.read();
                                let this_is_iface_or_abs = cm
                                    .get_class(class_id)
                                    .map(|c| c.is_interface() || c.is_abstract())
                                    .unwrap_or(false);
                                let recv_is_concrete = cm
                                    .get_class(refreshed_cid)
                                    .map(|c| !c.is_interface())
                                    .unwrap_or(false);
                                let is_subclass = cm.is_subclass_of(refreshed_cid, class_id);
                                let original_is_static =
                                    crate::classloading::find_method_recursive(
                                        class_id,
                                        method_name,
                                        descriptor,
                                        &cm.class_store,
                                    )
                                    .map(|(m, _)| m.is_static())
                                    .unwrap_or(false);
                                drop(cm);
                                if !original_is_static
                                    && this_is_iface_or_abs
                                    && recv_is_concrete
                                    && is_subclass
                                {
                                    return invoke_on_class_shared_no_retarget(
                                        shared,
                                        thread,
                                        refreshed_cid,
                                        method_name,
                                        descriptor,
                                        args,
                                    );
                                }
                            }
                        }
                    }
                }
                tracing::warn!(
                    method = format!("{class_name}.{method_name}{descriptor}{stub_hint}"),
                    caller = thread
                        .frames
                        .last()
                        .map(|f| format!(
                            "{}.{}{} @pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.method_descriptor(),
                            f.pc
                        ))
                        .unwrap_or_default(),
                    "NoSuchMethodError"
                );
                // H2-CID0, dispatch face (2026-08-02): the same flag-free
                // reclaimed-memory verdict the two `checkcast` reporters emit.
                // A receiver whose class resolved to `ClassId(0)` is ambiguous
                // — bare `java.lang.Object`, an un-hashed `new Object()`, and
                // the all-zero header the collector leaves over a reclaimed
                // span all wear that face — and until now the INVOKE path said
                // nothing at all about which. Free-list membership is not
                // ambiguous, so ask the heap.
                //
                // The receiver's class id is passed through: the helper takes
                // the heap locks only for the `ClassId(0)` face, so this
                // terminal — which is also reached in bulk on HEALTHY runs, a
                // missing method on a synthetic classpath stub logging here on
                // every call — pays only a lock-free ring probe otherwise. That
                // probe still matters off the zero face: once the allocator
                // hands a prematurely-freed block out again, the same stale
                // reference resolves to a REAL class and the miss reads
                // `SomeUnrelatedClass.hasNext()` instead.
                //
                // Witness: `NoSuchMethodError java/lang/Object.hasNext()Z` from
                // `TestMultiThread.testConcurrentUpdate @pc=252` — the
                // `for (Future<Void> job : jobs)` iterator, `num_fields=0`. See
                // bug-h2-classid0-stale-address-family-FIXED.md.
                if let Some(Value::Object(Some(recv))) = args.first().copied() {
                    let addr = recv.as_ptr() as usize;
                    // The RE-SERVED face. The free-list verdict below answers
                    // for a receiver still sitting in reclaimed memory; once
                    // the allocator has handed the address out again it reads
                    // as a perfectly valid object of an unrelated class and
                    // every probe there stays silent. That is precisely this
                    // dispatch miss's shape -- `Hashtable.openStream()` for a
                    // `URL` receiver. The history ledger discriminates it, and
                    // the backtrace names the VM code still holding it.
                    cratonvm_gc::gc_quiescence::check_stale_use(addr, "invoke dispatch");
                    if crate::memory::reclaim_guard::report_reclaimed_receiver(
                        shared,
                        addr,
                        "invoke dispatch",
                        &format!("{class_name}.{method_name}{descriptor}"),
                        class_id.as_u32(),
                    ) {
                        // The free-list verdict fired, so this receiver really
                        // is reclaimed memory. Say where it stood in THIS
                        // thread's own root bookkeeping — the collector cannot
                        // answer that about a peer, and it is the question the
                        // 2026-08-05 `DriverManager.getConnection` witness
                        // leaves open.
                        crate::memory::reclaim_guard::report_root_slice_provenance(
                            shared,
                            thread,
                            addr,
                            "invoke dispatch",
                        );
                    }
                }
                // CRATONVM_DBG_CCE_BT: a dispatch miss whose receiver resolved
                // to bare `java/lang/Object` is the stale-ObjectRef family's
                // cid=0 signature surfacing at INVOKE (the checkcast tracer's
                // sibling — e.g. `Object.read([CII)I` in Elytron's
                // MechanismDatabase when a Reader ref went stale). Dump the
                // frame stack so the producing frame is named, exactly like
                // CCE-BT-STK.
                //
                // `java/lang/Object` is only the cid=0 END of that family's
                // range. A recycled address that now holds a REAL object of an
                // unrelated class produces the identical defect with a concrete
                // class name — `EmbeddableInitializerImpl.add(Ljava/lang/Object;)Z`
                // for a `Collection.add` call site, say — and the old class-name
                // test made the tracer blind to exactly those, which are the ones
                // where naming the producing frame matters most. The flag is
                // opt-in and this is a terminal error path, so every miss gets
                // the dump; the receiver's resolved class is printed with it so
                // "wrong class" and "no class" stay distinguishable in the log.
                if crate::runtime::interpreter::dbg_cce_bt_enabled() {
                    eprintln!(
                        "CRATONVM_DBG_CCE_BT: site=nsme_dispatch method={class_name}.{method_name}{descriptor}"
                    );
                    for (i, f) in thread.frames.iter().enumerate().rev().take(15) {
                        eprintln!(
                            "  CCE-BT-STK[{i}] {}.{} pc={}",
                            f.class_name(),
                            f.method_name(),
                            f.pc
                        );
                    }
                    // The frame chain names WHERE the bad receiver was used; it
                    // does not say which slot still holds it, or whether the
                    // same address sits in a caller's local as well. Both are
                    // the difference between "the producer handed back a stale
                    // value" and "one slot went stale in place", and both are
                    // gone the moment this terminal returns. Dump the object
                    // slots of the innermost three frames with the class each
                    // address actually resolves to.
                    for (i, f) in thread.frames.iter().enumerate().rev().take(3) {
                        let mut show = |what: &str, idx: usize, o: ObjectRef| {
                            let a = o.as_ptr() as usize;
                            let cid = shared.mem.heap.class_id_of(o);
                            let cn = shared
                                .classes
                                .class_manager
                                .read()
                                .get_class(cid)
                                .map(|c| c.name.to_string())
                                .unwrap_or_else(|| format!("cid#{}", cid.as_u32()));
                            eprintln!("    CCE-BT-SLOT[{i}] {what}[{idx}] 0x{a:x} {cn}");
                        };
                        for li in 0..f.locals_len() {
                            if let Value::Object(Some(o)) = f.get_local(li as u16) {
                                show("local", li, o);
                            }
                        }
                        for si in 0..f.stack.len() {
                            if let Value::Object(Some(o)) = f.stack.peek_at(si) {
                                show("stack", si, o);
                            }
                        }
                    }
                    if let Some(Value::Object(Some(r))) = args.first() {
                        let addr = r.as_ptr() as usize;
                        let blocked_flag = thread
                            .gc_block_state
                            .in_blocked_region
                            .load(std::sync::atomic::Ordering::Acquire);
                        eprintln!(
                            "  NSME-RECV addr=0x{addr:x} tid={} blocked={} epoch={}",
                            thread.thread_id.0,
                            blocked_flag,
                            shared.mem.heap.collection_count(),
                        );
                        // What the receiver actually IS. Every other line in
                        // this dump describes the address's *history*; none of
                        // them answers the first question a wrong-receiver miss
                        // raises — is this the intended object carrying a wrong
                        // class, or a different object entirely? Those need
                        // opposite fixes, and the shape settles it: a `Class`
                        // mirror is registered in `class_mirrors_reverse`, so a
                        // hit there with a non-`java/lang/Class` header is a
                        // mirror-header defect, a miss with plausible field
                        // values is a wrong-value read, and a miss with junk is
                        // a recycled address.
                        let mirror_of = crate::vm::vm_object::class_id_from_mirror(shared, *r)
                            .and_then(|cid| {
                                shared
                                    .classes
                                    .class_manager
                                    .read()
                                    .get_class(cid)
                                    .map(|c| c.name.to_string())
                            });
                        let nf = shared.mem.heap.num_fields(*r);
                        let fields: Vec<String> = (0..nf.min(4))
                            .map(|i| format!("[{i}]={:?}", shared.mem.heap.get_field(*r, i)))
                            .collect();
                        eprintln!(
                            "  NSME-RECV SHAPE kind={:?} num_fields={nf} mirror_of={} {}",
                            shared.mem.heap.kind_of(*r),
                            mirror_of.as_deref().unwrap_or("<not a registered mirror>"),
                            fields.join(" "),
                        );
                        for (e, moved_to, mlen, as_dest) in crate::memory::gc::gcpart_probe(addr) {
                            eprintln!(
                                "  NSME-RECV [gcpart] epoch={e} map_len={mlen} moved_to={moved_to:x?} appears_as_dest={as_dest}"
                            );
                        }
                        for (ago, site) in crate::runtime::interpreter::push_prov_find(addr) {
                            eprintln!(
                                "  NSME-RECV [pushprov] pushed {ago} invoke-returns ago at {site}"
                            );
                        }
                        for (ago, desc) in crate::runtime::interpreter::deposit_gap_find(addr) {
                            eprintln!("  NSME-RECV [deposit-gap] {ago} entries ago: {desc}");
                        }
                        for (age, site, tag, s, l) in cratonvm_gc::zero_forensics::probe(addr) {
                            eprintln!(
                                "  NSME-RECV [zeroed] age={age} site={} tag={tag} range=0x{s:x}+0x{l:x}",
                                if site == 1 { "sweep-span" } else { "fromspace-reset" },
                            );
                        }
                        if crate::runtime::interpreter::remap_trace_on() {
                            eprintln!(
                                "  NSME-RECV [trace]\n    {}",
                                crate::runtime::interpreter::remap_trace_dump()
                            );
                            for (ago, cb, site) in crate::runtime::interpreter::nret_find(addr) {
                                eprintln!(
                                    "  NSME-RECV [nret] returned {} native-returns ago by {} at {}",
                                    ago,
                                    cratonvm_native_api::native_ring::name_of(cb)
                                        .unwrap_or_else(|| format!("<cb@{cb:#x}>")),
                                    site,
                                );
                            }
                            for (ago, parent, fidx) in
                                crate::runtime::interpreter::getfield_ring_find(addr)
                            {
                                let cur = shared
                                    .mem
                                    .heap
                                    .is_object_address(parent)
                                    .map(|p| format!("{:?}", shared.mem.heap.get_field(p, fidx)))
                                    .unwrap_or_else(|| "<parent-not-obj>".into());
                                eprintln!(
                                    "  NSME-RECV [getfield] pushed {} getfields ago from parent=0x{parent:x} fld[{fidx}] — parent's field NOW = {cur}",
                                    ago,
                                );
                            }
                        }
                        for (fi, f) in thread.frames.iter().enumerate().rev().take(6) {
                            if let Some(loc) = f.dbg_locate_addr(addr) {
                                eprintln!("  NSME-RECV LOCATE frame#{fi} {loc}");
                            }
                            eprintln!("  NSME-RECV RAWSTACK frame#{fi}{}", f.dbg_stack_dump());
                            eprintln!(
                                "  NSME-RECV POPPED frame#{fi}{}",
                                f.stack.dbg_dump_popped(5)
                            );
                        }
                    }
                }
                // Optional operator diagnostic: at the terminal not-found point
                // (no native and no loadable bytecode), emit one clear line so
                // operators can see exactly what is missing. Gated on
                // CRATONVM_TRACE_UNIMPLEMENTED to avoid spamming normal runs.
                // Uses var_os directly (no new env_cache accessor required).
                if crate::runtime::env_cache::trace_unimplemented() {
                    eprintln!(
                        "[cratonvm] unimplemented: {}.{}{} — no native implementation and no loadable bytecode (CratonVM)",
                        class_name, method_name, descriptor
                    );
                }
                // A missing Hibernate-enhanced `$$_hibernate_*` accessor (HHH-16572
                // `InvalidPropertyNameTest`) used to take a dedicated branch here
                // that built the `NoSuchMethodError` by hand, because this terminal
                // was once an uncatchable internal abort. It is not any more:
                // `throw_linkage_error` builds the same catchable throwable, with
                // HotSpot's `'ret C.m(params)'` wording rather than the branch's
                // raw `C.m(desc)ret` (removed 2026-09-23, interpreter round wave 3).
                //
                // An `invokeinterface` reaching here through the receiver's
                // class (`receiver_selection` names the site's interface) for
                // a name only a superinterface's STATIC answers is a
                // resolution failure of the REFERENCE: HotSpot names the
                // interface (`'int HJ.m()'`), not the receiver (interpreter
                // round i4 wave 98, lane L7; `L7W98StaticShadowDispatch` s4).
                // The site's interface: what the caller resolved, or else the
                // class it NAMED before the receiver retarget above (the JIT's
                // by-name invokeinterface miss arrives with `receiver_selection`
                // None and `named_class_id` = the interface; hand-back 4).
                let site_iface = match receiver_selection {
                    Some(crate::runtime::resolve::selection::ResolvedRef::Interface(Some(iface))) => {
                        Some(iface)
                    }
                    _ if named_class_id != class_id => Some(named_class_id),
                    _ => None,
                };
                let class_name = match site_iface {
                    Some(iface) => {
                        let cm = shared.classes.class_manager.read();
                        let store = &cm.class_store;
                        if crate::runtime::resolve::selection::reference_names_only_an_uninherited_static_interface_method(
                            store, iface, method_name, descriptor,
                        ) {
                            store.get(iface).map_or(class_name, |c| c.name.to_string())
                        } else {
                            class_name
                        }
                    }
                    _ => class_name,
                };
                return Err(crate::runtime::exceptions::throw_linkage_error(
                    shared,
                    thread,
                    LinkageError::NoSuchMethodError {
                        class_name,
                        method_name: method_name.to_string(),
                        method_descriptor: descriptor.to_string(),
                    },
                ));
            }
        }
    };

    // --- AOT training: record method invocation with receiver type ---
    #[cfg(feature = "experimental-aot")]
    {
        if crate::native::builtins::aot::is_aot_training() {
            let cm = shared.classes.class_manager.read();
            let class_name = cm
                .class_store
                .get(class_id)
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            // For virtual methods, extract receiver type from args[0]
            let receiver_type = if !is_static {
                if let Some(Value::Object(Some(receiver_obj))) = args.first() {
                    let receiver_class_id = shared.mem.heap.class_id_of(*receiver_obj);
                    cm.class_store
                        .get(receiver_class_id)
                        .map(|c| c.name.to_string())
                } else {
                    None
                }
            } else {
                None
            };
            drop(cm);
            crate::native::builtins::aot::aot_record_method_invocation(
                &class_name,
                method_name,
                descriptor,
                receiver_type.as_deref(),
            );
        }
    }

    if !is_native {
        let class_name_for_force = shared
            .classes
            .class_manager
            .read()
            .get_class(declaring_class_id)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        // (`ThreadPoolExecutor.execute` receiver-shape check deleted
        // 2026-08-06 — see `invoke_or_native` above and
        // `force_native_over_real_jdk_bytecode` in `native_override.rs`. This
        // site consults `should_force_registered_native_over_bytecode`, whose
        // receiver-blind `ThreadPoolExecutor.execute` arm went with it, so
        // there is no longer anything here to exempt.)
        //
        // §7 routing. This site's whole purpose is "force the registered native
        // in front of real JDK bytecode", so the pre-existing condition IS
        // `compat_native_wins` and `bytecode_available` is `true`. Evaluated
        // before the registry lookup, exactly as before, so a call that does
        // not force a native still pays no hash.
        let user_loader_declaring = cratonvm_native_builtins::classloader::any_defining_loader_registered(&shared.loader_identity)
            && !crate::runtime::interpreter::constants::is_global_resolution_namespace(&class_name_for_force)
            && (cratonvm_native_builtins::classloader::defining_loader_for(&shared.loader_identity, declaring_class_id.as_u32()).is_some()
                || shared.classes.class_manager.read().get_class(declaring_class_id).map(|c| matches!(c.loader_id, cratonvm_types::ClassLoaderId::UserDefined(_))).unwrap_or(false));
        if !user_loader_declaring
            && !is_static
            && shared.config.compatibility_mode == cratonvm_types::CompatibilityMode::Compatible
        {
            if let Some(callback) = crate::runtime::interpreter::real_http_url_connection_native(
                shared,
                &class_name_for_force,
                method_name,
                descriptor,
                args,
            ) {
                return safe_native_call(shared, thread, callback, args);
            }
        }
        let compat_native_wins = !user_loader_declaring
            && crate::runtime::interpreter::should_force_registered_native_over_bytecode(
                shared,
                &class_name_for_force,
                method_name,
                descriptor,
            );
        if compat_native_wins {
            // `find` -> `find_with_kind`: same 128-bit triple hash, same
            // callback, and the kind that `find` used to discard is what makes
            // §1.3/§1.4 decidable here at all.
            if let Some((callback, native_kind)) = shared.natives.native_methods.find_with_kind(
                &class_name_for_force,
                method_name,
                descriptor,
            ) {
                match resolve_native_dispatch_wave1_in(
                    &shared.natives.native_shadow_sink,
                    DispatchDoor::ClassSharedForce,
                    dispatch_policy(shared),
                    &class_name_for_force,
                    method_name,
                    descriptor,
                    Some((callback, native_kind)),
                    compat_native_wins,
                    true,
                ) {
                    Some(DispatchDecision::Reject(violation)) => {
                        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                    }
                    Some(decision) => {
                        if let Some(callback) = decision.native_callback() {
                            // CAPABILITY GATE, dispatch site 2 of 3 — the
                            // "force the registered native in front of real JDK
                            // bytecode" path. Same placement rule as site 1:
                            // immediately before the call, after routing has
                            // committed to the native.
                            check_native_dispatch_capability(
                                shared,
                                &class_name_for_force,
                                method_name,
                                descriptor,
                            )?;
                            // Under `native_sync_enabled` (default on;
                            // `CRATONVM_NATIVE_SYNC=0` disarms it) this forced
                            // native holds the synchronized method's monitor,
                            // as the interpreter's forced-native intercept
                            // does.
                            let _sync_guard = if is_synchronized
                                && crate::runtime::interpreter::native_sync_enabled(shared)
                            {
                                Some(class_shared_sync_monitor(
                                    shared,
                                    thread,
                                    declaring_class_id,
                                    is_static,
                                    &args_roots,
                                    args,
                                )?)
                            } else {
                                None
                            };
                            return safe_native_call(shared, thread, callback, args);
                        }
                    }
                    // JdkOnly, §7 step 3: real bytecode beats this bridge, so
                    // fall through to the bytecode path below. Unreachable
                    // under `Compatible`, where `compat_native_wins` is true
                    // here by construction.
                    None => {}
                }
            }
        }
    }

    // --- ACC_SYNCHRONIZED (JVMS §2.11.10) ---
    // The monitor is taken by whichever arm below answers the call, not up
    // front. The arms that answer with a native (registry, JNI, override of
    // bytecode, the link error) take it through `class_shared_sync_monitor`
    // immediately before; the bytecode tail leaves it to
    // `interpreter::execute`, which takes it itself and hands it to the frame
    // it pushes. Taking it here as well used to enter every synchronized
    // method reached through this door twice. (The `compat_native_wins` arm
    // above takes it under `native_sync_enabled`, like the interpreter's
    // forced-native intercept — see
    // `interpreter-L3-acc-synchronized-skipped-on-native-dispatch-FIXED-20260925.md`.)

    // §7 routing, hoisted OUT of the `if is_native` arm below — see
    // `bytecode_wins_under_strict` for why it cannot be decided inside it.
    //
    // `is_native` here is NOT the same thing as `method.is_native()`: the
    // `check_override` chain above also sets it for concrete methods whose real
    // bytecode we deliberately shadow. So this site can be about to run a native
    // in front of real bytes — exactly the §1.4 question — and it is the only
    // place that can answer it, because it is the only place holding
    // `declaring_class_id`.
    //
    // The answer is recovered from the SAME class-manager read that was already
    // being taken for `class_name`: no extra lock, no extra hierarchy walk. It
    // is computed only under `JdkOnly`, so a default `--fast-boot` run pays one
    // `is_jdk_only()` bool test and nothing else.
    let (class_name, native_shadows_bytecode, registry_native) = if is_native {
        let policy = dispatch_policy(shared);
        let strict = policy.is_jdk_only();
        let (class_name, native_shadows_bytecode) = {
            let cm = shared.classes.class_manager.read();
            let declaring = cm.get_class(declaring_class_id);
            let class_name = declaring.map(|c| c.name.to_string()).unwrap_or_default();
            let shadows = strict
                && declaring
                    .and_then(|c| c.find_method(method_name, descriptor))
                    .map(|m| !m.is_native() && m.code().is_some())
                    .unwrap_or(false);
            (class_name, shadows)
        };

        // `find` -> `find_with_kind`: identical hash, identical callback, plus
        // the `NativeKind` that decides §1.3 / §1.4. `compat_native_wins` is
        // `true` — the unconditional "the registered native runs here" this
        // site has always encoded — so `Compatible` is bit-for-bit unchanged.
        let registry_native =
            match shared
                .natives
                .native_methods
                .find_with_kind(&class_name, method_name, descriptor)
            {
                Some((callback, native_kind)) => match resolve_native_dispatch_wave1_in(
                    &shared.natives.native_shadow_sink,
                    DispatchDoor::ClassSharedNative,
                    policy,
                    &class_name,
                    method_name,
                    descriptor,
                    Some((callback, native_kind)),
                    true,
                    native_shadows_bytecode,
                ) {
                    Some(DispatchDecision::Reject(violation)) => {
                        return Err(MethodCallFailed::InternalError(VmError::JdkOnly(violation)));
                    }
                    Some(decision) => decision.native_callback(),
                    // JdkOnly, §7 step 3: this "native" is a bridge standing in
                    // front of concrete bytecode, and the bytecode wins.
                    None => None,
                },
                None => None,
            };
        (class_name, native_shadows_bytecode, registry_native)
    } else {
        (String::new(), false, None)
    };

    // §7 step 3, the part the code did not previously implement.
    //
    // The `if is_native` arm below has THREE outcomes — registry native, JNI
    // function pointer, or `UnsatisfiedLinkError`. It has no path to the
    // bytecode. So when §7 step 3 declined a shadowing native above, the old
    // code did not "fall through to the bytecode path exactly as an
    // unregistered native would have", as its comment claimed: it fell through
    // to the *link error*, for a method whose `Code` attribute is sitting right
    // there. Measured 2026-08-04 on JDK 25: `--real-jdk` could not start a
    // single `java.lang.Thread` — `UnsatisfiedLinkError: java/lang/Thread.run()V`,
    // whose real bytecode the class-path image plainly declares — so every
    // `new Thread(…)` and every `ExecutorService` was dead, and any workload
    // that joined on one hung rather than failed.
    //
    // Deciding it here, before the `if`, is what makes the bytecode branch
    // reachable at all.
    //
    // `Compatible` is bit-for-bit unchanged: `native_shadows_bytecode` is
    // `false` unless the policy is `JdkOnly`, so this whole term folds away.
    //
    // Genuinely unimplemented natives still raise. If `is_native` came from
    // `method.is_native()` then `native_shadows_bytecode` is `false` by
    // construction (`!m.is_native()` is one of its conjuncts), so an
    // `ACC_NATIVE` method with nothing behind it takes the error arm exactly as
    // before — including one a host library would have served over JNI, since a
    // `RegisterNatives` target is `ACC_NATIVE` too.
    let is_native = is_native && !(registry_native.is_none() && native_shadows_bytecode);

    let result = if is_native {
        // Every outcome of this arm (registry native, JNI function, missing
        // native) runs under the monitor, as HotSpot's native entry locks
        // before it links. Released by the guard's `Drop` on every exit edge.
        let _sync_guard = if is_synchronized {
            Some(class_shared_sync_monitor(
                shared,
                thread,
                declaring_class_id,
                is_static,
                &args_roots,
                args,
            )?)
        } else {
            None
        };
        let args: &[Value] = args;
        // `tcnative-*.dll` / `netty_tcnative*.dll` may RegisterNatives for Tomcat
        // `org/apache/tomcat/jni/*` or Netty `io/netty/internal/tcnative/*`. Those
        // function pointers are not ABI-compatible with CratonVM's libffi
        // `dispatch_jni_native` on Windows — using `find_jni_native` / dlsym
        // resolution here would bypass the Rust stub registry and fault with
        // 0xC0000005 during Spring Boot startup.
        //
        // `io/netty/internal/tcnative/**` is only refused while the real
        // library has NOT been loaded — i.e. while the Rust stubs above are the
        // only thing behind the package. Once `netty_tcnative_real` is set the
        // stubs have stood down, so refusing here too would leave the package
        // with nothing at all.
        let skip_jni_incompatible_host_lib = shared
            .natives
            .native_methods
            .has_app_workaround_prefix(
                cratonvm_native_api::AppWorkaroundKind::SkipIncompatibleHostJni,
                &class_name,
            )
            || (shared.natives.native_methods.has_app_workaround_prefix(
                cratonvm_native_api::AppWorkaroundKind::SkipIncompatibleHostJniUntilRealLoad,
                &class_name,
            ) && !shared
                .natives
                .netty_tcnative_real
                .load(std::sync::atomic::Ordering::Acquire));

        if let Some(callback) = registry_native {
            // CAPABILITY GATE, dispatch site 3 of 3 — the general
            // `is_native` dispatch every ACC_NATIVE method reaches. Chained
            // rather than sequenced so the arm still evaluates to the
            // `MethodCallResult` the surrounding `let result = if …` expects; a
            // refusal short-circuits and the native never runs.
            //
            // The JNI-function-pointer and bytecode arms below are NOT gated
            // here: a JNI native registered by a host library is not in
            // `classify_native`'s table, so the gate would be a guaranteed
            // `None`. Covering host-registered JNI needs `RegisterNatives`
            // itself to be gated, which is a separate row.
            check_native_dispatch_capability(shared, &class_name, method_name, descriptor).and_then(
                |()| {
                    // Interpreter round i1 wave 29 (lane L1): this door holds
                    // the method, which the funnel cannot name from its
                    // caller's invoke when reflection or a method handle
                    // (`Method.invoke`, `MethodHandle.linkTo*`) brought the
                    // call here; name it for the funnel's method-event report.
                    // One relaxed load, in a build with a JDWP server only.
                    #[cfg(feature = "experimental-debug")]
                    if shared.debug.debugger_gates.method_events_armed() {
                        return crate::runtime::interpreter::with_held_native(
                            shared,
                            declaring_class_id,
                            method_name,
                            descriptor,
                            callback as usize, // Cast: fn address
                            || safe_native_call(shared, thread, callback, args),
                        );
                    }
                    safe_native_call(shared, thread, callback, args)
                },
            )
        } else if let Some(fn_ptr) = if skip_jni_incompatible_host_lib {
            None
        } else {
            // gce e1/j: the JNI phase census (`CRATONVM_DBG_JNI_PHASE=1`);
            // off, each probe below is one `bool` load.
            let census = &shared.natives.jni_phase_census;
            let clock = census.start();
            let found = crate::native::jni::find_jni_native(
                &shared.natives,
                &class_name,
                method_name,
                descriptor,
            );
            census.stop(crate::native::jni::JniPhase::Lookup, clock);
            found
        } {
            // gci i8/dm: remember this dispatch for the per-thread JNI
            // dispatch memo (`CRATONVM_JNI_DISPATCH_MEMO`, default ON since
            // WARNING gci-user-flip-20261013, `=0` kills) when
            // nothing the by-name prefix above decided can differ on a later
            // call with the same class, name and descriptor
            // (`jni_dispatch_memo_fill` re-checks the rest under its own read).
            // gci i10/cr: a receiver-selected call fills too, recording the
            // reference it selected against (the fill admits it only where
            // the walk agrees with the selection).
            if class_id == named_class_id && !prefer_exact_class_native {
                jni_dispatch_memo_fill(
                    shared,
                    thread,
                    class_id,
                    declaring_class_id,
                    is_static,
                    is_synchronized,
                    &class_name,
                    method_name,
                    descriptor,
                    fn_ptr,
                    receiver_selection,
                );
            }
            jni_table_native_call(
                shared,
                thread,
                fn_ptr,
                declaring_class_id,
                is_static,
                &class_name,
                method_name,
                descriptor,
                args,
            )
        } else if let Some(fn_ptr) = if skip_jni_incompatible_host_lib {
            None
        } else {
            let loader_id = jni_loader_id_for_class(shared, declaring_class_id);
            let found = crate::native::jni::resolve_jni_native_in_libraries(
                &shared.natives,
                loader_id,
                &class_name,
                method_name,
                descriptor,
            );
            // Interpreter round i2 wave 63, lane L1: a C JVMTI env's
            // `NativeMethodBind` of the symbol just bound, the address it
            // hands back bound instead (MEASURED: the replacement is called).
            #[cfg(feature = "experimental-debug")]
            let found = found.map(|fn_ptr| {
                if !shared
                    .debug
                    .debugger_gates
                    .native_event_armed(crate::debug::NativeEvent::NativeBind)
                {
                    return fn_ptr;
                }
                let bound = crate::jvmti::native_env::post_native_method_bind(
                    shared,
                    thread.thread_id.0,
                    declaring_class_id,
                    method_name,
                    descriptor,
                    fn_ptr,
                );
                if bound != fn_ptr {
                    crate::native::jni::rebind_jni_native(
                        &shared.natives,
                        &class_name,
                        method_name,
                        descriptor,
                        bound,
                    );
                }
                bound
            });
            found
        } {
            // gci i13/jm (`gci-i10cr-jni-call-path-review-findings` F3): the
            // lazy bind just registered `fn_ptr` in the JNI table without
            // moving the generation (i10/cr), so the next call takes the
            // table-bound arm above and fills the door memo there; fill it
            // now instead, under the same gate, so the bind's own call marks
            // the call-site probe too (`jni_site_miss_note`'s premise: a
            // completed call that reached a JNI native always marks). The
            // fill re-confirms `fn_ptr` against the table and changes
            // nothing about this call.
            if class_id == named_class_id && !prefer_exact_class_native {
                jni_dispatch_memo_fill(
                    shared,
                    thread,
                    class_id,
                    declaring_class_id,
                    is_static,
                    is_synchronized,
                    &class_name,
                    method_name,
                    descriptor,
                    fn_ptr,
                    receiver_selection,
                );
            }
            // The native's JVMTI stack row, as in the `RegisterNatives` arm
            // (wave 42). Safety: as there.
            // Only `experimental-debug` builds have a JVMTI C table to read the
            // row, so release builds record nothing here.
            #[cfg(feature = "experimental-debug")]
            let _jvmti_native_row = unsafe {
                crate::jvmti::native_env::NativeFrameRow::enter_jni(
                    thread as *mut _,
                    declaring_class_id,
                    fn_ptr,
                )
            };
            // The entry's report, as in the `RegisterNatives` arm (wave 28).
            #[cfg(feature = "experimental-debug")]
            let jni_debugged = if shared.debug.debugger_gates.method_events_armed() {
                crate::runtime::interpreter::report_jni_native_method_entry(
                    shared,
                    thread,
                    declaring_class_id,
                    method_name,
                    descriptor,
                    args,
                )
            } else {
                None
            };
            #[cfg(feature = "experimental-debug")]
            let args: &[Value] = match jni_debugged.as_ref() {
                Some((_, Some(fresh))) => fresh.as_slice(),
                _ => args,
            };
            // Auto-resolved via JNI naming convention (dlsym in loaded libraries).
            //
            // SECURITY: same RAII discipline as the RegisterNatives path above —
            // the guard's `Drop` clears the TLS context on the arity-mismatch
            // early return, the normal return, and an unwind out of the native
            // bridge, so a panic cannot strand dangling `*mut JvmThread` /
            // `*mut SharedVm` pointers in TLS.
            //
            // Safety: `thread` is the live, exclusively-borrowed `&mut JvmThread`
            // for this dispatch; it outlives the guard per `set_jni_thread`.
            let _jni_guard = unsafe {
                JniContextGuard::install_for_native(shared, thread as *mut _, declaring_class_id)
            };

            // GC-correctness (vm-jni-roots #2): implicit local-ref frame for the
            // auto-resolved native, identical bracketing to the RegisterNatives
            // arm above (popped on normal/early/panic exit).
            let _jni_local_frame = JniImplicitFrameGuard::enter();

            let env = crate::native::jni::get_jni_env();
            // A static native's second C parameter is the `jclass` of the
            // class that DECLARES it — that is what `GetStaticMethodID(env,
            // cls, ...)`, `GetStaticFieldID`, `FindClass`-free upcalls and
            // `RegisterNatives`-style self-reference are all written against.
            // Passing `0` there made every one of those calls fail: the strict
            // corpus's `callBackTriple` returned its own "GetStaticMethodID
            // returned NULL" sentinel (-1) where HotSpot returned 27.
            //
            // The encoding is the one the rest of this JNI table uses for a
            // `JClass`: `class_id_to_jclass`, exactly what `FindClass` hands
            // back. It must go through that helper and not `as u32`: a bare
            // `ClassId` of 0 IS JNI NULL, and an untagged one is not
            // recognisable as a class by `NewGlobalRef`. JNA's `initIDs` does
            // `NewGlobalRef(cls)` on precisely this argument and reported
            // `UnsatisfiedLinkError: Can't obtain global reference for class
            // com.sun.jna.Native` when the two encodings disagreed.
            let (receiver, call_args) = if is_static {
                (
                    crate::native::jni::class_id_to_jclass(declaring_class_id),
                    args,
                )
            } else {
                let recv = match args.first() {
                    Some(Value::Object(Some(r))) => crate::native::jni::obj_to_jobject(*r),
                    _ => 0u64,
                };
                (recv, if args.is_empty() { args } else { &args[1..] })
            };

            // V2: same descriptor-arity vs call-frame sanity check as the
            // RegisterNatives path above — a malformed descriptor on the
            // auto-resolved (dlsym) symbol must not enter the unsafe dispatch
            // with a mismatched argument frame. The `_jni_guard` `Drop` clears
            // the TLS context on this early return.
            let expected_params = crate::runtime::proxy::count_descriptor_params(descriptor);
            if expected_params != call_args.len() {
                tracing::warn!(
                    method = %format!("{class_name}.{method_name}{descriptor}"),
                    expected_params,
                    got_args = call_args.len(),
                    "JNI native arity mismatch — refusing unsafe dispatch"
                );
                return Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::UnsatisfiedLinkError {
                        message: format!(
                            "{class_name}.{method_name}{descriptor}: descriptor arity {expected_params} != {} args",
                            call_args.len()
                        ),
                    },
                )));
            }

            // §4 census — the auto-resolved (dlsym) half of the same edge; see
            // the `RegisterNatives` arm above for why the count lives on the
            // realm rather than going through `record_invocation`.
            shared
                .natives
                .jni_bridge_invocations
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // gcd d4/k: the JNI census line, as in the `RegisterNatives` arm.
            tracing::debug!(
                target: "cratonvm::jni_census",
                class = %class_name,
                method = %method_name,
                descriptor = %descriptor,
                "JNI native dispatch (resolved)"
            );
            // Counted as a native frame, as in the `RegisterNatives` arm (wave 26).
            let _jni_native_frame = crate::runtime::native_callee_memo::enter_native();
            // Listed while its C code runs, as in the `RegisterNatives` arm.
            let jni_span = enter_jni_native(shared, thread);
            let result_value = {
                // The in-native mark, as in the `RegisterNatives` arm (wave 58).
                #[cfg(feature = "experimental-debug")]
                let _c_in_native = unsafe {
                    crate::jvmti::native_env::CInNativeMark::enter(shared, thread as *mut _)
                };
                unsafe {
                    crate::native::jni::dispatch_jni_native(
                        fn_ptr, env, receiver, call_args, descriptor,
                    )
                }
            };
            leave_jni_native(shared, thread, jni_span);

            // `_jni_guard` clears the TLS context when it drops at end of scope.
            let ret_char = descriptor
                .rfind(')')
                .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
                .unwrap_or(b'V');
            #[cfg_attr(not(feature = "experimental-debug"), allow(unused_mut))]
            let mut out = match jni_pending_exception_after_native(shared, thread) {
                Some(failed) => Err(failed),
                None if ret_char == b'V' => Ok(None),
                None => Ok(Some(result_value)),
            };
            // Wave 28: the native's `MethodExit`, with the thread's ordinary
            // state back first (a suspending request parks here, and the
            // report pins the result or the exception across the park).
            // Wave 45 (lane L1): then, a thread the debugger suspended while
            // the native ran parks here with the native on top
            // (`interpreter::park_if_suspended_at_native_exit`).
            #[cfg(feature = "experimental-debug")]
            {
                let reported = jni_debugged
                    .as_ref()
                    .map(|(native, _)| native)
                    .filter(|native| native.reports_exit(&out));
                let suspension = shared.debug.debugger_gates.suspension_in_force();
                if reported.is_some() || suspension {
                    drop(_jni_native_frame);
                    drop(_jni_local_frame);
                    drop(_jni_guard);
                    if let Some(native) = reported {
                        crate::runtime::interpreter::report_native_method_exit(
                            shared, thread, native, &mut out,
                        );
                    }
                    if suspension {
                        crate::runtime::interpreter::park_if_suspended_at_native_exit(
                            shared,
                            thread,
                            crate::runtime::interpreter::ReturningNative::Named(
                                u64::from(declaring_class_id.as_u32()),
                                crate::debug::jdwp_method_id(method_name, descriptor),
                            ),
                            &mut out,
                        );
                    }
                }
            }
            out
        } else if let Some(link) = prefixed_native_link(
            shared,
            declaring_class_id,
            &class_name,
            method_name,
            descriptor,
            !skip_jni_incompatible_host_lib,
        )? {
            // JVMTI `SetNativeMethodPrefix` (interpreter round i1 wave 44,
            // lane L3): no implementation under this native's own name, but
            // one under the name its agents' prefixes wrap.
            match link {
                PrefixedNativeLink::Registry(callback) => {
                    safe_native_call(shared, thread, callback, args)
                }
                PrefixedNativeLink::Jni(fn_ptr) => {
                    // Bound under this method's own name, so this call and
                    // every later one take the JNI arm above, with its JNI
                    // context, frames and reports.
                    crate::native::jni::register_jni_native(
                        &shared.natives,
                        &class_name,
                        method_name,
                        descriptor,
                        fn_ptr,
                    );
                    invoke_on_class_shared_inner(
                        shared,
                        thread,
                        declaring_class_id,
                        method_name,
                        descriptor,
                        args,
                        OnClassKind::NoRetarget,
                        None,
                    )
                }
            }
        } else {
            let full_sig = format!("{class_name}.{method_name}{descriptor}");
            // Name the mode this VM is actually in. The line used to say
            // "real-JDK mode" unconditionally, which is wrong in the one
            // configuration whose gaps had never been catalogued: a first
            // `--synthetic-jdk` corpus run (2026-08-29, P4-B) produced 53
            // distinct missing natives and every one of them claimed to come
            // from real-JDK mode. A reader's first move on seeing that is to
            // conclude the run was misconfigured.
            let mode = if shared.compatibility_mode().is_jdk_only() {
                "real-jdk mode"
            } else if shared.config.use_synthetic_jdk {
                "synthetic-JDK mode"
            } else {
                "real-JDK mode"
            };
            tracing::warn!(
                method = %full_sig,
                mode = %mode,
                "Missing native method"
            );
            // Record in structured audit log if enabled
            if shared.config.audit_missing_natives {
                // NEW-10: capture the caller frame (the topmost frame
                // on the thread's stack at the moment this missing
                // native was invoked) as a sample call-site so a
                // later reader of the committed baseline can locate
                // the Java code that reached the missing native.
                let sample_call_site = thread.frames.last().map(|frame| {
                    format!(
                        "{}.{}{}",
                        frame.class_name(),
                        frame.method_name(),
                        frame.method_descriptor()
                    )
                });
                shared.record_missing_native(
                    &class_name,
                    method_name,
                    descriptor,
                    sample_call_site,
                );
                // In audit mode, return a default value so execution continues.
                let ret_char = descriptor
                    .rfind(')')
                    .and_then(|i| descriptor.as_bytes().get(i + 1).copied())
                    .unwrap_or(b'V');
                match ret_char {
                    b'V' => Ok(None),
                    b'J' => Ok(Some(Value::Long(0))),
                    b'F' => Ok(Some(Value::Float(0.0))),
                    b'D' => Ok(Some(Value::Double(0.0))),
                    b'L' | b'[' => Ok(Some(Value::Object(None))),
                    _ => Ok(Some(Value::Int(0))),
                }
            } else {
                // In production mode, throw UnsatisfiedLinkError per JVM spec В§5.3.5,
                // spelled the way HotSpot spells it: `'int p.Foo.m(java.lang.Object)'`.
                Err(MethodCallFailed::InternalError(VmError::Runtime(
                    RuntimeError::UnsatisfiedLinkError {
                        message: format!(
                            "'{}'",
                            crate::runtime::resolve::selection::external_method_name(
                                &class_name,
                                method_name,
                                descriptor,
                            )
                        ),
                    },
                )))
            }
        }
    } else {
        // Before executing bytecode, check if we have a native override registered.
        // This handles JDK methods that are Java code but depend on JVM-internal
        // state we haven't set up (e.g. VM.getSavedProperty checks savedProps
        // which requires System.initPhase2 to have run).
        //
        // EXCEPTION — interface DEFAULT methods (declaring class is an
        // interface, instance method with code): natives registered on
        // interface names (`java/util/Collection`, `java/util/List`, …) are
        // bridges for synthetic receivers with no real hierarchy; a real
        // receiver that resolved default-method bytecode must run it. Twin of
        // the guard in `try_stackless_invoke` step 6 (interpreter.rs) — this
        // arm serves `invoke_or_native` / lambda method-ref dispatch /
        // `ctx.invoke_virtual`, so without it `List::stream` applied to a
        // foreign collection (Hibernate `JoinedList` inside `flatMap`)
        // dispatched the Collection bridge and materialised an empty stream
        // while the plain `coll.stream()` call site ran the real bytecode.
        // Deliberate interface-default overrides belong in
        // `force_native_over_real_jdk_bytecode`.
        let override_cb = match resolved_tail_override {
            Some(cb) => {
                let class_name_for_override = {
                    let cm = shared.classes.class_manager.read();
                    cm.get_class(declaring_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_default()
                };
                if crate::jit::helpers::woven_owner_body_wins(
                    shared,
                    declaring_class_id,
                    &class_name_for_override,
                    method_name,
                    descriptor,
                    if is_static {
                        None
                    } else {
                        match args.first() {
                            Some(Value::Object(Some(receiver))) => Some(*receiver),
                            _ => None,
                        }
                    },
                ) {
                    None
                } else {
                    Some(cb)
                }
            }
            None => None,
        };
        // gen r4w4/rooting2 (residual item 2): `args` was last refreshed at the
        // VM-dispatch retarget probe far above. Every step between there and
        // here was read to be a pure lookup (class-manager reads, registry
        // `find_with_kind`, `resolve_native_dispatch_wave1`, the capability
        // gate), so no collection can have run -- but this is the door every
        // bytecode tail and override leaves through, and `execute` roots its
        // arguments from entry, so a stale value here would be PINNED stale.
        // One compare loop over the dispatch pins makes the tail independent
        // of that reading staying true.
        args_roots.refresh(args);
        if let Some(callback) = override_cb {
            let _sync_guard = if is_synchronized {
                Some(class_shared_sync_monitor(
                    shared,
                    thread,
                    declaring_class_id,
                    is_static,
                    &args_roots,
                    args,
                )?)
            } else {
                None
            };
            safe_native_call(shared, thread, callback, args)
        } else {
            // `execute` takes a synchronized callee's monitor itself; this
            // door takes it only when `CRATONVM_NO_EXECUTE_SYNC` turned that
            // off, so the method is entered exactly once either way.
            let _sync_guard =
                if is_synchronized && crate::runtime::interpreter::execute_sync_disabled() {
                    Some(class_shared_sync_monitor(
                        shared,
                        thread,
                        declaring_class_id,
                        is_static,
                        &args_roots,
                        args,
                    )?)
                } else {
                    None
                };
            // THE ORDINARY TAIL, and the one site the native->Java callback
            // memo is allowed to learn from.
            //
            // Reaching here means every override, proxy, retarget and
            // policy arm above declined — so a memo filled from this point
            // cannot be smuggling past one of them, including arms added
            // after this line was written. See
            // `crate::runtime::native_callee_memo`'s "Why a witness, and not
            // a classifier".
            //
            // `!is_native && !is_synchronized` is the tail's own pair of
            // facts about the callee, and the second stays conservative: under
            // `CRATONVM_NO_EXECUTE_SYNC` a synchronized callee's monitor is
            // held by the `_sync_guard` above, which the memo's fast path does
            // not build.
            let tail_receiver_class = if is_static {
                None
            } else {
                match args.first() {
                    Some(Value::Object(Some(r))) => Some(shared.mem.heap.class_id_of(*r)),
                    _ => None,
                }
            };
            crate::runtime::native_callee_memo::note_plain_bytecode_tail(
                declaring_class_id,
                tail_receiver_class,
                method_name,
                descriptor,
                !is_native && !is_synchronized,
                thread.frames.len(),
            );
            // Round 13 wave 11 (lane callcost5): the static half of the same
            // witness (`invoke_static_settling` arms it). A static callee is
            // never retargeted, so `named_class_id` is the owner it armed.
            if is_static {
                crate::runtime::native_callee_memo::note_plain_static_tail_lazy(
                    declaring_class_id,
                    named_class_id,
                    || crate::runtime::native_callee_memo::static_first_arg_key(shared, &args[..]),
                    method_name,
                    descriptor,
                    !is_native && !is_synchronized,
                    thread.frames.len(),
                );
            }
            // gch h1/fr (d9d): `execute` roots its own copy of `args` and drops
            // it once the callee's frame holds them; this door's pins would
            // outlive that and keep a parameter the callee nulled reachable for
            // the whole call. Opt-in, read once per VM.
            if _sync_guard.is_none() && shared.jit.jit_cache.by_name_tail_unpins_args() {
                if args_roots.release_before_rooting_tail(thread) {
                    dbg_dispatch_tally("by-name-tail-unpinned", "", method_name, descriptor);
                }
            }
            // Execute bytecode via interpreter
            crate::runtime::interpreter::execute(
                shared,
                thread,
                declaring_class_id,
                method_name,
                descriptor,
                args,
            )
        }
    };

    result
}

