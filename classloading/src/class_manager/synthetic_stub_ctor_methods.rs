#[allow(unused_imports)]
use super::*;

pub(in crate::class_manager) fn synthetic_stub_ctor_methods(name: &str) -> Vec<ClassFileMethod> {
    let mut out = Vec::new();
    let mk_ctor = |descriptor: &str| ClassFileMethod {
        access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
        name: cratonvm_types::intern_arc("<init>"),
        descriptor: cratonvm_types::intern_arc(descriptor),
        attributes: vec![],
    };
    if name == "java/lang/Object" {
        for (method, descriptor) in [
            ("hashCode", "()I"),
            ("equals", "(Ljava/lang/Object;)Z"),
            ("toString", "()Ljava/lang/String;"),
        ] {
            out.push(ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc(method),
                descriptor: cratonvm_types::intern_arc(descriptor),
                attributes: vec![],
            });
        }
    }
    let is_throwable_like =
        name == "java/lang/Throwable" || name.ends_with("Exception") || name.ends_with("Error");
    if is_throwable_like {
        // Per class, from the table above — NOT a blanket four. This half and
        // `native-builtins::register_throwable_subclass_natives` read the same
        // list on purpose: a declaration here with no registration there is a
        // method that resolves and then has no body. A name the table has never
        // measured (an application `…/DbException`) still gets the historical
        // four — that is what the default arm of `throwable_ctor_descriptors`
        // is for.
        out.extend(throwable_ctor_descriptors(name).iter().map(|d| mk_ctor(d)));
    }
    if name == "java/lang/reflect/InvocationTargetException" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("getCause", "()Ljava/lang/Throwable;"),
            mk("getTargetException", "()Ljava/lang/Throwable;"),
        ]);
    }
    // proxy-real-classfile increment 2 — the synthetic `Proxy$Instance`
    // super of every generated `$ProxyN` (see `native-builtins`
    // `define_or_get_proxy_class` / `proxy_gen::emit_proxy_classfile`) must
    // declare the constructor the generated `<init>` delegates to via
    // `INVOKESPECIAL Proxy$Instance.<init>(InvocationHandler, Class[])V`.
    // The 3-field stub created by `ensure_synthetic_class` previously had an
    // empty method table, so any path that *executes* the generated `<init>`
    // (a JIT call site, or `new`+`invokespecial` rather than the
    // allocation-bypass in `native_proxy_new_instance`) hit a
    // `NoSuchMethodError` resolving the super ctor. The matching NATIVE
    // implementation (which populates slot 0 = handler, slot 1 = interfaces)
    // is registered as `Proxy$Instance.<init>` in
    // `native-builtins::register_reflect_proxy_natives`.
    if name == "java/lang/reflect/Proxy$Instance" {
        out.push(mk_ctor(
            "(Ljava/lang/reflect/InvocationHandler;[Ljava/lang/Class;)V",
        ));
    }
    if name == "java/lang/Class" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("getSimpleName", "()Ljava/lang/String;"),
            mk("getCanonicalName", "()Ljava/lang/String;"),
            mk("getTypeName", "()Ljava/lang/String;"),
            mk("getPackageName", "()Ljava/lang/String;"),
        ]);
    }
    if matches!(
        name,
        "jdk/internal/loader/ClassLoaders$AppClassLoader"
            | "jdk/internal/loader/ClassLoaders$PlatformClassLoader"
            | "jdk/internal/loader/BuiltinClassLoader"
    ) {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getResourceAsStream"),
            descriptor: cratonvm_types::intern_arc("(Ljava/lang/String;)Ljava/io/InputStream;"),
            attributes: vec![],
        });
    }
    if name == "java/net/InetSocketAddress" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(I)V"),
            mk_ctor("(Ljava/lang/String;I)V"),
            mk_ctor("(Ljava/net/InetAddress;I)V"),
            mk("getHostName", "()Ljava/lang/String;"),
            mk("getHostString", "()Ljava/lang/String;"),
            mk("getPort", "()I"),
            mk("getAddress", "()Ljava/net/InetAddress;"),
            mk("isUnresolved", "()Z"),
            mk("toString", "()Ljava/lang/String;"),
            mk("equals", "(Ljava/lang/Object;)Z"),
            mk("hashCode", "()I"),
            mk_static(
                "createUnresolved",
                "(Ljava/lang/String;I)Ljava/net/InetSocketAddress;",
            ),
        ]);
    }
    if name == "javax/net/ServerSocketFactory" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::STATIC
                    | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("getDefault"),
                descriptor: cratonvm_types::intern_arc("()Ljavax/net/ServerSocketFactory;"),
                attributes: vec![],
            },
            mk("createServerSocket", "()Ljava/net/ServerSocket;"),
            mk("createServerSocket", "(I)Ljava/net/ServerSocket;"),
            mk("createServerSocket", "(II)Ljava/net/ServerSocket;"),
            mk(
                "createServerSocket",
                "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
            ),
        ]);
    }
    if name == "java/util/Base64" {
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("getEncoder", "()Ljava/util/Base64$Encoder;"),
            mk_static("getUrlEncoder", "()Ljava/util/Base64$Encoder;"),
            mk_static("getMimeEncoder", "()Ljava/util/Base64$Encoder;"),
            mk_static("getDecoder", "()Ljava/util/Base64$Decoder;"),
            mk_static("getUrlDecoder", "()Ljava/util/Base64$Decoder;"),
            mk_static("getMimeDecoder", "()Ljava/util/Base64$Decoder;"),
        ]);
    }
    if name == "java/util/Base64$Encoder" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("encode", "([B)[B"),
            mk("encodeToString", "([B)Ljava/lang/String;"),
            mk("withoutPadding", "()Ljava/util/Base64$Encoder;"),
        ]);
    }
    if name == "java/util/Base64$Decoder" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("decode", "([B)[B"),
            mk("decode", "(Ljava/lang/String;)[B"),
        ]);
    }
    if name == "java/util/Arrays" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("hashCode"),
            descriptor: cratonvm_types::intern_arc("([B)I"),
            attributes: vec![],
        });
    }
    if name == "java/lang/ProcessBuilder" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/util/List;)V"),
            mk_ctor("([Ljava/lang/String;)V"),
            mk("command", "()Ljava/util/List;"),
            mk("command", "(Ljava/util/List;)Ljava/lang/ProcessBuilder;"),
            mk("environment", "()Ljava/util/Map;"),
            mk("directory", "(Ljava/io/File;)Ljava/lang/ProcessBuilder;"),
            mk("directory", "()Ljava/io/File;"),
            mk("redirectErrorStream", "(Z)Ljava/lang/ProcessBuilder;"),
            mk("start", "()Ljava/lang/Process;"),
        ]);
    }
    if name == "java/lang/Process" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("getOutputStream", "()Ljava/io/OutputStream;"),
            mk("getInputStream", "()Ljava/io/InputStream;"),
            mk("getErrorStream", "()Ljava/io/InputStream;"),
            mk("waitFor", "()I"),
            mk("exitValue", "()I"),
            mk("destroy", "()V"),
            mk("destroyForcibly", "()Ljava/lang/Process;"),
            mk("isAlive", "()Z"),
            mk("toHandle", "()Ljava/lang/ProcessHandle;"),
        ]);
    }
    // A missing JDK module entry must not leave SmallRye unable to resolve
    // these types. Its `Process.<clinit>` declares fields of both interfaces
    // and immediately calls `ProcessHandle.current().info()`.
    if name == "java/lang/ProcessHandle" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("current", "()Ljava/lang/ProcessHandle;"),
            mk("pid", "()J"),
            mk("isAlive", "()Z"),
            mk("children", "()Ljava/util/stream/Stream;"),
            mk("descendants", "()Ljava/util/stream/Stream;"),
            mk("onExit", "()Ljava/util/concurrent/CompletableFuture;"),
            mk("parent", "()Ljava/util/Optional;"),
            mk("supportsNormalTermination", "()Z"),
            mk("destroy", "()Z"),
            mk("destroyForcibly", "()Z"),
            mk("compareTo", "(Ljava/lang/ProcessHandle;)I"),
            mk("info", "()Ljava/lang/ProcessHandle$Info;"),
        ]);
    }
    if name == "java/lang/ProcessHandle$Info" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        // All SIX abstracts `javap 'java.lang.ProcessHandle$Info'` declares on
        // JDK 25 — `commandLine` included. It was the one this list omitted, and
        // the omission was load-bearing rather than cosmetic: a name absent here
        // fails at RESOLUTION, before native dispatch is reached, so
        // `phases_late.rs`'s `commandLine` registration (W7-10 §4) could only
        // ever be reached on a real JDK. `regression-suite/src/RJdkStrict.java`
        // asserts this surface as an exact six-name list, which is the assertion
        // that goes red when a seventh abstract appears on a future image or a
        // row is dropped from here again.
        // docs/feature-designs/class-resolution-and-caching.md
        out.extend([
            mk("command", "()Ljava/util/Optional;"),
            mk("commandLine", "()Ljava/util/Optional;"),
            mk("arguments", "()Ljava/util/Optional;"),
            mk("user", "()Ljava/util/Optional;"),
            mk("startInstant", "()Ljava/util/Optional;"),
            mk("totalCpuDuration", "()Ljava/util/Optional;"),
        ]);
    }
    if name == "java/io/FileDescriptor" {
        out.push(mk_ctor("()V"));
    }
    if name == "java/io/FileOutputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/io/FileDescriptor;)V"),
            mk("write", "(I)V"),
            mk("write", "([B)V"),
            mk("write", "([BII)V"),
            mk("flush", "()V"),
            mk("close", "()V"),
        ]);
    }
    if name == "java/io/FileInputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/io/FileDescriptor;)V"),
            mk("read", "()I"),
            mk("read", "([B)I"),
            mk("read", "([BII)I"),
            mk("skip", "(J)J"),
            mk("available", "()I"),
            mk("close", "()V"),
        ]);
    }
    if name == "java/io/BufferedInputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/io/InputStream;)V"),
            mk_ctor("(Ljava/io/InputStream;I)V"),
            mk("read", "()I"),
            mk("read", "([BII)I"),
            mk("available", "()I"),
            mk("close", "()V"),
        ]);
    }
    if name == "java/io/FilterOutputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/io/OutputStream;)V"),
            mk("write", "(I)V"),
            mk("write", "([B)V"),
            mk("write", "([BII)V"),
            mk("flush", "()V"),
            mk("close", "()V"),
        ]);
    }
    if name == "java/nio/file/attribute/PosixFilePermission" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/util/Collections" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/util/logging/Level" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            ClassFileMethod {
                access_flags: MethodAccessFlags::PROTECTED | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("<init>"),
                descriptor: cratonvm_types::intern_arc("(Ljava/lang/String;I)V"),
                attributes: vec![],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PROTECTED | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("<init>"),
                descriptor: cratonvm_types::intern_arc("(Ljava/lang/String;ILjava/lang/String;)V"),
                attributes: vec![],
            },
            mk("getName", "()Ljava/lang/String;"),
            mk("intValue", "()I"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/logging/LogRecord" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(Ljava/util/logging/Level;Ljava/lang/String;)V"),
            mk("getLevel", "()Ljava/util/logging/Level;"),
            mk("getMessage", "()Ljava/lang/String;"),
            mk("setMessage", "(Ljava/lang/String;)V"),
            mk("getSourceClassName", "()Ljava/lang/String;"),
            mk("setSourceClassName", "(Ljava/lang/String;)V"),
            mk("getSourceMethodName", "()Ljava/lang/String;"),
            mk("setSourceMethodName", "(Ljava/lang/String;)V"),
            mk("getLoggerName", "()Ljava/lang/String;"),
            mk("setLoggerName", "(Ljava/lang/String;)V"),
            mk("getMillis", "()J"),
            mk("setMillis", "(J)V"),
            mk("getParameters", "()[Ljava/lang/Object;"),
            mk("setParameters", "([Ljava/lang/Object;)V"),
            mk("getResourceBundle", "()Ljava/util/ResourceBundle;"),
            mk("setResourceBundle", "(Ljava/util/ResourceBundle;)V"),
            mk("getResourceBundleName", "()Ljava/lang/String;"),
            mk("setResourceBundleName", "(Ljava/lang/String;)V"),
            mk("getSequenceNumber", "()J"),
            mk("setSequenceNumber", "(J)V"),
            mk("getThreadID", "()I"),
            mk("setThreadID", "(I)V"),
            mk("getThrown", "()Ljava/lang/Throwable;"),
            mk("setThrown", "(Ljava/lang/Throwable;)V"),
        ]);
    }
    if name == "java/util/regex/Pattern" {
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("compile", "(Ljava/lang/String;)Ljava/util/regex/Pattern;"),
            mk_static("compile", "(Ljava/lang/String;I)Ljava/util/regex/Pattern;"),
            mk(
                "matcher",
                "(Ljava/lang/CharSequence;)Ljava/util/regex/Matcher;",
            ),
        ]);
    }
    if name == "java/util/regex/Matcher" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("matches", "()Z"),
            mk("find", "()Z"),
            mk("find", "(I)Z"),
            mk("group", "()Ljava/lang/String;"),
            mk("group", "(I)Ljava/lang/String;"),
        ]);
    }
    if name == "javax/management/ObjectInstance" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("getObjectName", "()Ljavax/management/ObjectName;"),
            mk("getClassName", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/lang/Runtime" {
        let mk =
            |method: &str, descriptor: &str, access_flags: MethodAccessFlags| ClassFileMethod {
                access_flags,
                name: cratonvm_types::intern_arc(method),
                descriptor: cratonvm_types::intern_arc(descriptor),
                attributes: vec![],
            };
        out.extend([
            mk(
                "version",
                "()Ljava/lang/Runtime$Version;",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            ),
            mk(
                "addShutdownHook",
                "(Ljava/lang/Thread;)V",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            ),
            mk(
                "removeShutdownHook",
                "(Ljava/lang/Thread;)Z",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            ),
        ]);
    }
    if name == "java/lang/Integer" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getInteger"),
            descriptor: cratonvm_types::intern_arc("(Ljava/lang/String;I)Ljava/lang/Integer;"),
            attributes: vec![],
        });
    }
    if name == "java/lang/Long" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getLong"),
            descriptor: cratonvm_types::intern_arc("(Ljava/lang/String;J)Ljava/lang/Long;"),
            attributes: vec![],
        });
    }
    if name == "java/lang/ref/Cleaner" {
        let mk =
            |method: &str, descriptor: &str, access_flags: MethodAccessFlags| ClassFileMethod {
                access_flags,
                name: cratonvm_types::intern_arc(method),
                descriptor: cratonvm_types::intern_arc(descriptor),
                attributes: vec![],
            };
        out.extend([
            mk(
                "create",
                "()Ljava/lang/ref/Cleaner;",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            ),
            mk(
                "register",
                "(Ljava/lang/Object;Ljava/lang/Runnable;)Ljava/lang/ref/Cleaner$Cleanable;",
                MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            ),
        ]);
    }
    if name == "java/lang/ref/Cleaner$Cleanable" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("clean"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "cratonvm/synthetic/AnonymousObject$2" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("hasMoreElements", "()Z"),
            mk("nextElement", "()Ljava/lang/Object;"),
            mk("hasNext", "()Z"),
            mk("next", "()Ljava/lang/Object;"),
        ]);
    }
    if name == "java/util/Collections$SynchronizedSet"
        || name == "java/util/Collections$SynchronizedCollection"
        || name == "java/util/Collections$SynchronizedMap"
    {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        if name == "java/util/Collections$SynchronizedSet" {
            out.push(mk("<init>", "(Ljava/util/Set;)V"));
            out.push(mk("<init>", "(Ljava/util/Set;Ljava/lang/Object;)V"));
        } else if name == "java/util/Collections$SynchronizedMap" {
            out.push(mk("<init>", "(Ljava/util/Map;)V"));
            out.push(mk("<init>", "(Ljava/util/Map;Ljava/lang/Object;)V"));
        } else {
            out.push(mk("<init>", "(Ljava/util/Collection;)V"));
            out.push(mk("<init>", "(Ljava/util/Collection;Ljava/lang/Object;)V"));
        }
        if name == "java/util/Collections$SynchronizedMap" {
            out.extend([
                mk("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                mk(
                    "put",
                    "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
                ),
                mk("containsKey", "(Ljava/lang/Object;)Z"),
                mk("remove", "(Ljava/lang/Object;)Ljava/lang/Object;"),
                mk("size", "()I"),
                mk("isEmpty", "()Z"),
                mk("entrySet", "()Ljava/util/Set;"),
                mk("keySet", "()Ljava/util/Set;"),
                mk("values", "()Ljava/util/Collection;"),
                mk("clear", "()V"),
                mk(
                    "computeIfAbsent",
                    "(Ljava/lang/Object;Ljava/util/function/Function;)Ljava/lang/Object;",
                ),
            ]);
        } else {
            out.extend([
                mk("add", "(Ljava/lang/Object;)Z"),
                mk("contains", "(Ljava/lang/Object;)Z"),
                mk("remove", "(Ljava/lang/Object;)Z"),
                mk("size", "()I"),
                mk("isEmpty", "()Z"),
                mk("iterator", "()Ljava/util/Iterator;"),
                mk("toArray", "()[Ljava/lang/Object;"),
                // The rest of the `Collection` contract. A synchronized wrapper
                // used to reach this VM only via
                // `Collections.synchronizedCollection(…)`; since 2026-08-13 it
                // is also what `Hashtable`/`Properties` hand back for
                // `keySet()`/`entrySet()`/`values()`, so the full read surface
                // is exercised. Undeclared here, those calls fall through to an
                // interface-level native that reads the WRAPPER as the
                // collection and reports it empty. Bodies:
                // `util_concurrent_ext::sync_collection_delegate`.
                mk("clear", "()V"),
                mk("toString", "()Ljava/lang/String;"),
                mk("stream", "()Ljava/util/stream/Stream;"),
                mk("spliterator", "()Ljava/util/Spliterator;"),
                mk("forEach", "(Ljava/util/function/Consumer;)V"),
                mk("containsAll", "(Ljava/util/Collection;)Z"),
                mk("addAll", "(Ljava/util/Collection;)Z"),
                mk("removeAll", "(Ljava/util/Collection;)Z"),
                mk("retainAll", "(Ljava/util/Collection;)Z"),
                mk("removeIf", "(Ljava/util/function/Predicate;)Z"),
                mk("toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
                mk(
                    "toArray",
                    "(Ljava/util/function/IntFunction;)[Ljava/lang/Object;",
                ),
            ]);
            // Only the Set wrapper overrides these in the JDK — see the
            // registration site for why the Collection wrapper must not.
            if name == "java/util/Collections$SynchronizedSet" {
                out.extend([mk("hashCode", "()I"), mk("equals", "(Ljava/lang/Object;)Z")]);
            }
        }
    }
    if matches!(
        name,
        "java/util/Collections$EmptyIterator" | "java/util/Collections$EmptyListIterator"
    ) {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("hasNext", "()Z"),
            mk("next", "()Ljava/lang/Object;"),
            mk("remove", "()V"),
        ]);
        if name == "java/util/Collections$EmptyListIterator" {
            out.extend([
                mk("hasPrevious", "()Z"),
                mk("previous", "()Ljava/lang/Object;"),
                mk("nextIndex", "()I"),
                mk("previousIndex", "()I"),
            ]);
        }
    }
    if name == "java/util/Collections$EmptyEnumeration" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("hasMoreElements", "()Z"),
            mk("nextElement", "()Ljava/lang/Object;"),
        ]);
    }
    if name == "java/util/function/Function" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("identity"),
            descriptor: cratonvm_types::intern_arc("()Ljava/util/function/Function;"),
            attributes: vec![],
        });
    }
    if name == "java/util/function/UnaryOperator" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("identity"),
            descriptor: cratonvm_types::intern_arc("()Ljava/util/function/UnaryOperator;"),
            attributes: vec![],
        });
    }
    if name == "java/util/function/Function$Identity" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("apply", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            mk(
                "andThen",
                "(Ljava/util/function/Function;)Ljava/util/function/Function;",
            ),
            mk(
                "compose",
                "(Ljava/util/function/Function;)Ljava/util/function/Function;",
            ),
        ]);
    }
    if name == "java/time/Instant" {
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("now", "()Ljava/time/Instant;"),
            mk_static("ofEpochSecond", "(J)Ljava/time/Instant;"),
            mk_static("ofEpochSecond", "(JJ)Ljava/time/Instant;"),
            mk_static("ofEpochMilli", "(J)Ljava/time/Instant;"),
            mk("getEpochSecond", "()J"),
            mk("getNano", "()I"),
            mk("toEpochMilli", "()J"),
            mk("plusSeconds", "(J)Ljava/time/Instant;"),
            mk("minusSeconds", "(J)Ljava/time/Instant;"),
            mk("plusMillis", "(J)Ljava/time/Instant;"),
            mk("plusNanos", "(J)Ljava/time/Instant;"),
            mk("isBefore", "(Ljava/time/Instant;)Z"),
            mk("isAfter", "(Ljava/time/Instant;)Z"),
            mk("toString", "()Ljava/lang/String;"),
            mk("equals", "(Ljava/lang/Object;)Z"),
            mk("hashCode", "()I"),
        ]);
    }
    if name == "java/lang/Runtime$Version" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([mk("feature", "()I"), mk("build", "()Ljava/util/Optional;")]);    }
    if name == "java/lang/Enum" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/lang/String;I)V"),
            mk("name", "()Ljava/lang/String;"),
            mk("ordinal", "()I"),
            mk("toString", "()Ljava/lang/String;"),
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::STATIC
                    | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("valueOf"),
                descriptor: cratonvm_types::intern_arc(
                    "(Ljava/lang/Class;Ljava/lang/String;)Ljava/lang/Enum;",
                ),
                attributes: vec![],
            },
        ]);
    }
    if name == "java/lang/StackWalker$Option" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/lang/StackWalker" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("getInstance", "()Ljava/lang/StackWalker;"),
            mk_static(
                "getInstance",
                "(Ljava/lang/StackWalker$Option;)Ljava/lang/StackWalker;",
            ),
            mk_static("getInstance", "(Ljava/util/Set;)Ljava/lang/StackWalker;"),
            mk_static("getInstance", "(Ljava/util/Set;I)Ljava/lang/StackWalker;"),
            mk("walk", "(Ljava/util/function/Function;)Ljava/lang/Object;"),
            mk("forEach", "(Ljava/util/function/Consumer;)V"),
            mk("getCallerClass", "()Ljava/lang/Class;"),
        ]);
    }
    if name == "java/lang/StackWalker$StackFrame" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("getClassName", "()Ljava/lang/String;"),
            mk("getMethodName", "()Ljava/lang/String;"),
            mk("getFileName", "()Ljava/lang/String;"),
            mk("getLineNumber", "()I"),
            mk("getByteCodeIndex", "()I"),
            mk("getDeclaringClass", "()Ljava/lang/Class;"),
            mk("getMethodType", "()Ljava/lang/invoke/MethodType;"),
            mk("isNativeMethod", "()Z"),
            mk("toStackTraceElement", "()Ljava/lang/StackTraceElement;"),
        ]);
    }
    if name == "java/util/concurrent/locks/ReentrantLock" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("()V"),
            mk_ctor("(Z)V"),
            mk("lock", "()V"),
            mk("lockInterruptibly", "()V"),
            mk("unlock", "()V"),
            mk("tryLock", "()Z"),
            mk("tryLock", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("isLocked", "()Z"),
            mk("isHeldByCurrentThread", "()Z"),
            mk("getHoldCount", "()I"),
            mk("isFair", "()Z"),
            mk("newCondition", "()Ljava/util/concurrent/locks/Condition;"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/concurrent/locks/Lock" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("lock", "()V"),
            mk("unlock", "()V"),
            mk("tryLock", "()Z"),
            mk("newCondition", "()Ljava/util/concurrent/locks/Condition;"),
        ]);
    }
    if name == "java/util/concurrent/locks/Condition" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("await", "()V"),
            mk("awaitUninterruptibly", "()V"),
            mk("await", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("awaitNanos", "(J)J"),
            mk("awaitUntil", "(Ljava/util/Date;)Z"),
            mk("signal", "()V"),
            mk("signalAll", "()V"),
        ]);
    }
    if name == "java/util/concurrent/LinkedBlockingDeque" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("()V"),
            mk("offer", "(Ljava/lang/Object;)Z"),
            mk("add", "(Ljava/lang/Object;)Z"),
            mk("offerFirst", "(Ljava/lang/Object;)Z"),
            mk("offerLast", "(Ljava/lang/Object;)Z"),
            mk("poll", "()Ljava/lang/Object;"),
            mk("pollFirst", "()Ljava/lang/Object;"),
            mk("pollLast", "()Ljava/lang/Object;"),
            mk("peek", "()Ljava/lang/Object;"),
            mk("peekFirst", "()Ljava/lang/Object;"),
            mk("peekLast", "()Ljava/lang/Object;"),
            mk("size", "()I"),
            mk("isEmpty", "()Z"),
            mk("iterator", "()Ljava/util/Iterator;"),
        ]);
    }
    if name == "java/lang/management/ManagementFactory" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getPlatformMBeanServer"),
            descriptor: cratonvm_types::intern_arc("()Ljavax/management/MBeanServer;"),
            attributes: vec![],
        });
    }
    if name == "java/io/File" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if matches!(name, "sun/misc/Unsafe" | "jdk/internal/misc/Unsafe") {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
    }
    if name == "java/nio/ByteBuffer" {
        out.extend([
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::STATIC
                    | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("allocate"),
                descriptor: cratonvm_types::intern_arc("(I)Ljava/nio/ByteBuffer;"),
                attributes: vec![],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC
                    | MethodAccessFlags::STATIC
                    | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("allocateDirect"),
                descriptor: cratonvm_types::intern_arc("(I)Ljava/nio/ByteBuffer;"),
                attributes: vec![],
            },
        ]);
    }
    if name == "java/nio/charset/CharsetDecoder" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("charset", "()Ljava/nio/charset/Charset;"),
            mk("averageCharsPerByte", "()F"),
            mk("maxCharsPerByte", "()F"),
            mk(
                "onMalformedInput",
                "(Ljava/nio/charset/CodingErrorAction;)Ljava/nio/charset/CharsetDecoder;",
            ),
            mk(
                "onUnmappableCharacter",
                "(Ljava/nio/charset/CodingErrorAction;)Ljava/nio/charset/CharsetDecoder;",
            ),
            mk(
                "replaceWith",
                "(Ljava/lang/String;)Ljava/nio/charset/CharsetDecoder;",
            ),
            mk("replacement", "()Ljava/lang/String;"),
            mk(
                "decode",
                "(Ljava/nio/ByteBuffer;Ljava/nio/CharBuffer;Z)Ljava/nio/charset/CoderResult;",
            ),
            mk(
                "flush",
                "(Ljava/nio/CharBuffer;)Ljava/nio/charset/CoderResult;",
            ),
            mk("reset", "()Ljava/nio/charset/CharsetDecoder;"),
        ]);
    }
    if name == "java/nio/charset/CodingErrorAction" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::STATIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<clinit>"),
            descriptor: cratonvm_types::intern_arc("()V"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("toString"),
            descriptor: cratonvm_types::intern_arc("()Ljava/lang/String;"),
            attributes: vec![],
        });
    }
    if name == "java/lang/ThreadLocal" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PROTECTED | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("initialValue"),
            descriptor: cratonvm_types::intern_arc("()Ljava/lang/Object;"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("withInitial"),
            descriptor: cratonvm_types::intern_arc(
                "(Ljava/util/function/Supplier;)Ljava/lang/ThreadLocal;",
            ),
            attributes: vec![],
        });
    }
    if name == "java/net/URLConnection" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("setContentHandlerFactory"),
            descriptor: cratonvm_types::intern_arc("(Ljava/net/ContentHandlerFactory;)V"),
            attributes: vec![],
        });
    }
    if matches!(
        name,
        "java/security/PermissionCollection" | "java/security/Permissions"
    ) {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("add", "(Ljava/security/Permission;)V"),
            mk("setReadOnly", "()V"),
            mk("isReadOnly", "()Z"),
        ]);
    }
    if matches!(
        name,
        "java/security/Permission"
            | "java/security/BasicPermission"
            | "java/lang/RuntimePermission"
            | "java/util/PropertyPermission"
            | "java/util/logging/LoggingPermission"
    ) {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.push(mk("getName", "()Ljava/lang/String;"));    }
    if name == "java/security/Permission" {
        out.push(mk_ctor("(Ljava/lang/String;)V"));
    }
    if matches!(
        name,
        "java/security/BasicPermission"
            | "java/lang/RuntimePermission"
            | "java/util/PropertyPermission"
            | "java/util/logging/LoggingPermission"
    ) {
        out.extend([
            mk_ctor("(Ljava/lang/String;)V"),
            mk_ctor("(Ljava/lang/String;Ljava/lang/String;)V"),
        ]);
    }
    if name == "java/util/concurrent/atomic/AtomicBoolean" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("()V"),
            mk_ctor("(Z)V"),
            mk("get", "()Z"),
            mk("set", "(Z)V"),
            mk("lazySet", "(Z)V"),
            mk("compareAndSet", "(ZZ)Z"),
            mk("getAndSet", "(Z)Z"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/lang/String" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static(
                "join",
                "(Ljava/lang/CharSequence;[Ljava/lang/CharSequence;)Ljava/lang/String;",
            ),
            mk_static(
                "join",
                "(Ljava/lang/CharSequence;Ljava/lang/Iterable;)Ljava/lang/String;",
            ),
            mk("replace", "(CC)Ljava/lang/String;"),
            mk("toUpperCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
            mk("toLowerCase", "(Ljava/util/Locale;)Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/ArrayList" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("listIterator", "()Ljava/util/ListIterator;"),
            mk("listIterator", "(I)Ljava/util/ListIterator;"),
        ]);
    }
    if name == "java/util/ArrayList$ListItr" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("hasNext", "()Z"),
            mk("next", "()Ljava/lang/Object;"),
            mk("hasPrevious", "()Z"),
            mk("previous", "()Ljava/lang/Object;"),
            mk("nextIndex", "()I"),
            mk("previousIndex", "()I"),
            mk("remove", "()V"),
            mk("set", "(Ljava/lang/Object;)V"),
            mk("add", "(Ljava/lang/Object;)V"),
        ]);
    }
    if name == "java/lang/System" {
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("console", "()Ljava/io/Console;"),
            mk_static("setIn", "(Ljava/io/InputStream;)V"),
            mk_static("setOut", "(Ljava/io/PrintStream;)V"),
            mk_static("setErr", "(Ljava/io/PrintStream;)V"),
        ]);
    }
    if name == "java/io/PrintStream" {
        out.extend([
            mk_ctor("(Ljava/io/OutputStream;)V"),
            mk_ctor("(Ljava/io/OutputStream;Z)V"),
        ]);
    }
    if name == "java/io/OutputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("write", "(I)V"),
            mk("write", "([B)V"),
            mk("write", "([BII)V"),
            mk("flush", "()V"),
            mk("close", "()V"),
        ]);
    }
    if matches!(
        name,
        "java/sql/Date" | "java/sql/Time" | "java/sql/Timestamp"
    ) {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(J)V"),
            mk("getTime", "()J"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/io/InputStreamReader" {
        out.extend([
            mk_ctor("(Ljava/io/InputStream;)V"),
            mk_ctor("(Ljava/io/InputStream;Ljava/nio/charset/Charset;)V"),
            mk_ctor("(Ljava/io/InputStream;Ljava/lang/String;)V"),
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("read"),
                descriptor: cratonvm_types::intern_arc("([CII)I"),
                attributes: vec![],
            },
        ]);
    }
    if name == "java/io/OutputStreamWriter" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/io/OutputStream;)V"),
            mk_ctor("(Ljava/io/OutputStream;Ljava/nio/charset/Charset;)V"),
            mk_ctor("(Ljava/io/OutputStream;Ljava/lang/String;)V"),
            mk("write", "(I)V"),
            mk("write", "(Ljava/lang/String;)V"),
            mk("write", "(Ljava/lang/String;II)V"),
            mk("flush", "()V"),
            mk("close", "()V"),
        ]);
    }
    if name == "java/io/StringReader" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/lang/String;)V"),
            mk("read", "()I"),
            mk("read", "([CII)I"),
            mk("ready", "()Z"),
            mk("close", "()V"),
            mk("skip", "(J)J"),
            mk("reset", "()V"),
            mk("markSupported", "()Z"),
        ]);
    }
    if name == "java/io/ByteArrayOutputStream" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("()V"),
            mk_ctor("(I)V"),
            mk("write", "(I)V"),
            mk("write", "([B)V"),
            mk("write", "([BII)V"),
            mk("toByteArray", "()[B"),
            mk("size", "()I"),
            mk("reset", "()V"),
            mk("toString", "()Ljava/lang/String;"),
            mk("toString", "(Ljava/lang/String;)Ljava/lang/String;"),
            mk("toString", "(Ljava/nio/charset/Charset;)Ljava/lang/String;"),
            mk("close", "()V"),
            mk("flush", "()V"),
        ]);
    }
    if name == "java/util/EnumSet" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static("noneOf", "(Ljava/lang/Class;)Ljava/util/EnumSet;"),
            mk_static("allOf", "(Ljava/lang/Class;)Ljava/util/EnumSet;"),
            mk_static("of", "(Ljava/lang/Enum;)Ljava/util/EnumSet;"),
            mk_static(
                "of",
                "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
            ),
            mk_static(
                "of",
                "(Ljava/lang/Enum;Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
            ),
            mk_static(
                "of",
                "(Ljava/lang/Enum;[Ljava/lang/Enum;)Ljava/util/EnumSet;",
            ),
            mk_static(
                "range",
                "(Ljava/lang/Enum;Ljava/lang/Enum;)Ljava/util/EnumSet;",
            ),
            mk_static("copyOf", "(Ljava/util/Collection;)Ljava/util/EnumSet;"),
            mk("add", "(Ljava/lang/Object;)Z"),
            mk("remove", "(Ljava/lang/Object;)Z"),
            mk("contains", "(Ljava/lang/Object;)Z"),
            mk("size", "()I"),
            mk("isEmpty", "()Z"),
            mk("clear", "()V"),
            mk("iterator", "()Ljava/util/Iterator;"),
            mk("toArray", "()[Ljava/lang/Object;"),
            mk("toArray", "([Ljava/lang/Object;)[Ljava/lang/Object;"),
            mk("clone", "()Ljava/lang/Object;"),
        ]);
    }
    if name == "java/util/EnumMap" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(Ljava/lang/Class;)V"),
            mk(
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            mk("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            mk("remove", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            mk("containsKey", "(Ljava/lang/Object;)Z"),
            mk("containsValue", "(Ljava/lang/Object;)Z"),
            mk("size", "()I"),
            mk("isEmpty", "()Z"),
            mk("clear", "()V"),
            mk("keySet", "()Ljava/util/Set;"),
            mk("values", "()Ljava/util/Collection;"),
            mk("entrySet", "()Ljava/util/Set;"),
            mk("clone", "()Ljava/lang/Object;"),
            mk("equals", "(Ljava/lang/Object;)Z"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/StringJoiner" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/lang/CharSequence;)V"),
            mk_ctor("(Ljava/lang/CharSequence;Ljava/lang/CharSequence;Ljava/lang/CharSequence;)V"),
            mk("add", "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;"),
            mk("toString", "()Ljava/lang/String;"),
            mk("length", "()I"),
            mk(
                "merge",
                "(Ljava/util/StringJoiner;)Ljava/util/StringJoiner;",
            ),
            mk(
                "setEmptyValue",
                "(Ljava/lang/CharSequence;)Ljava/util/StringJoiner;",
            ),
        ]);
    }
    if name == "java/math/BigDecimal" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/lang/String;)V"),
            mk_ctor("(D)V"),
            mk_ctor("(I)V"),
            mk_ctor("(J)V"),
            mk_static("valueOf", "(J)Ljava/math/BigDecimal;"),
            mk_static("valueOf", "(D)Ljava/math/BigDecimal;"),
            mk("add", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            mk("subtract", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            mk("multiply", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            mk("divide", "(Ljava/math/BigDecimal;)Ljava/math/BigDecimal;"),
            mk("divide", "(Ljava/math/BigDecimal;II)Ljava/math/BigDecimal;"),
            mk("compareTo", "(Ljava/math/BigDecimal;)I"),
            mk("equals", "(Ljava/lang/Object;)Z"),
            mk("toString", "()Ljava/lang/String;"),
            mk("toPlainString", "()Ljava/lang/String;"),
            mk("intValue", "()I"),
            mk("longValue", "()J"),
            mk("doubleValue", "()D"),
            mk("floatValue", "()F"),
            mk("toBigInteger", "()Ljava/math/BigInteger;"),
            mk("scale", "()I"),
            mk("precision", "()I"),
            mk("negate", "()Ljava/math/BigDecimal;"),
            mk("abs", "()Ljava/math/BigDecimal;"),
            mk("signum", "()I"),
            mk("setScale", "(I)Ljava/math/BigDecimal;"),
            mk("setScale", "(II)Ljava/math/BigDecimal;"),
            mk("stripTrailingZeros", "()Ljava/math/BigDecimal;"),
            mk("hashCode", "()I"),
        ]);
    }
    if name == "java/io/BufferedReader" {
        out.extend([
            mk_ctor("(Ljava/io/Reader;)V"),
            mk_ctor("(Ljava/io/Reader;I)V"),
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("read"),
                descriptor: cratonvm_types::intern_arc("()I"),
                attributes: vec![],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("read"),
                descriptor: cratonvm_types::intern_arc("([CII)I"),
                attributes: vec![],
            },
            ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("readLine"),
                descriptor: cratonvm_types::intern_arc("()Ljava/lang/String;"),
                attributes: vec![],
            },
        ]);
    }
    if name == "java/util/jar/Attributes" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("()V"),
            mk_ctor("(I)V"),
            mk(
                "putValue",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            ),
            mk("getValue", "(Ljava/lang/String;)Ljava/lang/String;"),
            mk("get", "(Ljava/lang/Object;)Ljava/lang/Object;"),
            mk(
                "put",
                "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
            ),
            mk("size", "()I"),
            mk("entrySet", "()Ljava/util/Set;"),
        ]);
    }
    if name == "java/util/jar/Attributes$Name" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_ctor("(Ljava/lang/String;)V"),
            mk("toString", "()Ljava/lang/String;"),
            mk("equals", "(Ljava/lang/Object;)Z"),
            mk("hashCode", "()I"),
        ]);
    }
    if name == "java/lang/Thread" {
        for desc in [
            "()V",
            "(Ljava/lang/Runnable;)V",
            "(Ljava/lang/Runnable;Ljava/lang/String;)V",
            "(Ljava/lang/String;)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;J)V",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;JZ)V",
        ] {
            out.push(ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc("<init>"),
                descriptor: cratonvm_types::intern_arc(desc),
                attributes: vec![],
            });
        }
        for (method, desc) in [
            ("start0", "()V"),
            ("start", "()V"),
            ("run", "()V"),
            ("join", "()V"),
            ("join", "(J)V"),
            ("join", "(JI)V"),
            ("checkAccess", "()V"),
            ("getName", "()Ljava/lang/String;"),
            ("setName", "(Ljava/lang/String;)V"),
            (
                "setUncaughtExceptionHandler",
                "(Ljava/lang/Thread$UncaughtExceptionHandler;)V",
            ),
            (
                "getUncaughtExceptionHandler",
                "()Ljava/lang/Thread$UncaughtExceptionHandler;",
            ),
        ] {
            out.push(ClassFileMethod {
                access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
                name: cratonvm_types::intern_arc(method),
                descriptor: cratonvm_types::intern_arc(desc),
                attributes: vec![],
            });
        }
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getThreadGroup"),
            descriptor: cratonvm_types::intern_arc("()Ljava/lang/ThreadGroup;"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("getPriority"),
            descriptor: cratonvm_types::intern_arc("()I"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("isDaemon"),
            descriptor: cratonvm_types::intern_arc("()Z"),
            attributes: vec![],
        });
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("setDaemon"),
            descriptor: cratonvm_types::intern_arc("(Z)V"),
            attributes: vec![],
        });
    }
    if name == "java/lang/Thread$FieldHolder" {
        out.push(ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc("<init>"),
            descriptor: cratonvm_types::intern_arc(
                "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;JIZ)V",
            ),
            attributes: vec![],
        });
    }
    if name == "java/lang/ThreadGroup" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(Ljava/lang/String;)V"),
            mk("<init>", "(Ljava/lang/ThreadGroup;Ljava/lang/String;)V"),
            mk("getName", "()Ljava/lang/String;"),
            mk("getParent", "()Ljava/lang/ThreadGroup;"),
            mk("isDaemon", "()Z"),
            mk("setDaemon", "(Z)V"),
            mk("toString", "()Ljava/lang/String;"),
            mk("activeCount", "()I"),
            mk("enumerate", "([Ljava/lang/Thread;)I"),
            mk("getMaxPriority", "()I"),
            mk("setMaxPriority", "(I)V"),
            mk("interrupt", "()V"),
            mk("destroy", "()V"),
            mk("list", "()V"),
            mk("activeGroupCount", "()I"),
            mk("parentOf", "(Ljava/lang/ThreadGroup;)Z"),
        ]);
    }
    if name == "java/util/concurrent/TimeUnit" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("sleep", "(J)V"),
            mk("toMillis", "(J)J"),
            mk("toNanos", "(J)J"),
            mk("toMicros", "(J)J"),
            mk("toSeconds", "(J)J"),
            mk("toMinutes", "(J)J"),
            mk("toHours", "(J)J"),
            mk("toDays", "(J)J"),
            mk("convert", "(JLjava/util/concurrent/TimeUnit;)J"),
        ]);
    }
    if name == "java/util/concurrent/CountDownLatch" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("countDown", "()V"),
            mk("await", "()V"),
            mk("await", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("getCount", "()J"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/concurrent/Semaphore" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("<init>", "(IZ)V"),
            mk("acquire", "()V"),
            mk("acquire", "(I)V"),
            mk("acquireUninterruptibly", "()V"),
            mk("release", "()V"),
            mk("release", "(I)V"),
            mk("tryAcquire", "()Z"),
            mk("tryAcquire", "(I)Z"),
            mk("tryAcquire", "(JLjava/util/concurrent/TimeUnit;)Z"),
            mk("availablePermits", "()I"),
            mk("drainPermits", "()I"),
            mk("isFair", "()Z"),
            mk("toString", "()Ljava/lang/String;"),
        ]);
    }
    if name == "java/util/concurrent/Executors" {
        let mk_static = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC
                | MethodAccessFlags::STATIC
                | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk_static(
                "newCachedThreadPool",
                "()Ljava/util/concurrent/ExecutorService;",
            ),
            mk_static(
                "newCachedThreadPool",
                "(Ljava/util/concurrent/ThreadFactory;)Ljava/util/concurrent/ExecutorService;",
            ),
        ]);
    }
    if name == "java/util/concurrent/ScheduledThreadPoolExecutor" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("<init>", "(I)V"),
            mk("<init>", "(ILjava/util/concurrent/ThreadFactory;)V"),
        ]);
    }
    if name == "java/util/concurrent/CopyOnWriteArrayList" {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([
            mk("contains", "(Ljava/lang/Object;)Z"),
            mk("addIfAbsent", "(Ljava/lang/Object;)Z"),
        ]);
    }
    // CratonVM's snapshot-backed collection iterators (`HashMap$KeyItr` …)
    // declare the `Iterator` query methods they implement via natives, so
    // reflective callers can discover them. `Class.getMethod("hasNext")` returned
    // a NoSuchMethodException without these entries, breaking Spring's
    // `ClassUtils.getInterfaceMethodIfPossible` late-binding (ClassUtilsTests).
    // The `Iterator` interface itself is wired up in `ensure_synthetic_class`.
    // `remove()` is intentionally NOT declared here: not every snapshot iterator
    // registers a `remove` native (e.g. `TreeMap$KeyItr`), and `Iterator.remove`
    // is already routed by the force-native dispatcher, so a bare NATIVE entry
    // with no backing impl would only risk turning the default UOE into a link
    // error.
    if ClassManager::is_synthetic_collection_iterator(name) {
        let mk = |method: &str, descriptor: &str| ClassFileMethod {
            access_flags: MethodAccessFlags::PUBLIC | MethodAccessFlags::NATIVE,
            name: cratonvm_types::intern_arc(method),
            descriptor: cratonvm_types::intern_arc(descriptor),
            attributes: vec![],
        };
        out.extend([mk("hasNext", "()Z"), mk("next", "()Ljava/lang/Object;")]);    }
    out}

/// The `-XX:MaxMetaspaceSize` predicate (gc-common w6-f): would charging
/// `additional` bytes on top of `charged` exceed `limit`? Never with no limit;
/// saturating, so an absurd class-file length cannot wrap into "fits".
pub fn metaspace_limit_exceeded(limit: Option<u64>, charged: u64, additional: u64) -> bool {
    match limit {
        Some(max) => charged.saturating_add(additional) > max,
        None => false,
    }
}

/// gci i33/ms (`-XX:MaxMetaspaceSize` is checked against HotSpot's committed
/// metaspace): per-arena sums of the VM's HotSpot metaspace model
/// (`vm_init::metaspace_limit_would_exceed`). The VM fills and reads it; this
/// crate only stores it ([`ClassManager::metaspace_arena_tally`]). Class ids
/// are never reused, so a tally that has summed the slots
/// `[0, scanned_slots)` stays right while classes are only added; when the
/// store's live count no longer matches `classes` (a class was unloaded) the
/// VM sums every live class again.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetaspaceArenaTally {
    /// The class-store slots summed so far (`[0, scanned_slots)`).
    pub scanned_slots: usize,
    /// The live classes summed.
    pub classes: usize,
    /// The boot loader's arenas: `[non-class, class]` bytes.
    pub boot: [u64; 2],
    /// Every other loader's arenas by its native id: `[non-class, class]` bytes.
    pub loaders: FxHashMap<u32, [u64; 2]>,
    /// The sum of `loaders`: `[non-class, class]` bytes.
    pub others_used: [u64; 2],
    /// The sum of the chunk footprints of `loaders` (the VM's model of the
    /// chunks each arena was handed): `[non-class, class]` bytes.
    pub others_footprint: [u64; 2],
}

/// gci i12/ul F4: does a live class with this name and origin count in
/// `ClassLoadingMXBean`'s loaded / unloaded figures? HotSpot counts
/// InstanceKlasses: not an array class (`[...`), and no compatibility stub
/// (nothing in HotSpot corresponds to one). Pure.
pub fn counts_as_loaded_instance_class(name: &str, origin: &ClassOrigin) -> bool {
    !name.starts_with('[') && !origin.is_compatibility_stub()
}

impl std::fmt::Debug for ClassManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassManager")
            .field("loaded_count", &self.loaded_count())
            .field("bootstrap", &self.bootstrap)
            .field("extension", &self.extension)
            .field("application", &self.application)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Field layout computation
// ---------------------------------------------------------------------------

/// Compute `(first_field_index, num_total_fields)` for a class being loaded.
///
/// Instance fields (non-static) from the superclass occupy slots `0..N`.
/// This class's own instance fields start at `N`. Static fields don't need
/// object slots; only instance fields are counted.
pub(in crate::class_manager) fn compute_field_layout(
    fields: &[cratonvm_reader::field::ClassFileField],
    superclass_id: Option<ClassId>,
    store: &ClassStore,
) -> (usize, usize) {
    let parent_total = match superclass_id {
        Some(super_id) => store.get(super_id).map_or(0, |sc| sc.num_total_fields),
        None => 0,
    };

    let own_instance_fields = fields
        .iter()
        .filter(|f| !f.access_flags.contains(FieldAccessFlags::STATIC))
        .count();

    let first_field_index = parent_total;
    let num_total_fields = parent_total + own_instance_fields;

    (first_field_index, num_total_fields)
}

pub(in crate::class_manager) fn field_trace_enabled() -> bool {
    // JIT round 23 wave 2 (proxy-define page): asked on every define, and an
    // environment read was ~1 % of the define arm's CPU. A `MemoSlot` (1 off,
    // 2 on) is re-derived when a test installs an override.
    static SLOT: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    match SLOT.load() {
        1 => false,
        2 => true,
        _ => {
            let on = cratonvm_types::flags::runtime_var("CRATON_FIELD_TRACE").is_ok();
            SLOT.publish(if on { 2 } else { 1 });
            on
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------



/// Internal names of classes provided by jars appended at runtime via
/// `Instrumentation.appendToBootstrapClassLoaderSearch` (Mockito injects its
/// `MockMethodDispatcher` this way and asserts a null defining loader).
/// `ClassLoader.loadClass`'s native parent-first delegation consults this so
/// an overriding user loader (e.g. Spring's `@CompileWithForkedClassLoader`
/// fork, which re-defines every resolvable name from resources) still lets
/// the BOOTSTRAP loader serve these classes -- exactly what HotSpot does,
/// since parent delegation always runs before `findClass`.
///
/// Per VM since interpreter round i1 wave 23, lane L5: an agent's append is to
/// ITS VM's bootstrap search, and a process-wide set made every other VM's
/// user loaders treat those names as bootstrap classes (parent delegation
/// before their `findClass` override, a miss not authoritative), although that
/// VM's bootstrap path has no such jar. A `ClassManager` field
/// (`bootstrap_appended_classes`) since round 14 wave 2 (MISC11-2).
///
/// Every `.class` entry of an appended bootstrap-search jar or directory.
pub(in crate::class_manager) fn bootstrap_appended_class_names(path: &str) -> Vec<String> {
    // A DIRECTORY (`-Xbootclasspath/a:<dir>`, or an Instrumentation append of
    // one) is recorded from its `.class` files, a snapshot taken now like the
    // jar arm's. It used to be skipped (the ZIP open fails), so every gate
    // keyed on this census -- `Class.forName(name, false, null)` above all --
    // refused its classes. Round 13 wave 10 (lane misc10,
    // `r13w10-misc10-bootstrap-appended-directories-not-censused-FIXED-20260928.md`);
    // `CRATONVM_BOOT_APPEND_DIR_CENSUS=0` skips directories again. Read per
    // append (rare), so no memo static.
    let dir = std::path::Path::new(path);
    if dir.is_dir() {
        if !true {
            return Vec::new();
        }
        let mut names: Vec<String> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let Some(rel) = p.strip_prefix(dir).ok().and_then(|r| r.to_str()) else {
                    continue;
                };
                if let Some(stem) = rel.strip_suffix(".class") {
                    let name = stem.replace('\\', "/");
                    if !name.starts_with("META-INF") {
                        names.push(name);
                    }
                }
            }
        }
        return names;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let Ok(mut archive) = zip::ZipArchive::new(std::io::BufReader::new(file)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index_raw(i) {
            let Ok(name) = entry.name() else { continue };
            if let Some(stripped) = name.strip_suffix(".class") {
                if !stripped.starts_with("META-INF") {
                    names.push(stripped.to_string());
                }
            }
        }
    }
    names
}

/// The ELEMENT class of a reference-array descriptor, in internal/slash form.
///
/// `[Lp/X;` → `Some("p/X")`; `[[[Lp/X;` → `Some("p/X")` (every dimension is
/// stripped, because §5.3.3 recurses through them to the same element class);
/// `[I` / `[[D` → `None` (primitive element — there is no element *class* to
/// load); a non-array name, or a malformed descriptor, → `None`.
///
/// Exists so a caller can tell the two failure shapes of array-class creation
/// apart, which look identical at the `Err` boundary. JVMS §5.3.3 creates an
/// array class *from* its element type — no `.class` file for the array itself
/// is ever consulted — so an absent element means the **requested** thing is
/// absent, not that a dependency of something already found is missing. The
/// two must not be conflated:
///
/// * absent element → `ClassNotFoundException` (the request itself failed),
/// * present element whose own supertype/interface is absent →
///   `NoClassDefFoundError` naming that supertype (JVMS §5.3/§5.4).
///
/// Verified against HotSpot 25 on 2026-08-06:
/// `Class.forName("[Lcom.cratonvm.absent.NoSuchClass;")` throws a bare
/// `ClassNotFoundException` (cause `null`) — never a `NoClassDefFoundError`.
/// Because `ClassNotFoundException` is a checked `Exception` and
/// `NoClassDefFoundError` is an `Error`, a `catch (ClassNotFoundException)`
/// does not catch the latter, so getting this wrong is not a cosmetic
/// message difference — it escapes the caller's handler entirely.
///
/// See `docs/feature-designs/class-resolution-and-caching.md`.
pub fn array_descriptor_element_class(descriptor: &str) -> Option<&str> {
    let element = descriptor.trim_start_matches('[');
    if element.len() == descriptor.len() {
        // No leading `[` was stripped — not an array descriptor at all.
        return None;
    }
    element
        .strip_prefix('L')
        .and_then(|inner| inner.strip_suffix(';'))
        .filter(|inner| !inner.is_empty())
}

impl ClassManager {
    /// Whether `internal` (slash-form) names a class made loadable by a jar or
    /// directory appended at runtime to THIS VM's bootstrap search. Natives
    /// reach it through `NativeContext::is_bootstrap_appended_class`.
    pub fn is_bootstrap_appended_class(&self, internal: &str) -> bool {
        self.bootstrap_appended_classes.contains(internal)
    }

    /// A handle to this manager's "some class was ever appended to the
    /// bootstrap search" flag, for a reader that must not take the
    /// class-manager lock (round 14 wave 3, lane fixup, RV3-4). `false` means
    /// [`Self::is_bootstrap_appended_class`] answers `false` for every name.
    /// The VM's `ClassRealm` takes one at construction, like
    /// [`Self::hidden_class_bits`].
    pub fn bootstrap_append_seen(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.bootstrap_append_seen)
    }
    /// The bootstrap-search entry of an appended package (see
    /// [`Self::bootstrap_append_packages`]) once the boot loader has DEFINED a
    /// class of it, `None` before (HotSpot's package table holds only
    /// packages a loaded class put there) and for any other package.
    pub fn boot_append_package_location(&self, package: &str) -> Option<&str> {
        let (path, names) = self.bootstrap_append_packages.get(package)?;
        self.boot_append_package_loaded(names)
            .then_some(path.as_str())
    }

    /// The appended packages the boot loader has defined a class in (see
    /// [`Self::boot_append_package_location`]), unsorted.
    pub fn boot_append_loaded_packages(&self) -> Vec<String> {
        self.bootstrap_append_packages
            .iter()
            .filter(|(_, (_, names))| self.boot_append_package_loaded(names))
            .map(|(package, _)| package.clone())
            .collect()
    }

    /// Has the boot loader defined one of `names`? The per-loader index only
    /// (`class_defined_by_loader_exact`'s cold fallback walks the whole store
    /// on a miss, once per name).
    pub(in crate::class_manager) fn boot_append_package_loaded(&self, names: &[String]) -> bool {
        names.iter().any(|name| {
            loaded_classes_probe(&self.loaded_classes, ClassLoaderId::Bootstrap, name).is_some()
        })
    }
}


impl ClassManager {
    /// Rebuild the compact field layout of every loaded class.
    ///
    /// Called once at VM init when compressed oops are enabled, after the
    /// bootstrap class set has been laid out with wide references but before
    /// any instance of those classes exists. See
    /// [`ClassStore::recompute_all_compact_layouts`].
    pub fn recompute_all_compact_layouts(&self) -> usize {
        self.class_store.recompute_all_compact_layouts()
    }
}

